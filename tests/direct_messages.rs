use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Directory(PathBuf);

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct RunningBot(Child);

impl Drop for RunningBot {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn standard_fortran_bot_receives_and_replies_to_a_direct_message() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("dfor77-dm-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let directory = Directory(path);
    let source = r#"      PROGRAM T
      INTEGER S, E, L, TR, CAN
      CHARACTER*32 CHAN, GUILD, TEXT
      CHARACTER*64 WHY
      CALL D7INI(S)
      IF (S.NE.0) STOP 1
   10 CALL D7NXT(E,S)
      IF (S.NE.0) STOP 2
      IF (E.NE.101) GOTO 10
      CALL D7GID(GUILD,32,L,TR,S)
      IF (S.NE.31) STOP 3
      CALL D7TXT(TEXT,32,L,TR,S)
      IF (S.NE.0.OR.TEXT.NE.'ping') STOP 4
      CALL D7CHN(CHAN,32,L,TR,S)
      IF (S.NE.0) STOP 5
      CALL D7CSP(CAN,WHY,64,L,TR,S)
      IF (S.NE.0.OR.CAN.NE.1) STOP 6
      CALL D7SND(CHAN,2,'pong',4,S)
      IF (S.NE.0) STOP 7
      CALL D7END(S)
      IF (S.NE.0) STOP 8
      END
"#;
    assert!(source.lines().all(|line| line.len() <= 72));
    std::fs::write(directory.0.join("bot.f"), source).unwrap();
    let compiled = Command::new("gfortran")
        .args([
            "-std=legacy",
            "-ffixed-line-length-72",
            "-fcheck=all",
            "-O2",
        ])
        .arg(directory.0.join("bot.f"))
        .arg(root.join("fortran/dfor77_wrapper.f"))
        .arg("-o")
        .arg(directory.0.join("bot"))
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let config = std::fs::read_to_string(root.join("config/dfor77.sample.toml"))
        .unwrap()
        .replace(
            "[\"GUILDS\", \"GUILD_MESSAGES\", \"MESSAGE_CONTENT\", \"GUILD_MEMBERS\"]",
            "[\"DIRECT_MESSAGES\"]",
        )
        .replace("live = true", "live = false");
    std::fs::write(directory.0.join("config.toml"), config).unwrap();
    std::fs::write(directory.0.join("events.jsonl"), concat!(
        "{\"op\":0,\"s\":1,\"t\":\"READY\",\"d\":{\"session_id\":\"test\",\"resume_gateway_url\":\"wss://resume\",\"user\":{\"id\":\"99\"}}}\n",
        "{\"op\":0,\"s\":2,\"t\":\"MESSAGE_CREATE\",\"d\":{\"channel_id\":\"20\",\"id\":\"30\",\"content\":\"ping\",\"type\":0,\"author\":{\"id\":\"40\"}}}\n",
    )).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut requests = Vec::new();
        while requests.len() < 2 {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "DM HTTP requests timed out");
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                assert!(bytes.len() <= 65536);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let head = std::str::from_utf8(&bytes[..end]).unwrap();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let body = if requests.is_empty() {
                r#"{"id":"20","type":1}"#
            } else {
                r#"{"id":"31","channel_id":"20","type":0,"author":{"id":"99"}}"#
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            requests.push(String::from_utf8(bytes).unwrap());
        }
        requests
    });
    let mut runtime = RunningBot(
        Command::new(env!("CARGO_BIN_EXE_dfor77"))
            .current_dir(&directory.0)
            .args([
                "--config",
                "config.toml",
                "--mock-gateway",
                "events.jsonl",
                "--mock-rest",
            ])
            .arg(&url)
            .arg("--bot")
            .arg(directory.0.join("bot"))
            .env("DISCORD_BOT_TOKEN", "test-token")
            .env("DFOR77_AUDIT_HASH_KEY", "test-audit-key")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = runtime.0.try_wait().unwrap() {
            let mut stderr = String::new();
            runtime
                .0
                .stderr
                .take()
                .unwrap()
                .take(65536)
                .read_to_string(&mut stderr)
                .unwrap();
            assert!(status.success(), "{status}: {stderr}");
            break;
        }
        assert!(Instant::now() < deadline, "DM Bot did not exit");
        std::thread::sleep(Duration::from_millis(5));
    }
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /api/v10/channels/20 HTTP/1.1"));
    assert!(requests[1].starts_with("POST /api/v10/channels/20/messages HTTP/1.1"));
    assert!(requests[1].contains(r#""content":"pong""#));
    let audit = std::fs::read_to_string(directory.0.join("dfor77-audit.jsonl")).unwrap();
    assert!(audit.contains(r#""action":"D7SND""#));
    assert!(!audit.contains("test-token"));
    assert!(!audit.contains("test-audit-key"));
}
