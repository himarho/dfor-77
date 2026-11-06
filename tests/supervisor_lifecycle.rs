#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct RunningTest {
    child: Child,
    directory: PathBuf,
}

impl Drop for RunningTest {
    fn drop(&mut self) {
        for name in ["bot.pid", "bot-0.pid", "bot-1.pid"] {
            let pid_file = self.directory.join(name);
            if let Ok(pid) = std::fs::read_to_string(pid_file) {
                if let Ok(pid) = pid.trim().parse::<u32>() {
                    if let Ok(command) = std::fs::read(format!("/proc/{pid}/cmdline")) {
                        if command
                            .windows(self.directory.as_os_str().as_encoded_bytes().len())
                            .any(|part| part == self.directory.as_os_str().as_encoded_bytes())
                        {
                            let _ = Command::new("kill")
                                .args(["-KILL", &pid.to_string()])
                                .stderr(Stdio::null())
                                .status();
                        }
                    }
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl RunningTest {
    fn launch(name: &str, arguments: &[&str], code: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("dfor77-{name}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_dfor77"))
            .args(arguments)
            .args(["--bot", "python3", "-c", code])
            .arg(&directory)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self { child, directory }
    }

    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "supervisor shutdown timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_reaped(&self, name: &str) {
        let pid = std::fs::read_to_string(self.directory.join(name))
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "Bot was left running or unreaped"
        );
    }
}

#[test]
fn one_shard_failure_stops_and_reaps_other_running_bots() {
    let code = r#"import os,sys,time
root=sys.argv[1]
shard=os.environ['DFOR77_SHARD_ID']
open(os.path.join(root,'bot-'+shard+'.pid'),'w').write(str(os.getpid()))
if shard=='0':
    while not os.path.exists(os.path.join(root,'bot-1.pid')):
        time.sleep(0.01)
    os._exit(1)
time.sleep(30)
"#;
    let mut running = RunningTest::launch("shard-cleanup", &["--shards", "2"], code);
    assert_eq!(running.wait(Duration::from_secs(12)).code(), Some(10));
    running.assert_reaped("bot-0.pid");
    running.assert_reaped("bot-1.pid");
}

#[test]
fn a_failed_shard_cancels_other_shards_delayed_startup() {
    let code = "import os;os._exit(1)";
    let mut running = RunningTest::launch("shard-startup", &["--shards", "2"], code);
    assert_eq!(running.wait(Duration::from_secs(3)).code(), Some(10));
}

#[test]
fn a_bot_which_never_reads_responses_is_stopped_and_reaped() {
    let code = r#"import os,sys
open(os.path.join(sys.argv[1],'bot.pid'),'w').write(str(os.getpid()))
frame=b'@D7/1 REQ 1 INI 0\n@D7/1 END 1\n'
while True:
    os.write(1,frame)
"#;
    let mut running = RunningTest::launch("ipc-write", &["--bot-io-timeout-ms", "100"], code);
    assert_eq!(running.wait(Duration::from_secs(5)).code(), Some(10));
    running.assert_reaped("bot.pid");
}

#[test]
fn closing_stdout_without_exiting_does_not_freeze_the_supervisor() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("dfor77-shutdown-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let pid_file = directory.join("bot.pid");
    let code = "import os,sys,time;open(sys.argv[1],'w').write(str(os.getpid()));os.close(1);time.sleep(30)";
    let child = Command::new(env!("CARGO_BIN_EXE_dfor77"))
        .args(["--bot-io-timeout-ms", "100", "--bot", "python3", "-c", code])
        .arg(&pid_file)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut running = RunningTest { child, directory };
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = running.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "supervisor shutdown timed out");
        std::thread::sleep(Duration::from_millis(10));
    };
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "Bot was left running or unreaped"
    );
    assert_eq!(status.code(), Some(10));
    let mut stderr = String::new();
    running
        .child
        .stderr
        .take()
        .unwrap()
        .take(65536)
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("Bot did not exit"), "{stderr}");
}

#[test]
fn cli_errors_redact_discord_credentials() {
    let token = [
        "abcdefghijklmnopqrstuvwx",
        "abcdef",
        "abcdefghijklmnopqrstuvwxyz1234567890",
    ]
    .join(".");
    let output = Command::new(env!("CARGO_BIN_EXE_dfor77"))
        .arg(&token)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(12));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown argument"));
    assert!(!stderr.contains(&token));
}

#[test]
fn zero_or_invalid_bot_deadlines_are_rejected_before_launch() {
    for timeout in ["0", "-1", "invalid"] {
        let output = Command::new(env!("CARGO_BIN_EXE_dfor77"))
            .args(["--bot-io-timeout-ms", timeout, "--bot", "must-not-run"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(12));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("requires positive milliseconds"));
    }
}

#[test]
fn gateway_and_bot_diagnostics_cannot_inject_lines_or_terminal_controls() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "dfor77-log-injection-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).unwrap();
    let gateway = directory.join("gateway.jsonl");
    let event = "FUTURE_EVENT\ndfor77 error FORGED\u{1b}[2J\u{202e}";
    std::fs::write(
        &gateway,
        format!(
            r#"{{"op":0,"s":1,"t":"{}","d":{{}}}}
"#,
            dfor77::audit::json_escape(event)
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_dfor77"))
        .arg("--stdio-server")
        .arg("--mock-gateway")
        .arg(&gateway)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut running = RunningTest { child, directory };
    let message = "診断 العربية 👩‍💻\u{2028}FORGED\u{2029}\u{2066}hidden\u{2069}";
    let encoded = dfor77::percent_encode(message);
    let requests = format!("@D7/1 REQ 1 INI 0\n@D7/1 END 1\n@D7/1 REQ 2 LOG 2\nK level raw 1 1\nK message pct {} {encoded}\n@D7/1 END 2\n@D7/1 REQ 3 END 0\n@D7/1 END 3\n", encoded.len());
    running
        .child
        .stdin
        .take()
        .unwrap()
        .write_all(requests.as_bytes())
        .unwrap();
    assert_eq!(running.wait(Duration::from_secs(3)).code(), Some(0));
    let mut stdout = String::new();
    running
        .child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert!(stdout.contains("@D7/1 RES 2 0 0"));
    let mut stderr = String::new();
    running
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(stderr.lines().count(), 2);
    assert!(stderr.contains("診断 العربية 👩‍💻"));
    for forbidden in [
        '\r', '\u{1b}', '\u{2028}', '\u{2029}', '\u{202e}', '\u{2066}', '\u{2069}',
    ] {
        assert!(
            !stderr.contains(forbidden),
            "unescaped diagnostic control U+{:04X}",
            forbidden as u32
        );
    }
}

#[test]
fn cli_errors_cannot_reorder_or_split_the_diagnostic_line() {
    let argument = "--unknown\u{2028}forged\u{202e}text\u{202c}";
    let output = Command::new(env!("CARGO_BIN_EXE_dfor77"))
        .arg(argument)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(12));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1);
    for forbidden in ['\u{2028}', '\u{202e}', '\u{202c}'] {
        assert!(!stderr.contains(forbidden));
    }
    assert!(stderr.contains("unknown argument"));
}
