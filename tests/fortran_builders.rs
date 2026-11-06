use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct TestDirectory(PathBuf);

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct RunningProgram(Child);

impl Drop for RunningProgram {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn compile_program(
    name: &str,
    source: &str,
    extension: bool,
    files: &[(&str, &[u8])],
) -> (TestDirectory, PathBuf) {
    assert!(Command::new("gfortran").arg("--version").output().is_ok());
    for (index, line) in source.lines().enumerate() {
        assert!(line.len() <= 72, "line {} exceeds column 72", index + 1);
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = TestDirectory(std::env::temp_dir().join(format!(
        "dfor77-builders-{name}-{}-{nonce}",
        std::process::id()
    )));
    std::fs::create_dir(&directory.0).unwrap();
    for (path, data) in files {
        std::fs::write(directory.0.join(path), data).unwrap();
    }
    let program = directory.0.join("test.f");
    let binary = directory.0.join("test");
    std::fs::write(&program, source).unwrap();
    let mut command = Command::new("gfortran");
    command.args([
        "-std=legacy",
        "-ffixed-line-length-72",
        "-fcheck=all",
        "-O2",
    ]);
    command
        .arg("-I")
        .arg(root.join("include"))
        .arg(&program)
        .arg(root.join("fortran/dfor77_wrapper.f"));
    if extension {
        command.arg(root.join("extension/dfor77_extension.f"));
    }
    let compile = command.arg("-o").arg(&binary).output().unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    (directory, binary)
}

fn run_program(name: &str, source: &str, extension: bool, files: &[(&str, &[u8])]) -> String {
    let (directory, binary) = compile_program(name, source, extension, files);
    let output = Command::new(binary)
        .current_dir(&directory.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{name}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn path_segment_calls() -> Vec<String> {
    let mut templates = [
        "D7GMS({id},{id},S)",
        "D7GRU({id},{id},'emoji',5,{id},25,0,S)",
        "D7PAV({id},{id},1,' ',0,25,S)",
        "D7EPL({id},{id},S)",
        "D7LMS({id},' ',0,S)",
        "D7EMS({id},{id},'{}',2,' ',0,S)",
        "D7BMD({id},'{}',2,' ',0,S)",
        "D7GCH({id},S)",
        "D7CPM({id},{id},'{}',2,' ',0,S)",
        "D7DPM({id},{id},' ',0,S)",
        "D7GIN({id},S)",
        "D7CIN({id},'{}',2,' ',0,S)",
        "D7FOL({id},'{}',2,S)",
        "D7SVC({id},'{}',2,' ',0,S)",
        "D7GGD({id},' ',0,S)",
        "D7GCS({id},S)",
        "D7MCP({id},'{}',2,' ',0,S)",
        "D7ATH({id},S)",
        "D7GMB({id},{id},S)",
        "D7LMB({id},' ',0,S)",
        "D7SMB({id},' ',0,S)",
        "D7AMB({id},{id},'{}',2,' ',0,S)",
        "D7ECM({id},'{}',2,' ',0,S)",
        "D7LRO({id},S)",
        "D7RPS({id},'{}',2,' ',0,S)",
        "D7GBN({id},{id},S)",
        "D7LBN({id},' ',0,S)",
        "D7BBN({id},'{}',2,' ',0,S)",
        "D7AUD({id},' ',0,S)",
        "D7CWH({id},'{}',2,' ',0,S)",
        "D7GWH({id},{id},S)",
        "D7EWH({id},{id},'{}',2,' ',0,S)",
        "D7DWH({id},{id},' ',0,S)",
        "D7WHP({id},{id},' ',0,'{}',2,S)",
        "D7GUR({id},S)",
        "D7CCR({id},'{}',2,' ',0,S)",
        "D7ECH({id},'{}',2,' ',0,S)",
        "D7DCH({id},' ',0,S)",
        "D7CRO({id},'{}',2,' ',0,S)",
        "D7ERO({id},{id},'{}',2,' ',0,S)",
        "D7DRO({id},{id},' ',0,S)",
        "D7AMR({id},{id},{id},' ',0,S)",
        "D7RMR({id},{id},{id},' ',0,S)",
        "D7EGM({id},{id},'{}',2,' ',0,S)",
        "D7KIK({id},{id},' ',0,S)",
        "D7BAN({id},{id},'{}',2,' ',0,S)",
        "D7UBN({id},{id},' ',0,S)",
        "D7EGD({id},'{}',2,' ',0,S)",
    ]
    .map(String::from)
    .to_vec();
    for mode in 1..=4 {
        let message = if mode == 1 { "' ',0" } else { "{id}" };
        templates.push(format!("D7PIN({mode},{{id}},{message},' ',0,S)"));
    }
    for mode in 1..=5 {
        let rule = if matches!(mode, 1 | 3) {
            "' ',0"
        } else {
            "{id}"
        };
        templates.push(format!("D7AUM({mode},{{id}},{rule},'{{}}',2,' ',0,S)"));
    }
    for mode in 1..=2 {
        templates.push(format!("D7LWH({mode},{{id}},S)"));
    }
    for mode in 1..=3 {
        templates.push(format!("D7WHM({mode},{{id}},{{id}},{{id}},'{{}}',2,S)"));
    }
    for mode in 1..=7 {
        let message = if mode >= 5 { "{id}" } else { "' ',0" };
        templates.push(format!("D7IFU({mode},{{id}},{{id}},{message},'{{}}',2,S)"));
    }
    for mode in 1..=11 {
        let id = if matches!(mode, 1 | 5 | 6 | 7) {
            "{id}"
        } else {
            "' ',0"
        };
        let payload = if matches!(mode, 1 | 2) {
            "'{}',2"
        } else {
            "' ',0"
        };
        templates.push(format!("D7THR({mode},{{id}},{id},{payload},' ',0,S)"));
    }
    let mut calls = Vec::new();
    for template in templates {
        let pieces = template.split("{id}").collect::<Vec<_>>();
        for target in 0..pieces.len() - 1 {
            let mut call = pieces[0].to_string();
            for (index, piece) in pieces[1..].iter().enumerate() {
                call.push_str(if index == target { "B,N" } else { "'20',2" });
                call.push_str(piece);
            }
            calls.push(call);
        }
    }
    calls
}

#[test]
fn typed_rest_paths_reject_route_changes_before_emitting_any_ipc() {
    let mut calls = path_segment_calls();
    calls.extend([
        "D7ICB(B,N,'token',5,'{}',2,S)".to_string(),
        "D7ICB('20',2,B,N,'{}',2,S)".to_string(),
    ]);
    let mut bad = (0_u8..=32)
        .chain(127..=255)
        .map(|byte| vec![byte])
        .collect::<Vec<_>>();
    bad.extend(
        [
            "/",
            "?",
            "#",
            "%2F",
            "%252F",
            "\\",
            ".",
            "..",
            "20/messages/30",
            "20?limit=1",
        ]
        .map(|value| value.as_bytes().to_vec()),
    );
    let mut source = format!(
        "      PROGRAM T\n      CHARACTER*32 B,BAD({})\n      INTEGER S,N,I,L({})\n",
        bad.len() + 2,
        bad.len() + 2
    );
    for (index, bytes) in bad.iter().enumerate() {
        let value = bytes
            .iter()
            .map(|byte| format!("CHAR({byte})"))
            .collect::<Vec<_>>();
        for (offset, chunk) in value.chunks(4).enumerate() {
            let prefix = if offset == 0 {
                format!("      BAD({}) = ", index + 1)
            } else {
                "     1 ".into()
            };
            let suffix = if (offset + 1) * 4 < value.len() {
                "//"
            } else {
                ""
            };
            source.push_str(&format!("{prefix}{}{suffix}\n", chunk.join("//")));
        }
        source.push_str(&format!("      L({}) = {}\n", index + 1, bytes.len()));
    }
    for (offset, length) in [-1, 33].iter().enumerate() {
        let index = bad.len() + offset + 1;
        source.push_str(&format!(
            "      BAD({index}) = '20'\n      L({index}) = {length}\n"
        ));
    }
    source.push_str(&format!(
        "      DO 10 I = 1, {}\n         B = BAD(I)\n         N = L(I)\n",
        bad.len() + 2
    ));
    for (index, call) in calls.iter().enumerate() {
        source.push_str(&format!(
            "         CALL {call}\n         IF (S.NE.30) THEN\n            WRITE(*,*) {},I,S\n            STOP 1\n         ENDIF\n",
            index + 1
        ));
    }
    source.push_str("   10 CONTINUE\n      WRITE(*,'(A)') 'blocked'\n      END\n");
    assert_eq!(
        run_program("path-injection", &source, false, &[]).trim(),
        "blocked"
    );
}

#[test]
fn typed_rest_paths_preserve_normal_routes_aliases_tokens_and_query_helpers() {
    let calls = path_segment_calls();
    let mut source =
        "      PROGRAM T\n      CHARACTER*32 B\n      INTEGER S,N,I\n      B = '20'\n      N = 2\n"
            .to_string();
    for call in &calls {
        source.push_str(&format!("      CALL {call}\n      IF (S.EQ.30) STOP 1\n"));
    }
    source.push_str(concat!(
        "      CALL D7GUR('@me',3,S)\n",
        "      CALL D7GUR('18446744073709551615',20,S)\n",
        "      CALL D7GUR('00000000000000000020',20,S)\n",
        "      CALL D7WHM(1,'20',2,'safe.token_-',12,\n",
        "     1           '@original',9,' ',0,S)\n",
        "      CALL D7GWH('20',2,' ',0,S)\n",
        "      CALL D7IFU(2,'20',2,'safe.token_-',12,' ',0,' ',0,S)\n",
        "      CALL D7XQ0(B,32,N,I,S)\n",
        "      CALL D7XQA(B,32,N,'limit',5,'25',2,N,I,S)\n",
        "      CALL D7LMS('20',2,B,N,S)\n",
        "      END\n",
    ));
    let output = run_program("valid-paths", &source, true, &[]);
    let mut input = std::io::Cursor::new(output.as_bytes());
    let mut paths = Vec::new();
    while let Some(request) = dfor77::read_request(&mut input).unwrap() {
        assert_eq!(request.verb, "RST");
        paths.push(request.field("path").unwrap().to_string());
    }
    assert_eq!(paths.len(), calls.len() + 7);
    assert_eq!(
        &paths[calls.len()..],
        [
            "/api/v10/users/@me",
            "/api/v10/users/18446744073709551615",
            "/api/v10/users/00000000000000000020",
            "/api/v10/webhooks/20/safe.token_-/messages/@original",
            "/api/v10/webhooks/20",
            "/api/v10/webhooks/20/safe.token_-/messages/@original",
            "/api/v10/channels/20/messages?limit=25",
        ]
    );
}

#[test]
fn fortran_snowflake_validation_matches_runtime_unsigned_boundaries() {
    let mut values = vec![
        "".to_string(),
        "0".into(),
        "00000000000000000000".into(),
        "1".into(),
        "00000000000000000001".into(),
        u64::MAX.to_string(),
        (u128::from(u64::MAX) + 1).to_string(),
        "99999999999999999999".into(),
        "100000000000000000000".into(),
        "@me".into(),
        "-1".into(),
        "+1".into(),
        "1.0".into(),
        "1e3".into(),
        " 1".into(),
        "1 ".into(),
    ];
    let mut seed = 0x91a4_52f0_b377_ee2a_u64;
    for _ in 0..256 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        values.push(seed.to_string());
        values.push(format!("{seed:020}"));
    }
    let mut source = "      PROGRAM T\n      CHARACTER*32 ID\n      LOGICAL D7ZIDK\n".to_string();
    for value in &values {
        source.push_str(&format!(
            "      ID = '{value}'\n      WRITE(*,'(L1)') D7ZIDK(ID,{})\n",
            value.len()
        ));
    }
    source.push_str("      END\n");
    let output = run_program("snowflake", &source, false, &[]);
    let actual = output.lines().collect::<Vec<_>>();
    assert_eq!(actual.len(), values.len());
    for (value, result) in values.iter().zip(actual) {
        assert_eq!(
            result == "T",
            dfor77::validation::validate_snowflake(value),
            "{value:?}"
        );
    }
}

#[test]
fn reaction_user_getter_rejects_invalid_parameters_without_emitting_ipc() {
    let output = run_program(
        "reaction-user-arguments",
        concat!(
            "      PROGRAM T\n      INTEGER S,I\n",
            "      CHARACTER*257 EMOJI\n",
            "      CALL D7GRU('20',2,'30',2,'x',1,' ',0,0,0,S)\n",
            "      IF (S.NE.30) STOP 1\n",
            "      CALL D7GRU('20',2,'30',2,'x',1,' ',0,101,0,S)\n",
            "      IF (S.NE.30) STOP 2\n",
            "      CALL D7GRU('20',2,'30',2,'x',1,' ',0,25,-1,S)\n",
            "      IF (S.NE.30) STOP 3\n",
            "      CALL D7GRU('20',2,'30',2,'x',1,' ',0,25,2,S)\n",
            "      IF (S.NE.30) STOP 4\n",
            "      CALL D7GRU('20',2,'30',2,' ',0,' ',0,25,0,S)\n",
            "      IF (S.NE.30) STOP 5\n",
            "      CALL D7GRU('20',2,'30',2,CHAR(255),1,' ',0,25,0,S)\n",
            "      IF (S.NE.30) STOP 6\n",
            "      CALL D7GRU('20',2,'30',2,'x',1,'0',1,25,0,S)\n",
            "      IF (S.NE.30) STOP 7\n",
            "      CALL D7GRU('20',2,'30',2,'x',1,\n",
            "     1           '18446744073709551616',20,25,0,S)\n",
            "      IF (S.NE.30) STOP 8\n",
            "      CALL D7GRU('0',1,'30',2,'x',1,' ',0,25,0,S)\n",
            "      IF (S.NE.30) STOP 9\n",
            "      CALL D7GRU('20',2,'-1',2,'x',1,' ',0,25,0,S)\n",
            "      IF (S.NE.30) STOP 10\n",
            "      DO 10 I = 1, 257\n         EMOJI(I:I) = 'a'\n",
            "   10 CONTINUE\n",
            "      CALL D7GRU('20',2,'30',2,EMOJI,257,' ',0,25,0,S)\n",
            "      IF (S.NE.30) STOP 11\n",
            "      DO 20 I = 1, 171\n         EMOJI(I:I) = '/'\n",
            "   20 CONTINUE\n",
            "      CALL D7GRU('20',2,'30',2,EMOJI,171,' ',0,25,0,S)\n",
            "      IF (S.NE.30) STOP 12\n",
            "      WRITE(*,'(A)') 'rejected'\n      END\n",
        ),
        false,
        &[],
    );
    assert_eq!(output.trim(), "rejected");
}

fn run_bodyless_http_program(
    name: &str,
    source: &str,
    extension: bool,
    token: &str,
    responses: &[(u16, &str)],
) -> (Vec<String>, String, String) {
    let (directory, binary) = compile_program(name, source, extension, &[]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut runtime = RunningProgram(
        Command::new(env!("CARGO_BIN_EXE_dfor77"))
            .current_dir(&directory.0)
            .arg("--mock-rest")
            .arg(url)
            .args(["--audit-log", "audit.jsonl", "--bot"])
            .arg(binary)
            .env("DISCORD_BOT_TOKEN", token)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut requests = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = runtime.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "{name} HTTP test timed out");
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(error) => panic!("{error}"),
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut bytes = Vec::new();
        while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
            assert!(Instant::now() < deadline, "{name} request timed out");
            let mut buffer = [0; 1024];
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            assert!(bytes.len() <= 8192);
        }
        let head = std::str::from_utf8(&bytes).unwrap();
        for line in head.lines() {
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    assert_eq!(value.trim(), "0");
                }
            }
        }
        assert!(bytes.ends_with(b"\r\n\r\n"));
        let (code, body) = responses
            .get(requests.len())
            .expect("unexpected HTTP request");
        write!(
            stream,
            "HTTP/1.1 {code} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        requests.push(String::from_utf8(bytes).unwrap());
    };
    let mut stderr = String::new();
    runtime
        .0
        .stderr
        .take()
        .unwrap()
        .take(65536)
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "{name}, extension={extension}: {status}: {stderr}"
    );
    assert_eq!(requests.len(), responses.len(), "{name}");
    let audit = std::fs::read_to_string(directory.0.join("audit.jsonl")).unwrap();
    (requests, stderr, audit)
}

