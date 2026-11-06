use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn sample_fortran_bot_runs_through_runtime_ipc() {
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping sample bot acceptance test");
        return;
    };
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping sample bot acceptance test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-acceptance-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let bot_bin = out_dir.join("sample_moderation");
    let mock_gateway = out_dir.join("mock_gateway.jsonl");
    let audit_log = out_dir.join("audit.jsonl");

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    let compile_bot = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "examples/sample_moderation/sample_moderation.f",
        ])
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&bot_bin)
        .status()
        .expect("failed to run gfortran for sample bot");
    assert!(compile_bot.success());
    std::fs::write(
        &mock_gateway,
        concat!(
            "{\"op\":0,\"s\":1,\"t\":\"READY\",\"d\":{\"session_id\":\"abc\",\"resume_gateway_url\":\"wss://resume\",\"user\":{\"id\":\"100000000000000009\"}}}\n",
            "{\"op\":0,\"s\":2,\"t\":\"GUILD_CREATE\",\"d\":{\"id\":\"100000000000000001\",\"owner_id\":\"100000000000000009\",\"channels\":[{\"id\":\"100000000000000002\",\"type\":0,\"parent_id\":\"100000000000000010\"},{\"id\":\"100000000000000006\",\"type\":0,\"parent_id\":null}],\"roles\":[{\"id\":\"100000000000000001\",\"permissions\":\"0\",\"position\":0},{\"id\":\"100000000000000005\",\"permissions\":\"0\",\"position\":1}],\"members\":[{\"user\":{\"id\":\"100000000000000009\"},\"roles\":[],\"communication_disabled_until\":null},{\"user\":{\"id\":\"100000000000000004\"},\"roles\":[\"100000000000000005\"],\"communication_disabled_until\":null}]}}\n",
            "{\"op\":0,\"s\":3,\"t\":\"MESSAGE_CREATE\",\"d\":{\"guild_id\":\"100000000000000001\",\"channel_id\":\"100000000000000002\",\"id\":\"100000000000000003\",\"content\":\"moderate demo\",\"type\":0,\"attachments\":[{\"filename\":\"a.png\",\"content_type\":\"image/png\"},{\"filename\":\"b.jpg\",\"content_type\":\"image/jpeg\"},{\"filename\":\"c.gif\",\"content_type\":\"image/gif\"}],\"author\":{\"id\":\"100000000000000004\",\"bot\":false},\"member\":{\"roles\":[\"100000000000000005\"]}}}\n"
        ),
    )
    .expect("failed to write mock gateway frames");

    let mut child = Command::new(runtime)
        .current_dir(&root)
        .arg("--mock-gateway")
        .arg(&mock_gateway)
        .arg("--audit-log")
        .arg(&audit_log)
        .args(["--dry-run", "--bot"])
        .arg(&bot_bin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run dfor77 sample bot acceptance");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed = String::new();
    loop {
        if let Ok(audit) = std::fs::read_to_string(&audit_log) {
            observed = audit;
            if observed.contains("\"action\":\"D7SND\"")
                && observed.contains("\"action\":\"D7DEL\"")
                && observed.contains("\"action\":\"D7TMO\"")
            {
                break;
            }
        }
        if let Some(status) = child.try_wait().expect("failed to poll sample bot runtime") {
            let output = child
                .wait_with_output()
                .expect("failed to collect finished runtime output");
            panic!(
                "runtime exited before sample actions completed: {status}\nstdout:\n{}\nstderr:\n{}\naudit:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                observed
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("failed to collect timed out runtime output");
            panic!(
                "sample bot acceptance timed out\nstdout:\n{}\nstderr:\n{}\naudit:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                observed
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait_with_output();
    let audit = std::fs::read_to_string(&audit_log).expect("failed to read audit log");
    assert!(audit.contains("\"action\":\"D7SND\""));
    assert!(audit.contains("\"action\":\"D7DEL\""));
    assert!(audit.contains("\"action\":\"D7TMO\""));
    assert!(audit.contains("\"dry_run\":true"));
    assert!(audit.contains("\"guild_id\":\"100000000000000001\""));
    assert!(!audit.contains("DISCORD_BOT_TOKEN"));
    assert!(!audit.contains("Bot "));
    validate_sample_audit_log(runtime, &root, &audit_log);

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(bot_bin);
    let _ = std::fs::remove_file(mock_gateway);
    let _ = std::fs::remove_file(audit_log);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn sample_fortran_bot_executes_http_moderation_actions() {
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping sample bot HTTP acceptance test");
        return;
    };
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping sample bot HTTP acceptance test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir =
        std::env::temp_dir().join(format!("dfor77-http-acceptance-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let (wrapper_obj, bot_bin) = compile_sample_bot(&root, &out_dir);
    let mock_gateway = out_dir.join("mock_gateway.jsonl");
    let audit_log = out_dir.join("audit.jsonl");
    std::fs::write(&mock_gateway, sample_moderation_gateway_frames())
        .expect("failed to write mock gateway frames");

    let Some((base_url, received, server)) = serve_sample_rest() else {
        return;
    };
    let mut child = Command::new(runtime)
        .current_dir(&root)
        .arg("--mock-gateway")
        .arg(&mock_gateway)
        .arg("--mock-rest")
        .arg(&base_url)
        .arg("--audit-log")
        .arg(&audit_log)
        .arg("--bot")
        .arg(&bot_bin)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run dfor77 sample bot HTTP acceptance");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut requests = Vec::new();
    let mut saw_delete = false;
    let mut saw_timeout = false;
    let mut saw_log_send = false;
    while !(saw_delete && saw_timeout && saw_log_send) {
        assert!(
            Instant::now() < deadline,
            "sample bot HTTP acceptance timed out with requests:\n{}",
            requests.join("\n---\n")
        );
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                saw_delete |= request.starts_with(
                    "DELETE /api/v10/channels/100000000000000002/messages/100000000000000003 HTTP/1.1",
                );
                saw_timeout |= request.starts_with(
                    "PATCH /api/v10/guilds/100000000000000001/members/100000000000000004 HTTP/1.1",
                );
                saw_log_send |= request
                    .starts_with("POST /api/v10/channels/100000000000000006/messages HTTP/1.1");
                requests.push(request);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait().expect("failed to poll HTTP acceptance") {
                    let output = child
                        .wait_with_output()
                        .expect("failed to collect finished HTTP acceptance output");
                    panic!(
                        "runtime exited before HTTP moderation completed: {status}\nstdout:\n{}\nstderr:\n{}\nrequests:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                        requests.join("\n---\n")
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    server.join().unwrap();
    let audit_deadline = Instant::now() + Duration::from_secs(5);
    let mut audit = String::new();
    loop {
        if let Ok(content) = std::fs::read_to_string(&audit_log) {
            audit = content;
        }
        if audit.contains("\"action\":\"D7DEL\"")
            && audit.contains("\"action\":\"D7TMO\"")
            && audit.contains("\"action\":\"D7SND\"")
        {
            break;
        }
        if Instant::now() >= audit_deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("failed to collect timed out HTTP acceptance output");
            panic!(
                "audit did not contain all HTTP moderation actions\nstdout:\n{}\nstderr:\n{}\naudit:\n{}\nrequests:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                audit,
                requests.join("\n---\n")
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("failed to collect HTTP acceptance output");

    assert!(saw_delete);
    assert!(saw_timeout);
    assert!(saw_log_send);
    let joined = requests.join("\n---\n");
    assert!(joined.contains("\r\nAuthorization: Bot secret-token\r\n"));
    assert!(joined.contains("X-Audit-Log-Reason: Demo%20moderation"));
    assert!(joined.contains("\"communication_disabled_until\""));
    assert!(joined.contains("Demo moderation result"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-token"));

    assert!(audit.contains("\"action\":\"D7DEL\""));
    assert!(audit.contains("\"action\":\"D7TMO\""));
    assert!(audit.contains("\"action\":\"D7SND\""));
    assert!(audit.contains("\"status\":0"));
    assert!(audit.contains("\"dry_run\":false"));
    assert!(audit.contains("\"committed\":true"));
    assert!(!audit.contains("secret-token"));
    validate_sample_audit_log(runtime, &root, &audit_log);

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(bot_bin);
    let _ = std::fs::remove_file(mock_gateway);
    let _ = std::fs::remove_file(audit_log);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn strict_fortran_rich_actions_execute_http_without_extension() {
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping strict rich HTTP test");
        return;
    };
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping strict rich HTTP test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-strict-http-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("strict_http.f");
    let program_bin = out_dir.join("strict_http");
    let mock_gateway = out_dir.join("mock_gateway.jsonl");
    let audit_log = out_dir.join("audit.jsonl");
    let result_file = out_dir.join("result.txt");
    let first_file = out_dir.join("a.txt");
    let second_file = out_dir.join("b.bin");
    std::fs::write(&mock_gateway, sample_gateway_frames())
        .expect("failed to write rich mock gateway frames");
    std::fs::write(&first_file, b"first-body").expect("failed to write first upload file");
    std::fs::write(&second_file, b"second-body").expect("failed to write second upload file");

    let source = strict_rich_http_program();
    assert_strict_fortran_program(&source);
    std::fs::write(&program_src, source).expect("failed to write strict rich HTTP program");

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for strict rich HTTP wrapper");
    assert!(compile_wrapper.success());

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-72"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for strict rich HTTP program");
    assert!(compile_program.success());

    let Some((base_url, received, server)) = serve_rich_rest() else {
        return;
    };
    let mut child = Command::new(runtime)
        .current_dir(&out_dir)
        .arg("--mock-gateway")
        .arg(&mock_gateway)
        .arg("--mock-rest")
        .arg(&base_url)
        .arg("--audit-log")
        .arg(&audit_log)
        .arg("--bot")
        .arg(&program_bin)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run strict rich HTTP acceptance");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut requests = Vec::new();
    let mut saw_sticker = false;
    let mut saw_poll = false;
    let mut saw_reaction = false;
    let mut saw_upload = false;
    while !(saw_sticker && saw_poll && saw_reaction && saw_upload) {
        assert!(
            Instant::now() < deadline,
            "strict rich HTTP acceptance timed out with requests:\n{}",
            requests.join("\n---\n")
        );
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                let first_line = request.lines().next().unwrap_or("").to_string();
                saw_sticker |= first_line
                    .starts_with("POST /api/v10/channels/100000000000000002/messages ")
                    && request.contains(r#""sticker_ids":["100000000000000011"]"#);
                saw_poll |= first_line
                    .starts_with("POST /api/v10/channels/100000000000000002/messages ")
                    && request.contains("\"question\":{\"text\":\"Deploy now?\"}");
                saw_reaction |= first_line.starts_with(
                    "PUT /api/v10/channels/100000000000000002/messages/100000000000000003/reactions/party%3A100000000000000012/@me ",
                );
                saw_upload |= first_line
                    .starts_with("POST /api/v10/channels/100000000000000002/messages ")
                    && request.contains("filename=\"a.txt\"")
                    && request.contains("filename=\"b.bin\"")
                    && request.contains("first-body")
                    && request.contains("second-body");
                requests.push(request);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait().expect("failed to poll rich HTTP") {
                    let output = child
                        .wait_with_output()
                        .expect("failed to collect finished rich HTTP output");
                    panic!(
                        "runtime exited before rich HTTP actions completed: {status}\nstdout:\n{}\nstderr:\n{}\nrequests:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                        requests.join("\n---\n")
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    server.join().unwrap();
    let audit_deadline = Instant::now() + Duration::from_secs(5);
    let mut audit = String::new();
    loop {
        if let Ok(content) = std::fs::read_to_string(&audit_log) {
            audit = content;
        }
        if audit.contains("\"action\":\"D7STK\"")
            && audit.contains("\"action\":\"D7POL\"")
            && audit.contains("\"action\":\"D7RAC\"")
            && audit.contains("\"action\":\"D7UPM\"")
        {
            break;
        }
        if Instant::now() >= audit_deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("failed to collect timed out rich HTTP output");
            panic!(
                "audit did not contain all rich HTTP actions\nstdout:\n{}\nstderr:\n{}\naudit:\n{}\nrequests:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                audit,
                requests.join("\n---\n")
            );
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("failed to collect strict rich HTTP output");
    let statuses = std::fs::read_to_string(&result_file).expect("failed to read rich result");
    let parsed = statuses
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid rich status"))
        .collect::<Vec<_>>();
    assert_eq!(parsed, vec![0, 0, 0, 0]);
    let joined = requests.join("\n---\n");
    assert!(joined.contains("\r\nAuthorization: Bot secret-token\r\n"));
    assert!(joined.contains("\"content\":\"sticker\""));
    assert!(joined.contains("\"content\":\"poll msg\""));
    assert!(joined.contains("\"allow_multiselect\":true"));
    assert!(joined.contains("multipart/form-data; boundary=dfor77-"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-token"));
    assert!(audit.contains("\"status\":0"));
    assert!(audit.contains("\"committed\":true"));
    assert!(!audit.contains("secret-token"));
    assert!(!audit.contains("sticker"));
    assert!(!audit.contains("poll msg"));
    assert!(!audit.contains("first-body"));

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(mock_gateway);
    let _ = std::fs::remove_file(audit_log);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_file(first_file);
    let _ = std::fs::remove_file(second_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn strict_fortran_management_rest_executes_http_without_extension() {
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping strict management HTTP test");
        return;
    };
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping strict management HTTP test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-strict-mgmt-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("strict_mgmt.f");
    let program_bin = out_dir.join("strict_mgmt");
    let mock_gateway = out_dir.join("mock_gateway.jsonl");
    let audit_log = out_dir.join("audit.jsonl");
    let result_file = out_dir.join("result.txt");
    std::fs::write(&mock_gateway, sample_gateway_frames())
        .expect("failed to write management mock gateway frames");

    let source = strict_management_http_program();
    assert_strict_fortran_program(&source);
    std::fs::write(&program_src, source).expect("failed to write strict management HTTP program");

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for strict management wrapper");
    assert!(compile_wrapper.success());

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-72"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for strict management program");
    assert!(compile_program.success());

    let Some((base_url, received, server)) = serve_management_rest() else {
        return;
    };
    let mut child = Command::new(runtime)
        .current_dir(&out_dir)
        .arg("--mock-gateway")
        .arg(&mock_gateway)
        .arg("--mock-rest")
        .arg(&base_url)
        .arg("--audit-log")
        .arg(&audit_log)
        .arg("--bot")
        .arg(&program_bin)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run strict management HTTP acceptance");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut requests = Vec::new();
    let mut saw_ccr = false;
    let mut saw_ech = false;
    let mut saw_dch = false;
    let mut saw_cro = false;
    let mut saw_ero = false;
    let mut saw_dro = false;
    let mut saw_amr = false;
    let mut saw_rmr = false;
    let mut saw_egm = false;
    let mut saw_kik = false;
    let mut saw_ban = false;
    let mut saw_ubn = false;
    let mut saw_egd = false;
    let mut saw_raw = false;
    while !(saw_ccr
        && saw_ech
        && saw_dch
        && saw_cro
        && saw_ero
        && saw_dro
        && saw_amr
        && saw_rmr
        && saw_egm
        && saw_kik
        && saw_ban
        && saw_ubn
        && saw_egd
        && saw_raw)
    {
        assert!(
            Instant::now() < deadline,
            "strict management HTTP acceptance timed out with requests:\n{}",
            requests.join("\n---\n")
        );
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                let first_line = request.lines().next().unwrap_or("").to_string();
                saw_ccr |= first_line
                    .starts_with("POST /api/v10/guilds/100000000000000001/channels ")
                    && request.contains(r#""name":"bot-log""#);
                saw_ech |= first_line.starts_with("PATCH /api/v10/channels/100000000000000002 ")
                    && request.contains(r#""name":"edited""#);
                saw_dch |= first_line.starts_with("DELETE /api/v10/channels/100000000000000002 ");
                saw_cro |= first_line.starts_with("POST /api/v10/guilds/100000000000000001/roles ")
                    && request.contains(r#""name":"helper""#);
                saw_ero |= first_line.starts_with(
                    "PATCH /api/v10/guilds/100000000000000001/roles/100000000000000005 ",
                ) && request.contains(r#""name":"helper2""#);
                saw_dro |= first_line.starts_with(
                    "DELETE /api/v10/guilds/100000000000000001/roles/100000000000000005 ",
                );
                saw_amr |= first_line.starts_with(
                    "PUT /api/v10/guilds/100000000000000001/members/100000000000000004/roles/100000000000000005 ",
                );
                saw_rmr |= first_line.starts_with(
                    "DELETE /api/v10/guilds/100000000000000001/members/100000000000000004/roles/100000000000000005 ",
                );
                saw_egm |= first_line.starts_with(
                    "PATCH /api/v10/guilds/100000000000000001/members/100000000000000004 ",
                ) && request.contains(r#""nick":"bot""#);
                saw_kik |= first_line.starts_with(
                    "DELETE /api/v10/guilds/100000000000000001/members/100000000000000004 ",
                );
                saw_ban |= first_line
                    .starts_with("PUT /api/v10/guilds/100000000000000001/bans/100000000000000004 ")
                    && request.contains("{}");
                saw_ubn |= first_line.starts_with(
                    "DELETE /api/v10/guilds/100000000000000001/bans/100000000000000004 ",
                );
                saw_egd |= first_line.starts_with("PATCH /api/v10/guilds/100000000000000001 ")
                    && request.contains(r#""name":"guild""#);
                saw_raw |= first_line.starts_with("GET /api/v10/users/@me ");
                requests.push(request);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait().expect("failed to poll management HTTP") {
                    let output = child
                        .wait_with_output()
                        .expect("failed to collect finished management HTTP output");
                    panic!(
                        "runtime exited before management HTTP actions completed: {status}\nstdout:\n{}\nstderr:\n{}\nrequests:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                        requests.join("\n---\n")
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    server.join().unwrap();
    let audit_deadline = Instant::now() + Duration::from_secs(5);
    let mut audit = String::new();
    loop {
        if let Ok(content) = std::fs::read_to_string(&audit_log) {
            audit = content;
        }
        if audit.matches("\"action\":\"D7RST\"").count() >= 14 {
            break;
        }
        if Instant::now() >= audit_deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("failed to collect timed out management HTTP output");
            panic!(
                "audit did not contain management REST actions\nstdout:\n{}\nstderr:\n{}\naudit:\n{}\nrequests:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                audit,
                requests.join("\n---\n")
            );
        }
        thread::sleep(Duration::from_millis(50));
    }

    let result_deadline = Instant::now() + Duration::from_secs(5);
    let parsed = loop {
        let statuses = std::fs::read_to_string(&result_file).unwrap_or_default();
        let parsed = statuses
            .split_whitespace()
            .map(|value| value.parse::<i32>().expect("invalid management status"))
            .collect::<Vec<_>>();
        if parsed.len() >= 17 {
            break parsed;
        }
        if Instant::now() >= result_deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("failed to collect timed out management result output");
            panic!(
                "management result did not finish\nstdout:\n{}\nstderr:\n{}\nresult:\n{}\nrequests:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                statuses,
                requests.join("\n---\n")
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("failed to collect strict management HTTP output");
    assert_eq!(parsed.len(), 17);
    assert!(parsed[..15].iter().all(|status| *status == 0), "{parsed:?}");
    assert!(parsed[15] >= 20, "{parsed:?}");
    assert_eq!(parsed[16], 0, "{parsed:?}");
    let joined = requests.join("\n---\n");
    assert!(joined.contains("\r\nAuthorization: Bot secret-token\r\n"));
    assert!(joined.contains("X-Audit-Log-Reason: mgmt"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-token"));
    assert!(audit.contains("\"status\":0"));
    assert!(audit.contains("\"committed\":true"));
    assert!(!audit.contains("secret-token"));
    assert!(!audit.contains("bot-log"));
    assert!(!audit.contains("helper2"));

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(mock_gateway);
    let _ = std::fs::remove_file(audit_log);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn strict_fortran_misc_actions_execute_http_without_extension() {
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping strict misc HTTP test");
        return;
    };
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping strict misc HTTP test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-strict-misc-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("strict_misc.f");
    let program_bin = out_dir.join("strict_misc");
    let mock_gateway = out_dir.join("mock_gateway.jsonl");
    let audit_log = out_dir.join("audit.jsonl");
    let result_file = out_dir.join("result.txt");
    let upload_file = out_dir.join("c.txt");
    std::fs::write(&mock_gateway, sample_gateway_frames())
        .expect("failed to write misc mock gateway frames");
    std::fs::write(&upload_file, b"single-body").expect("failed to write single upload file");

    let source = strict_misc_http_program();
    assert_strict_fortran_program(&source);
    std::fs::write(&program_src, source).expect("failed to write strict misc HTTP program");

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for strict misc wrapper");
    assert!(compile_wrapper.success());

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-72"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for strict misc program");
    assert!(compile_program.success());

    let Some((base_url, received, server)) = serve_misc_rest() else {
        return;
    };
    let mut child = Command::new(runtime)
        .current_dir(&out_dir)
        .arg("--mock-gateway")
        .arg(&mock_gateway)
        .arg("--mock-rest")
        .arg(&base_url)
        .arg("--audit-log")
        .arg(&audit_log)
        .arg("--bot")
        .arg(&program_bin)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run strict misc HTTP acceptance");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut requests = Vec::new();
    let mut saw_json_send = false;
    let mut saw_upload = false;
    let mut saw_callback = false;
    let mut saw_command = false;
    let mut saw_typing = false;
    while !(saw_json_send && saw_upload && saw_callback && saw_command && saw_typing) {
        assert!(
            Instant::now() < deadline,
            "strict misc HTTP acceptance timed out with requests:\n{}",
            requests.join("\n---\n")
        );
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                let first_line = request.lines().next().unwrap_or("").to_string();
                saw_json_send |= first_line
                    .starts_with("POST /api/v10/channels/100000000000000002/messages ")
                    && request.contains(r#""content":"json send""#);
                saw_upload |= first_line
                    .starts_with("POST /api/v10/channels/100000000000000002/messages ")
                    && request.contains("filename=\"single.txt\"")
                    && request.contains("single-body");
                saw_callback |= first_line
                    .starts_with("POST /api/v10/interactions/100000000000000006/tok-123/callback ")
                    && request.contains(r#""type":4"#);
                saw_command |= first_line.starts_with(
                    "POST /api/v10/applications/100000000000000007/guilds/100000000000000001/commands ",
                ) && request.contains(r#""name":"ping""#);
                saw_typing |=
                    first_line.starts_with("POST /api/v10/channels/100000000000000002/typing ");
                requests.push(request);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait().expect("failed to poll misc HTTP") {
                    let output = child
                        .wait_with_output()
                        .expect("failed to collect finished misc HTTP output");
                    panic!(
                        "runtime exited before misc HTTP actions completed: {status}\nstdout:\n{}\nstderr:\n{}\nrequests:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                        requests.join("\n---\n")
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    server.join().unwrap();
    let audit_deadline = Instant::now() + Duration::from_secs(5);
    let mut audit = String::new();
    loop {
        if let Ok(content) = std::fs::read_to_string(&audit_log) {
            audit = content;
        }
        if audit.contains("\"action\":\"D7SJP\"")
            && audit.contains("\"action\":\"D7UPL\"")
            && audit.contains("\"action\":\"D7ICB\"")
            && audit.contains("\"action\":\"D7ACR\"")
            && audit.contains("\"action\":\"D7TPI\"")
        {
            break;
        }
        if Instant::now() >= audit_deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("failed to collect timed out misc HTTP output");
            panic!(
                "audit did not contain all misc actions\nstdout:\n{}\nstderr:\n{}\naudit:\n{}\nrequests:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                audit,
                requests.join("\n---\n")
            );
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("failed to collect strict misc HTTP output");
    let statuses = std::fs::read_to_string(&result_file).expect("failed to read misc result");
    let parsed = statuses
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid misc status"))
        .collect::<Vec<_>>();
    assert_eq!(parsed, vec![0, 0, 0, 0, 0]);
    let joined = requests.join("\n---\n");
    assert!(joined.contains("\r\nAuthorization: Bot secret-token\r\n"));
    assert!(joined.contains("multipart/form-data; boundary=dfor77-"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-token"));
    assert!(audit.contains("\"status\":0"));
    assert!(audit.contains("\"committed\":true"));
    assert!(!audit.contains("secret-token"));
    assert!(!audit.contains("json send"));
    assert!(!audit.contains("single-body"));

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(mock_gateway);
    let _ = std::fs::remove_file(audit_log);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_file(upload_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn strict_fortran_rest_errors_map_without_extension() {
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping strict REST error test");
        return;
    };
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping strict REST error test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-strict-errors-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("strict_errors.f");
    let program_bin = out_dir.join("strict_errors");
    let mock_gateway = out_dir.join("mock_gateway.jsonl");
    let audit_log = out_dir.join("audit.jsonl");
    let result_file = out_dir.join("result.txt");
    std::fs::write(&mock_gateway, sample_gateway_frames())
        .expect("failed to write REST error mock gateway frames");

    let source = strict_rest_error_program();
    assert_strict_fortran_program(&source);
    std::fs::write(&program_src, source).expect("failed to write strict REST error program");

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for strict REST error wrapper");
    assert!(compile_wrapper.success());

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-72"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for strict REST error program");
    assert!(compile_program.success());

    let Some((base_url, received, server)) = serve_error_rest() else {
        return;
    };
    let mut child = Command::new(runtime)
        .current_dir(&out_dir)
        .arg("--mock-gateway")
        .arg(&mock_gateway)
        .arg("--mock-rest")
        .arg(&base_url)
        .arg("--audit-log")
        .arg(&audit_log)
        .arg("--bot")
        .arg(&program_bin)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run strict REST error acceptance");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut requests = Vec::new();
    let expected_content = rest_error_contents();
    let mut seen_content = HashSet::new();
    let mut saw_read = false;
    while !(seen_content.len() == expected_content.len() && saw_read) {
        assert!(
            Instant::now() < deadline,
            "strict REST error acceptance timed out with requests:\n{}",
            requests.join("\n---\n")
        );
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                let first_line = request.lines().next().unwrap_or("").to_string();
                if first_line.starts_with("POST /api/v10/channels/100000000000000002/messages ") {
                    for content in expected_content {
                        if request_has_content(&request, content) {
                            seen_content.insert(*content);
                        }
                    }
                }
                saw_read |= first_line.starts_with("GET /api/v10/users/@me ");
                requests.push(request);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait().expect("failed to poll REST error HTTP") {
                    let output = child
                        .wait_with_output()
                        .expect("failed to collect finished REST error output");
                    panic!(
                        "runtime exited before REST errors completed: {status}\nstdout:\n{}\nstderr:\n{}\nrequests:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                        requests.join("\n---\n")
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    server.join().unwrap();
    let result_deadline = Instant::now() + Duration::from_secs(5);
    let parsed = loop {
        let statuses = std::fs::read_to_string(&result_file).unwrap_or_default();
        let parsed = statuses
            .split_whitespace()
            .map(|value| value.parse::<i32>().expect("invalid REST error status"))
            .collect::<Vec<_>>();
        if parsed.len() >= 105 {
            break parsed;
        }
        if Instant::now() >= result_deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("failed to collect timed out REST error output");
            panic!(
                "REST error result did not finish\nstdout:\n{}\nstderr:\n{}\nresult:\n{}\nrequests:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                statuses,
                requests.join("\n---\n")
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("failed to collect strict REST error output");
    assert_eq!(
        parsed,
        vec![
            42, 0, 42, 400, 50035, 43, 0, 43, 429, 0, 43, 0, 43, 400, 20016, 43, 0, 43, 400, 20028,
            43, 0, 43, 400, 20029, 40, 0, 40, 403, 50013, 40, 0, 40, 403, 50001, 41, 0, 41, 404,
            10003, 41, 0, 41, 404, 10004, 41, 0, 41, 404, 10007, 41, 0, 41, 404, 10008, 41, 0, 41,
            404, 10013, 40, 0, 40, 400, 50083, 40, 0, 40, 400, 50021, 40, 0, 40, 400, 50068, 40, 0,
            40, 403, 160005, 40, 0, 40, 400, 40058, 40, 0, 40, 400, 50024, 42, 0, 42, 400, 0, 53,
            0, 53, 500, 0, 50, 0, 50, 500, 0,
        ]
    );
    let joined = requests.join("\n---\n");
    assert!(joined.contains("\r\nAuthorization: Bot secret-token\r\n"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-token"));
    let audit = std::fs::read_to_string(&audit_log).expect("failed to read REST error audit");
    assert!(audit.matches("\"action\":\"D7RST\"").count() >= 21);
    assert!(audit.contains("\"status\":42"));
    assert!(audit.contains("\"status\":43"));
    assert!(audit.contains("\"status\":40"));
    assert!(audit.contains("\"status\":41"));
    assert!(audit.contains("\"status\":53"));
    assert!(audit.contains("\"status\":50"));
    for why in [
        "INVALID_FORM_BODY",
        "RATE_LIMITED",
        "MISSING_PERMISSION",
        "MISSING_ACCESS",
        "CHANNEL_NOT_FOUND",
        "GUILD_NOT_FOUND",
        "MEMBER_NOT_FOUND",
        "MESSAGE_NOT_FOUND",
        "USER_NOT_FOUND",
        "THREAD_ARCHIVED",
        "SYSTEM_MESSAGE_ACTION_FORBIDDEN",
        "INVALID_MESSAGE_TYPE",
        "THREAD_LOCKED",
        "CANNOT_SEND_TO_FORUM_PARENT",
        "CHANNEL_TYPE_UNSUPPORTED",
        "UNKNOWN_BAD_REQUEST",
        "UNKNOWN_COMMIT",
        "SNAPSHOT_INCOMPLETE",
    ] {
        assert!(audit.contains(&format!("\"why\":\"{why}\"")), "{why}");
    }
    assert!(!audit.contains("secret-token"));
    for content in expected_content {
        assert!(!request_has_content(&audit, content), "{content}");
    }

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(mock_gateway);
    let _ = std::fs::remove_file(audit_log);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn strict_fortran_preflights_evaluate_runtime_snapshots_without_extension() {
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping strict runtime preflight test");
        return;
    };
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping strict runtime preflight test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-strict-pfrun-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("strict_pfrun.f");
    let program_bin = out_dir.join("strict_pfrun");
    let mock_gateway = out_dir.join("mock_gateway.jsonl");
    let result_file = out_dir.join("result.txt");
    std::fs::write(&mock_gateway, strict_preflight_runtime_gateway_frames())
        .expect("failed to write runtime preflight mock gateway frames");

    let source = strict_preflight_runtime_program();
    assert_strict_fortran_program(&source);
    std::fs::write(&program_src, source).expect("failed to write strict runtime preflight program");

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for strict runtime preflight wrapper");
    assert!(compile_wrapper.success());

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-72"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for strict runtime preflight program");
    assert!(compile_program.success());

    let output = Command::new(runtime)
        .current_dir(&out_dir)
        .arg("--mock-gateway")
        .arg(&mock_gateway)
        .args(["--dry-run", "--bot"])
        .arg(&program_bin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run strict runtime preflight acceptance");
    assert!(
        output.status.success(),
        "strict runtime preflight failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = std::fs::read_to_string(&result_file).expect("failed to read runtime preflight");
    let rows = result
        .lines()
        .map(|line| {
            line.split_whitespace()
                .map(|value| value.parse::<i32>().expect("invalid preflight row"))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 30, "{result}");
    let expected = [
        (1, 1, 1),
        (1, 2, 1),
        (1, 3, 1),
        (1, 4, 1),
        (1, 5, 1),
        (1, 6, 1),
        (2, 1, 0),
        (2, 2, 0),
        (2, 3, 0),
        (2, 4, 0),
        (2, 5, 0),
        (2, 6, 0),
        (3, 1, 0),
        (3, 2, 1),
        (3, 3, 1),
        (3, 4, 0),
        (3, 5, 1),
        (3, 6, 1),
        (4, 1, 0),
        (4, 2, 1),
        (4, 3, 1),
        (4, 4, 0),
        (4, 5, 1),
        (4, 6, 1),
        (5, 1, 0),
        (5, 2, 1),
        (5, 3, 1),
        (5, 4, 0),
        (5, 5, 1),
        (5, 6, 1),
    ];
    for (row, (event, op, can)) in rows.iter().zip(expected) {
        assert_eq!(row[0], event, "{row:?}");
        assert_eq!(row[1], op, "{row:?}");
        assert_eq!(row[2], 0, "{row:?}");
        assert_eq!(row[3], can, "{row:?}");
        if can == 0 {
            assert!(row[4] > 0, "{row:?}");
        }
    }

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(mock_gateway);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn strict_fortran_bot_uses_broad_api_surface_without_extension() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping strict API surface test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-strict-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("strict_api.f");
    let program_bin = out_dir.join("strict_api");
    let result_file = out_dir.join("result.txt");
    let source = strict_api_surface_program();
    assert_strict_fortran_program(&source);
    std::fs::write(&program_src, source).expect("failed to write strict API program");

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for strict wrapper");
    assert!(compile_wrapper.success());

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-72"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for strict API program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .current_dir(&out_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run strict API program");
    child
        .stdin
        .as_mut()
        .expect("missing strict API stdin")
        .write_all(strict_api_surface_responses().as_bytes())
        .expect("failed to write strict API fake responses");
    let output = child
        .wait_with_output()
        .expect("failed to wait for strict API program");
    assert!(
        output.status.success(),
        "strict API program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read strict result");
    let failures = result
        .split_whitespace()
        .next()
        .expect("missing strict result")
        .parse::<i32>()
        .expect("invalid strict result");
    assert_eq!(
        failures,
        0,
        "strict API program reported failures\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for verb in [
        "INI", "NXT", "GET", "ADM", "CAD", "CDL", "CTM", "CSN", "CDP", "CTP", "CSP", "SND", "SJP",
        "DEL", "TMO", "UNT", "STK", "POL", "UPL", "UPM", "ICB", "ACR", "TPI", "RAC", "RST", "RBD",
        "GWS", "PRS", "VST", "GOP", "GRT", "ERR", "WHY", "LOG", "END",
    ] {
        assert!(
            stdout.contains(&format!(" {verb} ")),
            "missing strict verb {verb}"
        );
    }

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn strict_fortran_payload_builders_match_json_without_extension() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping strict payload test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir =
        std::env::temp_dir().join(format!("dfor77-strict-payload-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("strict_payload.f");
    let program_bin = out_dir.join("strict_payload");
    let result_file = out_dir.join("result.txt");
    let source = strict_payload_builder_program();
    assert_strict_fortran_program(&source);
    std::fs::write(&program_src, source).expect("failed to write strict payload program");

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for strict payload wrapper");
    assert!(compile_wrapper.success());

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-72"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for strict payload program");
    assert!(compile_program.success());

    let output = Command::new(&program_bin)
        .current_dir(&out_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run strict payload program");
    assert!(
        output.status.success(),
        "strict payload program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = std::fs::read_to_string(&result_file).expect("failed to read strict payload");
    let rows = result
        .lines()
        .map(parse_strict_payload_row)
        .collect::<Vec<_>>();
    assert_payload_row(&rows[0], "JES", 0, r#"A\"B"#);
    assert_payload_row(
        &rows[1],
        "PEM",
        0,
        r#"{"content":"c","allowed_mentions":{"parse":[]},"embeds":[{"title":"t","description":"d","color":1}]}"#,
    );
    assert_payload_row(
        &rows[2],
        "PE2",
        0,
        r#"{"content":"c","allowed_mentions":{"parse":[]},"embeds":[{"title":"t","description":"d","color":1}]}"#,
    );
    assert_payload_row(
        &rows[3],
        "PBT",
        0,
        r#"{"content":"c","allowed_mentions":{"parse":[]},"components":[{"type":1,"components":[{"type":2,"style":1,"label":"OK","custom_id":"btn"}]}]}"#,
    );
    assert_payload_row(
        &rows[4],
        "IMR",
        0,
        r#"{"type":4,"data":{"content":"ok","allowed_mentions":{"parse":[]},"flags":64}}"#,
    );
    assert_payload_row(&rows[5], "IDR", 0, r#"{"type":5}"#);
    assert_payload_row(
        &rows[6],
        "IMD",
        0,
        r#"{"type":9,"data":{"title":"m","custom_id":"modal","components":[{"type":1,"components":[{"type":4,"custom_id":"field","label":"answer","style":1,"required":true}]}]}}"#,
    );
    assert_payload_row(
        &rows[7],
        "SCM",
        0,
        r#"{"name":"hello","description":"say","type":1}"#,
    );
    assert_payload_row(
        &rows[8],
        "SCX",
        0,
        r#"{"name":"search","description":"find","type":1,"options":[{"type":3,"name":"query","description":"find","required":true},{"type":6,"name":"user","description":"target","required":false}]}"#,
    );
    assert_payload_row(
        &rows[9],
        "PSM",
        0,
        r#"{"content":"choose","allowed_mentions":{"parse":[]},"components":[{"type":1,"components":[{"type":3,"custom_id":"menu","options":[{"label":"Red","value":"red"},{"label":"Blue","value":"blue"}]}]}]}"#,
    );
    assert_payload_row(
        &rows[10],
        "PAC",
        0,
        r#"{"content":"act","allowed_mentions":{"parse":[]},"components":[{"type":1,"components":[{"type":2,"style":1,"label":"Red","custom_id":"ca"},{"type":2,"style":1,"label":"Blue","custom_id":"cb"}]}]}"#,
    );

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn strict_fortran_rich_actions_emit_ipc_without_extension() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping strict action test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir =
        std::env::temp_dir().join(format!("dfor77-strict-actions-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("strict_actions.f");
    let program_bin = out_dir.join("strict_actions");
    let result_file = out_dir.join("result.txt");
    let source = strict_action_request_program();
    assert_strict_fortran_program(&source);
    std::fs::write(&program_src, source).expect("failed to write strict action program");

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for strict action wrapper");
    assert!(compile_wrapper.success());

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-72"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for strict action program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .current_dir(&out_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run strict action program");
    let mut responses = String::new();
    for rid in 1..=4 {
        push_ok(&mut responses, rid);
    }
    child
        .stdin
        .as_mut()
        .expect("missing strict action stdin")
        .write_all(responses.as_bytes())
        .expect("failed to write strict action fake responses");
    let output = child
        .wait_with_output()
        .expect("failed to wait for strict action program");
    assert!(
        output.status.success(),
        "strict action program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = std::fs::read_to_string(&result_file).expect("failed to read strict actions");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid action status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![0, 30, 0, 30, 0, 30, 0, 30]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("@D7/1 REQ 1 STK 5"));
    assert!(stdout.contains("K sticker_id1 raw 18 100000000000000002"));
    assert!(stdout.contains("K sticker_id2 raw 18 100000000000000003"));
    assert!(stdout.contains("@D7/1 REQ 2 POL 9"));
    assert!(stdout.contains("K question pct 15 Deploy%20now%3F"));
    assert!(stdout.contains("K answer3 pct 5 later"));
    assert!(stdout.contains("@D7/1 REQ 3 RAC 5"));
    assert!(stdout.contains("K emoji pct 26 party%3A100000000000000003"));
    assert!(stdout.contains("K mode raw 1 3"));
    assert!(stdout.contains("@D7/1 REQ 4 UPM 7"));
    assert!(stdout.contains("K file_path1 pct 5 a.txt"));
    assert!(stdout.contains("K file_name2 pct 5 b.bin"));
    assert_eq!(stdout.matches("@D7/1 REQ").count(), 4);

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn strict_fortran_preflight_contracts_without_extension() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping strict preflight test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir =
        std::env::temp_dir().join(format!("dfor77-strict-preflight-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("strict_preflight.f");
    let program_bin = out_dir.join("strict_preflight");
    let result_file = out_dir.join("result.txt");
    let source = strict_preflight_contract_program();
    assert_strict_fortran_program(&source);
    std::fs::write(&program_src, source).expect("failed to write strict preflight program");

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for strict preflight wrapper");
    assert!(compile_wrapper.success());

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-72"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for strict preflight program");
    assert!(compile_program.success());

    let input = concat!(
        "@D7/1 RES 1 31 2\n",
        "K can raw 1 1\n",
        "K why raw 3 bad\n",
        "@D7/1 END 1\n",
        "@D7/1 RES 2 31 2\n",
        "K can raw 1 1\n",
        "K why raw 3 bad\n",
        "@D7/1 END 2\n",
        "@D7/1 RES 3 0 2\n",
        "K can raw 1 1\n",
        "K why raw 4 deny\n",
        "@D7/1 END 3\n",
        "@D7/1 RES 4 0 2\n",
        "K can raw 1 1\n",
        "K why raw 5 hi\n",
        "@D7/1 END 4\n",
    );
    let mut child = Command::new(&program_bin)
        .current_dir(&out_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run strict preflight program");
    child
        .stdin
        .as_mut()
        .expect("missing strict preflight stdin")
        .write_all(input.as_bytes())
        .expect("failed to write strict preflight fake responses");
    let output = child
        .wait_with_output()
        .expect("failed to wait for strict preflight program");
    assert!(
        output.status.success(),
        "strict preflight program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = std::fs::read_to_string(&result_file).expect("failed to read strict preflight");
    let lines = result.lines().collect::<Vec<_>>();
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        vec!["CSP", "31", "0", "3", "0", "bad",]
    );
    assert_eq!(
        lines[1].split_whitespace().collect::<Vec<_>>(),
        vec!["CSN", "31", "0", "3", "0", "bad",]
    );
    assert_eq!(
        lines[2].split_whitespace().collect::<Vec<_>>(),
        vec!["CDL", "10", "1", "4", "1", "de",]
    );
    assert_eq!(
        lines[3].split_whitespace().collect::<Vec<_>>(),
        vec!["CDP", "60", "0", "0", "0",]
    );
    for line in &lines[4..] {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        assert_eq!(fields[1], "30", "unexpected local failure row {line}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("@D7/1 REQ").count(), 4);
    assert!(stdout.contains("@D7/1 REQ 1 CSP 0"));
    assert!(stdout.contains("@D7/1 REQ 2 CSN 2"));
    assert!(stdout.contains("@D7/1 REQ 3 CDL 3"));
    assert!(stdout.contains("@D7/1 REQ 4 CDP 0"));

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

fn parse_strict_payload_row(line: &str) -> (String, i32, usize, i32, String) {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    assert!(parts.len() >= 5, "invalid strict payload row: {line}");
    (
        parts[0].to_string(),
        parts[1].parse().expect("invalid status"),
        parts[2].parse().expect("invalid out length"),
        parts[3].parse().expect("invalid truncation flag"),
        parts[4].to_string(),
    )
}

fn assert_payload_row(
    row: &(String, i32, usize, i32, String),
    tag: &str,
    status: i32,
    payload: &str,
) {
    assert_eq!(row.0, tag);
    assert_eq!(row.1, status);
    assert_eq!(row.2, payload.len());
    assert_eq!(row.3, 0);
    assert_eq!(row.4, payload);
}

fn strict_payload_builder_program() -> String {
    let lines = [
        "      PROGRAM PBLD",
        "      INTEGER D7BPR,D7TIS,D7EPH,D7OST,D7OUS",
        "      PARAMETER (D7BPR=1,D7TIS=1,D7EPH=64)",
        "      PARAMETER (D7OST=3,D7OUS=6)",
        "      INTEGER STAT,OUTL,TRUN",
        "      CHARACTER*60000 PAY",
        "      CHARACTER*16 ONAM(2),ODSC(2),LABL(2),VALU(2)",
        "      CHARACTER*16 CID(2),FNAM(1),FVAL(1)",
        "      INTEGER OLEN(2),ODLN(2),OTYP(2),OREQ(2)",
        "      INTEGER LLEN(2),VLEN(2),IDLEN(2),STYLE(2),ROW(2)",
        "      INTEGER FNLN(1),FVLN(1),FINL(1)",
        "      OPEN(UNIT=9,FILE='result.txt',STATUS='UNKNOWN')",
        "      ONAM(1) = 'query'",
        "      ONAM(2) = 'user'",
        "      ODSC(1) = 'find'",
        "      ODSC(2) = 'target'",
        "      OLEN(1) = 5",
        "      OLEN(2) = 4",
        "      ODLN(1) = 4",
        "      ODLN(2) = 6",
        "      OTYP(1) = D7OST",
        "      OTYP(2) = D7OUS",
        "      OREQ(1) = 1",
        "      OREQ(2) = 0",
        "      LABL(1) = 'Red'",
        "      LABL(2) = 'Blue'",
        "      VALU(1) = 'red'",
        "      VALU(2) = 'blue'",
        "      CID(1) = 'ca'",
        "      CID(2) = 'cb'",
        "      LLEN(1) = 3",
        "      LLEN(2) = 4",
        "      VLEN(1) = 3",
        "      VLEN(2) = 4",
        "      IDLEN(1) = 2",
        "      IDLEN(2) = 2",
        "      STYLE(1) = D7BPR",
        "      STYLE(2) = D7BPR",
        "      ROW(1) = 1",
        "      ROW(2) = 1",
        "      FNLN(1) = 0",
        "      FVLN(1) = 0",
        "      FINL(1) = 0",
        "      CALL D7JES('A\"B',3,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL WROW(9,'JES',PAY,OUTL,TRUN,STAT)",
        "      CALL D7PEM('c',1,'t',1,'d',1,1,PAY,60000,",
        "     1 OUTL,TRUN,STAT)",
        "      CALL WROW(9,'PEM',PAY,OUTL,TRUN,STAT)",
        "      CALL D7PE2('c',1,'t',1,'d',1,' ',0,1,' ',0,",
        "     1 ' ',0,' ',0,' ',0,' ',0,' ',0,",
        "     2 ' ',0,' ',0,0,FNAM,FNLN,FVAL,FVLN,FINL,",
        "     3 PAY,60000,OUTL,TRUN,STAT)",
        "      CALL WROW(9,'PE2',PAY,OUTL,TRUN,STAT)",
        "      CALL D7PBT('c',1,'OK',2,'btn',3,D7BPR,PAY,",
        "     1 60000,OUTL,TRUN,STAT)",
        "      CALL WROW(9,'PBT',PAY,OUTL,TRUN,STAT)",
        "      CALL D7IMR('ok',2,D7EPH,PAY,60000,OUTL,TRUN,",
        "     1 STAT)",
        "      CALL WROW(9,'IMR',PAY,OUTL,TRUN,STAT)",
        "      CALL D7IDR(0,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL WROW(9,'IDR',PAY,OUTL,TRUN,STAT)",
        "      CALL D7IMD('m',1,'modal',5,'answer',6,'field',5,",
        "     1 D7TIS,1,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL WROW(9,'IMD',PAY,OUTL,TRUN,STAT)",
        "      CALL D7SCM('hello',5,'say',3,PAY,60000,OUTL,",
        "     1 TRUN,STAT)",
        "      CALL WROW(9,'SCM',PAY,OUTL,TRUN,STAT)",
        "      CALL D7SCX('search',6,'find',4,2,ONAM,OLEN,ODSC,",
        "     1 ODLN,OTYP,OREQ,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL WROW(9,'SCX',PAY,OUTL,TRUN,STAT)",
        "      CALL D7PSM('choose',6,'menu',4,2,LABL,LLEN,VALU,",
        "     1 VLEN,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL WROW(9,'PSM',PAY,OUTL,TRUN,STAT)",
        "      CALL D7PAC('act',3,2,LABL,LLEN,CID,IDLEN,STYLE,",
        "     1 ROW,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL WROW(9,'PAC',PAY,OUTL,TRUN,STAT)",
        "      CLOSE(9)",
        "      STOP",
        "      END",
        "      SUBROUTINE WROW(UNIT,TAG,PAY,OUTL,TRUN,STAT)",
        "      INTEGER UNIT,OUTL,TRUN,STAT",
        "      CHARACTER*(*) TAG,PAY",
        "      WRITE(UNIT,1000) TAG,STAT,OUTL,TRUN,PAY(1:OUTL)",
        " 1000 FORMAT(A,1X,I6,1X,I6,1X,I6,1X,A)",
        "      RETURN",
        "      END",
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn strict_action_request_program() -> String {
    let lines = [
        "      PROGRAM RACT",
        "      INTEGER D7RDU",
        "      PARAMETER (D7RDU=3)",
        "      INTEGER STAT",
        "      INTEGER SLEN(2),ALEN(3),FLEN(2),NLEN(2)",
        "      CHARACTER*18 CHID,MID,USER,STKID(2)",
        "      CHARACTER*24 EMOJI",
        "      CHARACTER*12 QUEST",
        "      CHARACTER*8 ANS(3),MSG",
        "      CHARACTER*16 PATHS(2),NAMES(2)",
        "      CHID = '100000000000000001'",
        "      STKID(1) = '100000000000000002'",
        "      STKID(2) = '100000000000000003'",
        "      MID = '100000000000000004'",
        "      USER = '100000000000000005'",
        "      EMOJI = 'party:100000000000000003'",
        "      QUEST = 'Deploy now?'",
        "      ANS(1) = 'yes'",
        "      ANS(2) = 'no'",
        "      ANS(3) = 'later'",
        "      MSG = 'poll msg'",
        "      PATHS(1) = 'a.txt'",
        "      PATHS(2) = 'b.bin'",
        "      NAMES(1) = 'a.txt'",
        "      NAMES(2) = 'b.bin'",
        "      SLEN(1) = 18",
        "      SLEN(2) = 18",
        "      ALEN(1) = 3",
        "      ALEN(2) = 2",
        "      ALEN(3) = 5",
        "      FLEN(1) = 5",
        "      FLEN(2) = 5",
        "      NLEN(1) = 5",
        "      NLEN(2) = 5",
        "      OPEN(UNIT=9,FILE='result.txt',STATUS='UNKNOWN')",
        "      CALL D7STK(CHID,18,2,STKID,SLEN,MSG,7,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7STK(CHID,18,0,STKID,SLEN,MSG,7,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7POL(CHID,18,QUEST,11,3,ANS,ALEN,24,1,",
        "     1 MSG,8,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7POL(CHID,18,QUEST,11,1,ANS,ALEN,24,1,",
        "     1 MSG,8,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7RAC(CHID,18,MID,18,EMOJI,24,USER,18,",
        "     1 D7RDU,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7RAC(CHID,18,MID,18,EMOJI,24,USER,18,",
        "     1 0,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7UPM(CHID,18,2,PATHS,FLEN,NAMES,NLEN,",
        "     1 MSG,8,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7UPM(CHID,18,0,PATHS,FLEN,NAMES,NLEN,",
        "     1 MSG,8,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CLOSE(9)",
        "      STOP",
        "      END",
        "      SUBROUTINE WSTA(UNIT,STAT)",
        "      INTEGER UNIT,STAT",
        "      WRITE(UNIT,1000) STAT",
        " 1000 FORMAT(I6)",
        "      RETURN",
        "      END",
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn strict_preflight_contract_program() -> String {
    let lines = [
        "      PROGRAM PCHK",
        "      INTEGER STAT,CAN,OUTL,TRUN,ISADM",
        "      CHARACTER*18 GID,CHID,MID,UID",
        "      CHARACTER*8 WHY,REAS,MSG",
        "      GID = '100000000000000001'",
        "      CHID = '100000000000000002'",
        "      MID = '100000000000000003'",
        "      UID = '100000000000000004'",
        "      REAS = 'test'",
        "      MSG = 'ping'",
        "      OPEN(UNIT=9,FILE='result.txt',STATUS='UNKNOWN')",
        "      CAN = -1",
        "      CALL D7CSP(CAN,WHY,8,OUTL,TRUN,STAT)",
        "      WRITE(9,1000) 'CSP',STAT,CAN,OUTL,TRUN,WHY(1:OUTL)",
        "      CAN = -1",
        "      CALL D7CSN(GID,18,CHID,18,CAN,WHY,8,OUTL,",
        "     1 TRUN,STAT)",
        "      WRITE(9,1000) 'CSN',STAT,CAN,OUTL,TRUN,WHY(1:OUTL)",
        "      CAN = -1",
        "      CALL D7CDL(GID,18,CHID,18,MID,18,CAN,WHY,2,",
        "     1 OUTL,TRUN,STAT)",
        "      WRITE(9,1000) 'CDL',STAT,CAN,OUTL,TRUN,WHY(1:2)",
        "      CAN = -1",
        "      CALL D7CDP(CAN,WHY,8,OUTL,TRUN,STAT)",
        "      WRITE(9,1010) 'CDP',STAT,CAN,OUTL,TRUN",
        "      CALL D7SND(CHID,0,MSG,4,STAT)",
        "      WRITE(9,1020) 'SND',STAT",
        "      CALL D7DEL(CHID,0,MID,18,REAS,4,STAT)",
        "      WRITE(9,1020) 'DEL',STAT",
        "      CALL D7TMO(GID,0,UID,18,60,REAS,4,STAT)",
        "      WRITE(9,1020) 'TMO',STAT",
        "      CALL D7ADM(GID,0,UID,18,ISADM,STAT)",
        "      WRITE(9,1020) 'ADM',STAT",
        "      CALL D7CDL(GID,0,CHID,18,MID,18,CAN,WHY,4,",
        "     1 OUTL,TRUN,STAT)",
        "      WRITE(9,1020) 'CDL',STAT",
        "      CALL D7CTM(GID,18,UID,0,CAN,WHY,4,OUTL,",
        "     1 TRUN,STAT)",
        "      WRITE(9,1020) 'CTM',STAT",
        "      CALL D7CSN(GID,18,CHID,0,CAN,WHY,4,OUTL,",
        "     1 TRUN,STAT)",
        "      WRITE(9,1020) 'CSN',STAT",
        "      CLOSE(9)",
        "      STOP",
        " 1000 FORMAT(A,1X,I6,1X,I6,1X,I6,1X,I6,1X,A)",
        " 1010 FORMAT(A,1X,I6,1X,I6,1X,I6,1X,I6)",
        " 1020 FORMAT(A,1X,I6)",
        "      END",
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn strict_rich_http_program() -> String {
    let lines = [
        "      PROGRAM RHTTP",
        "      INTEGER D7OK,D7EOF,D7TIM,D7MSG,D7RAD",
        "      PARAMETER (D7OK=0,D7EOF=1,D7TIM=51)",
        "      PARAMETER (D7MSG=101,D7RAD=1)",
        "      INTEGER STAT,EVNT",
        "      INTEGER SLEN(1),ALEN(3),FLEN(2),NLEN(2)",
        "      CHARACTER*18 CHID,MID,STKID(1)",
        "      CHARACTER*24 EMOJI",
        "      CHARACTER*12 QUEST",
        "      CHARACTER*8 ANS(3),MSG",
        "      CHARACTER*16 PATHS(2),NAMES(2)",
        "      CHID = '100000000000000002'",
        "      MID = '100000000000000003'",
        "      STKID(1) = '100000000000000011'",
        "      EMOJI = 'party:100000000000000012'",
        "      QUEST = 'Deploy now?'",
        "      ANS(1) = 'yes'",
        "      ANS(2) = 'no'",
        "      ANS(3) = 'later'",
        "      PATHS(1) = 'a.txt'",
        "      PATHS(2) = 'b.bin'",
        "      NAMES(1) = 'a.txt'",
        "      NAMES(2) = 'b.bin'",
        "      SLEN(1) = 18",
        "      ALEN(1) = 3",
        "      ALEN(2) = 2",
        "      ALEN(3) = 5",
        "      FLEN(1) = 5",
        "      FLEN(2) = 5",
        "      NLEN(1) = 5",
        "      NLEN(2) = 5",
        "      CALL D7INI(STAT)",
        "      IF (STAT .NE. D7OK) STOP 1",
        "  100 CALL D7NXT(EVNT,STAT)",
        "      IF (STAT .EQ. D7TIM) GOTO 100",
        "      IF (STAT .EQ. D7EOF) GOTO 900",
        "      IF (STAT .NE. D7OK) GOTO 100",
        "      IF (EVNT .NE. D7MSG) GOTO 100",
        "      OPEN(UNIT=9,FILE='result.txt',STATUS='UNKNOWN')",
        "      CALL D7STK(CHID,18,1,STKID,SLEN,'sticker',7,",
        "     1 STAT)",
        "      CALL WSTA(9,STAT)",
        "      MSG = 'poll msg'",
        "      CALL D7POL(CHID,18,QUEST,11,3,ANS,ALEN,24,1,",
        "     1 MSG,8,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7RAC(CHID,18,MID,18,EMOJI,24,' ',0,",
        "     1 D7RAD,STAT)",
        "      CALL WSTA(9,STAT)",
        "      MSG = 'file msg'",
        "      CALL D7UPM(CHID,18,2,PATHS,FLEN,NAMES,NLEN,",
        "     1 MSG,8,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CLOSE(9)",
        "  900 CALL D7END(STAT)",
        "      STOP",
        "      END",
        "      SUBROUTINE WSTA(UNIT,STAT)",
        "      INTEGER UNIT,STAT",
        "      WRITE(UNIT,1000) STAT",
        " 1000 FORMAT(I6)",
        "      RETURN",
        "      END",
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn strict_management_http_program() -> String {
    let lines = [
        "      PROGRAM MHTTP",
        "      INTEGER D7OK,D7EOF,D7TIM,D7MSG,D7HGT",
        "      PARAMETER (D7OK=0,D7EOF=1,D7TIM=51)",
        "      PARAMETER (D7MSG=101,D7HGT=1)",
        "      INTEGER STAT,EVNT,OUTL,TRUN",
        "      CHARACTER*18 GID,CHID,USER,ROLE",
        "      CHARACTER*64 PAY",
        "      CHARACTER*256 BODY",
        "      GID = '100000000000000001'",
        "      CHID = '100000000000000002'",
        "      USER = '100000000000000004'",
        "      ROLE = '100000000000000005'",
        "      CALL D7INI(STAT)",
        "      IF (STAT .NE. D7OK) STOP 1",
        "  100 CALL D7NXT(EVNT,STAT)",
        "      IF (STAT .EQ. D7TIM) GOTO 100",
        "      IF (STAT .EQ. D7EOF) GOTO 900",
        "      IF (STAT .NE. D7OK) GOTO 100",
        "      IF (EVNT .NE. D7MSG) GOTO 100",
        "      OPEN(UNIT=9,FILE='result.txt',STATUS='UNKNOWN')",
        "      PAY = '{\"name\":\"bot-log\"}'",
        "      CALL D7CCR(GID,18,PAY,18,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      PAY = '{\"name\":\"edited\"}'",
        "      CALL D7ECH(CHID,18,PAY,17,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7DCH(CHID,18,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      PAY = '{\"name\":\"helper\"}'",
        "      CALL D7CRO(GID,18,PAY,17,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      PAY = '{\"name\":\"helper2\"}'",
        "      CALL D7ERO(GID,18,ROLE,18,PAY,18,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7DRO(GID,18,ROLE,18,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7AMR(GID,18,USER,18,ROLE,18,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7RMR(GID,18,USER,18,ROLE,18,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      PAY = '{\"nick\":\"bot\"}'",
        "      CALL D7EGM(GID,18,USER,18,PAY,14,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7KIK(GID,18,USER,18,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      PAY = '{}'",
        "      CALL D7BAN(GID,18,USER,18,PAY,2,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7UBN(GID,18,USER,18,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      PAY = '{\"name\":\"guild\"}'",
        "      CALL D7EGD(GID,18,PAY,16,'mgmt',4,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7RST(D7HGT,'/api/v10/users/@me',18,' ',0,",
        "     1 ' ',0,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7RBD(BODY,256,OUTL,TRUN,STAT)",
        "      IF (INDEX(BODY,'100000000000000009') .LE. 0) STAT = 70",
        "      CALL WSTA(9,STAT)",
        "      CALL WSTA(9,OUTL)",
        "      CALL WSTA(9,TRUN)",
        "      CLOSE(9)",
        "  900 CALL D7END(STAT)",
        "      STOP",
        "      END",
        "      SUBROUTINE WSTA(UNIT,STAT)",
        "      INTEGER UNIT,STAT",
        "      WRITE(UNIT,1000) STAT",
        " 1000 FORMAT(I6)",
        "      RETURN",
        "      END",
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn strict_misc_http_program() -> String {
    let lines = [
        "      PROGRAM XHTTP",
        "      INTEGER D7OK,D7EOF,D7TIM,D7MSG,D7HPO",
        "      PARAMETER (D7OK=0,D7EOF=1,D7TIM=51)",
        "      PARAMETER (D7MSG=101,D7HPO=2)",
        "      INTEGER STAT,EVNT",
        "      CHARACTER*18 GID,CHID,IID,APP",
        "      CHARACTER*16 TOK",
        "      CHARACTER*80 PAY",
        "      GID = '100000000000000001'",
        "      CHID = '100000000000000002'",
        "      IID = '100000000000000006'",
        "      APP = '100000000000000007'",
        "      TOK = 'tok-123'",
        "      CALL D7INI(STAT)",
        "      IF (STAT .NE. D7OK) STOP 1",
        "  100 CALL D7NXT(EVNT,STAT)",
        "      IF (STAT .EQ. D7TIM) GOTO 100",
        "      IF (STAT .EQ. D7EOF) GOTO 900",
        "      IF (STAT .NE. D7OK) GOTO 100",
        "      IF (EVNT .NE. D7MSG) GOTO 100",
        "      OPEN(UNIT=9,FILE='result.txt',STATUS='UNKNOWN')",
        "      PAY = '{\"content\":\"json send\"}'",
        "      CALL D7SJP(CHID,18,PAY,23,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7UPL(CHID,18,'c.txt',5,'single.txt',10,",
        "     1 'single msg',10,STAT)",
        "      CALL WSTA(9,STAT)",
        "      PAY = '{\"type\":4}'",
        "      CALL D7ICB(IID,18,TOK,7,PAY,10,STAT)",
        "      CALL WSTA(9,STAT)",
        "      PAY = '{\"name\":\"ping\"}'",
        "      CALL D7ACR(APP,18,GID,18,' ',0,D7HPO,PAY,15,",
        "     1 STAT)",
        "      CALL WSTA(9,STAT)",
        "      CALL D7TPI(CHID,18,STAT)",
        "      CALL WSTA(9,STAT)",
        "      CLOSE(9)",
        "  900 CALL D7END(STAT)",
        "      STOP",
        "      END",
        "      SUBROUTINE WSTA(UNIT,STAT)",
        "      INTEGER UNIT,STAT",
        "      WRITE(UNIT,1000) STAT",
        " 1000 FORMAT(I6)",
        "      RETURN",
        "      END",
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn strict_rest_error_program() -> String {
    let lines = [
        "      PROGRAM EHTTP",
        "      INTEGER D7OK,D7HGT,D7HPO",
        "      PARAMETER (D7OK=0,D7HGT=1,D7HPO=2)",
        "      INTEGER STAT",
        "      CHARACTER*64 PATH",
        "      CHARACTER*32 PAY",
        "      CALL D7INI(STAT)",
        "      IF (STAT .NE. D7OK) STOP 1",
        "      OPEN(UNIT=9,FILE='result.txt',STATUS='UNKNOWN')",
        "      PATH = '/api/v10/channels/100000000000000002/messages'",
        "      PAY = '{\"content\":\"bad\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,17,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"rate\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,18,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"slow\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,18,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"chanrl\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,20,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"guildrl\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,21,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"forbid\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,20,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"access\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,20,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"gone\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,18,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"guild\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,19,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"member\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,20,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"msg\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,17,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"user\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,18,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"thread\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,20,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"system\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,20,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"itype\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,19,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"locked\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,20,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"forum\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,19,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"ctype\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,19,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"rawbad\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,20,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PAY = '{\"content\":\"unc\"}'",
        "      CALL D7RST(D7HPO,PATH,45,PAY,17,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      PATH = '/api/v10/users/@me'",
        "      CALL D7RST(D7HGT,PATH,18,' ',0,' ',0,STAT)",
        "      CALL RERR(9,STAT)",
        "      CLOSE(9)",
        "      CALL D7END(STAT)",
        "      STOP",
        "      END",
        "      SUBROUTINE RERR(UNIT,STAT)",
        "      INTEGER UNIT,STAT,ECODE,HTTP,DCODE,OUTL,TRUN,EST",
        "      CHARACTER*80 MSG",
        "      CALL WSTA(UNIT,STAT)",
        "      CALL D7ERR(1,ECODE,HTTP,DCODE,MSG,80,OUTL,TRUN,EST)",
        "      CALL WSTA(UNIT,EST)",
        "      CALL WSTA(UNIT,ECODE)",
        "      CALL WSTA(UNIT,HTTP)",
        "      CALL WSTA(UNIT,DCODE)",
        "      RETURN",
        "      END",
        "      SUBROUTINE WSTA(UNIT,STAT)",
        "      INTEGER UNIT,STAT",
        "      WRITE(UNIT,1000) STAT",
        " 1000 FORMAT(I6)",
        "      RETURN",
        "      END",
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn strict_preflight_runtime_program() -> String {
    let lines = [
        "      PROGRAM PFRUN",
        "      INTEGER D7OK,D7EOF,D7TIM,D7MSG",
        "      PARAMETER (D7OK=0,D7EOF=1,D7TIM=51)",
        "      PARAMETER (D7MSG=101)",
        "      INTEGER STAT,EVNT,NMSG,CAN,OUTL,TRUN",
        "      INTEGER GLEN,CLEN,MLEN,ULEN",
        "      CHARACTER*32 GID,CHID,MID,UID,WHY",
        "      NMSG = 0",
        "      CALL D7INI(STAT)",
        "      IF (STAT .NE. D7OK) STOP 1",
        "      OPEN(UNIT=9,FILE='result.txt',STATUS='UNKNOWN')",
        "  100 CALL D7NXT(EVNT,STAT)",
        "      IF (STAT .EQ. D7TIM) GOTO 100",
        "      IF (STAT .EQ. D7EOF) GOTO 900",
        "      IF (STAT .NE. D7OK) GOTO 100",
        "      IF (EVNT .NE. D7MSG) GOTO 100",
        "      NMSG = NMSG + 1",
        "      CALL D7GID(GID,32,GLEN,TRUN,STAT)",
        "      CALL D7CHN(CHID,32,CLEN,TRUN,STAT)",
        "      CALL D7MID(MID,32,MLEN,TRUN,STAT)",
        "      CALL D7USR(UID,32,ULEN,TRUN,STAT)",
        "      CALL D7CSP(CAN,WHY,32,OUTL,TRUN,STAT)",
        "      CALL WROW(9,NMSG,1,STAT,CAN,OUTL)",
        "      CALL D7CDP(CAN,WHY,32,OUTL,TRUN,STAT)",
        "      CALL WROW(9,NMSG,2,STAT,CAN,OUTL)",
        "      CALL D7CTP(CAN,WHY,32,OUTL,TRUN,STAT)",
        "      CALL WROW(9,NMSG,3,STAT,CAN,OUTL)",
        "      CALL D7CSN(GID,GLEN,CHID,CLEN,CAN,WHY,32,",
        "     1 OUTL,TRUN,STAT)",
        "      CALL WROW(9,NMSG,4,STAT,CAN,OUTL)",
        "      CALL D7CDL(GID,GLEN,CHID,CLEN,MID,MLEN,CAN,",
        "     1 WHY,32,OUTL,TRUN,STAT)",
        "      CALL WROW(9,NMSG,5,STAT,CAN,OUTL)",
        "      CALL D7CTM(GID,GLEN,UID,ULEN,CAN,WHY,32,",
        "     1 OUTL,TRUN,STAT)",
        "      CALL WROW(9,NMSG,6,STAT,CAN,OUTL)",
        "      IF (NMSG .LT. 5) GOTO 100",
        "  900 CLOSE(9)",
        "      CALL D7END(STAT)",
        "      STOP",
        "      END",
        "      SUBROUTINE WROW(UNIT,NMSG,OP,STAT,CAN,OUTL)",
        "      INTEGER UNIT,NMSG,OP,STAT,CAN,OUTL",
        "      WRITE(UNIT,1000) NMSG,OP,STAT,CAN,OUTL",
        " 1000 FORMAT(5I8)",
        "      RETURN",
        "      END",
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn strict_api_surface_program() -> String {
    let lines = [
        "      PROGRAM T",
        "      INTEGER D7OK,D7HGT,D7HPO,D7HPU,D7HPA,D7HDE",
        "      INTEGER D7BPR,D7TIS,D7EPH,D7OST,D7OUS,D7ACT",
        "      INTEGER D7RAD",
        "      PARAMETER (D7OK=0,D7HGT=1,D7HPO=2,D7HPU=3)",
        "      PARAMETER (D7HPA=4,D7HDE=5,D7BPR=1)",
        "      PARAMETER (D7TIS=1,D7EPH=64,D7OST=3)",
        "      PARAMETER (D7OUS=6,D7ACT=1,D7RAD=1)",
        "      INTEGER STAT,FAIL,EVNT,OUTL,TRUN,VAL,CAN",
        "      INTEGER ECODE,HTTP,DCODE",
        "      CHARACTER*32 GID,CHID,MID,USER,ROLE,IID,APP",
        "      CHARACTER*32 CMD",
        "      CHARACTER*256 TOK",
        "      CHARACTER*60000 PAY,BODY",
        "      CHARACTER*64 WHY,MSG",
        "      CHARACTER*32 STKID(2)",
        "      CHARACTER*16 ANS(2),PATHS(2),NAMES(2)",
        "      CHARACTER*16 LABL(2),VALU(2),CID(2)",
        "      CHARACTER*16 ONAM(2),ODSC(2)",
        "      INTEGER SLEN(2),ALEN(2),FLEN(2),NLEN(2)",
        "      INTEGER LLEN(2),VLEN(2),IDLEN(2),STYLE(2),ROW(2)",
        "      INTEGER OLEN(2),ODLN(2),OTYP(2),OREQ(2)",
        "      FAIL = 0",
        "      GID = '100000000000000001'",
        "      CHID = '100000000000000002'",
        "      MID = '100000000000000003'",
        "      USER = '100000000000000004'",
        "      ROLE = '100000000000000005'",
        "      IID = '100000000000000006'",
        "      APP = '100000000000000007'",
        "      CMD = '100000000000000008'",
        "      TOK = 'token'",
        "      MSG = 'OK'",
        "      PAY = '{}'",
        "      STKID(1) = '100000000000000009'",
        "      STKID(2) = '100000000000000010'",
        "      SLEN(1) = 18",
        "      SLEN(2) = 18",
        "      ANS(1) = 'YES'",
        "      ANS(2) = 'NO'",
        "      ALEN(1) = 3",
        "      ALEN(2) = 2",
        "      PATHS(1) = 'a.txt'",
        "      PATHS(2) = 'b.txt'",
        "      NAMES(1) = 'a.txt'",
        "      NAMES(2) = 'b.txt'",
        "      FLEN(1) = 5",
        "      FLEN(2) = 5",
        "      NLEN(1) = 5",
        "      NLEN(2) = 5",
        "      LABL(1) = 'A'",
        "      LABL(2) = 'B'",
        "      VALU(1) = 'a'",
        "      VALU(2) = 'b'",
        "      CID(1) = 'ca'",
        "      CID(2) = 'cb'",
        "      LLEN(1) = 1",
        "      LLEN(2) = 1",
        "      VLEN(1) = 1",
        "      VLEN(2) = 1",
        "      IDLEN(1) = 2",
        "      IDLEN(2) = 2",
        "      STYLE(1) = D7BPR",
        "      STYLE(2) = D7BPR",
        "      ROW(1) = 1",
        "      ROW(2) = 1",
        "      ONAM(1) = 'query'",
        "      ONAM(2) = 'user'",
        "      ODSC(1) = 'find'",
        "      ODSC(2) = 'target'",
        "      OLEN(1) = 5",
        "      OLEN(2) = 4",
        "      ODLN(1) = 4",
        "      ODLN(2) = 6",
        "      OTYP(1) = D7OST",
        "      OTYP(2) = D7OUS",
        "      OREQ(1) = 1",
        "      OREQ(2) = 0",
        "      CALL D7INI(STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7NXT(EVNT,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      IF (EVNT .NE. 101) FAIL = FAIL + 1",
        "      CALL D7TYP(VAL,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7ENM(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7GID(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CHN(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CAT(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7MID(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7IID(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7APP(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7ITK(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7INM(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7ITP(VAL,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7USR(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7TXT(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7TAV(VAL,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7BOT(VAL,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7IMC(VAL,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7RLC(VAL,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7RLI(1,BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7ADM(GID,18,USER,18,VAL,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CAD(VAL,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CDL(GID,18,CHID,18,MID,18,CAN,WHY,64,",
        "     1 OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CTM(GID,18,USER,18,CAN,WHY,64,OUTL,TRUN,",
        "     1 STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CSN(GID,18,CHID,18,CAN,WHY,64,OUTL,TRUN,",
        "     1 STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CDP(CAN,WHY,64,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CTP(CAN,WHY,64,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CSP(CAN,WHY,64,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7JES('A\"B',3,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7PEM('c',1,'t',1,'d',1,1,PAY,60000,",
        "     1 OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7PE2('c',1,'t',1,'d',1,' ',0,1,' ',0,",
        "     1 ' ',0,' ',0,' ',0,' ',0,' ',0,",
        "     2 ' ',0,' ',0,0,ONAM,OLEN,ODSC,ODLN,OREQ,",
        "     3 PAY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7PBT('c',1,'A',1,'btn',3,D7BPR,PAY,60000,",
        "     1 OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7IMR('ok',2,D7EPH,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7IDR(0,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7IMD('m',1,'modal',5,'answer',6,'field',5,",
        "     1 D7TIS,1,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7SCM('hello',5,'say',3,PAY,60000,OUTL,TRUN,",
        "     1 STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7SCX('search',6,'find',4,2,ONAM,OLEN,ODSC,",
        "     1 ODLN,OTYP,OREQ,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7PSM('choose',6,'menu',4,2,LABL,LLEN,VALU,",
        "     1 VLEN,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7PAC('act',3,2,LABL,LLEN,CID,IDLEN,STYLE,",
        "     1 ROW,PAY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      PAY = '{}'",
        "      CALL D7SND(CHID,18,MSG,2,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7SJP(CHID,18,PAY,2,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7DEL(CHID,18,MID,18,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7TMO(GID,18,USER,18,60,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7UNT(GID,18,USER,18,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7STK(CHID,18,2,STKID,SLEN,MSG,2,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7POL(CHID,18,'q',1,2,ANS,ALEN,1,0,MSG,2,",
        "     1 STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7UPL(CHID,18,'a.txt',5,'a.txt',5,MSG,2,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7UPM(CHID,18,2,PATHS,FLEN,NAMES,NLEN,MSG,2,",
        "     1 STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7ICB(IID,18,TOK,5,PAY,2,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7ACR(APP,18,GID,18,CMD,18,D7HPO,PAY,2,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7TPI(CHID,18,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7RAC(CHID,18,MID,18,'ok',2,' ',0,D7RAD,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7RST(D7HGT,'/api/v10/users/@me',18,' ',0,",
        "     1 ' ',0,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7RBD(BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CCR(GID,18,PAY,2,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7ECH(CHID,18,PAY,2,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7DCH(CHID,18,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7CRO(GID,18,PAY,2,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7ERO(GID,18,ROLE,18,PAY,2,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7DRO(GID,18,ROLE,18,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7AMR(GID,18,USER,18,ROLE,18,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7RMR(GID,18,USER,18,ROLE,18,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7EGM(GID,18,USER,18,PAY,2,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7KIK(GID,18,USER,18,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7BAN(GID,18,USER,18,PAY,2,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7UBN(GID,18,USER,18,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7EGD(GID,18,PAY,2,'why',3,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7GWS(PAY,2,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7PRS(PAY,2,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7VST(GID,18,CHID,18,1,0,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7GOP(VAL,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7GRT(VAL,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7ERR(D7ACT,ECODE,HTTP,DCODE,BODY,60000,",
        "     1 OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7WHY(D7ACT,BODY,60000,OUTL,TRUN,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7LOG(1,MSG,2,STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      CALL D7END(STAT)",
        "      CALL CHK(STAT,FAIL)",
        "      OPEN(UNIT=9,FILE='result.txt',STATUS='UNKNOWN')",
        "      WRITE(9,1000) FAIL",
        " 1000 FORMAT(I6)",
        "      CLOSE(9)",
        "      STOP",
        "      END",
        "      SUBROUTINE CHK(STAT,FAIL)",
        "      INTEGER STAT,FAIL",
        "      IF (STAT .NE. 0) FAIL = FAIL + 1",
        "      RETURN",
        "      END",
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn assert_strict_fortran_program(source: &str) {
    for (index, line) in source.lines().enumerate() {
        let upper = line.to_ascii_uppercase();
        assert!(line.is_ascii(), "strict line {} is not ASCII", index + 1);
        assert!(
            line.len() <= 72,
            "strict line {} exceeds 72 columns: {}",
            index + 1,
            line
        );
        assert!(!line.contains('\t'), "strict line {} has tab", index + 1);
        assert!(
            !upper.trim_start().starts_with("INCLUDE"),
            "strict line {} uses INCLUDE",
            index + 1
        );
        assert!(!upper.contains("I0"), "strict line {} uses I0", index + 1);
    }
}

fn strict_api_surface_responses() -> String {
    let mut responses = String::new();
    push_ok(&mut responses, 1);
    push_fields(&mut responses, 2, &[("event_type", "101")]);
    push_fields(&mut responses, 3, &[("event_type", "101")]);
    push_fields(&mut responses, 4, &[("value", "MSG")]);
    for rid in 5..=12 {
        push_fields(&mut responses, rid, &[("value", "100000000000000001")]);
    }
    push_fields(&mut responses, 13, &[("value", "2")]);
    push_fields(&mut responses, 14, &[("value", "100000000000000004")]);
    push_fields(&mut responses, 15, &[("value", "hello")]);
    for rid in 16..=19 {
        push_fields(&mut responses, rid, &[("value", "1")]);
    }
    push_fields(&mut responses, 20, &[("role_id", "100000000000000005")]);
    push_fields(&mut responses, 21, &[("is_admin", "1")]);
    push_fields(&mut responses, 22, &[("is_admin", "1")]);
    for rid in 23..=28 {
        push_fields(&mut responses, rid, &[("can", "1"), ("why", "ok")]);
    }
    for rid in 29..=42 {
        push_ok(&mut responses, rid);
    }
    push_fields(
        &mut responses,
        43,
        &[("body", "{}"), ("out_len", "2"), ("truncated", "0")],
    );
    for rid in 44..=59 {
        push_ok(&mut responses, rid);
    }
    push_fields(&mut responses, 60, &[("opcode", "0")]);
    push_fields(&mut responses, 61, &[("retry_ms", "0")]);
    push_fields(
        &mut responses,
        62,
        &[
            ("errcode", "0"),
            ("http", "0"),
            ("discord_code", "0"),
            ("message", "ok"),
        ],
    );
    push_fields(&mut responses, 63, &[("message", "ok")]);
    push_ok(&mut responses, 64);
    push_ok(&mut responses, 65);
    responses
}

fn push_ok(responses: &mut String, rid: i32) {
    responses.push_str(&format!("@D7/1 RES {rid} 0 0\n@D7/1 END {rid}\n"));
}

fn push_fields(responses: &mut String, rid: i32, fields: &[(&str, &str)]) {
    responses.push_str(&format!("@D7/1 RES {rid} 0 {}\n", fields.len()));
    for (key, value) in fields {
        responses.push_str(&format!("K {key} raw {} {value}\n", value.len()));
    }
    responses.push_str(&format!("@D7/1 END {rid}\n"));
}

fn compile_sample_bot(root: &Path, out_dir: &Path) -> (PathBuf, PathBuf) {
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let bot_bin = out_dir.join("sample_moderation");
    let compile_wrapper = Command::new("gfortran")
        .current_dir(root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());
    let compile_bot = Command::new("gfortran")
        .current_dir(root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "examples/sample_moderation/sample_moderation.f",
        ])
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&bot_bin)
        .status()
        .expect("failed to run gfortran for sample bot");
    assert!(compile_bot.success());
    (wrapper_obj, bot_bin)
}

fn validate_sample_audit_log(runtime: &str, root: &Path, audit_log: &Path) {
    let output = Command::new(runtime)
        .current_dir(root)
        .arg("--validate-audit-log")
        .arg(audit_log)
        .args(["--audit-require-action", "D7SND"])
        .args(["--audit-require-action", "D7DEL"])
        .args(["--audit-require-action", "D7TMO"])
        .args(["--audit-forbid-action", "D7UNC"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 audit log validation");
    assert!(
        output.status.success(),
        "audit log validation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("dfor77 audit log ok:"),
        "unexpected audit validation stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).is_empty(),
        "unexpected audit validation stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sample_gateway_frames() -> &'static str {
    concat!(
        "{\"op\":0,\"s\":1,\"t\":\"READY\",\"d\":{\"session_id\":\"abc\",\"resume_gateway_url\":\"wss://resume\",\"user\":{\"id\":\"100000000000000009\"}}}\n",
        "{\"op\":0,\"s\":2,\"t\":\"GUILD_CREATE\",\"d\":{\"id\":\"100000000000000001\",\"owner_id\":\"100000000000000009\",\"channels\":[{\"id\":\"100000000000000002\",\"type\":0,\"parent_id\":\"100000000000000010\"}],\"roles\":[{\"id\":\"100000000000000001\",\"permissions\":\"0\",\"position\":0},{\"id\":\"100000000000000005\",\"permissions\":\"0\",\"position\":1}],\"members\":[{\"user\":{\"id\":\"100000000000000009\"},\"roles\":[],\"communication_disabled_until\":null},{\"user\":{\"id\":\"100000000000000004\"},\"roles\":[\"100000000000000005\"],\"communication_disabled_until\":null}]}}\n",
        "{\"op\":0,\"s\":3,\"t\":\"MESSAGE_CREATE\",\"d\":{\"guild_id\":\"100000000000000001\",\"channel_id\":\"100000000000000002\",\"id\":\"100000000000000003\",\"content\":\"moderate demo\",\"type\":0,\"attachments\":[{\"filename\":\"a.png\",\"content_type\":\"image/png\"},{\"filename\":\"b.jpg\",\"content_type\":\"image/jpeg\"},{\"filename\":\"c.gif\",\"content_type\":\"image/gif\"}],\"author\":{\"id\":\"100000000000000004\",\"bot\":false},\"member\":{\"roles\":[\"100000000000000005\"]}}}\n",
    )
}

fn sample_moderation_gateway_frames() -> &'static str {
    concat!(
        "{\"op\":0,\"s\":1,\"t\":\"READY\",\"d\":{\"session_id\":\"abc\",\"resume_gateway_url\":\"wss://resume\",\"user\":{\"id\":\"100000000000000009\"}}}\n",
        "{\"op\":0,\"s\":2,\"t\":\"GUILD_CREATE\",\"d\":{\"id\":\"100000000000000001\",\"owner_id\":\"100000000000000009\",\"channels\":[{\"id\":\"100000000000000002\",\"type\":0,\"parent_id\":\"100000000000000010\"},{\"id\":\"100000000000000006\",\"type\":0,\"parent_id\":null}],\"roles\":[{\"id\":\"100000000000000001\",\"permissions\":\"0\",\"position\":0},{\"id\":\"100000000000000005\",\"permissions\":\"0\",\"position\":1}],\"members\":[{\"user\":{\"id\":\"100000000000000009\"},\"roles\":[],\"communication_disabled_until\":null},{\"user\":{\"id\":\"100000000000000004\"},\"roles\":[\"100000000000000005\"],\"communication_disabled_until\":null}]}}\n",
        "{\"op\":0,\"s\":3,\"t\":\"MESSAGE_CREATE\",\"d\":{\"guild_id\":\"100000000000000001\",\"channel_id\":\"100000000000000002\",\"id\":\"100000000000000003\",\"content\":\"moderate demo\",\"type\":0,\"attachments\":[{\"filename\":\"a.png\",\"content_type\":\"image/png\"},{\"filename\":\"b.jpg\",\"content_type\":\"image/jpeg\"},{\"filename\":\"c.gif\",\"content_type\":\"image/gif\"}],\"author\":{\"id\":\"100000000000000004\",\"bot\":false},\"member\":{\"roles\":[\"100000000000000005\"]}}}\n",
    )
}

fn strict_preflight_runtime_gateway_frames() -> &'static str {
    concat!(
        "{\"op\":0,\"s\":1,\"t\":\"READY\",\"d\":{\"session_id\":\"abc\",\"resume_gateway_url\":\"wss://resume\",\"user\":{\"id\":\"100000000000000009\"}}}\n",
        "{\"op\":0,\"s\":2,\"t\":\"GUILD_CREATE\",\"d\":{\"id\":\"100000000000000001\",\"owner_id\":\"100000000000000009\",\"channels\":[{\"id\":\"100000000000000002\",\"type\":0,\"parent_id\":null}],\"roles\":[{\"id\":\"100000000000000001\",\"permissions\":\"0\",\"position\":0},{\"id\":\"100000000000000005\",\"permissions\":\"0\",\"position\":1}],\"members\":[{\"user\":{\"id\":\"100000000000000009\"},\"roles\":[],\"communication_disabled_until\":null},{\"user\":{\"id\":\"100000000000000004\"},\"roles\":[\"100000000000000005\"],\"communication_disabled_until\":null}]}}\n",
        "{\"op\":0,\"s\":3,\"t\":\"MESSAGE_CREATE\",\"d\":{\"guild_id\":\"100000000000000001\",\"channel_id\":\"100000000000000002\",\"id\":\"100000000000000003\",\"content\":\"ALLOW\",\"type\":0,\"attachments\":[],\"author\":{\"id\":\"100000000000000004\",\"bot\":false},\"member\":{\"roles\":[\"100000000000000005\"]}}}\n",
        "{\"op\":0,\"s\":4,\"t\":\"GUILD_CREATE\",\"d\":{\"id\":\"200000000000000001\",\"owner_id\":\"200000000000000099\",\"channels\":[{\"id\":\"200000000000000002\",\"type\":0,\"parent_id\":null}],\"roles\":[{\"id\":\"200000000000000001\",\"permissions\":\"0\",\"position\":0}],\"members\":[{\"user\":{\"id\":\"100000000000000009\"},\"roles\":[],\"communication_disabled_until\":null},{\"user\":{\"id\":\"200000000000000004\"},\"roles\":[],\"communication_disabled_until\":null}]}}\n",
        "{\"op\":0,\"s\":5,\"t\":\"MESSAGE_CREATE\",\"d\":{\"guild_id\":\"200000000000000001\",\"channel_id\":\"200000000000000002\",\"id\":\"200000000000000003\",\"content\":\"DENY\",\"type\":0,\"attachments\":[],\"author\":{\"id\":\"200000000000000004\",\"bot\":false},\"member\":{\"roles\":[]}}}\n",
        "{\"op\":0,\"s\":6,\"t\":\"GUILD_CREATE\",\"d\":{\"id\":\"300000000000000001\",\"owner_id\":\"100000000000000009\",\"channels\":[{\"id\":\"300000000000000002\",\"type\":15,\"parent_id\":null}],\"roles\":[{\"id\":\"300000000000000001\",\"permissions\":\"0\",\"position\":0}],\"members\":[{\"user\":{\"id\":\"100000000000000009\"},\"roles\":[],\"communication_disabled_until\":null},{\"user\":{\"id\":\"300000000000000004\"},\"roles\":[],\"communication_disabled_until\":null}]}}\n",
        "{\"op\":0,\"s\":7,\"t\":\"MESSAGE_CREATE\",\"d\":{\"guild_id\":\"300000000000000001\",\"channel_id\":\"300000000000000002\",\"id\":\"300000000000000003\",\"content\":\"FORUM\",\"type\":0,\"attachments\":[],\"author\":{\"id\":\"300000000000000004\",\"bot\":false},\"member\":{\"roles\":[]}}}\n",
        "{\"op\":0,\"s\":8,\"t\":\"GUILD_CREATE\",\"d\":{\"id\":\"400000000000000001\",\"owner_id\":\"100000000000000009\",\"channels\":[{\"id\":\"400000000000000010\",\"type\":0,\"parent_id\":null}],\"threads\":[{\"id\":\"400000000000000002\",\"type\":11,\"parent_id\":\"400000000000000010\",\"thread_metadata\":{\"archived\":true,\"locked\":false}}],\"roles\":[{\"id\":\"400000000000000001\",\"permissions\":\"0\",\"position\":0}],\"members\":[{\"user\":{\"id\":\"100000000000000009\"},\"roles\":[],\"communication_disabled_until\":null},{\"user\":{\"id\":\"400000000000000004\"},\"roles\":[],\"communication_disabled_until\":null}]}}\n",
        "{\"op\":0,\"s\":9,\"t\":\"MESSAGE_CREATE\",\"d\":{\"guild_id\":\"400000000000000001\",\"channel_id\":\"400000000000000002\",\"id\":\"400000000000000003\",\"content\":\"ARCHIVED\",\"type\":0,\"attachments\":[],\"author\":{\"id\":\"400000000000000004\",\"bot\":false},\"member\":{\"roles\":[]}}}\n",
        "{\"op\":0,\"s\":10,\"t\":\"GUILD_CREATE\",\"d\":{\"id\":\"500000000000000001\",\"owner_id\":\"100000000000000009\",\"channels\":[{\"id\":\"500000000000000010\",\"type\":0,\"parent_id\":null}],\"threads\":[{\"id\":\"500000000000000002\",\"type\":11,\"parent_id\":\"500000000000000010\",\"thread_metadata\":{\"archived\":false,\"locked\":true}}],\"roles\":[{\"id\":\"500000000000000001\",\"permissions\":\"0\",\"position\":0}],\"members\":[{\"user\":{\"id\":\"100000000000000009\"},\"roles\":[],\"communication_disabled_until\":null},{\"user\":{\"id\":\"500000000000000004\"},\"roles\":[],\"communication_disabled_until\":null}]}}\n",
        "{\"op\":0,\"s\":11,\"t\":\"MESSAGE_CREATE\",\"d\":{\"guild_id\":\"500000000000000001\",\"channel_id\":\"500000000000000002\",\"id\":\"500000000000000003\",\"content\":\"LOCKED\",\"type\":0,\"attachments\":[],\"author\":{\"id\":\"500000000000000004\",\"bot\":false},\"member\":{\"roles\":[]}}}\n",
    )
}

fn serve_sample_rest() -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("cannot bind local TCP listener; skipping sample bot HTTP acceptance test");
            return None;
        }
        Err(error) => panic!("failed to bind sample REST listener: {error}"),
    };
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut saw_delete = false;
        let mut saw_timeout = false;
        let mut saw_log_send = false;
        for _ in 0..10 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let request_line = request.lines().next().unwrap_or("");
            let (status, body) = sample_rest_response(request_line);
            saw_delete |= request_line.starts_with(
                "DELETE /api/v10/channels/100000000000000002/messages/100000000000000003 ",
            );
            saw_timeout |= request_line.starts_with(
                "PATCH /api/v10/guilds/100000000000000001/members/100000000000000004 ",
            );
            saw_log_send |=
                request_line.starts_with("POST /api/v10/channels/100000000000000006/messages ");
            tx.send(request).unwrap();
            write_http_response(&mut stream, status, body);
            if saw_delete && saw_timeout && saw_log_send {
                break;
            }
        }
    });
    Some((format!("http://{}", addr), rx, handle))
}

fn serve_rich_rest() -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("cannot bind local TCP listener; skipping strict rich HTTP test");
            return None;
        }
        Err(error) => panic!("failed to bind rich REST listener: {error}"),
    };
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut saw_sticker = false;
        let mut saw_poll = false;
        let mut saw_reaction = false;
        let mut saw_upload = false;
        for _ in 0..16 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let request_line = request.lines().next().unwrap_or("");
            saw_sticker |= request_line
                .starts_with("POST /api/v10/channels/100000000000000002/messages ")
                && request.contains(r#""sticker_ids":["100000000000000011"]"#);
            saw_poll |= request_line
                .starts_with("POST /api/v10/channels/100000000000000002/messages ")
                && request.contains("\"question\":{\"text\":\"Deploy now?\"}");
            saw_reaction |= request_line.starts_with(
                "PUT /api/v10/channels/100000000000000002/messages/100000000000000003/reactions/party%3A100000000000000012/@me ",
            );
            saw_upload |= request_line
                .starts_with("POST /api/v10/channels/100000000000000002/messages ")
                && request.contains("filename=\"a.txt\"")
                && request.contains("filename=\"b.bin\"");
            let (status, body) = rich_rest_response(request_line);
            tx.send(request).unwrap();
            write_http_response(&mut stream, status, body);
            if saw_sticker && saw_poll && saw_reaction && saw_upload {
                break;
            }
        }
    });
    Some((format!("http://{}", addr), rx, handle))
}

fn serve_management_rest() -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("cannot bind local TCP listener; skipping strict management HTTP test");
            return None;
        }
        Err(error) => panic!("failed to bind management REST listener: {error}"),
    };
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut saw_ccr = false;
        let mut saw_ech = false;
        let mut saw_dch = false;
        let mut saw_cro = false;
        let mut saw_ero = false;
        let mut saw_dro = false;
        let mut saw_amr = false;
        let mut saw_rmr = false;
        let mut saw_egm = false;
        let mut saw_kik = false;
        let mut saw_ban = false;
        let mut saw_ubn = false;
        let mut saw_egd = false;
        let mut saw_raw = false;
        for _ in 0..24 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let request_line = request.lines().next().unwrap_or("");
            saw_ccr |= request_line
                .starts_with("POST /api/v10/guilds/100000000000000001/channels ")
                && request.contains(r#""name":"bot-log""#);
            saw_ech |= request_line.starts_with("PATCH /api/v10/channels/100000000000000002 ")
                && request.contains(r#""name":"edited""#);
            saw_dch |= request_line.starts_with("DELETE /api/v10/channels/100000000000000002 ");
            saw_cro |= request_line.starts_with("POST /api/v10/guilds/100000000000000001/roles ")
                && request.contains(r#""name":"helper""#);
            saw_ero |= request_line
                .starts_with("PATCH /api/v10/guilds/100000000000000001/roles/100000000000000005 ")
                && request.contains(r#""name":"helper2""#);
            saw_dro |= request_line
                .starts_with("DELETE /api/v10/guilds/100000000000000001/roles/100000000000000005 ");
            saw_amr |= request_line.starts_with(
                "PUT /api/v10/guilds/100000000000000001/members/100000000000000004/roles/100000000000000005 ",
            );
            saw_rmr |= request_line.starts_with(
                "DELETE /api/v10/guilds/100000000000000001/members/100000000000000004/roles/100000000000000005 ",
            );
            saw_egm |= request_line.starts_with(
                "PATCH /api/v10/guilds/100000000000000001/members/100000000000000004 ",
            ) && request.contains(r#""nick":"bot""#);
            saw_kik |= request_line.starts_with(
                "DELETE /api/v10/guilds/100000000000000001/members/100000000000000004 ",
            );
            saw_ban |= request_line
                .starts_with("PUT /api/v10/guilds/100000000000000001/bans/100000000000000004 ")
                && request.contains("{}");
            saw_ubn |= request_line
                .starts_with("DELETE /api/v10/guilds/100000000000000001/bans/100000000000000004 ");
            saw_egd |= request_line.starts_with("PATCH /api/v10/guilds/100000000000000001 ")
                && request.contains(r#""name":"guild""#);
            saw_raw |= request_line.starts_with("GET /api/v10/users/@me ");
            let (status, body) = management_rest_response(request_line);
            tx.send(request).unwrap();
            write_http_response(&mut stream, status, body);
            if saw_ccr
                && saw_ech
                && saw_dch
                && saw_cro
                && saw_ero
                && saw_dro
                && saw_amr
                && saw_rmr
                && saw_egm
                && saw_kik
                && saw_ban
                && saw_ubn
                && saw_egd
                && saw_raw
            {
                break;
            }
        }
    });
    Some((format!("http://{}", addr), rx, handle))
}

fn serve_misc_rest() -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("cannot bind local TCP listener; skipping strict misc HTTP test");
            return None;
        }
        Err(error) => panic!("failed to bind misc REST listener: {error}"),
    };
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut saw_json_send = false;
        let mut saw_upload = false;
        let mut saw_callback = false;
        let mut saw_command = false;
        let mut saw_typing = false;
        for _ in 0..12 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let request_line = request.lines().next().unwrap_or("");
            saw_json_send |= request_line
                .starts_with("POST /api/v10/channels/100000000000000002/messages ")
                && request.contains(r#""content":"json send""#);
            saw_upload |= request_line
                .starts_with("POST /api/v10/channels/100000000000000002/messages ")
                && request.contains("filename=\"single.txt\"")
                && request.contains("single-body");
            saw_callback |= request_line
                .starts_with("POST /api/v10/interactions/100000000000000006/tok-123/callback ")
                && request.contains(r#""type":4"#);
            saw_command |= request_line.starts_with(
                "POST /api/v10/applications/100000000000000007/guilds/100000000000000001/commands ",
            ) && request.contains(r#""name":"ping""#);
            saw_typing |=
                request_line.starts_with("POST /api/v10/channels/100000000000000002/typing ");
            let (status, body) = misc_rest_response(request_line);
            tx.send(request).unwrap();
            write_http_response(&mut stream, status, body);
            if saw_json_send && saw_upload && saw_callback && saw_command && saw_typing {
                break;
            }
        }
    });
    Some((format!("http://{}", addr), rx, handle))
}

fn serve_error_rest() -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("cannot bind local TCP listener; skipping strict REST error test");
            return None;
        }
        Err(error) => panic!("failed to bind REST error listener: {error}"),
    };
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let expected_content = rest_error_contents();
        let mut seen_content = HashSet::new();
        let mut saw_read = false;
        for _ in 0..40 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let request_line = request.lines().next().unwrap_or("");
            if request_line.starts_with("POST /api/v10/channels/100000000000000002/messages ") {
                for content in expected_content {
                    if request_has_content(&request, content) {
                        seen_content.insert(*content);
                    }
                }
            }
            saw_read |= request_line.starts_with("GET /api/v10/users/@me ");
            let (status, body) = error_rest_response(request_line, &request);
            tx.send(request).unwrap();
            write_http_response(&mut stream, status, body);
            if seen_content.len() == expected_content.len() && saw_read {
                break;
            }
        }
    });
    Some((format!("http://{}", addr), rx, handle))
}

fn rest_error_contents() -> &'static [&'static str] {
    &[
        "bad", "rate", "slow", "chanrl", "guildrl", "forbid", "access", "gone", "guild", "member",
        "msg", "user", "thread", "system", "itype", "locked", "forum", "ctype", "rawbad", "unc",
    ]
}

fn request_has_content(request: &str, content: &str) -> bool {
    request.contains(&format!(r#""content":"{content}""#))
}

fn rich_rest_response(request_line: &str) -> (u16, &'static str) {
    if request_line.starts_with(
        "PUT /api/v10/channels/100000000000000002/messages/100000000000000003/reactions/party%3A100000000000000012/@me ",
    ) {
        return (204, "");
    }
    sample_rest_response(request_line)
}

fn management_rest_response(request_line: &str) -> (u16, &'static str) {
    if request_line.starts_with("GET /api/v10/users/@me ") {
        return (200, r#"{"id":"100000000000000009","username":"bot"}"#);
    }
    if request_line.starts_with("DELETE ")
        || request_line.starts_with(
            "PUT /api/v10/guilds/100000000000000001/members/100000000000000004/roles/100000000000000005 ",
        )
        || request_line.starts_with(
            "PUT /api/v10/guilds/100000000000000001/bans/100000000000000004 ",
        )
    {
        return (204, "");
    }
    if request_line.starts_with("POST ")
        || request_line.starts_with("PATCH ")
        || request_line.starts_with("PUT ")
    {
        return (200, r#"{"id":"100000000000000099"}"#);
    }
    (404, r#"{"message":"not found"}"#)
}

fn misc_rest_response(request_line: &str) -> (u16, &'static str) {
    if request_line.starts_with("POST /api/v10/interactions/100000000000000006/tok-123/callback ")
        || request_line.starts_with("POST /api/v10/channels/100000000000000002/typing ")
    {
        return (204, "");
    }
    if request_line.starts_with("POST /api/v10/channels/100000000000000002/messages ")
        || request_line.starts_with(
            "POST /api/v10/applications/100000000000000007/guilds/100000000000000001/commands ",
        )
    {
        return (200, r#"{"id":"100000000000000099"}"#);
    }
    (404, r#"{"message":"not found"}"#)
}

fn error_rest_response(request_line: &str, request: &str) -> (u16, &'static str) {
    if request_line.starts_with("GET /api/v10/users/@me ") {
        return (500, r#"{"message":"server error"}"#);
    }
    if request_line.starts_with("POST /api/v10/channels/100000000000000002/messages ") {
        if request.contains(r#""content":"bad""#) {
            return (400, r#"{"code":50035,"message":"Invalid Form Body"}"#);
        }
        if request.contains(r#""content":"rate""#) {
            return (429, r#"{"message":"rate limited"}"#);
        }
        if request.contains(r#""content":"slow""#) {
            return (400, r#"{"code":20016,"message":"Slowmode rate limit"}"#);
        }
        if request.contains(r#""content":"chanrl""#) {
            return (
                400,
                r#"{"code":20028,"message":"Channel write rate limit"}"#,
            );
        }
        if request.contains(r#""content":"guildrl""#) {
            return (400, r#"{"code":20029,"message":"Guild write rate limit"}"#);
        }
        if request.contains(r#""content":"forbid""#) {
            return (403, r#"{"code":50013,"message":"Missing Permissions"}"#);
        }
        if request.contains(r#""content":"access""#) {
            return (403, r#"{"code":50001,"message":"Missing Access"}"#);
        }
        if request.contains(r#""content":"gone""#) {
            return (404, r#"{"code":10003,"message":"Unknown Channel"}"#);
        }
        if request.contains(r#""content":"guild""#) {
            return (404, r#"{"code":10004,"message":"Unknown Guild"}"#);
        }
        if request.contains(r#""content":"member""#) {
            return (404, r#"{"code":10007,"message":"Unknown Member"}"#);
        }
        if request.contains(r#""content":"msg""#) {
            return (404, r#"{"code":10008,"message":"Unknown Message"}"#);
        }
        if request.contains(r#""content":"user""#) {
            return (404, r#"{"code":10013,"message":"Unknown User"}"#);
        }
        if request.contains(r#""content":"thread""#) {
            return (400, r#"{"code":50083,"message":"Thread is archived"}"#);
        }
        if request.contains(r#""content":"system""#) {
            return (
                400,
                r#"{"code":50021,"message":"Cannot execute action on a system message"}"#,
            );
        }
        if request.contains(r#""content":"itype""#) {
            return (400, r#"{"code":50068,"message":"Invalid message type"}"#);
        }
        if request.contains(r#""content":"locked""#) {
            return (403, r#"{"code":160005,"message":"Thread is locked"}"#);
        }
        if request.contains(r#""content":"forum""#) {
            return (
                400,
                r#"{"code":40058,"message":"Cannot send a message in a forum channel"}"#,
            );
        }
        if request.contains(r#""content":"ctype""#) {
            return (
                400,
                r#"{"code":50024,"message":"Cannot execute action on this channel type"}"#,
            );
        }
        if request.contains(r#""content":"rawbad""#) {
            return (400, "not-json");
        }
        if request.contains(r#""content":"unc""#) {
            return (500, r#"{"message":"server error"}"#);
        }
    }
    (404, r#"{"message":"not found"}"#)
}

fn sample_rest_response(request_line: &str) -> (u16, &'static str) {
    if request_line.starts_with("GET /api/v10/users/@me ") {
        return (200, r#"{"id":"100000000000000009"}"#);
    }
    if request_line.starts_with("GET /api/v10/channels/100000000000000002 ") {
        return (
            200,
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0,"parent_id":"100000000000000010","permission_overwrites":[]}"#,
        );
    }
    if request_line.starts_with("GET /api/v10/channels/100000000000000002 ") {
        return (
            200,
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0,"parent_id":"100000000000000010","permission_overwrites":[]}"#,
        );
    }
    if request_line.starts_with("GET /api/v10/channels/100000000000000006 ") {
        return (
            200,
            r#"{"id":"100000000000000006","guild_id":"100000000000000001","type":0,"parent_id":null,"permission_overwrites":[]}"#,
        );
    }
    if request_line
        .starts_with("GET /api/v10/guilds/100000000000000001/members/100000000000000009 ")
    {
        return (
            200,
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#,
        );
    }
    if request_line
        .starts_with("GET /api/v10/guilds/100000000000000001/members/100000000000000004 ")
    {
        return (
            200,
            r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000005"],"communication_disabled_until":null}"#,
        );
    }
    if request_line.starts_with("GET /api/v10/guilds/100000000000000001/roles ") {
        return (
            200,
            r#"[{"id":"100000000000000001","permissions":"0","position":0},{"id":"100000000000000005","permissions":"0","position":1}]"#,
        );
    }
    if request_line
        .starts_with("DELETE /api/v10/channels/100000000000000002/messages/100000000000000003 ")
    {
        return (204, "");
    }
    if request_line
        .starts_with("DELETE /api/v10/channels/100000000000000002/messages/100000000000000003 ")
    {
        return (204, "");
    }
    if request_line
        .starts_with("PATCH /api/v10/guilds/100000000000000001/members/100000000000000004 ")
    {
        return (
            200,
            r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000005"],"communication_disabled_until":"2030-01-01T00:10:00.000Z"}"#,
        );
    }
    if request_line.starts_with("POST /api/v10/channels/100000000000000002/messages ") {
        return (200, r#"{"id":"100000000000000099"}"#);
    }
    if request_line.starts_with("POST /api/v10/channels/100000000000000006/messages ") {
        return (200, r#"{"id":"100000000000000099"}"#);
    }
    (404, r#"{"message":"not found"}"#)
}

fn read_http_request(stream: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some((head, body_start)) = split_head_body(&bytes) {
            let content_length = parse_content_length(head).unwrap_or(0);
            if bytes.len().saturating_sub(body_start) >= content_length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn split_head_body(bytes: &[u8]) -> Option<(&str, usize)> {
    let marker = b"\r\n\r\n";
    let index = bytes
        .windows(marker.len())
        .position(|window| window == marker)?;
    let head = std::str::from_utf8(&bytes[..index]).ok()?;
    Some((head, index + marker.len()))
}

fn parse_content_length(head: &str) -> Option<usize> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
}

fn write_http_response(stream: &mut TcpStream, status: u16, body: &str) {
    let mut response = format!("HTTP/1.1 {} OK\r\n", status);
    response.push_str("Content-Type: application/json\r\nContent-Length: ");
    response.push_str(&body.len().to_string());
    response.push_str("\r\nConnection: close\r\n\r\n");
    response.push_str(body);
    stream.write_all(response.as_bytes()).unwrap();
}

#[test]
fn fortran_payload_builders_generate_discord_json() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping payload builder test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-payload-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("payload_builders.f");
    let program_bin = out_dir.join("payload_builders");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS, OUTLEN, TRUNC\n",
                "      CHARACTER*60000 PAY\n",
                "      CHARACTER*8 RAW\n",
                "      CHARACTER*16 ONAM(2), ODSC(2), LAB(3), VAL(3)\n",
                "      CHARACTER*16 FNAM(2), FVAL(2)\n",
                "      CHARACTER*8 BLAB(6), BCID(6)\n",
                "      INTEGER OLEN(2), ODLN(2), OTYP(2), OREQ(2)\n",
                "      INTEGER LLEN(3), VLEN(3), BLLN(6), BCLN(6)\n",
                "      INTEGER FNLN(2), FVLN(2), FINL(2)\n",
                "      INTEGER BSTY(6), BROW(6)\n",
                "      RAW = 'A\"B'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      ONAM(1) = 'query'\n",
                "      ONAM(2) = 'user'\n",
                "      OLEN(1) = 5\n",
                "      OLEN(2) = 4\n",
                "      ODSC(1) = 'search text'\n",
                "      ODSC(2) = 'target user'\n",
                "      ODLN(1) = 11\n",
                "      ODLN(2) = 11\n",
                "      OTYP(1) = D7OST\n",
                "      OTYP(2) = D7OUS\n",
                "      OREQ(1) = 1\n",
                "      OREQ(2) = 0\n",
                "      LAB(1) = 'Red'\n",
                "      LAB(2) = 'Blue'\n",
                "      LAB(3) = 'Green'\n",
                "      VAL(1) = 'red'\n",
                "      VAL(2) = 'blue'\n",
                "      VAL(3) = 'green'\n",
                "      LLEN(1) = 3\n",
                "      LLEN(2) = 4\n",
                "      LLEN(3) = 5\n",
                "      VLEN(1) = 3\n",
                "      VLEN(2) = 4\n",
                "      VLEN(3) = 5\n",
                "      BLAB(1) = 'A'\n",
                "      BLAB(2) = 'B'\n",
                "      BLAB(3) = 'C'\n",
                "      BLAB(4) = 'D'\n",
                "      BLAB(5) = 'E'\n",
                "      BLAB(6) = 'F'\n",
                "      BCID(1) = 'a'\n",
                "      BCID(2) = 'b'\n",
                "      BCID(3) = 'c'\n",
                "      BCID(4) = 'd'\n",
                "      BCID(5) = 'e'\n",
                "      BCID(6) = 'f'\n",
                "      BLLN(1) = 1\n",
                "      BLLN(2) = 1\n",
                "      BLLN(3) = 1\n",
                "      BLLN(4) = 1\n",
                "      BLLN(5) = 1\n",
                "      BLLN(6) = 1\n",
                "      BCLN(1) = 1\n",
                "      BCLN(2) = 1\n",
                "      BCLN(3) = 1\n",
                "      BCLN(4) = 1\n",
                "      BCLN(5) = 1\n",
                "      BCLN(6) = 1\n",
                "      BSTY(1) = D7BPR\n",
                "      BSTY(2) = D7BSE\n",
                "      BSTY(3) = D7BSU\n",
                "      BSTY(4) = D7BDN\n",
                "      BSTY(5) = D7BPR\n",
                "      BSTY(6) = D7BSE\n",
                "      BROW(1) = 1\n",
                "      BROW(2) = 1\n",
                "      BROW(3) = 1\n",
                "      BROW(4) = 2\n",
                "      BROW(5) = 2\n",
                "      BROW(6) = 2\n",
                "      FNAM(1) = 'n1'\n",
                "      FNAM(2) = 'n2'\n",
                "      FVAL(1) = 'v1'\n",
                "      FVAL(2) = 'v2'\n",
                "      FNLN(1) = 2\n",
                "      FNLN(2) = 2\n",
                "      FVLN(1) = 2\n",
                "      FVLN(2) = 2\n",
                "      FINL(1) = 1\n",
                "      FINL(2) = 0\n",
                "      CALL D7JES(RAW, 3, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'JES', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7PEM('hello', 5, 'title', 5, 'desc', 4, 255, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'EMB', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7PE2('hello', 5, 'title', 5, 'desc', 4, 'https://x', 9, 255, '2026-01-01T00:00:00Z', 20, 'foot', 4, 'https://f', 9, 'https://i', 9, 'https://t', 9, 'auth', 4, 'https://a', 9, 'https://c', 9, 2, FNAM, FNLN, FVAL, FVLN, FINL, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'EM2', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7PBT('press', 5, 'go', 2, 'btn1', 4, D7BPR, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'BTN', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7IMR('ok', 2, D7EPH, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'IMR', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7IDR(0, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'IDR', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7IMD('m', 1, 'modal1', 6, 'answer', 6, 'field1', 6, D7TIS, 1, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'IMD', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7SCM('hello', 5, 'say hello', 9, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'SCM', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7SCX('search', 6, 'find things', 11, 2, ONAM, OLEN, ODSC, ODLN, OTYP, OREQ, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'SCX', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7PSM('choose', 6, 'menu1', 5, 3, LAB, LLEN, VAL, VLEN, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'PSM', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7PAC('actions', 7, 6, BLAB, BLLN, BCID, BCLN, BSTY, BROW, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'PAC', STATUS, OUTLEN, TRUNC, PAY(1:OUTLEN)\n",
                "      CALL D7PEM('hello', 5, 'title', 5, 'desc', 4, 255, PAY, 12, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'TRN', STATUS, OUTLEN, TRUNC, PAY(1:12)\n",
                "      CALL D7SCM('Bad', 3, 'bad', 3, PAY, 60000, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1010) 'BAD', STATUS, OUTLEN, TRUNC\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(A,1X,I0,1X,I0,1X,I0,1X,A)\n",
                " 1010 FORMAT(A,1X,I0,1X,I0,1X,I0)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write payload builder program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for payload builder program");
    assert!(compile_program.success());

    let output = Command::new(&program_bin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run payload builder program");
    assert!(
        output.status.success(),
        "payload builder program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let lines = result.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], r#"JES 0 4 0 A\"B"#);
    assert_eq!(
        lines[1],
        r#"EMB 0 113 0 {"content":"hello","allowed_mentions":{"parse":[]},"embeds":[{"title":"title","description":"desc","color":255}]}"#
    );
    assert_eq!(
        lines[2],
        r#"EM2 0 434 0 {"content":"hello","allowed_mentions":{"parse":[]},"embeds":[{"title":"title","description":"desc","url":"https://x","timestamp":"2026-01-01T00:00:00Z","color":255,"footer":{"text":"foot","icon_url":"https://f"},"image":{"url":"https://i"},"thumbnail":{"url":"https://t"},"author":{"name":"auth","url":"https://a","icon_url":"https://c"},"fields":[{"name":"n1","value":"v1","inline":true},{"name":"n2","value":"v2","inline":false}]}]}"#
    );
    assert_eq!(
        lines[3],
        r#"BTN 0 145 0 {"content":"press","allowed_mentions":{"parse":[]},"components":[{"type":1,"components":[{"type":2,"style":1,"label":"go","custom_id":"btn1"}]}]}"#
    );
    assert_eq!(
        lines[4],
        r#"IMR 0 77 0 {"type":4,"data":{"content":"ok","allowed_mentions":{"parse":[]},"flags":64}}"#
    );
    assert_eq!(lines[5], r#"IDR 0 10 0 {"type":5}"#);
    assert_eq!(
        lines[6],
        r#"IMD 0 168 0 {"type":9,"data":{"title":"m","custom_id":"modal1","components":[{"type":1,"components":[{"type":4,"custom_id":"field1","label":"answer","style":1,"required":true}]}]}}"#
    );
    assert_eq!(
        lines[7],
        r#"SCM 0 51 0 {"name":"hello","description":"say hello","type":1}"#
    );
    assert_eq!(
        lines[8],
        r#"SCX 0 206 0 {"name":"search","description":"find things","type":1,"options":[{"type":3,"name":"query","description":"search text","required":true},{"type":6,"name":"user","description":"target user","required":false}]}"#
    );
    assert_eq!(
        lines[9],
        r#"PSM 0 232 0 {"content":"choose","allowed_mentions":{"parse":[]},"components":[{"type":1,"components":[{"type":3,"custom_id":"menu1","options":[{"label":"Red","value":"red"},{"label":"Blue","value":"blue"},{"label":"Green","value":"green"}]}]}]}"#
    );
    assert_eq!(
        lines[10],
        r#"PAC 0 414 0 {"content":"actions","allowed_mentions":{"parse":[]},"components":[{"type":1,"components":[{"type":2,"style":1,"label":"A","custom_id":"a"},{"type":2,"style":2,"label":"B","custom_id":"b"},{"type":2,"style":3,"label":"C","custom_id":"c"}]},{"type":1,"components":[{"type":2,"style":4,"label":"D","custom_id":"d"},{"type":2,"style":1,"label":"E","custom_id":"e"},{"type":2,"style":2,"label":"F","custom_id":"f"}]}]}"#
    );
    assert!(lines[11].starts_with("TRN 10 113 1 "));
    assert!(lines[11].ends_with(r#"{"content":""#));
    assert_eq!(lines[12], "BAD 30 0 0");

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_sends_multi_attachment_request() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper multi upload test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-upm-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_upm.f");
    let program_bin = out_dir.join("wrapper_upm");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS\n",
                "      INTEGER FLEN(2), NLEN(2)\n",
                "      CHARACTER*18 CHID\n",
                "      CHARACTER*16 PATHS(2), NAMES(2)\n",
                "      CHARACTER*8 MSG\n",
                "      CHID = '100000000000000001'\n",
                "      PATHS(1) = 'a.txt'\n",
                "      PATHS(2) = 'b.bin'\n",
                "      NAMES(1) = 'a.txt'\n",
                "      NAMES(2) = 'b.bin'\n",
                "      FLEN(1) = 5\n",
                "      FLEN(2) = 5\n",
                "      NLEN(1) = 5\n",
                "      NLEN(2) = 5\n",
                "      MSG = 'hi file'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7UPM(CHID, 18, 2, PATHS, FLEN, NAMES, NLEN, MSG, 7, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7UPM(CHID, 18, 0, PATHS, FLEN, NAMES, NLEN, MSG, 7, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(I0)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper multi upload program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper multi upload program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run wrapper multi upload program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(b"@D7/1 RES 1 0 0\n@D7/1 END 1\n")
        .expect("failed to write fake runtime response");
    let output = child
        .wait_with_output()
        .expect("failed to wait for wrapper multi upload program");
    assert!(
        output.status.success(),
        "wrapper multi upload program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![0, 30]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("@D7/1 REQ 1 UPM 7"));
    assert!(stdout.contains("K channel_id raw 18 100000000000000001"));
    assert!(stdout.contains("K count raw 1 2"));
    assert!(stdout.contains("K content pct 9 hi%20file"));
    assert!(stdout.contains("K file_path1 pct 5 a.txt"));
    assert!(stdout.contains("K file_name1 pct 5 a.txt"));
    assert!(stdout.contains("K file_path2 pct 5 b.bin"));
    assert!(stdout.contains("K file_name2 pct 5 b.bin"));
    assert_eq!(stdout.matches("@D7/1 REQ").count(), 1);

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_sends_sticker_request() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper sticker test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-stk-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_stk.f");
    let program_bin = out_dir.join("wrapper_stk");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS\n",
                "      INTEGER SLEN(2)\n",
                "      CHARACTER*18 CHID\n",
                "      CHARACTER*18 STKID(2)\n",
                "      CHARACTER*8 MSG\n",
                "      CHID = '100000000000000001'\n",
                "      STKID(1) = '100000000000000002'\n",
                "      STKID(2) = '100000000000000003'\n",
                "      SLEN(1) = 18\n",
                "      SLEN(2) = 18\n",
                "      MSG = 'sticker'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7STK(CHID, 18, 2, STKID, SLEN, MSG, 7, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7STK(CHID, 18, 0, STKID, SLEN, MSG, 7, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(I0)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper sticker program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper sticker program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run wrapper sticker program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(b"@D7/1 RES 1 0 0\n@D7/1 END 1\n")
        .expect("failed to write fake runtime response");
    let output = child
        .wait_with_output()
        .expect("failed to wait for wrapper sticker program");
    assert!(
        output.status.success(),
        "wrapper sticker program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![0, 30]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("@D7/1 REQ 1 STK 5"));
    assert!(stdout.contains("K channel_id raw 18 100000000000000001"));
    assert!(stdout.contains("K count raw 1 2"));
    assert!(stdout.contains("K content pct 7 sticker"));
    assert!(stdout.contains("K sticker_id1 raw 18 100000000000000002"));
    assert!(stdout.contains("K sticker_id2 raw 18 100000000000000003"));
    assert_eq!(stdout.matches("@D7/1 REQ").count(), 1);

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_sends_poll_request() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper poll test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-pol-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_pol.f");
    let program_bin = out_dir.join("wrapper_pol");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS\n",
                "      INTEGER ALEN(3)\n",
                "      CHARACTER*18 CHID\n",
                "      CHARACTER*12 QUEST\n",
                "      CHARACTER*8 ANS(3)\n",
                "      CHARACTER*8 MSG\n",
                "      CHID = '100000000000000001'\n",
                "      QUEST = 'Deploy now?'\n",
                "      ANS(1) = 'yes'\n",
                "      ANS(2) = 'no'\n",
                "      ANS(3) = 'later'\n",
                "      ALEN(1) = 3\n",
                "      ALEN(2) = 2\n",
                "      ALEN(3) = 5\n",
                "      MSG = 'poll msg'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7POL(CHID, 18, QUEST, 11, 3, ANS, ALEN, 24, 1, MSG, 8, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7POL(CHID, 18, QUEST, 11, 1, ANS, ALEN, 24, 1, MSG, 8, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(I0)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper poll program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper poll program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run wrapper poll program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(b"@D7/1 RES 1 0 0\n@D7/1 END 1\n")
        .expect("failed to write fake runtime response");
    let output = child
        .wait_with_output()
        .expect("failed to wait for wrapper poll program");
    assert!(
        output.status.success(),
        "wrapper poll program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![0, 30]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("@D7/1 REQ 1 POL 9"));
    assert!(stdout.contains("K channel_id raw 18 100000000000000001"));
    assert!(stdout.contains("K question pct 15 Deploy%20now%3F"));
    assert!(stdout.contains("K count raw 1 3"));
    assert!(stdout.contains("K duration_hours raw 2 24"));
    assert!(stdout.contains("K allow_multiselect raw 1 1"));
    assert!(stdout.contains("K content pct 10 poll%20msg"));
    assert!(stdout.contains("K answer1 pct 3 yes"));
    assert!(stdout.contains("K answer2 pct 2 no"));
    assert!(stdout.contains("K answer3 pct 5 later"));
    assert_eq!(stdout.matches("@D7/1 REQ").count(), 1);

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_sends_reaction_request() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper reaction test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-rac-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_rac.f");
    let program_bin = out_dir.join("wrapper_rac");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS\n",
                "      CHARACTER*18 CHID, MID, USER\n",
                "      CHARACTER*24 EMOJI\n",
                "      CHID = '100000000000000001'\n",
                "      MID = '100000000000000002'\n",
                "      USER = '100000000000000004'\n",
                "      EMOJI = 'party:100000000000000003'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7RAC(CHID, 18, MID, 18, EMOJI, 24, USER, 18, D7RDU, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7RAC(CHID, 18, MID, 18, EMOJI, 24, USER, 18, D7RAD, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(I0)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper reaction program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper reaction program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run wrapper reaction program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(b"@D7/1 RES 1 0 0\n@D7/1 END 1\n")
        .expect("failed to write fake runtime response");
    let output = child
        .wait_with_output()
        .expect("failed to wait for wrapper reaction program");
    assert!(
        output.status.success(),
        "wrapper reaction program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![0, 30]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("@D7/1 REQ 1 RAC 5"));
    assert!(stdout.contains("K channel_id raw 18 100000000000000001"));
    assert!(stdout.contains("K message_id raw 18 100000000000000002"));
    assert!(stdout.contains("K emoji pct 26 party%3A100000000000000003"));
    assert!(stdout.contains("K user_id raw 18 100000000000000004"));
    assert!(stdout.contains("K mode raw 1 3"));
    assert_eq!(stdout.matches("@D7/1 REQ").count(), 1);

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_sends_typed_rest_requests() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping typed REST wrapper test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-typed-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_typed.f");
    let program_bin = out_dir.join("wrapper_typed");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INTEGER STATUS\n",
                "      CHARACTER*18 CHID, MID, GID, APP\n",
                "      CHARACTER*5 TOK\n",
                "      CHARACTER*8 QRY\n",
                "      CHARACTER*2 PAY\n",
                "      CHID = '100000000000000001'\n",
                "      MID = '100000000000000002'\n",
                "      GID = '100000000000000003'\n",
                "      APP = '100000000000000004'\n",
                "      TOK = 'tokxx'\n",
                "      QRY = '?limit=2'\n",
                "      PAY = '{{}}'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7GMS(CHID,18,MID,18,STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7LMS(CHID,18,QRY,8,STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7GCH(CHID,18,STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7AUM(1,GID,18,' ',0,' ',0,' ',0,STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7AUM(3,GID,18,' ',0,PAY,2,'why',3,STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7IFU(1,APP,18,TOK,5,' ',0,PAY,2,STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7IFU(2,APP,18,TOK,5,' ',0,' ',0,STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7THR(9,CHID,18,' ',0,'?limit=1',8,' ',0,STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7GCU(STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7CDM(PAY,2,STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7GWB(STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(I0)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write typed wrapper program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for typed wrapper program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run typed wrapper program");
    let mut responses = Vec::new();
    for id in 1..=11 {
        responses.extend_from_slice(format!("@D7/1 RES {id} 0 0\n@D7/1 END {id}\n").as_bytes());
    }
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(&responses)
        .expect("failed to write fake runtime responses");
    let output = child
        .wait_with_output()
        .expect("failed to wait for typed wrapper program");
    assert!(
        output.status.success(),
        "typed wrapper program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![0; 11]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("@D7/1 REQ").count(), 11);
    assert!(stdout.contains("/api/v10/channels/100000000000000001/messages/100000000000000002"));
    assert!(stdout.contains("/api/v10/channels/100000000000000001/messages?limit=2"));
    assert!(stdout.contains("/api/v10/channels/100000000000000001"));
    assert!(stdout.contains("/api/v10/guilds/100000000000000003/auto-moderation/rules"));
    assert!(stdout.contains("K method raw 1 2"));
    assert!(stdout.contains("K reason pct 3 why"));
    assert!(stdout.contains("/api/v10/webhooks/100000000000000004/tokxx"));
    assert!(stdout.contains("/api/v10/webhooks/100000000000000004/tokxx/messages/@original"));
    assert!(stdout.contains("/api/v10/channels/100000000000000001/threads/archived/public?limit=1"));
    assert!(stdout.contains("/api/v10/users/@me"));
    assert!(stdout.contains("/api/v10/users/@me/channels"));
    assert!(stdout.contains("/api/v10/gateway/bot"));

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_reads_message_snapshot_search() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping message snapshot search wrapper test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-msf-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_msf.f");
    let program_bin = out_dir.join("wrapper_msf");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INTEGER STATUS,COUNT,TRUNC,CLEN,CTRUN,MLEN,MTRUN\n",
                "      INTEGER IMG,DELTA\n",
                "      CHARACTER*18 GID,USER,BASE,CHID,MID\n",
                "      GID = '100000000000000001'\n",
                "      USER = '100000000000000004'\n",
                "      BASE = '100000000000000099'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7MSF(GID,18,USER,18,BASE,18,60,60,2,10,\n",
                "     1            COUNT,TRUNC,STATUS)\n",
                "      WRITE(9,1000) STATUS,COUNT,TRUNC\n",
                "      CALL D7MSI(1,CHID,18,CLEN,CTRUN,MID,18,MLEN,\n",
                "     1            MTRUN,IMG,DELTA,STATUS)\n",
                "      WRITE(9,1000) STATUS,CLEN,MLEN,IMG,DELTA\n",
                "      CALL D7SFD(MID,18,BASE,18,DELTA,STATUS)\n",
                "      WRITE(9,1000) STATUS,DELTA\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(8(I0,1X))\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write message snapshot search wrapper program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for message snapshot search wrapper program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run message snapshot search wrapper program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(
            concat!(
                "@D7/1 RES 1 0 2\n",
                "K count raw 1 2\n",
                "K truncated raw 1 0\n",
                "@D7/1 END 1\n",
                "@D7/1 RES 2 0 4\n",
                "K channel_id raw 18 100000000000000010\n",
                "K message_id raw 18 100000000000000011\n",
                "K image_count raw 1 2\n",
                "K delta_seconds raw 3 -30\n",
                "@D7/1 END 2\n",
                "@D7/1 RES 3 0 1\n",
                "K delta_seconds raw 2 30\n",
                "@D7/1 END 3\n"
            )
            .as_bytes(),
        )
        .expect("failed to write fake runtime responses");
    let output = child
        .wait_with_output()
        .expect("failed to wait for message snapshot search wrapper program");
    assert!(
        output.status.success(),
        "message snapshot search wrapper program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    assert_eq!(
        result
            .split_whitespace()
            .map(|value| value.parse::<i32>().expect("invalid status"))
            .collect::<Vec<_>>(),
        vec![0, 2, 0, 0, 18, 18, 2, -30, 0, 30]
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("@D7/1 REQ").count(), 3);
    assert!(stdout.contains("@D7/1 REQ 1 MSF 7"));
    assert!(stdout.contains("K guild_id raw 18 100000000000000001"));
    assert!(stdout.contains("K user_id raw 18 100000000000000004"));
    assert!(stdout.contains("K base_message_id raw 18 100000000000000099"));
    assert!(stdout.contains("K before_seconds raw 2 60"));
    assert!(stdout.contains("K after_seconds raw 2 60"));
    assert!(stdout.contains("K min_image_count raw 1 2"));
    assert!(stdout.contains("K limit raw 2 10"));
    assert!(stdout.contains("@D7/1 REQ 2 MSI 1"));
    assert!(stdout.contains("K index raw 1 1"));
    assert!(stdout.contains("@D7/1 REQ 3 SFD 2"));
    assert!(stdout.contains("K id1 raw 18 100000000000000011"));
    assert!(stdout.contains("K id2 raw 18 100000000000000099"));

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_reads_reaction_emoji_getter() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping reaction emoji getter test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-emj-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_emj.f");
    let program_bin = out_dir.join("wrapper_emj");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INTEGER STAT, OUTL, TRUN\n",
                "      CHARACTER*32 BUF\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7EMJ(BUF,32,OUTL,TRUN,STAT)\n",
                "      WRITE(9,1000) STAT,OUTL,TRUN,BUF(1:OUTL)\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(I0,1X,I0,1X,I0,1X,A)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write reaction emoji getter program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for reaction emoji program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run reaction emoji program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(b"@D7/1 RES 1 0 1\nK value pct 10 party%3A50\n@D7/1 END 1\n")
        .expect("failed to write fake runtime response");
    let output = child
        .wait_with_output()
        .expect("failed to wait for reaction emoji program");
    assert!(
        output.status.success(),
        "reaction emoji program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    assert_eq!(result.trim(), "0 8 0 party:50");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("@D7/1 REQ 1 GET 1"));
    assert!(stdout.contains("K name raw 3 EMJ"));

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_extension_helpers_build_payloads_and_file_data_uri() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping extension helper test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-ext-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let extension_obj = out_dir.join("dfor77_extension.o");
    let program_src = out_dir.join("wrapper_ext.f");
    let program_bin = out_dir.join("wrapper_ext");
    let result_file = out_dir.join("result.txt");
    let data_file = out_dir.join("data.bin");
    std::fs::write(&data_file, b"abc").expect("failed to write extension data file");
    let result_path = result_file.to_string_lossy();
    let data_path = data_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());
    let compile_extension = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "extension/dfor77_extension.f",
            "-o",
        ])
        .arg(&extension_obj)
        .status()
        .expect("failed to run gfortran for extension");
    assert!(compile_extension.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      CHARACTER*60000 BUF\n",
                "      INTEGER STAT, OUTL, TRUN\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7XQ0(BUF,60000,OUTL,TRUN,STAT)\n",
                "      CALL D7XQA(BUF,60000,OUTL,'limit',5,'50',2,OUTL,TRUN,STAT)\n",
                "      CALL D7XQA(BUF,60000,OUTL,'query',5,'a b',3,OUTL,TRUN,STAT)\n",
                "      WRITE(9,1000) 'QRY',STAT,OUTL,TRUN,BUF(1:OUTL)\n",
                "      CALL D7XACB('hello',5,'say hello',9,1,BUF,60000,OUTL,TRUN,STAT)\n",
                "      WRITE(9,1000) 'ACB',STAT,OUTL,TRUN,BUF(1:OUTL)\n",
                "      CALL D7XAMK('block',5,'spam',4,BUF,60000,OUTL,TRUN,STAT)\n",
                "      WRITE(9,1000) 'AMK',STAT,OUTL,TRUN,BUF(1:OUTL)\n",
                "      CALL D7XURI('image/png',9,'YWJj',4,BUF,60000,OUTL,TRUN,STAT)\n",
                "      WRITE(9,1000) 'URI',STAT,OUTL,TRUN,BUF(1:OUTL)\n",
                "      CALL D7XF64('text/plain',10,'{}',{},BUF,60000,OUTL,TRUN,STAT)\n",
                "      WRITE(9,1000) 'F64',STAT,OUTL,TRUN,BUF(1:OUTL)\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(A,1X,I0,1X,I0,1X,I0,1X,A)\n",
                "      END\n"
            ),
            result_path,
            data_path,
            data_path.len()
        ),
    )
    .expect("failed to write extension helper program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg(&extension_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for extension helper program");
    assert!(compile_program.success());

    let output = Command::new(&program_bin)
        .output()
        .expect("failed to run extension helper program");
    assert!(
        output.status.success(),
        "extension helper program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    assert!(result.contains("QRY 0 21 0 ?limit=50&query=a%20b"));
    assert!(result.contains(r#"ACB 0 51 0 {"name":"hello","description":"say hello","type":1}"#));
    assert!(result.contains(r#"AMK 0 118 0 {"name":"block","event_type":1,"trigger_type":1,"trigger_metadata":{"keyword_filter":["spam"]},"actions":[{"type":1}]}"#));
    assert!(result.contains("URI 0 26 0 data:image/png;base64,YWJj"));
    assert!(result.contains("F64 0 27 0 data:text/plain;base64,YWJj"));

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(extension_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_file(data_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_maps_malformed_ipc_to_d7prt() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper protocol test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-wrapper-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_protocol.f");
    let program_bin = out_dir.join("wrapper_protocol");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            "      PROGRAM T\n      INCLUDE 'dfor77.inc'\n      INTEGER STATUS, OUTLEN, TRUNC\n      CHARACTER*32 BUF\n      OUTLEN = -1\n      TRUNC = -1\n      CALL D7GID(BUF, 32, OUTLEN, TRUNC, STATUS)\n      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n 1000 FORMAT(I0,1X,I0,1X,I0)\n      CLOSE(9)\n      END\n",
            result_path
        ),
    )
    .expect("failed to write wrapper protocol program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper protocol program");
    assert!(compile_program.success());

    let mut oversized_field_count = String::from("@D7/1 RES 1 0 65\n");
    for _ in 0..65 {
        oversized_field_count.push_str("K other raw 1 x\n");
    }
    oversized_field_count.push_str("@D7/1 END 1\n");
    let long_key = format!(
        "@D7/1 RES 1 0 1\nK {} raw 1 x\n@D7/1 END 1\n",
        "a".repeat(33)
    );
    let valid_leading_zero_id = "@D7/1 RES 001 0 1\nK value raw 2 ok\n@D7/1 END 001\n";
    let _ = std::fs::remove_file(&result_file);
    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run wrapper protocol program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(valid_leading_zero_id.as_bytes())
        .expect("failed to write fake runtime response");
    let output = child
        .wait_with_output()
        .expect("failed to wait for wrapper protocol program");
    assert!(
        output.status.success(),
        "wrapper protocol program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let values = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid result integer"))
        .collect::<Vec<_>>();
    assert_eq!(values, vec![0, 2, 0]);

    let inputs = [
        "BROKEN\n@D7/1 END 1\n".to_string(),
        "@D7/1 RES +1 0 1\nK value raw 2 ok\n@D7/1 END +1\n".to_string(),
        "@D7/1 RES 0 0 1\nK value raw 2 ok\n@D7/1 END 0\n".to_string(),
        "@D7/1 RES 9223372036854775808 0 1\nK value raw 2 ok\n@D7/1 END 9223372036854775808\n"
            .to_string(),
        "@D7/1 RES 1 0 1 extra\nK value raw 2 ok\n@D7/1 END 1\n".to_string(),
        "@D7/1 RES 1 0 1\nK value raw 5 hi\n@D7/1 END 1\n".to_string(),
        "@D7/1 RES 1 0 1\nK bad-key raw 1 x\n@D7/1 END 1\n".to_string(),
        "@D7/1 RES 1 0 1\nK value raw 0 extra\n@D7/1 END 1\n".to_string(),
        "@D7/1 RES 1 0 1\nK value raw 2 ok extra\n@D7/1 END 1\n".to_string(),
        "@D7/1 RES 1 0 1\nK value pct 3 %FF\n@D7/1 END 1\n".to_string(),
        "@D7/1 RES 1 0 1\nK value pct 6 %C0%AF\n@D7/1 END 1\n".to_string(),
        "@D7/1 RES 1 0 1\nK value raw 2 ok\n@D7/1 FIN 1\n".to_string(),
        "@D7/1 RES 1 0 1\nK value raw 2 ok\n@D7/1 END 1 extra\n".to_string(),
        oversized_field_count,
        long_key,
    ];
    for input in inputs {
        let _ = std::fs::remove_file(&result_file);
        let mut child = Command::new(&program_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to run wrapper protocol program");
        child
            .stdin
            .as_mut()
            .expect("missing child stdin")
            .write_all(input.as_bytes())
            .expect("failed to write fake runtime response");
        let output = child
            .wait_with_output()
            .expect("failed to wait for wrapper protocol program");
        assert!(
            output.status.success(),
            "wrapper protocol program failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
        let values = result
            .split_whitespace()
            .map(|value| value.parse::<i32>().expect("invalid result integer"))
            .collect::<Vec<_>>();
        assert_eq!(values, vec![60, 0, 0], "input {input:?}");
    }

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_preserves_skipped_field_protocol_errors() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper skipped field test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-skip-protocol-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_skip_protocol.f");
    let program_bin = out_dir.join("wrapper_skip_protocol");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            "      PROGRAM T\n      INCLUDE 'dfor77.inc'\n      INTEGER STATUS\n      CALL D7END(STATUS)\n      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n      WRITE(9,1000) STATUS\n 1000 FORMAT(I0)\n      CLOSE(9)\n      END\n",
            result_path
        ),
    )
    .expect("failed to write wrapper skipped field program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper skipped field program");
    assert!(compile_program.success());

    let oversized_frame = format!(
        "@D7/1 RES 1 0 2\nK first raw 60000 {}\nK second raw 6000 {}\n@D7/1 END 1\n",
        "a".repeat(60000),
        "b".repeat(6000)
    );
    let inputs = [
        "@D7/1 RES 1 0 1\nK ignored raw 5 hi\n@D7/1 END 1\n".to_string(),
        oversized_frame,
    ];
    for input in inputs {
        let _ = std::fs::remove_file(&result_file);
        let mut child = Command::new(&program_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to run wrapper skipped field program");
        child
            .stdin
            .as_mut()
            .expect("missing child stdin")
            .write_all(input.as_bytes())
            .expect("failed to write fake runtime response");
        let output = child
            .wait_with_output()
            .expect("failed to wait for wrapper skipped field program");
        assert!(
            output.status.success(),
            "wrapper skipped field program failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
        let status = result.trim().parse::<i32>().expect("invalid status");
        assert_eq!(status, 60);
    }

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_can_wrappers_clear_can_on_error_status() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper can status test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-can-status-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_can_status.f");
    let program_bin = out_dir.join("wrapper_can_status");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS, CAN, OUTLEN, TRUNC\n",
                "      CHARACTER*18 GID, CHID, MID\n",
                "      CHARACTER*8 WHY\n",
                "      GID = '100000000000000001'\n",
                "      CHID = '100000000000000002'\n",
                "      MID = '100000000000000003'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CAN = -1\n",
                "      CALL D7CSP(CAN, WHY, 8, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'CSP', STATUS, CAN, OUTLEN, TRUNC, WHY(1:OUTLEN)\n",
                "      CAN = -1\n",
                "      CALL D7CSN(GID, 18, CHID, 18, CAN, WHY, 8, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'CSN', STATUS, CAN, OUTLEN, TRUNC, WHY(1:OUTLEN)\n",
                "      CAN = -1\n",
                "      CALL D7CDL(GID, 18, CHID, 18, MID, 18, CAN, WHY, 2, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1010) 'CDL', STATUS, CAN, OUTLEN, TRUNC, WHY(1:2)\n",
                "      CAN = -1\n",
                "      CALL D7CDP(CAN, WHY, 8, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1020) 'CDP', STATUS, CAN, OUTLEN, TRUNC\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(A,1X,I0,1X,I0,1X,I0,1X,I0,1X,A)\n",
                " 1010 FORMAT(A,1X,I0,1X,I0,1X,I0,1X,I0,1X,A)\n",
                " 1020 FORMAT(A,1X,I0,1X,I0,1X,I0,1X,I0)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper can status program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper can status program");
    assert!(compile_program.success());

    let input = concat!(
        "@D7/1 RES 1 31 2\n",
        "K can raw 1 1\n",
        "K why raw 3 bad\n",
        "@D7/1 END 1\n",
        "@D7/1 RES 2 31 2\n",
        "K can raw 1 1\n",
        "K why raw 3 bad\n",
        "@D7/1 END 2\n",
        "@D7/1 RES 3 0 2\n",
        "K can raw 1 1\n",
        "K why raw 4 deny\n",
        "@D7/1 END 3\n",
        "@D7/1 RES 4 0 2\n",
        "K can raw 1 1\n",
        "K why raw 5 hi\n",
        "@D7/1 END 4\n",
    );
    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run wrapper can status program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(input.as_bytes())
        .expect("failed to write fake runtime responses");
    let output = child
        .wait_with_output()
        .expect("failed to wait for wrapper can status program");
    assert!(
        output.status.success(),
        "wrapper can status program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let lines = result.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "CSP 31 0 3 0 bad");
    assert_eq!(lines[1], "CSN 31 0 3 0 bad");
    assert_eq!(lines[2], "CDL 10 1 4 1 de");
    assert_eq!(lines[3], "CDP 60 0 0 0");

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_applies_output_string_copy_contract() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper string copy test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-string-copy-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_string_copy.f");
    let program_bin = out_dir.join("wrapper_string_copy");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS, OUTLEN, TRUNC\n",
                "      CHARACTER*8 BUF\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      BUF = 'XXXXXXXX'\n",
                "      CALL D7GID(BUF, 0, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'EMPTY0', STATUS, OUTLEN, TRUNC\n",
                "      BUF = 'XXXXXXXX'\n",
                "      CALL D7GID(BUF, 0, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) 'NONEMPTY0', STATUS, OUTLEN, TRUNC\n",
                "      BUF = 'XXXXXXXX'\n",
                "      CALL D7GID(BUF, 3, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1010) 'EXACT', STATUS, OUTLEN, TRUNC, BUF(1:3)\n",
                "      BUF = 'XXXXXXXX'\n",
                "      CALL D7GID(BUF, 3, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1020) 'UTF8', STATUS, OUTLEN, TRUNC, ICHAR(BUF(1:1)), ICHAR(BUF(2:2)), ICHAR(BUF(3:3))\n",
                " 1000 FORMAT(A,1X,I0,1X,I0,1X,I0)\n",
                " 1010 FORMAT(A,1X,I0,1X,I0,1X,I0,1X,A)\n",
                " 1020 FORMAT(A,1X,I0,1X,I0,1X,I0,1X,I0,1X,I0,1X,I0)\n",
                "      CLOSE(9)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper string copy program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper string copy program");
    assert!(compile_program.success());

    let input = concat!(
        "@D7/1 RES 1 0 1\n",
        "K value raw 0\n",
        "@D7/1 END 1\n",
        "@D7/1 RES 2 0 1\n",
        "K value raw 3 abc\n",
        "@D7/1 END 2\n",
        "@D7/1 RES 3 0 1\n",
        "K value raw 3 abc\n",
        "@D7/1 END 3\n",
        "@D7/1 RES 4 0 1\n",
        "K value pct 11 a%E3%81%82b\n",
        "@D7/1 END 4\n",
    );
    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run wrapper string copy program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(input.as_bytes())
        .expect("failed to write fake runtime responses");
    let output = child
        .wait_with_output()
        .expect("failed to wait for wrapper string copy program");
    assert!(
        output.status.success(),
        "wrapper string copy program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let lines = result.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "EMPTY0 0 0 0");
    assert_eq!(lines[1], "NONEMPTY0 10 3 1");
    assert_eq!(lines[2], "EXACT 0 3 0 abc");
    assert_eq!(lines[3], "UTF8 10 5 1 97 32 32");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("@D7/1 REQ").count(), 4);

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_rejects_oversized_message_content_locally() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper content length test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir =
        std::env::temp_dir().join(format!("dfor77-content-length-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_content_length.f");
    let program_bin = out_dir.join("wrapper_content_length");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            "      PROGRAM T\n      INCLUDE 'dfor77.inc'\n      INTEGER STATUS\n      CHARACTER*18 CHID\n      CHARACTER*3000 MSG\n      CHID = '100000000000000001'\n      MSG = ' '\n      CALL D7SND(CHID, 18, MSG, 3000, STATUS)\n      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n      WRITE(9,1000) STATUS\n 1000 FORMAT(I0)\n      CLOSE(9)\n      END\n",
            result_path
        ),
    )
    .expect("failed to write wrapper content length program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper content length program");
    assert!(compile_program.success());

    let output = Command::new(&program_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run wrapper content length program");
    assert!(
        output.status.success(),
        "wrapper content length program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let status = result.trim().parse::<i32>().expect("invalid status");
    assert_eq!(status, 30);
    assert!(output.stdout.is_empty());

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_sends_long_percent_encoded_content() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper long content test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-long-content-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_long_content.f");
    let program_bin = out_dir.join("wrapper_long_content");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS, I, J\n",
                "      CHARACTER*18 CHID\n",
                "      CHARACTER*4000 MSG\n",
                "      CHID = '100000000000000001'\n",
                "      DO 10 I = 1, 1000\n",
                "         J = (I - 1) * 4\n",
                "         MSG(J+1:J+1) = CHAR(240)\n",
                "         MSG(J+2:J+2) = CHAR(159)\n",
                "         MSG(J+3:J+3) = CHAR(152)\n",
                "         MSG(J+4:J+4) = CHAR(128)\n",
                "   10 CONTINUE\n",
                "      CALL D7SND(CHID, 18, MSG, 4000, STATUS)\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      WRITE(9,1000) STATUS\n",
                " 1000 FORMAT(I0)\n",
                "      CLOSE(9)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper long content program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper long content program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run wrapper long content program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(b"@D7/1 RES 1 0 0\n@D7/1 END 1\n")
        .expect("failed to write fake runtime response");
    let output = child
        .wait_with_output()
        .expect("failed to wait for wrapper long content program");
    assert!(
        output.status.success(),
        "wrapper long content program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    assert_eq!(result.trim(), "0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("K content pct 12000 "), "{stdout}");
    assert!(stdout.contains("%F0%9F%98%80%F0%9F%98%80"), "{stdout}");

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_percent_encodes_high_bit_utf8_bytes() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper UTF-8 encode test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-utf8-encode-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_utf8_encode.f");
    let program_bin = out_dir.join("wrapper_utf8_encode");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS\n",
                "      CHARACTER*3 MSG\n",
                "      MSG(1:1) = CHAR(227)\n",
                "      MSG(2:2) = CHAR(129)\n",
                "      MSG(3:3) = CHAR(130)\n",
                "      CALL D7LOG(1, MSG, 3, STATUS)\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      WRITE(9,1000) STATUS\n",
                " 1000 FORMAT(I0)\n",
                "      CLOSE(9)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper UTF-8 encode program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper UTF-8 encode program");
    assert!(compile_program.success());

    let mut child = Command::new(&program_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run wrapper UTF-8 encode program");
    child
        .stdin
        .as_mut()
        .expect("missing child stdin")
        .write_all(b"@D7/1 RES 1 0 0\n@D7/1 END 1\n")
        .expect("failed to write fake runtime response");
    let output = child
        .wait_with_output()
        .expect("failed to wait for wrapper UTF-8 encode program");
    assert!(
        output.status.success(),
        "wrapper UTF-8 encode program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    assert_eq!(result.trim(), "0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("K message pct 9 %E3%81%82"), "{stdout}");

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_rejects_negative_input_lengths_locally() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper negative length test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir =
        std::env::temp_dir().join(format!("dfor77-negative-length-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_negative_length.f");
    let program_bin = out_dir.join("wrapper_negative_length");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS\n",
                "      CHARACTER*18 GID, CHID, MID, UID\n",
                "      CHARACTER*4 REASON, MSG\n",
                "      GID = '100000000000000001'\n",
                "      CHID = '100000000000000002'\n",
                "      MID = '100000000000000003'\n",
                "      UID = '100000000000000004'\n",
                "      REASON = 'test'\n",
                "      MSG = 'ping'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7DEL(CHID, -1, MID, 18, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7DEL(CHID, 18, MID, -1, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7DEL(CHID, 18, MID, 18, REASON, -1, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7TMO(GID, -1, UID, 18, 60, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7TMO(GID, 18, UID, -1, 60, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7TMO(GID, 18, UID, 18, 60, REASON, -1, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7UNT(GID, -1, UID, 18, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7UNT(GID, 18, UID, -1, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7UNT(GID, 18, UID, 18, REASON, -1, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7SND(CHID, -1, MSG, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7SND(CHID, 18, MSG, -1, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7LOG(1, MSG, -1, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                " 1000 FORMAT(I0)\n",
                "      CLOSE(9)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper negative length program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper negative length program");
    assert!(compile_program.success());

    let output = Command::new(&program_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run wrapper negative length program");
    assert!(
        output.status.success(),
        "wrapper negative length program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![30; 12]);
    assert!(output.stdout.is_empty());

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_rejects_invalid_timeout_seconds_locally() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper timeout seconds test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir =
        std::env::temp_dir().join(format!("dfor77-timeout-seconds-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_timeout_seconds.f");
    let program_bin = out_dir.join("wrapper_timeout_seconds");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS\n",
                "      CHARACTER*18 GID, UID\n",
                "      CHARACTER*4 REASON\n",
                "      GID = '100000000000000001'\n",
                "      UID = '100000000000000004'\n",
                "      REASON = 'test'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7TMO(GID, 18, UID, 18, -1, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7TMO(GID, 18, UID, 18, 0, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7TMO(GID, 18, UID, 18, 2419201, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                " 1000 FORMAT(I0)\n",
                "      CLOSE(9)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper timeout seconds program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper timeout seconds program");
    assert!(compile_program.success());

    let output = Command::new(&program_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run wrapper timeout seconds program");
    assert!(
        output.status.success(),
        "wrapper timeout seconds program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![30; 3]);
    assert!(output.stdout.is_empty());

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_rejects_invalid_log_levels_locally() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper log level test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-log-level-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_log_level.f");
    let program_bin = out_dir.join("wrapper_log_level");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS\n",
                "      CHARACTER*4 MSG\n",
                "      MSG = 'test'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7LOG(-1, MSG, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7LOG(4, MSG, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                " 1000 FORMAT(I0)\n",
                "      CLOSE(9)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper log level program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper log level program");
    assert!(compile_program.success());

    let output = Command::new(&program_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run wrapper log level program");
    assert!(
        output.status.success(),
        "wrapper log level program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![30; 2]);
    assert!(output.stdout.is_empty());

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_rejects_oversized_audit_reasons_locally() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper audit reason test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-reason-length-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_reason_length.f");
    let program_bin = out_dir.join("wrapper_reason_length");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS\n",
                "      CHARACTER*18 GID, CHID, MID, UID\n",
                "      CHARACTER*513 REASON\n",
                "      GID = '100000000000000001'\n",
                "      CHID = '100000000000000002'\n",
                "      MID = '100000000000000003'\n",
                "      UID = '100000000000000004'\n",
                "      REASON = 'x'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7DEL(CHID, 18, MID, 18, REASON, 513, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7TMO(GID, 18, UID, 18, 60, REASON, 513, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7UNT(GID, 18, UID, 18, REASON, 513, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                " 1000 FORMAT(I0)\n",
                "      CLOSE(9)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper audit reason program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper audit reason program");
    assert!(compile_program.success());

    let output = Command::new(&program_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run wrapper audit reason program");
    assert!(
        output.status.success(),
        "wrapper audit reason program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![30; 3]);
    assert!(output.stdout.is_empty());

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_rejects_empty_required_ids_locally() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper empty id test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-empty-id-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_empty_id.f");
    let program_bin = out_dir.join("wrapper_empty_id");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS, CAN, OUTLEN, TRUNC, ISADM\n",
                "      CHARACTER*18 GID, CHID, MID, UID\n",
                "      CHARACTER*4 REASON, MSG, WHY\n",
                "      GID = '100000000000000001'\n",
                "      CHID = '100000000000000002'\n",
                "      MID = '100000000000000003'\n",
                "      UID = '100000000000000004'\n",
                "      REASON = 'test'\n",
                "      MSG = 'ping'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7SND(CHID, 0, MSG, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7DEL(CHID, 0, MID, 18, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7DEL(CHID, 18, MID, 0, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7TMO(GID, 0, UID, 18, 60, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7TMO(GID, 18, UID, 0, 60, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7UNT(GID, 0, UID, 18, REASON, 4, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7ADM(GID, 0, UID, 18, ISADM, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7CDL(GID, 0, CHID, 18, MID, 18, CAN, WHY, 4, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7CDL(GID, 18, CHID, 0, MID, 18, CAN, WHY, 4, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7CDL(GID, 18, CHID, 18, MID, 0, CAN, WHY, 4, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7CTM(GID, 18, UID, 0, CAN, WHY, 4, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                "      CALL D7CSN(GID, 18, CHID, 0, CAN, WHY, 4, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS\n",
                " 1000 FORMAT(I0)\n",
                "      CLOSE(9)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper empty id program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper empty id program");
    assert!(compile_program.success());

    let output = Command::new(&program_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run wrapper empty id program");
    assert!(
        output.status.success(),
        "wrapper empty id program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("invalid status"))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![30; 12]);
    assert!(output.stdout.is_empty());

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_wrapper_rejects_invalid_output_buffer_lengths_locally() {
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper output length test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-output-length-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_output_length.f");
    let program_bin = out_dir.join("wrapper_output_length");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS, OUTLEN, TRUNC, ERRCODE, HTTP, DCODE\n",
                "      INTEGER CAN\n",
                "      CHARACTER*18 GID, CHID, MID, UID\n",
                "      CHARACTER*4 BUF, WHY, MSG\n",
                "      GID = '100000000000000001'\n",
                "      CHID = '100000000000000002'\n",
                "      MID = '100000000000000003'\n",
                "      UID = '100000000000000004'\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      CALL D7GID(BUF, -1, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                "      CALL D7GID(BUF, 5, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                "      CALL D7RLI(1, BUF, -1, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                "      CALL D7ERR(D7ACT, ERRCODE, HTTP, DCODE, MSG, -1, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                "      CALL D7ERR(D7ACT, ERRCODE, HTTP, DCODE, MSG, 5, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                "      CALL D7WHY(D7ACT, MSG, -1, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                "      CALL D7WHY(D7ACT, MSG, 5, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                "      CALL D7CDP(CAN, WHY, -1, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                "      CALL D7CDP(CAN, WHY, 5, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                "      CALL D7CDL(GID, 18, CHID, 18, MID, 18, CAN, WHY, -1, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                "      CALL D7CTM(GID, 18, UID, 18, CAN, WHY, 5, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1000) STATUS, OUTLEN, TRUNC\n",
                " 1000 FORMAT(I0,1X,I0,1X,I0)\n",
                "      CLOSE(9)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper output length program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper output length program");
    assert!(compile_program.success());

    let output = Command::new(&program_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run wrapper output length program");
    assert!(
        output.status.success(),
        "wrapper output length program failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let statuses = result
        .lines()
        .map(|line| {
            line.split_whitespace()
                .map(|value| value.parse::<i32>().expect("invalid output length value"))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec![vec![30, 0, 0]; 11]);
    assert!(output.stdout.is_empty());

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}

#[test]
fn fortran_error_readers_truncate_without_clearing_runtime_error() {
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping wrapper error reader test");
        return;
    };
    if Command::new("gfortran").arg("--version").output().is_err() {
        eprintln!("gfortran is not available; skipping wrapper error reader test");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = std::env::temp_dir().join(format!("dfor77-error-reader-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);
    let wrapper_obj = out_dir.join("dfor77_wrapper.o");
    let program_src = out_dir.join("wrapper_error_reader.f");
    let program_bin = out_dir.join("wrapper_error_reader");
    let result_file = out_dir.join("result.txt");
    let result_path = result_file.to_string_lossy();

    let compile_wrapper = Command::new("gfortran")
        .current_dir(&root)
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-Iinclude",
            "-c",
            "fortran/dfor77_wrapper.f",
            "-o",
        ])
        .arg(&wrapper_obj)
        .status()
        .expect("failed to run gfortran for wrapper");
    assert!(compile_wrapper.success());

    std::fs::write(
        &program_src,
        format!(
            concat!(
                "      PROGRAM T\n",
                "      INCLUDE 'dfor77.inc'\n",
                "      INTEGER STATUS, OUTLEN, TRUNC, ERRCODE, HTTP, DCODE\n",
                "      CHARACTER*3 CHID\n",
                "      CHARACTER*2 CONTENT\n",
                "      CHARACTER*4 SMALL\n",
                "      CHARACTER*32 LARGE\n",
                "      CHID = 'bad'\n",
                "      CONTENT = 'hi'\n",
                "      CALL D7INI(STATUS)\n",
                "      CALL D7SND(CHID, 3, CONTENT, 2, STATUS)\n",
                "      OPEN(UNIT=9, FILE='{}', STATUS='UNKNOWN')\n",
                "      WRITE(9,1000) 'SND', STATUS\n",
                "      CALL D7WHY(D7ACT, SMALL, 4, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1010) 'WHY4', STATUS, OUTLEN, TRUNC, SMALL\n",
                "      CALL D7WHY(D7ACT, LARGE, 32, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1010) 'WHY32', STATUS, OUTLEN, TRUNC, LARGE(1:OUTLEN)\n",
                "      CALL D7ERR(D7ACT, ERRCODE, HTTP, DCODE, SMALL, 4, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1020) 'ERR4', STATUS, OUTLEN, TRUNC, ERRCODE, HTTP, DCODE, SMALL\n",
                "      CALL D7ERR(D7ACT, ERRCODE, HTTP, DCODE, LARGE, 32, OUTLEN, TRUNC, STATUS)\n",
                "      WRITE(9,1020) 'ERR32', STATUS, OUTLEN, TRUNC, ERRCODE, HTTP, DCODE, LARGE(1:OUTLEN)\n",
                "      CALL D7END(STATUS)\n",
                "      CLOSE(9)\n",
                " 1000 FORMAT(A,1X,I0)\n",
                " 1010 FORMAT(A,1X,I0,1X,I0,1X,I0,1X,A)\n",
                " 1020 FORMAT(A,1X,I0,1X,I0,1X,I0,1X,I0,1X,I0,1X,I0,1X,A)\n",
                "      END\n"
            ),
            result_path
        ),
    )
    .expect("failed to write wrapper error reader program");

    let compile_program = Command::new("gfortran")
        .current_dir(&root)
        .args(["-std=legacy", "-ffixed-line-length-none", "-Iinclude"])
        .arg(&program_src)
        .arg(&wrapper_obj)
        .arg("-o")
        .arg(&program_bin)
        .status()
        .expect("failed to run gfortran for wrapper error reader program");
    assert!(compile_program.success());

    let output = Command::new(runtime)
        .current_dir(&root)
        .args(["--dry-run", "--bot"])
        .arg(&program_bin)
        .output()
        .expect("failed to run wrapper error reader program through runtime");
    assert!(
        output.status.success(),
        "wrapper error reader runtime failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::fs::read_to_string(&result_file).expect("failed to read result file");
    let lines = result.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "SND 30");
    assert_eq!(lines[1], "WHY4 10 17 1 INVA");
    assert_eq!(lines[2], "WHY32 0 17 0 INVALID_FORM_BODY");
    assert_eq!(lines[3], "ERR4 10 17 1 30 0 0 INVA");
    assert_eq!(lines[4], "ERR32 0 17 0 30 0 0 INVALID_FORM_BODY");

    let _ = std::fs::remove_file(wrapper_obj);
    let _ = std::fs::remove_file(program_src);
    let _ = std::fs::remove_file(program_bin);
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_dir(out_dir);
}