#[test]
fn standard_and_extension_reaction_users_round_trip_through_runtime_and_http() {
    for extension in [false, true] {
        let constants = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            "      INTEGER D7RNM,D7RBS\n      PARAMETER (D7RNM=0,D7RBS=1)\n"
        };
        let source = format!(
            "      PROGRAM T\n{constants}{}",
            concat!(
                "      INTEGER S,L,TR\n      CHARACTER*64 B\n",
                "      CHARACTER*4 EMOJI\n",
                "      EMOJI = CHAR(240)//CHAR(159)//CHAR(148)//CHAR(165)\n",
                "      CALL D7INI(S)\n      IF (S.NE.0) STOP 1\n",
                "      CALL D7GRU('20',2,'30',2,EMOJI,4,' ',0,1,D7RNM,S)\n",
                "      IF (S.NE.0) STOP 2\n",
                "      CALL D7RBD(B,1,L,TR,S)\n",
                "      IF (S.NE.10.OR.TR.NE.1) STOP 3\n",
                "      IF (L.NE.LEN('[{\"id\":\"40\"}]')) STOP 4\n",
                "      CALL D7RBD(B,64,L,TR,S)\n",
                "      IF (S.NE.0.OR.B.NE.'[{\"id\":\"40\"}]') STOP 5\n",
                "      CALL D7GRU('20',2,'30',2,'party:77',8,\n",
                "     1           '40',2,100,D7RBS,S)\n",
                "      IF (S.NE.0) STOP 6\n",
                "      CALL D7RBD(B,64,L,TR,S)\n",
                "      IF (S.NE.0.OR.B.NE.'[{\"id\":\"41\"}]') STOP 7\n",
                "      CALL D7GRU('20',2,'30',2,'party:77',8,\n",
                "     1           '41',2,100,D7RBS,S)\n",
                "      IF (S.NE.0) STOP 8\n",
                "      CALL D7RBD(B,64,L,TR,S)\n",
                "      IF (S.NE.0.OR.B.NE.'[]'.OR.L.NE.2) STOP 9\n",
                "      CALL D7GRU('20',2,'30',2,'party:77',8,\n",
                "     1           '42',2,100,D7RBS,S)\n",
                "      IF (S.NE.41) STOP 10\n",
                "      CALL D7RBD(B,64,L,TR,S)\n",
                "      IF (S.NE.31) STOP 11\n",
                "      CALL D7END(S)\n      IF (S.NE.0) STOP 12\n",
                "      END\n",
            )
        );
        assert!(extension || !source.contains("INCLUDE"));
        let responses = [
            (200, r#"[{"id":"40"}]"#),
            (200, r#"[{"id":"41"}]"#),
            (200, "[]"),
            (404, r#"{"message":"Unknown Message","code":10008}"#),
        ];
        let (requests, stderr, audit) = run_bodyless_http_program(
            "reaction-users-http",
            &source,
            extension,
            "reaction-users-test-token",
            &responses,
        );
        let paths = requests
            .iter()
            .map(|request| request.lines().next().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(paths, [
            "GET /api/v10/channels/20/messages/30/reactions/%F0%9F%94%A5?type=0&limit=1 HTTP/1.1",
            "GET /api/v10/channels/20/messages/30/reactions/party%3A77?type=1&limit=100&after=40 HTTP/1.1",
            "GET /api/v10/channels/20/messages/30/reactions/party%3A77?type=1&limit=100&after=41 HTTP/1.1",
            "GET /api/v10/channels/20/messages/30/reactions/party%3A77?type=1&limit=100&after=42 HTTP/1.1",
        ]);
        assert!(requests.iter().all(
            |request| request.contains("\r\nAuthorization: Bot reaction-users-test-token\r\n")
        ));
        assert!(!stderr.contains("reaction-users-test-token"));
        assert!(!audit.contains("reaction-users-test-token"));
        assert!(!audit.contains(r#"[{"id":"40"}]"#));
    }
}

#[test]
fn poll_helpers_reject_invalid_arguments_without_emitting_ipc() {
    let calls = [
        "D7PAV('20',2,'30',2,-1,' ',0,25,S)",
        "D7PAV('20',2,'30',2,1,' ',0,0,S)",
        "D7PAV('20',2,'30',2,1,' ',0,101,S)",
        "D7PAV('20',2,'30',2,1,' ',0,-1,S)",
        "D7PAV('0',1,'30',2,1,' ',0,25,S)",
        "D7PAV('20',3,'30',2,1,' ',0,25,S)",
        "D7PAV('20',-1,'30',2,1,' ',0,25,S)",
        "D7PAV('20',2,'30',0,1,' ',0,25,S)",
        "D7PAV('20',2,'-1',2,1,' ',0,25,S)",
        "D7PAV('20',2,'30',2,1,'0',1,25,S)",
        "D7PAV('20',2,'30',2,1,'40',3,25,S)",
        "D7PAV('20',2,'30',2,1,'40',-1,25,S)",
        "D7PAV('20',2,'30',2,1,'4/0',3,25,S)",
        "D7PAV('20',2,'30',2,1,\n     1 '18446744073709551616',20,25,S)",
        "D7PAV(CHAR(255),1,'30',2,1,' ',0,25,S)",
        "D7EPL('0',1,'30',2,S)",
        "D7EPL('20',3,'30',2,S)",
        "D7EPL('20',2,'30',0,S)",
        "D7EPL('20',2,'3/0',3,S)",
        "D7EPL('20',2,'18446744073709551616',20,S)",
    ];
    for extension in [false, true] {
        let include = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            ""
        };
        let mut source = format!("      PROGRAM T\n{include}      INTEGER S\n");
        for (index, call) in calls.iter().enumerate() {
            source.push_str(&format!(
                "      CALL {call}\n      IF (S.NE.30) STOP {}\n",
                index + 1
            ));
        }
        source.push_str("      WRITE(*,'(A)') 'rejected'\n      END\n");
        let output = run_program("poll-local-arguments", &source, extension, &[]);
        assert_eq!(output.trim(), "rejected");
    }
}

#[test]
fn standard_and_extension_poll_helpers_preserve_pagination_results_and_write_uncertainty() {
    for extension in [false, true] {
        let constants = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            concat!(
                "      INTEGER D7OK,D7TRN,D7STA,D7NFD,D7FOR,D7UNC,D7ACT\n",
                "      PARAMETER (D7OK=0,D7TRN=10,D7STA=31)\n",
                "      PARAMETER (D7NFD=41,D7FOR=40,D7UNC=53,D7ACT=1)\n",
            )
        };
        let source = format!(
            "      PROGRAM T\n{constants}{}",
            concat!(
                "      INTEGER S,L,TR,ECODE,HTTP,DCODE\n",
                "      CHARACTER*512 B\n",
                "      CALL D7INI(S)\n      IF (S.NE.D7OK) STOP 1\n",
                "      CALL D7PAV('20',2,'30',2,12345,' ',0,1,S)\n",
                "      IF (S.NE.D7OK) STOP 2\n",
                "      CALL D7RBD(B,1,L,TR,S)\n",
                "      IF (S.NE.D7TRN.OR.TR.NE.1) STOP 3\n",
                "      IF (L.NE.LEN('{\"users\":[{\"id\":\"40\"}]}')) STOP 4\n",
                "      CALL D7RBD(B,512,L,TR,S)\n",
                "      IF (S.NE.D7OK) STOP 5\n",
                "      IF (B.NE.'{\"users\":[{\"id\":\"40\"}]}') STOP 6\n",
                "      CALL D7PAV('20',2,'30',2,12345,'40',2,100,S)\n",
                "      IF (S.NE.D7OK) STOP 7\n",
                "      CALL D7RBD(B,512,L,TR,S)\n",
                "      IF (S.NE.D7OK) STOP 8\n",
                "      IF (B.NE.'{\"users\":[{\"id\":\"41\"}]}') STOP 9\n",
                "      CALL D7PAV('20',2,'30',2,0,\n",
                "     1           '18446744073709551615',20,100,S)\n",
                "      IF (S.NE.D7OK) STOP 10\n",
                "      CALL D7RBD(B,512,L,TR,S)\n",
                "      IF (S.NE.D7OK.OR.B.NE.'{\"users\":[]}') STOP 11\n",
                "      CALL D7PAV('20',2,'31',2,7,' ',0,25,S)\n",
                "      IF (S.NE.D7NFD) STOP 12\n",
                "      CALL D7RBD(B,512,L,TR,S)\n",
                "      IF (S.NE.D7STA) STOP 13\n",
                "      CALL D7EPL('20',2,'30',2,S)\n",
                "      IF (S.NE.D7OK) STOP 14\n",
                "      CALL D7RBD(B,512,L,TR,S)\n",
                "      IF (S.NE.D7OK.OR.TR.NE.0) STOP 15\n",
                "      IF (INDEX(B,'\"is_finalized\":false').EQ.0) STOP 16\n",
                "      IF (INDEX(B,'poll-response-private').EQ.0) STOP 17\n",
                "      CALL D7EPL('20',2,'31',2,S)\n",
                "      IF (S.NE.D7FOR) STOP 18\n",
                "      CALL D7RBD(B,512,L,TR,S)\n",
                "      IF (S.NE.D7STA) STOP 19\n",
                "      CALL D7ERR(D7ACT,ECODE,HTTP,DCODE,B,512,L,TR,S)\n",
                "      IF (S.NE.D7OK.OR.ECODE.NE.D7FOR) STOP 20\n",
                "      IF (HTTP.NE.403.OR.DCODE.NE.50013) STOP 21\n",
                "      CALL D7EPL('20',2,'32',2,S)\n",
                "      IF (S.NE.D7UNC) STOP 22\n",
                "      CALL D7RBD(B,512,L,TR,S)\n",
                "      IF (S.NE.D7STA) STOP 23\n",
                "      CALL D7ERR(D7ACT,ECODE,HTTP,DCODE,B,512,L,TR,S)\n",
                "      IF (S.NE.D7OK.OR.ECODE.NE.D7UNC) STOP 24\n",
                "      IF (HTTP.NE.500) STOP 25\n",
                "      CALL D7PAV('20',2,'30',2,1,' ',0,25,S)\n",
                "      IF (S.NE.D7OK) STOP 26\n",
                "      CALL D7RBD(B,512,L,TR,S)\n",
                "      IF (S.NE.D7OK.OR.B.NE.'{\"users\":[]}') STOP 27\n",
                "      CALL D7END(S)\n      IF (S.NE.D7OK) STOP 28\n",
                "      END\n",
            )
        );
        assert!(extension || !source.contains("INCLUDE"));
        let responses = [
            (200, r#"{"users":[{"id":"40"}]}"#),
            (200, r#"{"users":[{"id":"41"}]}"#),
            (200, r#"{"users":[]}"#),
            (404, r#"{"message":"Unknown Message","code":10008}"#),
            (
                200,
                r#"{"id":"30","content":"poll-response-private","poll":{"results":{"is_finalized":false}}}"#,
            ),
            (403, r#"{"message":"Missing Permissions","code":50013}"#),
            (500, r#"{"message":"try later"}"#),
            (200, r#"{"users":[]}"#),
        ];
        let token = "poll-api-test-token";
        let (requests, stderr, audit) =
            run_bodyless_http_program("poll-api-http", &source, extension, token, &responses);
        assert_eq!(requests.iter().map(|request| request.lines().next().unwrap()).collect::<Vec<_>>(), [
            "GET /api/v10/channels/20/polls/30/answers/12345?limit=1 HTTP/1.1",
            "GET /api/v10/channels/20/polls/30/answers/12345?limit=100&after=40 HTTP/1.1",
            "GET /api/v10/channels/20/polls/30/answers/0?limit=100&after=18446744073709551615 HTTP/1.1",
            "GET /api/v10/channels/20/polls/31/answers/7?limit=25 HTTP/1.1",
            "POST /api/v10/channels/20/polls/30/expire HTTP/1.1",
            "POST /api/v10/channels/20/polls/31/expire HTTP/1.1",
            "POST /api/v10/channels/20/polls/32/expire HTTP/1.1",
            "GET /api/v10/channels/20/polls/30/answers/1?limit=25 HTTP/1.1",
        ]);
        assert!(requests
            .iter()
            .all(|request| request.contains(&format!("\r\nAuthorization: Bot {token}\r\n"))));
        assert!(!stderr.contains(token));
        assert!(!audit.contains(token));
        assert!(!audit.contains("poll-response-private"));
        assert!(!audit.contains("is_finalized"));
        let records = audit
            .lines()
            .map(|line| dfor77::parse_json(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), responses.len());
        for (record, message) in records
            .iter()
            .zip(["30", "30", "30", "31", "30", "31", "32", "30"])
        {
            assert_eq!(
                record
                    .get("target_channel_id")
                    .and_then(dfor77::JsonValue::as_str),
                Some("20")
            );
            assert_eq!(
                record
                    .get("target_message_id")
                    .and_then(dfor77::JsonValue::as_str),
                Some(message)
            );
        }
        assert_eq!(
            records[6]
                .get("commit_state")
                .and_then(dfor77::JsonValue::as_str),
            Some("unknown")
        );
    }
}

#[test]
fn standard_binary_builders_handle_all_bytes_and_buffer_boundaries() {
    let output = run_program(
        "binary",
        r#"      PROGRAM T
      CHARACTER*45001 DATA
      CHARACTER*60000 OUT
      INTEGER I, S, L, T1
      DO 10 I = 1, 256
         DATA(I:I) = CHAR(I-1)
   10 CONTINUE
      CALL D7B64(DATA,256,OUT,60000,L,T1,S)
      IF (S.NE.0.OR.L.NE.344.OR.T1.NE.0) STOP 1
      WRITE(*,'(A)') OUT(1:L)
      CALL D7B64('f',1,OUT,60000,L,T1,S)
      IF (S.NE.0.OR.OUT(1:L).NE.'Zg==') STOP 2
      CALL D7B64('fo',2,OUT,60000,L,T1,S)
      IF (S.NE.0.OR.OUT(1:L).NE.'Zm8=') STOP 3
      CALL D7B64('foobar',6,OUT,60000,L,T1,S)
      IF (S.NE.0.OR.OUT(1:L).NE.'Zm9vYmFy') STOP 4
      CALL D7B64(' ',0,OUT,60000,L,T1,S)
      IF (S.NE.0.OR.L.NE.0.OR.T1.NE.0) STOP 5
      OUT = 'sentinel'
      CALL D7B64('foo',3,OUT,0,L,T1,S)
      IF (S.NE.10.OR.L.NE.4.OR.T1.NE.1) STOP 6
      IF (OUT.NE.'sentinel') STOP 7
      CALL D7B64('foo',3,OUT,2,L,T1,S)
      IF (S.NE.10.OR.L.NE.4.OR.OUT.NE.'Zm') STOP 8
      DATA = 'a'
      CALL D7B64(DATA,45000,OUT,60000,L,T1,S)
      IF (S.NE.0.OR.L.NE.60000.OR.T1.NE.0) STOP 9
      OUT = 'sentinel'
      CALL D7B64(DATA,45001,OUT,60000,L,T1,S)
      IF (S.NE.30.OR.L.NE.0.OR.T1.NE.0) STOP 10
      IF (OUT.NE.'sentinel') STOP 11
      CALL D7B64('x',2,OUT,60000,L,T1,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 12
      CALL D7B64('x',-1,OUT,60000,L,T1,S)
      IF (S.NE.30) STOP 13
      END
"#,
        false,
        &[],
    );
    assert_eq!(
        output.trim(),
        concat!(
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4v",
            "MDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5f",
            "YGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6e3x9fn+AgYKDhIWGh4iJiouMjY6P",
            "kJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmqq6ytrq+wsbKztLW2t7i5uru8vb6/",
            "wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g4eLj5OXm5+jp6uvs7e7v",
            "8PHy8/T19vf4+fr7/P3+/w=="
        )
    );
}

#[test]
fn standard_data_uri_validates_media_types_and_canonical_base64() {
    let output = run_program(
        "uri",
        r#"      PROGRAM T
      CHARACTER*60000 OUT, BIG
      CHARACTER*32 MIME
      CHARACTER*8 BAD(13)
      INTEGER S, L, T1, I, N
      DATA BAD/'A===','====','Zg= ','Zg=a','Zg==x','Zg',
     1         'Zh==','Zm9=','!!!!','YW J','YW-J','YW_J',
     2         '=AAA'/
      DO 10 I = 1, 13
         OUT = 'sentinel'
         N = 4
         IF (I.EQ.5) N = 5
         IF (I.EQ.6) N = 2
         CALL D7URI('image/png',9,BAD(I),N,OUT,60000,L,T1,S)
         IF (S.NE.30.OR.L.NE.0.OR.T1.NE.0) STOP 1
         IF (OUT.NE.'sentinel') STOP 2
   10 CONTINUE
      CALL D7URI('image/png',9,'Zg==',4,OUT,60000,L,T1,S)
      IF (S.NE.0.OR.OUT(1:L).NE.'data:image/png;base64,Zg==')
     1   STOP 3
      CALL D7URI('image/png',9,' ',0,OUT,60000,L,T1,S)
      IF (S.NE.0.OR.OUT(1:L).NE.'data:image/png;base64,') STOP 4
      MIME = 'text/plain;charset=utf-8'
      CALL D7URI(MIME,24,'YWJj',4,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 5
      WRITE(*,'(A)') OUT(1:L)
      MIME = 'application/a#b^c'
      CALL D7URI(MIME,17,'YWJj',4,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 6
      WRITE(*,'(A)') OUT(1:L)
      MIME = 'text/plain;name=a%2fb'
      CALL D7URI(MIME,21,'YWJj',4,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 7
      MIME = 'image/png'//CHAR(10)
      OUT = 'sentinel'
      CALL D7URI(MIME,10,'YWJj',4,OUT,60000,L,T1,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 8
      CALL D7URI('image/png,evil',14,'YWJj',4,OUT,60000,L,T1,S)
      IF (S.NE.30) STOP 9
      CALL D7URI('image/png;base64',16,'YWJj',4,
     1           OUT,60000,L,T1,S)
      IF (S.NE.30) STOP 10
      MIME = 'text/plain;name=a%GG'
      CALL D7URI(MIME,19,'YWJj',4,OUT,60000,L,T1,S)
      IF (S.NE.30) STOP 11
      CALL D7URI('image/png',9,'YWJj',4,OUT,0,L,T1,S)
      IF (S.NE.10.OR.L.NE.26.OR.T1.NE.1) STOP 12
      IF (OUT.NE.'sentinel') STOP 13
      BIG = 'A'
      CALL D7URI('image/png',9,BIG,60000,OUT,60000,L,T1,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 14
      CALL D7URI('image/png',9,'AAA=',4,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 15
      CALL D7URI('text/plain;name=a%20b',21,' ',0,
     1           OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 16
      END
"#,
        false,
        &[],
    );
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        [
            "data:text/plain;charset=utf-8;base64,YWJj",
            "data:application/a%23b%5Ec;base64,YWJj",
        ]
    );
}

#[test]
fn extension_file_encoding_preserves_open_units_and_matches_memory_encoding() {
    let all_bytes = (0..=255).collect::<Vec<u8>>();
    let oversized = vec![0; 45_001];
    run_program(
        "file",
        r#"      PROGRAM T
      CHARACTER*256 DATA
      CHARACTER*60000 FILE64, MEM64
      CHARACTER*8 TEXT
      INTEGER I, S, L, T1, M, USED
      LOGICAL OPENED
      OPEN(UNIT=97,FILE='keep.txt',STATUS='UNKNOWN')
      WRITE(97,'(A)') 'before'
      DO 10 I = 1, 256
         DATA(I:I) = CHAR(I-1)
   10 CONTINUE
      CALL D7X64('application/octet-stream',24,DATA,256,
     1           MEM64,60000,M,T1,S)
      IF (S.NE.0) STOP 1
      CALL D7XF64('application/octet-stream',24,'bytes',5,
     1            FILE64,60000,L,T1,S)
      IF (S.NE.0.OR.L.NE.M.OR.FILE64.NE.MEM64) STOP 2
      INQUIRE(UNIT=97,OPENED=OPENED)
      IF (.NOT.OPENED) STOP 3
      WRITE(97,'(A)') 'after'
      REWIND(97)
      READ(97,'(A)') TEXT
      IF (TEXT.NE.'before') STOP 4
      READ(97,'(A)') TEXT
      IF (TEXT.NE.'after') STOP 5
      CALL D7XF64('text/plain',10,'empty',5,
     1            FILE64,60000,L,T1,S)
      IF (S.NE.0.OR.FILE64(1:L).NE.'data:text/plain;base64,')
     1   STOP 6
      FILE64 = 'sentinel'
      CALL D7XF64('text/plain',10,'missing',7,
     1            FILE64,60000,L,T1,S)
      IF (S.NE.30.OR.L.NE.0.OR.T1.NE.0) STOP 7
      IF (FILE64.NE.'sentinel') STOP 8
      CALL D7XF64('text/plain',10,'oversized',9,
     1            FILE64,60000,L,T1,S)
      IF (S.NE.30.OR.FILE64.NE.'sentinel') STOP 9
      DO 20 I = 10, 99
         IF (I.NE.97) OPEN(UNIT=I,STATUS='SCRATCH')
   20 CONTINUE
      CALL D7XF64('text/plain',10,'bytes',5,
     1            FILE64,60000,L,T1,S)
      IF (S.NE.30.OR.FILE64.NE.'sentinel') STOP 10
      USED = 0
      DO 30 I = 10, 99
         INQUIRE(UNIT=I,OPENED=OPENED)
         IF (OPENED) USED = USED + 1
         CLOSE(I)
   30 CONTINUE
      IF (USED.NE.90) STOP 11
      END
"#,
        true,
        &[
            ("bytes", &all_bytes),
            ("empty", b""),
            ("oversized", &oversized),
        ],
    );
}

#[test]
fn command_builders_support_context_menus_and_twenty_five_options() {
    let output = run_program(
        "commands",
        r#"      PROGRAM T
      CHARACTER*60000 OUT, OTHER
      CHARACTER*132 UNAME
      CHARACTER*32 NAMES(25)
      CHARACTER*400 DESC, DESCS(25)
      INTEGER LENS(25), DLENS(25), TYPES(25), REQS(25)
      INTEGER I, S, L, T1, L2, T2, S2
      CALL D7CMD('Inspect User',12,' ',0,2,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 1
      WRITE(*,'(A)') OUT(1:L)
      CALL D7XACB('Inspect User',12,'ignored',7,2,OTHER,60000,
     1            L2,T2,S2)
      IF (S2.NE.0.OR.OUT.NE.OTHER.OR.L.NE.L2) STOP 2
      CALL D7CMD('Inspect Message',15,' ',0,3,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 3
      WRITE(*,'(A)') OUT(1:L)
      CALL D7CMD('hello',5,'say hello',9,1,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 4
      CALL D7SCM('hello',5,'say hello',9,OTHER,60000,L2,T2,S2)
      IF (S2.NE.0.OR.OUT.NE.OTHER) STOP 5
      OUT = 'sentinel'
      CALL D7CMD('hello',5,'say hello',9,4,OUT,60000,L,T1,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 6
      CALL D7XACB('Bad Name',8,'say',3,1,OUT,60000,L,T1,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 7
      DO 10 I = 1, 100
         DESC(2*I-1:2*I) = CHAR(195)//CHAR(169)
   10 CONTINUE
      CALL D7SCM('hello',5,DESC,200,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 8
      DO 20 I = 1, 25
         WRITE(NAMES(I),'(A,I2.2)') 'arg',I
         LENS(I) = 5
         DESCS(I) = DESC
         DLENS(I) = 200
         TYPES(I) = 3
         REQS(I) = 1
   20 CONTINUE
      CALL D7SCX('hello',5,DESC,200,25,NAMES,LENS,DESCS,
     1           DLENS,TYPES,REQS,OUT,60000,L,T1,S)
      IF (S.NE.0.OR.T1.NE.0) STOP 9
      WRITE(*,'(A)') OUT(1:L)
      DESC(201:202) = CHAR(195)//CHAR(169)
      OUT = 'sentinel'
      CALL D7SCM('hello',5,DESC,202,OUT,60000,L,T1,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 10
      DO 30 I = 1, 33
         UNAME(2*I-1:2*I) = CHAR(195)//CHAR(169)
   30 CONTINUE
      CALL D7CMD(UNAME,64,'say',3,1,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 11
      OUT = 'sentinel'
      CALL D7CMD(UNAME,66,'say',3,1,OUT,60000,L,T1,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 12
      END
"#,
        true,
        &[],
    );
    let lines = output.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], r#"{"name":"Inspect User","type":2}"#);
    assert_eq!(lines[1], r#"{"name":"Inspect Message","type":3}"#);
    let payload = dfor77::parse_json(lines[2]).unwrap();
    let options = payload.get("options").unwrap().as_array().unwrap();
    assert_eq!(options.len(), 25);
    assert_eq!(options[24].get("name").unwrap().as_str(), Some("arg25"));
    assert_eq!(
        payload
            .get("description")
            .unwrap()
            .as_str()
            .unwrap()
            .chars()
            .count(),
        100
    );
}

#[test]
fn command_options_reject_duplicate_names_before_writing_the_payload() {
    let output = run_program(
        "command-option-names",
        r#"      PROGRAM T
      CHARACTER*60000 OUT
      CHARACTER*128 NAMES(25), DESCS(25)
      INTEGER LENS(25), DLENS(25), TYPES(25), REQS(25)
      INTEGER I, S, L, TR
      DO 10 I = 1, 25
         WRITE(NAMES(I),'(A,I2.2)') 'arg',I
         LENS(I) = 5
         DESCS(I) = 'description'
         DLENS(I) = 11
         TYPES(I) = 3
         REQS(I) = 0
   10 CONTINUE
      NAMES(25) = NAMES(1)
      OUT = 'sentinel'
      CALL D7SCX('hello',5,'description',11,25,NAMES,LENS,
     1           DESCS,DLENS,TYPES,REQS,OUT,60000,L,TR,S)
      IF (S.NE.30.OR.L.NE.0.OR.TR.NE.0) STOP 1
      IF (OUT.NE.'sentinel') STOP 2
      NAMES(25) = 'arg25'
      REQS(25) = 1
      CALL D7SCX('hello',5,'description',11,25,NAMES,LENS,
     1           DESCS,DLENS,TYPES,REQS,OUT,60000,L,TR,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 6
      REQS(25) = 0
      NAMES(1) = CHAR(195)//CHAR(169)
      LENS(1) = 2
      NAMES(2) = NAMES(1)
      LENS(2) = 2
      CALL D7SCX('hello',5,'description',11,2,NAMES,LENS,
     1           DESCS,DLENS,TYPES,REQS,OUT,60000,L,TR,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 3
      NAMES(2) = NAMES(1)
      NAMES(2)(3:3) = 'x'
      LENS(2) = 3
      CALL D7SCX('hello',5,'description',11,2,NAMES,LENS,
     1           DESCS,DLENS,TYPES,REQS,OUT,60000,L,TR,S)
      IF (S.NE.0.OR.TR.NE.0) STOP 4
      WRITE(*,'(A)') OUT(1:L)
      NAMES(1) = 'first'
      LENS(1) = 5
      NAMES(2) = 'firstly'
      LENS(2) = 7
      CALL D7SCX('hello',5,'description',11,2,NAMES,LENS,
     1           DESCS,DLENS,TYPES,REQS,OUT,60000,L,TR,S)
      IF (S.NE.0.OR.TR.NE.0) STOP 5
      WRITE(*,'(A)') OUT(1:L)
      END
"#,
        false,
        &[],
    );
    let names = output
        .lines()
        .map(|line| {
            let payload = dfor77::parse_json(line).unwrap();
            payload
                .get("options")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|option| option.get("name").unwrap().as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(names, [vec!["é", "éx"], vec!["first", "firstly"]]);
}

#[test]
fn command_options_obey_required_ordering_and_subcommand_boundaries() {
    let mut cases = String::new();
    let mut valid = 0;
    for left_type in 1..=11 {
        for right_type in 1..=11 {
            for left_required in 0..=1 {
                for right_required in 0..=1 {
                    let structural = left_type <= 2 && right_type <= 2;
                    let parameters = left_type >= 3 && right_type >= 3;
                    let ordered = left_required == 1 || right_required == 0;
                    let status = if structural || parameters && ordered {
                        0
                    } else {
                        30
                    };
                    valid += usize::from(status == 0);
                    cases.push_str(&format!(
                        "{left_type} {right_type} {left_required} {right_required} {status}\n"
                    ));
                }
            }
        }
    }
    let output = run_program(
        "command-option-schema",
        r#"      PROGRAM T
      CHARACTER*60000 OUT
      CHARACTER*32 NAMES(2), DESCS(2)
      INTEGER LENS(2), DLENS(2), TYPES(2), REQS(2)
      INTEGER S, L, TR, IOS, EXPECT
      NAMES(1) = 'first'
      NAMES(2) = 'second'
      LENS(1) = 5
      LENS(2) = 6
      DESCS(1) = 'description'
      DESCS(2) = 'description'
      DLENS(1) = 11
      DLENS(2) = 11
      OPEN(10,FILE='cases',STATUS='OLD')
   10 READ(10,*,IOSTAT=IOS) TYPES(1),TYPES(2),REQS(1),
     1                         REQS(2),EXPECT
      IF (IOS.LT.0) GOTO 20
      IF (IOS.NE.0) STOP 1
      OUT = 'sentinel'
      L = -1
      TR = -1
      CALL D7SCX('hello',5,'description',11,2,NAMES,LENS,
     1           DESCS,DLENS,TYPES,REQS,OUT,60000,L,TR,S)
      IF (S.NE.EXPECT.OR.TR.NE.0) STOP 2
      IF (S.NE.0) THEN
         IF (L.NE.0.OR.OUT.NE.'sentinel') STOP 3
      ELSE
         WRITE(*,'(A)') OUT(1:L)
      ENDIF
      GOTO 10
   20 CLOSE(10)
      CALL D7SCX('hello',5,'description',11,0,NAMES,LENS,
     1           DESCS,DLENS,TYPES,REQS,OUT,60000,L,TR,S)
      IF (S.NE.0.OR.TR.NE.0) STOP 4
      WRITE(*,'(A)') OUT(1:L)
      END
"#,
        true,
        &[("cases", cases.as_bytes())],
    );
    let lines = output.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), valid + 1);
    for line in &lines[..valid] {
        let payload = dfor77::parse_json(line).unwrap();
        let options = payload.get("options").unwrap().as_array().unwrap();
        assert_eq!(options.len(), 2);
        for option in options {
            let option_type = option.get("type").unwrap().as_u64().unwrap();
            assert_eq!(option.get("required").is_some(), option_type >= 3);
        }
    }
    assert_eq!(
        lines[valid],
        r#"{"name":"hello","description":"description","type":1}"#
    );
}

#[test]
fn button_ids_are_unique_across_rows_without_trimming_or_normalising_them() {
    let output = run_program(
        "button-identifiers",
        r#"      PROGRAM T
      CHARACTER*60000 OUT
      CHARACTER*400 IDS(25), LABELS(25)
      INTEGER ILENS(25), LLENS(25), STYLES(25), ROWS(25)
      INTEGER I, S, L, TR
      DO 10 I = 1, 25
         WRITE(IDS(I),'(A,I2.2)') 'id',I
         ILENS(I) = 4
         LABELS(I) = 'label'
         LLENS(I) = 5
         STYLES(I) = 1+MOD(I,4)
         ROWS(I) = 1+(I-1)/5
   10 CONTINUE
      CALL D7PAC(' ',0,25,LABELS,LLENS,IDS,ILENS,STYLES,
     1           ROWS,OUT,60000,L,TR,S)
      IF (S.NE.0.OR.TR.NE.0) STOP 1
      WRITE(*,'(A)') OUT(1:L)
      IDS(25) = IDS(1)
      OUT = 'sentinel'
      CALL D7PAC(' ',0,25,LABELS,LLENS,IDS,ILENS,STYLES,
     1           ROWS,OUT,60000,L,TR,S)
      IF (S.NE.30.OR.L.NE.0.OR.TR.NE.0) STOP 2
      IF (OUT.NE.'sentinel') STOP 3
      IDS(2) = IDS(1)
      CALL D7PAC(' ',0,2,LABELS,LLENS,IDS,ILENS,STYLES,
     1           ROWS,OUT,60000,L,TR,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 4
      IDS(1) = 'id'
      IDS(2) = 'id '
      ILENS(1) = 2
      ILENS(2) = 3
      CALL D7PAC(' ',0,2,LABELS,LLENS,IDS,ILENS,STYLES,
     1           ROWS,OUT,60000,L,TR,S)
      IF (S.NE.0.OR.TR.NE.0) STOP 5
      WRITE(*,'(A)') OUT(1:L)
      IDS(1) = CHAR(195)//CHAR(169)
      IDS(2) = IDS(1)
      ILENS(1) = 2
      ILENS(2) = 2
      OUT = 'sentinel'
      CALL D7PAC(' ',0,2,LABELS,LLENS,IDS,ILENS,STYLES,
     1           ROWS,OUT,60000,L,TR,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 6
      IDS(1) = CHAR(230)//CHAR(151)//CHAR(165)//
     1         CHAR(230)//CHAR(156)//CHAR(172)
      IDS(2) = IDS(1)
      ILENS(1) = 6
      ILENS(2) = 6
      CALL D7PAC(' ',0,2,LABELS,LLENS,IDS,ILENS,STYLES,
     1           ROWS,OUT,60000,L,TR,S)
      IF (S.NE.30.OR.OUT.NE.'sentinel') STOP 7
      IDS(2)(7:9) = CHAR(232)//CHAR(170)//CHAR(158)
      ILENS(2) = 9
      CALL D7PAC(' ',0,2,LABELS,LLENS,IDS,ILENS,STYLES,
     1           ROWS,OUT,60000,L,TR,S)
      IF (S.NE.0.OR.TR.NE.0) STOP 8
      WRITE(*,'(A)') OUT(1:L)
      IDS(1) = CHAR(195)//CHAR(169)
      IDS(2) = 'e'//CHAR(204)//CHAR(129)
      ILENS(1) = 2
      ILENS(2) = 3
      CALL D7PAC(' ',0,2,LABELS,LLENS,IDS,ILENS,STYLES,
     1           ROWS,OUT,60000,L,TR,S)
      IF (S.NE.0.OR.TR.NE.0) STOP 9
      WRITE(*,'(A)') OUT(1:L)
      END
"#,
        false,
        &[],
    );
    let rows = output
        .lines()
        .map(|line| dfor77::parse_json(line).unwrap())
        .collect::<Vec<_>>();
    let components = rows[0].get("components").unwrap().as_array().unwrap();
    assert_eq!(components.len(), 5);
    let ids = components
        .iter()
        .flat_map(|row| row.get("components").unwrap().as_array().unwrap())
        .map(|button| button.get("custom_id").unwrap().as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 25);
    let buttons = rows[1].get("components").unwrap().as_array().unwrap()[0]
        .get("components")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(buttons[0].get("custom_id").unwrap().as_str(), Some("id"));
    assert_eq!(buttons[1].get("custom_id").unwrap().as_str(), Some("id "));
    for (payload, expected) in rows[2..]
        .iter()
        .zip([["日本", "日本語"], ["é", "e\u{301}"]])
    {
        let buttons = payload.get("components").unwrap().as_array().unwrap()[0]
            .get("components")
            .unwrap()
            .as_array()
            .unwrap();
        let ids = buttons
            .iter()
            .map(|button| button.get("custom_id").unwrap().as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, expected);
    }
    assert_eq!(rows.len(), 4);
}

#[test]
fn standard_context_menu_builder_links_without_extension() {
    let output = run_program(
        "context",
        r#"      PROGRAM T
      CHARACTER*60000 OUT
      INTEGER S, L, T1
      CALL D7CMD('Inspect User',12,' ',0,2,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 1
      WRITE(*,'(A)') OUT(1:L)
      CALL D7CMD('Inspect Message',15,' ',0,3,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 2
      WRITE(*,'(A)') OUT(1:L)
      END
"#,
        false,
        &[],
    );
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        [
            r#"{"name":"Inspect User","type":2}"#,
            r#"{"name":"Inspect Message","type":3}"#,
        ]
    );
}

#[test]
fn fortran_utf8_validation_matches_rust_for_byte_pairs_and_unicode_boundaries() {
    let mut cases = String::new();
    let mut add = |bytes: &[u8]| {
        let expected = usize::from(std::str::from_utf8(bytes).is_ok());
        let mut padded = [0; 4];
        padded[..bytes.len()].copy_from_slice(bytes);
        cases.push_str(&format!(
            "{} {} {} {} {} {expected}\n",
            bytes.len(),
            padded[0],
            padded[1],
            padded[2],
            padded[3]
        ));
    };
    add(&[]);
    for first in 0..=255 {
        add(&[first]);
        for second in 0..=255 {
            add(&[first, second]);
        }
    }
    for first in 0xe0..=0xef {
        for second in 0..=255 {
            for third in [0x7f, 0x80, 0x8f, 0x90, 0x9f, 0xa0, 0xbf, 0xc0] {
                add(&[first, second, third]);
            }
        }
    }
    for first in 0xf0..=0xf4 {
        for second in 0..=255 {
            for third in [0x7f, 0x80, 0xbf, 0xc0] {
                for fourth in [0x7f, 0x80, 0xbf, 0xc0] {
                    add(&[first, second, third, fourth]);
                }
            }
        }
    }
    run_program(
        "utf8-oracle",
        r#"      PROGRAM T
      CHARACTER*4 TEXT
      CHARACTER*32 OUT
      INTEGER B(4), N, EXPECT, I, COUNT, L, TR, S
      LOGICAL OK
      OPEN(UNIT=10,FILE='utf8-cases',STATUS='OLD')
      COUNT = 0
   10 READ(10,*,END=30) N,(B(I),I=1,4),EXPECT
      TEXT = ' '
      DO 20 I = 1, N
         TEXT(I:I) = CHAR(B(I))
   20 CONTINUE
      CALL D7ZUOK(TEXT,N,OK)
      IF (OK.NEQV.(EXPECT.EQ.1)) STOP 1
      COUNT = COUNT + 1
      GOTO 10
   30 CLOSE(10)
      IF (COUNT.LT.100000) STOP 2
      TEXT = CHAR(224)//CHAR(128)//CHAR(128)
      OUT = 'sentinel'
      CALL D7JES(TEXT,3,OUT,32,L,TR,S)
      IF (S.NE.30.OR.L.NE.0.OR.TR.NE.0) STOP 3
      IF (OUT.NE.'sentinel') STOP 4
      END
"#,
        false,
        &[("utf8-cases", cases.as_bytes())],
    );
}

#[test]
fn extension_query_builder_preserves_errors_and_encodes_repeated_keys() {
    run_program(
        "query",
        r#"      PROGRAM T
      CHARACTER*60001 QRY, SAVED
      CHARACTER*2 VALUE
      INTEGER S, L, T1
      CALL D7XQ0(QRY,60001,L,T1,S)
      CALL D7XQA(QRY,60001,L,'tag',3,'a b&c',5,L,T1,S)
      IF (S.NE.0.OR.QRY(1:L).NE.'?tag=a%20b%26c') STOP 1
      CALL D7XQA(QRY,60001,L,'tag',3,' ',0,L,T1,S)
      IF (S.NE.0.OR.QRY(1:L).NE.'?tag=a%20b%26c&tag=') STOP 2
      VALUE = CHAR(195)//CHAR(169)
      CALL D7XQA(QRY,60001,L,'q',1,VALUE,2,L,T1,S)
      IF (S.NE.0) STOP 3
      IF (QRY(1:L).NE.'?tag=a%20b%26c&tag=&q=%C3%A9') STOP 4
      SAVED = QRY
      CALL D7XQA(QRY,60001,L,' ',0,'v',1,L,T1,S)
      IF (S.NE.30.OR.L.NE.0.OR.T1.NE.0) STOP 5
      IF (QRY.NE.SAVED) STOP 6
      QRY = 'a'
      SAVED = QRY
      CALL D7XQA(QRY,60001,60001,'k',1,'v',1,L,T1,S)
      IF (S.NE.30.OR.L.NE.0.OR.T1.NE.0) STOP 7
      IF (QRY.NE.SAVED) STOP 8
      CALL D7XQA(QRY,60001,60000,'k',1,'v',1,L,T1,S)
      IF (S.NE.30.OR.QRY.NE.SAVED) STOP 9
      CALL D7XQA(QRY,60001,59998,'k',1,' ',0,L,T1,S)
      IF (S.NE.30.OR.QRY.NE.SAVED) STOP 10
      CALL D7XQA(QRY,4,0,'k',1,'v',1,L,T1,S)
      IF (S.NE.0.OR.L.NE.4.OR.QRY.NE.'?k=v') STOP 11
      QRY = 'sentinel'
      CALL D7XQA(QRY,3,0,'k',1,'v',1,L,T1,S)
      IF (S.NE.10.OR.L.NE.4.OR.T1.NE.1) STOP 12
      IF (QRY.NE.'?k=') STOP 13
      END
"#,
        true,
        &[],
    );
}

#[test]
fn text_helpers_reject_invalid_lengths_and_preserve_utf8_when_clipping() {
    run_program(
        "text-bounds",
        r#"      PROGRAM T
      CHARACTER*8 OUT
      CHARACTER*1 SMALL
      CHARACTER*4 TEXT
      INTEGER L, T1, S, N
      LOGICAL OK
      OUT = 'sentinel'
      CALL D7ZCPY(OUT,8,'x',-1,L,T1,S)
      CALL BAD(OUT,L,T1,S)
      CALL D7ZCPY(OUT,8,'x',2,L,T1,S)
      CALL BAD(OUT,L,T1,S)
      CALL D7ZUOK('x',2,OK)
      IF (OK) STOP 1
      CALL D7ZUOK('x',-1,OK)
      IF (OK) STOP 2
      CALL D7ZPEN('x',2,OUT,L,S)
      IF (S.NE.30.OR.L.NE.0) STOP 3
      CALL D7ZPEN('x',-1,OUT,L,S)
      IF (S.NE.30.OR.L.NE.0) STOP 4
      CALL D7ZPDC('x',2,OUT,L,S)
      IF (S.NE.60.OR.L.NE.0) STOP 5
      CALL D7ZPDC('x',-1,OUT,L,S)
      IF (S.NE.60.OR.L.NE.0) STOP 6
      CALL D7ZPDC('%41%42',6,SMALL,L,S)
      IF (S.NE.60.OR.L.NE.0) STOP 7
      CALL D7ZPDC('ab',2,SMALL,L,S)
      IF (S.NE.60.OR.L.NE.0) STOP 8
      CALL D7ZPDC('%',1,SMALL,L,S)
      IF (S.NE.60.OR.L.NE.0) STOP 9
      CALL D7ZPDC('%41',3,SMALL,L,S)
      IF (S.NE.0.OR.L.NE.1.OR.SMALL.NE.'A') STOP 10
      TEXT = CHAR(240)//CHAR(159)//CHAR(152)//CHAR(128)
      DO 10 N = 0, 3
         OUT = 'sentinel'
         CALL D7ZCPY(OUT,N,TEXT,4,L,T1,S)
         IF (S.NE.10.OR.L.NE.4.OR.T1.NE.1) STOP 11
         IF (N.EQ.0.AND.OUT.NE.'sentinel') STOP 12
         IF (N.GT.0.AND.OUT.NE.' ') STOP 13
   10 CONTINUE
      CALL D7ZCPY(OUT,4,TEXT,4,L,T1,S)
      IF (S.NE.0.OR.L.NE.4.OR.T1.NE.0) STOP 14
      IF (OUT(1:4).NE.TEXT) STOP 15
      END

      SUBROUTINE BAD(OUT,L,T1,S)
      CHARACTER*(*) OUT
      INTEGER L,T1,S
      IF (S.NE.30.OR.L.NE.0.OR.T1.NE.0) STOP 90
      IF (OUT.NE.'sentinel') STOP 91
      END
"#,
        false,
        &[],
    );
}

#[test]
fn unicode_component_and_embed_limits_count_characters_instead_of_bytes() {
    let output = run_program(
        "unicode-payloads",
        r#"      PROGRAM T
      CHARACTER*16400 TEXT
      CHARACTER*60000 OUT
      CHARACTER*4 CH
      CHARACTER*400 LABL(1), VALU(1)
      INTEGER LL(1), VL(1), STYLE(1), ROW(1)
      INTEGER L,T1,S,K,N,I,J
      STYLE(1) = 1
      ROW(1) = 1
      DO 30 K = 1, 2
         N = 3
         CH = CHAR(230)//CHAR(151)//CHAR(165)
         IF (K.EQ.2) THEN
            N = 4
            CH = CHAR(240)//CHAR(159)//CHAR(152)//CHAR(128)
         ENDIF
         DO 10 I = 1, 4100
            J = (I-1)*N+1
            TEXT(J:J+N-1) = CH(1:N)
   10    CONTINUE
         CALL D7PEM(' ',0,TEXT,256*N,TEXT,4096*N,-1,
     1              OUT,60000,L,T1,S)
         CALL EMIT(OUT,L,T1,S)
         OUT = 'sentinel'
         CALL D7PEM(' ',0,TEXT,257*N,' ',0,-1,
     1              OUT,60000,L,T1,S)
         CALL BAD(OUT,L,T1,S)
         CALL D7PEM(' ',0,' ',0,TEXT,4097*N,-1,
     1              OUT,60000,L,T1,S)
         CALL BAD(OUT,L,T1,S)
         CALL D7PBT(' ',0,TEXT,80*N,TEXT,100*N,1,
     1              OUT,60000,L,T1,S)
         CALL EMIT(OUT,L,T1,S)
         OUT = 'sentinel'
         CALL D7PBT(' ',0,TEXT,81*N,TEXT,100*N,1,
     1              OUT,60000,L,T1,S)
         CALL BAD(OUT,L,T1,S)
         CALL D7PBT(' ',0,TEXT,80*N,TEXT,101*N,1,
     1              OUT,60000,L,T1,S)
         CALL BAD(OUT,L,T1,S)
         CALL D7IMD(TEXT,45*N,TEXT,100*N,TEXT,45*N,
     1              TEXT,100*N,1,1,OUT,60000,L,T1,S)
         CALL EMIT(OUT,L,T1,S)
         OUT = 'sentinel'
         CALL D7IMD(TEXT,46*N,TEXT,100*N,TEXT,45*N,
     1              TEXT,100*N,1,1,OUT,60000,L,T1,S)
         CALL BAD(OUT,L,T1,S)
         LABL(1) = TEXT(1:100*N)
         VALU(1) = TEXT(1:100*N)
         LL(1) = 100*N
         VL(1) = 100*N
         CALL D7PSM(' ',0,TEXT,100*N,1,LABL,LL,VALU,VL,
     1              OUT,60000,L,T1,S)
         CALL EMIT(OUT,L,T1,S)
         OUT = 'sentinel'
         CALL D7PSM(' ',0,TEXT,101*N,1,LABL,LL,VALU,VL,
     1              OUT,60000,L,T1,S)
         CALL BAD(OUT,L,T1,S)
         LL(1) = 80*N
         CALL D7PAC(' ',0,1,LABL,LL,VALU,VL,STYLE,ROW,
     1              OUT,60000,L,T1,S)
         CALL EMIT(OUT,L,T1,S)
         OUT = 'sentinel'
         LL(1) = 81*N
         CALL D7PAC(' ',0,1,LABL,LL,VALU,VL,STYLE,ROW,
     1              OUT,60000,L,T1,S)
         CALL BAD(OUT,L,T1,S)
   30 CONTINUE
      END

      SUBROUTINE BAD(OUT,L,T1,S)
      CHARACTER*(*) OUT
      INTEGER L,T1,S
      IF (S.NE.30.OR.L.NE.0.OR.T1.NE.0) STOP 90
      IF (OUT.NE.'sentinel') STOP 91
      END

      SUBROUTINE EMIT(OUT,L,T1,S)
      CHARACTER*(*) OUT
      INTEGER L,T1,S
      IF (S.NE.0.OR.T1.NE.0.OR.L.LE.0) STOP 92
      WRITE(*,'(A)') OUT(1:L)
      END
"#,
        false,
        &[],
    );
    let payloads = output
        .lines()
        .map(|line| dfor77::parse_json(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 10);
    for (group, character) in payloads.chunks_exact(5).zip(['日', '😀']) {
        let assert_text = |value: &dfor77::JsonValue, expected| {
            assert_eq!(
                value.as_str().unwrap(),
                character.to_string().repeat(expected)
            );
        };
        let embeds = group[0].get("embeds").unwrap().as_array().unwrap();
        assert_text(embeds[0].get("title").unwrap(), 256);
        assert_text(embeds[0].get("description").unwrap(), 4096);
        for payload in [&group[1], &group[3], &group[4]] {
            let row = &payload.get("components").unwrap().as_array().unwrap()[0];
            let component = &row.get("components").unwrap().as_array().unwrap()[0];
            assert_text(component.get("custom_id").unwrap(), 100);
            if let Some(label) = component.get("label") {
                assert_text(label, 80);
            } else {
                let option = &component.get("options").unwrap().as_array().unwrap()[0];
                assert_text(option.get("label").unwrap(), 100);
                assert_text(option.get("value").unwrap(), 100);
            }
            assert!(payload
                .get("allowed_mentions")
                .unwrap()
                .get("parse")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty());
        }
        let data = group[2].get("data").unwrap();
        assert_text(data.get("title").unwrap(), 45);
        assert_text(data.get("custom_id").unwrap(), 100);
        let row = &data.get("components").unwrap().as_array().unwrap()[0];
        let input = &row.get("components").unwrap().as_array().unwrap()[0];
        assert_text(input.get("label").unwrap(), 45);
        assert_text(input.get("custom_id").unwrap(), 100);
    }
}

#[test]
fn embed_total_limit_includes_all_text_fields_and_trims_outer_whitespace() {
    let output = run_program(
        "embed-total",
        r#"      PROGRAM T
      CHARACTER*60000 OUT
      CHARACTER*256 TITLE, AUTHOR
      CHARACTER*4100 DESC
      CHARACTER*2050 FOOT
      CHARACTER*1024 FNAM(1), FVAL(1)
      INTEGER FL(1), VL(1), INL(1), L,T1,S,I,NF,AL,DL,FT
      TITLE = REPEAT('t',256)
      DESC = REPEAT('d',4096)
      FOOT = REPEAT('f',1648)
      AUTHOR = 'a'
      FNAM(1) = 'n'
      FVAL(1) = 'v'
      FL(1) = 1
      VL(1) = 1
      INL(1) = 0
      DO 10 I = 1, 6
         AL = 0
         NF = 0
         DL = 4096
         FT = 1648
         IF (I.EQ.2) THEN
            FOOT(1649:1649) = 'f'
            FT = 1649
         ENDIF
         IF (I.EQ.3) AL = 1
         IF (I.EQ.4) NF = 1
         IF (I.EQ.5) THEN
            NF = 1
            FT = 1646
         ENDIF
         IF (I.EQ.6) THEN
            DESC = ' '//REPEAT('d',4096)//CHAR(9)
            DL = 4098
            FOOT = CHAR(227)//CHAR(128)//CHAR(128)//
     1             REPEAT('f',1648)
            FT = 1651
         ENDIF
         OUT = 'sentinel'
         CALL D7PE2(' ',0,TITLE,256,DESC,DL,' ',0,-1,' ',0,
     1        FOOT,FT,' ',0,' ',0,' ',0,AUTHOR,AL,
     2        ' ',0,' ',0,NF,FNAM,FL,FVAL,VL,INL,
     3        OUT,60000,L,T1,S)
         IF (I.GE.2.AND.I.LE.4) THEN
            IF (S.NE.30.OR.L.NE.0.OR.T1.NE.0) STOP 1
            IF (OUT.NE.'sentinel') STOP 2
         ELSE
            IF (S.NE.0.OR.T1.NE.0) STOP 3
            WRITE(*,'(A)') OUT(1:L)
         ENDIF
   10 CONTINUE
      END
"#,
        false,
        &[],
    );
    let payloads = output
        .lines()
        .map(|line| dfor77::parse_json(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 3);
    for payload in &payloads {
        let embed = &payload.get("embeds").unwrap().as_array().unwrap()[0];
        let mut count = 0;
        for key in ["title", "description"] {
            count += embed
                .get(key)
                .unwrap()
                .as_str()
                .unwrap()
                .trim()
                .chars()
                .count();
        }
        count += embed
            .get("footer")
            .unwrap()
            .get("text")
            .unwrap()
            .as_str()
            .unwrap()
            .trim()
            .chars()
            .count();
        if let Some(fields) = embed.get("fields") {
            for field in fields.as_array().unwrap() {
                count += field.get("name").unwrap().as_str().unwrap().chars().count();
                count += field
                    .get("value")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .chars()
                    .count();
            }
        }
        assert_eq!(count, 6000);
    }
}

#[test]
fn standard_and_extension_automod_builders_share_validation_and_output() {
    let source = r#"      PROGRAM T
      CHARACTER*60000 OUT, OTHER
      CHARACTER*244 WORD
      CHARACTER*4 CH
      INTEGER I,S,L,T1,L2,T2,S2
      CH = CHAR(240)//CHAR(159)//CHAR(152)//CHAR(128)
      DO 10 I = 1, 61
         WORD((I-1)*4+1:I*4) = CH
   10 CONTINUE
      CALL D7AMK('block',5,WORD,240,OUT,60000,L,T1,S)
      IF (S.NE.0.OR.T1.NE.0) STOP 1
      WRITE(*,'(A)') OUT(1:L)
      CALL D7XAMK('block',5,WORD,240,OTHER,60000,L2,T2,S2)
      IF (S2.NE.S.OR.L2.NE.L.OR.T2.NE.T1) STOP 2
      IF (OUT.NE.OTHER) STOP 3
      OUT = 'sentinel'
      OTHER = 'sentinel'
      CALL D7AMK('block',5,WORD,244,OUT,60000,L,T1,S)
      CALL D7XAMK('block',5,WORD,244,OTHER,60000,L2,T2,S2)
      IF (S.NE.30.OR.S2.NE.30.OR.L.NE.0.OR.L2.NE.0) STOP 4
      IF (T1.NE.0.OR.T2.NE.0) STOP 5
      IF (OUT.NE.'sentinel'.OR.OTHER.NE.'sentinel') STOP 6
      CALL D7AMK('a"b',3,'x"y',3,OUT,60000,L,T1,S)
      IF (S.NE.0) STOP 7
      WRITE(*,'(A)') OUT(1:L)
      CALL D7AMK('block',5,WORD,240,OUT,0,L,T1,S)
      IF (S.NE.10.OR.T1.NE.1) STOP 8
      END
"#;
    let output = run_program("automod", source, true, &[]);
    let standard_source = source
        .lines()
        .take_while(|line| !line.contains("CALL D7XAMK"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n      END\n";
    let standard_output = run_program("standard-automod", &standard_source, false, &[]);
    assert_eq!(
        standard_output,
        output.lines().next().unwrap().to_owned() + "\n"
    );
    let payloads = output
        .lines()
        .map(|line| dfor77::parse_json(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 2);
    assert_eq!(
        payloads[0]
            .get("trigger_metadata")
            .unwrap()
            .get("keyword_filter")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .as_str(),
        Some("😀".repeat(60).as_str())
    );
    assert_eq!(payloads[1].get("name").unwrap().as_str(), Some("a\"b"));
    assert_eq!(
        payloads[1]
            .get("trigger_metadata")
            .unwrap()
            .get("keyword_filter")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .as_str(),
        Some("x\"y")
    );
}

#[test]
fn malformed_ipc_numeric_fields_return_errors_without_terminating_fortran() {
    let fields = [
        "K",
        "K value raw",
        "K value raw /",
        "K value raw 1/ x",
        "K value raw 1,2 x",
        "K value raw 1.0 x",
        "K value raw 2147483648 x",
        "K value raw -1 x",
        "K value bogus 0",
        "K value raw 0 extra",
        "K value raw 2 x",
        "K value pct 1 %",
    ]
    .join("\n")
        + "\n";
    let output = run_program(
        "invalid-ipc-numbers",
        r#"      PROGRAM T
      CHARACTER*32 KEY
      CHARACTER*60000 VAL
      INTEGER I,S,L,FB
      COMMON /D7FRM/ FB
      OPEN(UNIT=5,FILE='fields',STATUS='OLD')
      DO 10 I = 1, 12
         FB = 0
         CALL D7ZRFL(KEY,VAL,L,S)
         IF (S.NE.60.OR.L.NE.0) STOP 1
   10 CONTINUE
      END
"#,
        false,
        &[("fields", fields.as_bytes())],
    );
    assert!(output.is_empty());
}

#[test]
fn fortran_integer_parsing_matches_rust_at_i32_boundaries_and_rejects_list_syntax() {
    let mut cases = String::new();
    let mut append = |input: &str| {
        let expected = input.parse::<i32>().ok();
        cases.push_str(input);
        cases.push('\n');
        cases.push_str(&format!(
            "{} {} {}\n",
            input.len(),
            u8::from(expected.is_some()),
            expected.unwrap_or(0)
        ));
    };
    for input in [
        "",
        "+",
        "-",
        "/",
        "1/",
        "1,2",
        "1 2",
        " 1",
        "1 ",
        "1.0",
        "1e2",
        "2*1",
        "0",
        "+0",
        "-0",
        "000000000000000000000000000001",
        "2147483647",
        "2147483648",
        "-2147483648",
        "-2147483649",
        "+2147483647",
        "+2147483648",
        "99999999999999999999",
    ] {
        append(input);
    }
    for value in (i32::MAX as i64 - 100)..=(i32::MAX as i64 + 100) {
        append(&value.to_string());
        append(&format!("+{value}"));
        append(&format!("-{value}"));
    }
    for value in -10_000..10_000 {
        append(&value.to_string());
    }
    run_program(
        "integer-oracle",
        r#"      PROGRAM T
      CHARACTER*128 TEXT
      INTEGER L,E,V,SOME,N
      LOGICAL OK,D7ZIOK
      OPEN(UNIT=9,FILE='cases',STATUS='OLD')
      N = 0
   10 READ(9,'(A)',END=20) TEXT
      READ(9,*) L,E,V
      SOME = 123
      OK = D7ZIOK(TEXT,L,SOME)
      IF (OK.NEQV.(E.EQ.1)) STOP 1
      IF (SOME.NE.V) STOP 2
      N = N+1
      GOTO 10
   20 IF (N.LT.20000) STOP 3
      SOME = 123
      OK = D7ZIOK('1',2,SOME)
      IF (OK.OR.SOME.NE.0) STOP 4
      OK = D7ZIOK('1',-1,SOME)
      IF (OK.OR.SOME.NE.0) STOP 5
      END
"#,
        false,
        &[("cases", cases.as_bytes())],
    );
}

#[test]
fn public_integer_accessors_reject_bad_values_without_terminating_fortran() {
    let fields = [
        ("event_type", "/"),
        ("event_type", "1/"),
        ("value", ""),
        ("is_admin", "1,0"),
        ("can", "2147483648"),
    ];
    let mut frames = String::new();
    for (index, (key, value)) in fields.into_iter().enumerate() {
        let rid = index + 1;
        frames.push_str(&format!("@D7/1 RES {rid} 0 1\nK {key} raw {}", value.len()));
        if !value.is_empty() {
            frames.push(' ');
            frames.push_str(value);
        }
        frames.push_str(&format!("\n@D7/1 END {rid}\n"));
    }
    run_program(
        "integer-accessors",
        r#"      PROGRAM T
      CHARACTER*128 WHY
      INTEGER VALUE,S,L,T1
      OPEN(UNIT=5,FILE='frames',STATUS='OLD')
      CALL D7NXT(VALUE,S)
      CALL BAD(VALUE,S)
      CALL D7TYP(VALUE,S)
      CALL BAD(VALUE,S)
      CALL D7ITP(VALUE,S)
      CALL BAD(VALUE,S)
      CALL D7ADM('1',1,'2',1,VALUE,S)
      CALL BAD(VALUE,S)
      CALL D7CDL('1',1,'2',1,'3',1,VALUE,WHY,128,L,T1,S)
      CALL BAD(VALUE,S)
      IF (L.NE.0.OR.T1.NE.0) STOP 2
      END

      SUBROUTINE BAD(VALUE,S)
      INTEGER VALUE,S
      IF (S.NE.60.OR.VALUE.NE.0) STOP 1
      END
"#,
        false,
        &[("frames", frames.as_bytes())],
    );
}

#[test]
fn invalid_outgoing_requests_emit_nothing_and_leave_the_next_response_unread() {
    let paths = "a".repeat(40_000);
    let frames = concat!(
        "@D7/1 RES 4 0 0\n@D7/1 END 4\n",
        "@D7/1 RES 5 0 0\n@D7/1 END 5\n"
    );
    let output = run_program(
        "request-preflight",
        r#"      PROGRAM T
      CHARACTER*40000 PATHS(2)
      CHARACTER*1 NAMES(2)
      CHARACTER*32 CHID, PATH, TEXT
      INTEGER FL(2),NL(2),S
      OPEN(UNIT=9,FILE='path',STATUS='OLD')
      READ(9,'(A)') PATHS(1)
      CLOSE(9)
      PATHS(2) = PATHS(1)
      NAMES(1) = 'a'
      NAMES(2) = 'b'
      FL(1) = 40000
      FL(2) = 40000
      NL(1) = 1
      NL(2) = 1
      OPEN(UNIT=5,FILE='frames',STATUS='OLD')
      CALL D7UPM('1',1,2,PATHS,FL,NAMES,NL,' ',0,S)
      IF (S.NE.30) STOP 1
      CHID = '1'//CHAR(10)//'@D7/1 END 2'
      CALL D7SND(CHID,14,'hello',5,S)
      IF (S.NE.30) STOP 2
      PATH = '/api/v10/users/@me'//CHAR(13)
      CALL D7RST(1,PATH,19,' ',0,' ',0,S)
      IF (S.NE.30) STOP 3
      CALL D7INI(S)
      IF (S.NE.0) STOP 4
      TEXT = 'one'//CHAR(10)//'two'//CHAR(13)//CHAR(9)//
     1       CHAR(230)//CHAR(151)//CHAR(165)//CHAR(240)//
     2       CHAR(159)//CHAR(152)//CHAR(128)
      CALL D7SND('1',1,TEXT,16,S)
      IF (S.NE.0) STOP 5
      END
"#,
        false,
        &[("path", paths.as_bytes()), ("frames", frames.as_bytes())],
    );
    let mut reader = std::io::Cursor::new(output.as_bytes());
    let request = dfor77::ipc::read_request(&mut reader).unwrap().unwrap();
    assert_eq!(request.id, 4);
    assert_eq!(request.verb, "INI");
    assert!(request.fields.is_empty());
    let request = dfor77::ipc::read_request(&mut reader).unwrap().unwrap();
    assert_eq!(request.id, 5);
    assert_eq!(request.verb, "SND");
    assert_eq!(request.field("content"), Some("one\ntwo\r\t日😀"));
    assert!(dfor77::ipc::read_request(&mut reader).unwrap().is_none());
}

#[test]
fn request_frame_budget_accepts_the_exact_limit_and_rejects_one_more_byte() {
    let header = "@D7/1 REQ 1 TST 2\n";
    let first_field_header = "K a raw 60000 ";
    let end = "@D7/1 END 1\n";
    let available = (0..=6000)
        .find(|length| {
            header.len()
                + first_field_header.len()
                + 60_000
                + 1
                + format!("K b raw {length} ").len()
                + length
                + 1
                + end.len()
                == dfor77::ipc::MAX_FRAME_BYTES
        })
        .unwrap();
    let source = r#"      PROGRAM T
      CHARACTER*60000 BIG
      CHARACTER*6000 SMALL
      INTEGER RID,S,N,F,I
      OPEN(UNIT=9,FILE='big',STATUS='OLD')
      READ(9,'(A)') BIG
      CLOSE(9)
      SMALL = BIG(1:6000)
      OPEN(UNIT=5,FILE='frames',STATUS='OLD')
      N = NMAX
      DO 10 I = 1, 2
         CALL D7ZBEG(RID,'TST',2)
         CALL D7ZRAW('a',BIG,60000)
         CALL D7ZRAW('b',SMALL,N)
         CALL D7ZFIN(RID)
         CALL D7ZRSP(RID,S,F)
         IF (F.NE.0) STOP 1
         CALL D7ZRFI(RID,S,F)
         IF (I.EQ.1.AND.S.NE.0) STOP 2
         IF (I.EQ.2.AND.S.NE.30) STOP 3
         N = N+1
   10 CONTINUE
      CALL D7INI(S)
      IF (S.NE.0) STOP 4
      END
"#
    .replace("NMAX", &available.to_string());
    let big = vec![b'x'; 60_000];
    let frames = b"@D7/1 RES 1 0 0\n@D7/1 END 1\n@D7/1 RES 3 0 0\n@D7/1 END 3\n";
    let output = run_program(
        "request-limit",
        &source,
        false,
        &[("big", &big), ("frames", frames)],
    );
    let mut reader = std::io::Cursor::new(output.as_bytes());
    let first = dfor77::ipc::read_request(&mut reader).unwrap().unwrap();
    assert_eq!(reader.position() as usize, dfor77::ipc::MAX_FRAME_BYTES);
    assert_eq!(first.verb, "TST");
    assert_eq!(first.field("a"), Some("x".repeat(60_000).as_str()));
    assert_eq!(first.field("b"), Some("x".repeat(available).as_str()));
    let last = dfor77::ipc::read_request(&mut reader).unwrap().unwrap();
    assert_eq!(last.id, 3);
    assert_eq!(last.verb, "INI");
    assert!(dfor77::ipc::read_request(&mut reader).unwrap().is_none());
}

#[test]
fn request_validation_rejects_bad_fields_and_supports_all_sixty_four_fields() {
    let frames = b"@D7/1 RES 12 0 0\n@D7/1 END 12\n@D7/1 RES 13 0 0\n@D7/1 END 13\n";
    let output = run_program(
        "request-fields",
        r#"      PROGRAM T
      CHARACTER*60001 BIG
      CHARACTER*32 KEY
      INTEGER I,J,RID,S,F,NF
      BIG = ' '
      DO 10 J = 1, 20001
         BIG(J:J) = CHAR(0)
   10 CONTINUE
      OPEN(UNIT=5,FILE='frames',STATUS='OLD')
      DO 20 I = 1, 11
         NF = 1
         IF (I.EQ.1.OR.I.EQ.3) NF = 2
         IF (I.EQ.2) NF = 0
         IF (I.EQ.10) NF = 65
         IF (I.EQ.9) THEN
            CALL D7ZBEG(RID,'TST'//CHAR(10),NF)
         ELSE
            CALL D7ZBEG(RID,'TST',NF)
         ENDIF
         IF (I.LE.3) THEN
            CALL D7ZRAW('a','x',1)
            IF (I.EQ.3) CALL D7ZRAW('a','y',1)
         ELSEIF (I.EQ.4) THEN
            CALL D7ZPCT('a',BIG,20001)
         ELSEIF (I.EQ.5) THEN
            CALL D7ZPCT('a',CHAR(255),1)
         ELSEIF (I.EQ.6) THEN
            CALL D7ZRAW('a',CHAR(255),1)
         ELSEIF (I.EQ.7) THEN
            CALL D7ZRAW('a','x',2)
         ELSEIF (I.EQ.8) THEN
            CALL D7ZRAW('bad key','x',1)
         ELSEIF (I.EQ.11) THEN
            CALL D7ZRAW('a',BIG,60001)
         ELSE
            CALL D7ZRAW('a','x',1)
         ENDIF
         CALL D7ZFIN(RID)
         CALL D7ZRSP(RID,S,F)
         IF (S.NE.30.OR.F.NE.0) STOP 1
         CALL D7ZRFI(RID,S,F)
         IF (S.NE.30.OR.F.NE.0) STOP 2
   20 CONTINUE
      CALL D7ZBEG(RID,'TST',64)
      DO 30 I = 1, 64
         CALL D7ZKEY(KEY,'v',I)
         CALL D7ZRAW(KEY,'x',1)
   30 CONTINUE
      CALL D7ZFIN(RID)
      CALL D7ZRSP(RID,S,F)
      IF (S.NE.0.OR.F.NE.0) STOP 3
      CALL D7ZRFI(RID,S,F)
      IF (S.NE.0.OR.F.NE.0) STOP 4
      CALL D7INI(S)
      IF (S.NE.0) STOP 5
      END
"#,
        false,
        &[("frames", frames)],
    );
    let mut reader = std::io::Cursor::new(output.as_bytes());
    let request = dfor77::ipc::read_request(&mut reader).unwrap().unwrap();
    assert_eq!(request.id, 12);
    assert_eq!(request.fields.len(), 64);
    for index in 1..=64 {
        assert_eq!(request.field(&format!("v{index}")), Some("x"));
    }
    let last = dfor77::ipc::read_request(&mut reader).unwrap().unwrap();
    assert_eq!(last.id, 13);
    assert_eq!(last.verb, "INI");
    assert!(dfor77::ipc::read_request(&mut reader).unwrap().is_none());
}

#[test]
fn response_errors_are_preserved_and_non_boolean_permissions_are_rejected() {
    let fields = [
        (40, "role_id", "17"),
        (0, "is_admin", "2"),
        (0, "is_admin", "-1"),
        (0, "can", "2"),
        (0, "can", "-1"),
        (0, "can", "2"),
        (0, "value", "2"),
        (0, "value", "-1"),
    ];
    let frames = fields
        .into_iter()
        .enumerate()
        .map(|(index, (status, key, value))| {
            let rid = index + 1;
            format!(
                "@D7/1 RES {rid} {status} 1\nK {key} raw {} {value}\n@D7/1 END {rid}\n",
                value.len()
            )
        })
        .collect::<String>();
    run_program(
        "response-status-booleans",
        r#"      PROGRAM T
      CHARACTER*32 BUF
      INTEGER L,T1,S,V
      OPEN(UNIT=5,FILE='frames',STATUS='OLD')
      CALL D7RLI(1,BUF,32,L,T1,S)
      IF (S.NE.40) STOP 1
      CALL D7ADM('1',1,'2',1,V,S)
      CALL BAD(V,S)
      CALL D7CAD(V,S)
      CALL BAD(V,S)
      CALL D7CDP(V,BUF,32,L,T1,S)
      CALL BAD(V,S)
      CALL D7CTM('1',1,'2',1,V,BUF,32,L,T1,S)
      CALL BAD(V,S)
      CALL D7CDL('1',1,'2',1,'3',1,V,BUF,32,L,T1,S)
      CALL BAD(V,S)
      CALL D7TAV(V,S)
      CALL BAD(V,S)
      CALL D7BOT(V,S)
      CALL BAD(V,S)
      END

      SUBROUTINE BAD(V,S)
      INTEGER V,S
      IF (S.NE.60.OR.V.NE.0) STOP 2
      END
"#,
        false,
        &[("frames", frames.as_bytes())],
    );
}

#[test]
fn duplicate_response_fields_do_not_grant_permissions_or_replace_role_ids() {
    let keys = ["is_admin", "can", "value", "role_id"];
    let frames = keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| {
            let rid = index + 1;
            format!("@D7/1 RES {rid} 0 2\nK {key} raw 1 0\nK {key} raw 1 1\n@D7/1 END {rid}\n")
        })
        .collect::<String>();
    run_program(
        "duplicate-response-fields",
        r#"      PROGRAM T
      CHARACTER*32 BUF
      INTEGER L,T1,S,V
      OPEN(UNIT=5,FILE='frames',STATUS='OLD')
      CALL D7CAD(V,S)
      CALL BAD(V,S)
      CALL D7CDP(V,BUF,32,L,T1,S)
      CALL BAD(V,S)
      IF (L.NE.0.OR.T1.NE.0) STOP 3
      CALL D7TAV(V,S)
      CALL BAD(V,S)
      CALL D7RLI(1,BUF,32,L,T1,S)
      IF (S.NE.60.OR.L.NE.0.OR.T1.NE.0) STOP 4
      END

      SUBROUTINE BAD(V,S)
      INTEGER V,S
      IF (S.NE.60.OR.V.NE.0) STOP 2
      END
"#,
        false,
        &[("frames", frames.as_bytes())],
    );
}

#[test]
fn response_field_tracking_accepts_sixty_four_keys_and_resets_between_responses() {
    let mut frames = "@D7/1 RES 1 0 64\n".to_owned();
    for index in 1..=64 {
        frames.push_str(&format!("K v{index} raw 0\n"));
    }
    frames.push_str(concat!(
        "@D7/1 END 1\n",
        "@D7/1 RES 2 0 1\nK v1 raw 0\n@D7/1 END 2\n",
        "@D7/1 RES 3 0 2\nK v1 raw 0\nK v1 raw 0\n@D7/1 END 3\n",
        "@D7/1 RES 4 0 1\nK v1 raw 0\n@D7/1 END 4\n"
    ));
    run_program(
        "response-field-tracking",
        r#"      PROGRAM T
      INTEGER I,S
      OPEN(UNIT=5,FILE='frames',STATUS='OLD')
      DO 10 I = 1, 4
         CALL D7INI(S)
         IF (I.EQ.3) THEN
            IF (S.NE.60) STOP 1
         ELSE
            IF (S.NE.0) STOP 2
         ENDIF
   10 CONTINUE
      END
"#,
        false,
        &[("frames", frames.as_bytes())],
    );
}

#[test]
fn request_write_errors_return_protocol_failure_without_consuming_a_response() {
    run_program(
        "request-write-error",
        r#"      PROGRAM T
      CHARACTER*80 LINE
      INTEGER S
      OPEN(UNIT=5,FILE='frames',STATUS='OLD')
      OPEN(UNIT=6,FILE='readonly',STATUS='OLD',ACTION='READ')
      CALL D7INI(S)
      IF (S.NE.60) STOP 1
      CLOSE(6)
      OPEN(UNIT=6,FILE='wire',STATUS='UNKNOWN')
      CALL D7INI(S)
      IF (S.NE.0) STOP 2
      CLOSE(6)
      OPEN(UNIT=9,FILE='wire',STATUS='OLD')
      READ(9,'(A)') LINE
      IF (LINE.NE.'@D7/1 REQ 2 INI 0') STOP 3
      READ(9,'(A)') LINE
      IF (LINE.NE.'@D7/1 END 2') STOP 4
      READ(9,'(A)',END=10) LINE
      STOP 5
   10 CLOSE(9)
      END
"#,
        false,
        &[
            ("frames", b"@D7/1 RES 2 0 0\n@D7/1 END 2\n"),
            ("readonly", b"unchanged\n"),
        ],
    );
}
