use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex, MutexGuard};
use std::thread;

use dfor77::constants::{D7FOR, D7NFD, D7STA};
use dfor77::{
    percent_encode, AuditContentPolicy, AuditRecord, CommitState, DforRuntime, Field,
    GatewayAction, GatewayTransport, HttpRestBackend, Request, RestPlanner,
    TungsteniteGatewayTransport, WsGatewayTransport, D7OK,
};

static INTEGRATION_TEST_LOCK: Mutex<()> = Mutex::new(());

fn integration_test_lock() -> MutexGuard<'static, ()> {
    INTEGRATION_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn prime_send_permissions(runtime: &mut DforRuntime) {
    let now_ms = current_test_ms();
    runtime
        .ingest_gateway_json(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
            now_ms,
        )
        .unwrap();
    runtime
        .ingest_gateway_json(
            r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"100000000000000001","owner_id":"100000000000000009","channels":[{"id":"100000000000000001","type":0}],"roles":[{"id":"100000000000000001","permissions":"0","position":0}],"members":[{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}]}}"#,
            now_ms,
        )
        .unwrap();
}

fn current_test_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn fake_discord_token() -> String {
    [
        "abcdefghijklmnopqrstuvwx",
        "abcdef",
        "abcdefghijklmnopqrstuvwxyz1234567890",
    ]
    .join(".")
}

struct FortranFixture {
    directory: std::path::PathBuf,
    process: Option<std::process::Child>,
}

impl FortranFixture {
    fn new(label: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("dfor77-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        Self {
            directory,
            process: None,
        }
    }

    fn compile(&self, source: &str, extension: bool) -> std::path::PathBuf {
        assert!(source.lines().all(|line| line.len() <= 72));
        let program = self.directory.join("bot.f");
        let binary = self.directory.join("bot");
        std::fs::write(&program, source).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut command = Command::new("gfortran");
        command
            .args([
                "-std=legacy",
                "-ffixed-line-length-72",
                "-fcheck=all",
                "-O2",
            ])
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
        binary
    }

    fn wait(&mut self) -> String {
        let process = self.process.as_mut().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = process.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "FORTRAN Bot timed out"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        };
        let mut stderr = String::new();
        process
            .stderr
            .take()
            .unwrap()
            .take(65536)
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(status.success(), "{status}: {stderr}");
        stderr
    }
}

impl Drop for FortranFixture {
    fn drop(&mut self) {
        if let Some(process) = &mut self.process {
            let _ = process.kill();
            let _ = process.wait();
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn rest_channel_proofs_revalidate_delayed_changes_before_send_over_http() {
    let _guard = integration_test_lock();
    for thread in [false, true] {
        let channel = |allowed: bool| {
            if thread {
                format!(
                    r#"{{"id":"20","guild_id":"10","parent_id":"30","type":11,"permission_overwrites":[],"thread_metadata":{{"archived":{},"locked":false}}}}"#,
                    !allowed
                )
            } else {
                format!(
                    r#"{{"id":"20","guild_id":"10","type":0,"permission_overwrites":{}}}"#,
                    if allowed {
                        "[]"
                    } else {
                        r#"[{"id":"10","type":0,"allow":"0","deny":"1024"}]"#
                    }
                )
            }
        };
        for kind in 0..5 {
            if kind == 3 && !thread {
                continue;
            }
            let fresh = channel(true);
            let event = match kind {
                0 | 1 => format!(
                    r#"{{"op":0,"s":3,"t":"{}","d":{fresh}}}"#,
                    match (thread, kind) {
                        (false, 0) => "CHANNEL_UPDATE",
                        (false, _) => "CHANNEL_CREATE",
                        (true, 0) => "THREAD_UPDATE",
                        _ => "THREAD_CREATE",
                    }
                ),
                2 => format!(
                    r#"{{"op":0,"s":3,"t":"{}","d":{fresh}}}"#,
                    if thread {
                        "THREAD_DELETE"
                    } else {
                        "CHANNEL_DELETE"
                    }
                ),
                3 => format!(
                    r#"{{"op":0,"s":3,"t":"THREAD_LIST_SYNC","d":{{"guild_id":"10","channel_ids":["30"],"threads":[{fresh}],"members":[]}}}}"#
                ),
                _ => format!(
                    r#"{{"op":0,"s":3,"t":"GUILD_CREATE","d":{{"id":"10","owner_id":"70","channels":[{{"id":"30","type":0,"permission_overwrites":[]}}{}],"threads":[{}]}}}}"#,
                    if thread {
                        String::new()
                    } else {
                        format!(",{fresh}")
                    },
                    if thread { fresh } else { String::new() }
                ),
            };
            for current in 0..4 {
                let response = match current {
                    0 | 1 => (200, channel(current == 1)),
                    2 => (404, r#"{"code":10003,"message":"Unknown Channel"}"#.into()),
                    _ => (403, r#"{"code":50001,"message":"Missing Access"}"#.into()),
                };
                let Some((base_url, received, server)) =
                    serve_sequence(vec![(200, channel(false)), response])
                else {
                    return;
                };
                let backend = HttpRestBackend::new(
                    &base_url,
                    "secret-token",
                    "dfor77-channel-order-test",
                    1000,
                )
                .unwrap();
                let mut runtime = DforRuntime::new(false, false);
                runtime.use_http_rest_backend(backend);
                let now = current_test_ms();
                runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, now).unwrap();
                runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"274877910016","position":0}],"members":[{"user":{"id":"99"},"roles":[],"communication_disabled_until":null}],"channels":[{"id":"30","type":0,"permission_overwrites":[]}]}}"#, now).unwrap();
                assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
                assert_eq!(
                    runtime
                        .handle(request(
                            2,
                            "RST",
                            vec![
                                Field::raw("method", "GET"),
                                Field::raw("path", "/api/v10/channels/20")
                            ]
                        ))
                        .status,
                    D7OK
                );
                runtime
                    .ingest_gateway_json(&event, current_test_ms())
                    .unwrap();
                let decision = runtime.handle(request(
                    3,
                    "CSN",
                    vec![Field::raw("guild_id", "10"), Field::raw("channel_id", "20")],
                ));
                assert_eq!(
                    decision.status,
                    [D7OK, D7OK, D7NFD, D7FOR][current],
                    "{event}: {current}"
                );
                assert_eq!(
                    decision.field("can"),
                    Some(if current == 1 { "1" } else { "0" }),
                    "{event}: {current}"
                );
                for _ in 0..2 {
                    assert!(received
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .unwrap()
                        .starts_with("GET /api/v10/channels/20 HTTP/1.1\r\n"));
                }
                server.join().unwrap();
            }
        }
    }
}

#[test]
fn rest_guild_proofs_revalidate_delayed_ownership_and_availability_over_http() {
    let _guard = integration_test_lock();
    let first = r#"{"id":"10","owner_id":"70"}"#;
    let events = [
        r#"{"op":0,"s":3,"t":"GUILD_UPDATE","d":{"id":"10","owner_id":"40"}}"#,
        r#"{"op":0,"s":3,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"40"}}"#,
        r#"{"op":0,"s":3,"t":"GUILD_DELETE","d":{"id":"10","unavailable":true}}"#,
        r#"{"op":0,"s":3,"t":"GUILD_DELETE","d":{"id":"10"}}"#,
    ];
    for event in events {
        for (status, current, expected_status, expected_admin) in [
            (200, first, D7OK, Some("0")),
            (200, r#"{"id":"10","owner_id":"40"}"#, D7OK, Some("1")),
            (
                404,
                r#"{"code":10004,"message":"Unknown Guild"}"#,
                D7NFD,
                None,
            ),
            (
                403,
                r#"{"code":50001,"message":"Missing Access"}"#,
                D7FOR,
                None,
            ),
            (
                200,
                r#"{"id":"10","owner_id":"70","unavailable":true}"#,
                D7STA,
                None,
            ),
        ] {
            let Some((base_url, received, server)) = serve_sequence(vec![
                (200, first.to_string()),
                (status, current.to_string()),
            ]) else {
                return;
            };
            let backend =
                HttpRestBackend::new(&base_url, "secret-token", "dfor77-guild-order-test", 1000)
                    .unwrap();
            let mut runtime = DforRuntime::new(false, false);
            runtime.use_http_rest_backend(backend);
            let now = current_test_ms();
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0}],"members":[{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}]}}"#, now).unwrap();
            runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"0"}}}"#, now).unwrap();
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            runtime.handle(request(2, "NXT", vec![]));
            runtime.handle(request(3, "NXT", vec![]));
            assert_eq!(
                runtime
                    .handle(request(
                        4,
                        "RST",
                        vec![
                            Field::raw("method", "GET"),
                            Field::raw("path", "/api/v10/guilds/10")
                        ]
                    ))
                    .status,
                D7OK
            );
            runtime
                .ingest_gateway_json(event, current_test_ms())
                .unwrap();
            let decision = runtime.handle(request(5, "CAD", vec![]));
            assert_eq!(decision.status, expected_status, "{event}: {current}");
            assert_eq!(
                decision.field("is_admin"),
                expected_admin,
                "{event}: {current}"
            );
            for _ in 0..2 {
                assert!(received
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .starts_with("GET /api/v10/guilds/10 HTTP/1.1\r\n"));
            }
            server.join().unwrap();
        }
    }
}

#[test]
fn rest_role_proofs_revalidate_delayed_changes_and_removals_over_http() {
    let _guard = integration_test_lock();
    let roles = |admin| {
        format!(
            r#"[{{"id":"10","permissions":"0","position":0}},{{"id":"50","permissions":"{}","position":1}}]"#,
            if admin { "8" } else { "0" }
        )
    };
    let events = [
        r#"{"op":0,"s":3,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":"8","position":1}}}"#,
        r#"{"op":0,"s":3,"t":"GUILD_ROLE_CREATE","d":{"guild_id":"10","role":{"id":"50","permissions":"8","position":1}}}"#,
        r#"{"op":0,"s":3,"t":"GUILD_ROLE_DELETE","d":{"guild_id":"10","role_id":"50"}}"#,
        r#"{"op":0,"s":3,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}]}}"#,
    ];
    for (index, event) in events.into_iter().enumerate() {
        let remove = index == 2;
        for actual_admin in [false, true] {
            let mut responses = vec![(
                200,
                if remove {
                    r#"[{"id":"10","permissions":"0","position":0}]"#.into()
                } else {
                    roles(false)
                },
            )];
            let mut paths = vec!["/api/v10/guilds/10/roles"];
            if remove {
                responses.push((200, format!(r#"{{"user":{{"id":"40"}},"roles":{},"communication_disabled_until":null}}"#, if actual_admin { r#"["50"]"# } else { "[]" })));
                paths.push("/api/v10/guilds/10/members/40");
            }
            responses.push((
                200,
                if remove && !actual_admin {
                    r#"[{"id":"10","permissions":"0","position":0}]"#.into()
                } else {
                    roles(actual_admin)
                },
            ));
            paths.push("/api/v10/guilds/10/roles");
            let Some((base_url, received, server)) = serve_sequence(responses) else {
                return;
            };
            let backend =
                HttpRestBackend::new(&base_url, "secret-token", "dfor77-role-order-test", 1000)
                    .unwrap();
            let mut runtime = DforRuntime::new(false, false);
            runtime.use_http_rest_backend(backend);
            let now = current_test_ms();
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}],"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}]}}"#, now).unwrap();
            runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"0"}}}"#, now).unwrap();
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            runtime.handle(request(2, "NXT", vec![]));
            runtime.handle(request(3, "NXT", vec![]));
            assert_eq!(
                runtime
                    .handle(request(
                        4,
                        "RST",
                        vec![
                            Field::raw("method", "GET"),
                            Field::raw("path", "/api/v10/guilds/10/roles"),
                        ]
                    ))
                    .status,
                D7OK
            );
            runtime
                .ingest_gateway_json(event, current_test_ms())
                .unwrap();
            let decision = runtime.handle(request(5, "CAD", vec![]));
            assert_eq!(decision.status, D7OK, "{event}");
            assert_eq!(
                decision.field("is_admin"),
                Some(if actual_admin { "1" } else { "0" }),
                "{event}"
            );
            for path in paths {
                assert!(received
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .starts_with(&format!("GET {path} HTTP/1.1\r\n")));
            }
            server.join().unwrap();
        }
    }
}

#[test]
fn rest_member_proofs_survive_delayed_gateway_events_and_revalidate_over_http() {
    let _guard = integration_test_lock();
    let first = r#"{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}"#;
    let events = [
        r#"{"op":0,"s":3,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"40"},"roles":["50"]}}"#,
        r#"{"op":0,"s":3,"t":"GUILD_MEMBER_REMOVE","d":{"guild_id":"10","user":{"id":"40"}}}"#,
        r#"{"op":0,"s":3,"t":"GUILD_MEMBER_ADD","d":{"guild_id":"10","user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}}"#,
        r#"{"op":0,"s":3,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}]}}"#,
    ];
    for event in events {
        for actual_admin in [Some(false), Some(true), None] {
            let (status, current) = match actual_admin {
                Some(false) => (200, first),
                Some(true) => (
                    200,
                    r#"{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}"#,
                ),
                None => (404, r#"{"code":10007,"message":"Unknown Member"}"#),
            };
            let Some((base_url, received, server)) = serve_sequence(vec![
                (200, first.to_string()),
                (status, current.to_string()),
            ]) else {
                return;
            };
            let backend =
                HttpRestBackend::new(&base_url, "secret-token", "dfor77-member-order-test", 1000)
                    .unwrap();
            let mut runtime = DforRuntime::new(false, false);
            runtime.use_http_rest_backend(backend);
            let now = current_test_ms();
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}],"members":[{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}]}}"#, now).unwrap();
            runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"0"}}}"#, now).unwrap();
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            runtime.handle(request(2, "NXT", vec![]));
            runtime.handle(request(3, "NXT", vec![]));
            assert_eq!(
                runtime
                    .handle(request(
                        4,
                        "RST",
                        vec![
                            Field::raw("method", "GET"),
                            Field::raw("path", "/api/v10/guilds/10/members/40"),
                        ]
                    ))
                    .status,
                D7OK
            );
            runtime
                .ingest_gateway_json(event, current_test_ms())
                .unwrap();
            let decision = runtime.handle(request(5, "CAD", vec![]));
            if let Some(actual_admin) = actual_admin {
                assert_eq!(decision.status, D7OK, "{event}");
                assert_eq!(
                    decision.field("is_admin"),
                    Some(if actual_admin { "1" } else { "0" }),
                    "{event}"
                );
            } else {
                assert_eq!(decision.status, dfor77::constants::D7NFD, "{event}");
            }
            for _ in 0..2 {
                assert!(received
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .starts_with("GET /api/v10/guilds/10/members/40 HTTP/1.1\r\n"));
            }
            server.join().unwrap();
        }
    }
}

#[test]
fn message_edit_http_responses_and_delayed_gateway_updates_keep_searches_current() {
    let _guard = integration_test_lock();
    let edited = r#"{"id":"30","channel_id":"20","author":{"id":"40"},"content":"REST edit","type":0,"attachments":[],"edited_timestamp":"1970-01-01T00:00:02Z"}"#;
    let old_reply = r#"{"id":"30","channel_id":"20","author":{"id":"40"},"content":"older REST","type":0,"attachments":[{"filename":"old.png"},{"filename":"old2.png"}],"edited_timestamp":"1970-01-01T00:00:01Z"}"#;
    let recovered = r#"{"id":"30","channel_id":"20","author":{"id":"40"},"content":"recovered","type":0,"attachments":[{"filename":"recovered.png"}],"edited_timestamp":"1970-01-01T00:00:04Z"}"#;
    let Some((base_url, received, server)) = serve_sequence(vec![
        (200, edited.into()),
        (200, old_reply.into()),
        (200, recovered.into()),
    ]) else {
        return;
    };
    let backend =
        HttpRestBackend::new(&base_url, "local-revision-token", "revision-test", 1000).unwrap();
    let mut runtime = DforRuntime::new(false, false);
    runtime.use_http_rest_backend(backend);
    let now = current_test_ms();
    runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","author":{"id":"40"},"content":"created","type":0,"attachments":[{"filename":"one.png"},{"filename":"two.png"}],"edited_timestamp":null}}"#, now).unwrap();
    assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
    let search = || {
        request(
            2,
            "MSF",
            vec![
                Field::raw("guild_id", "10"),
                Field::raw("user_id", "40"),
                Field::raw("base_message_id", "40"),
                Field::raw("before_seconds", "60"),
                Field::raw("after_seconds", "60"),
                Field::raw("min_image_count", "1"),
                Field::raw("limit", "10"),
            ],
        )
    };
    assert_eq!(runtime.handle(search()).field("count"), Some("1"));
    let edit = || {
        request(
            3,
            "RST",
            vec![
                Field::raw("method", "PATCH"),
                Field::raw("path", "/api/v10/channels/20/messages/30"),
                Field::percent("payload", r#"{"attachments":[]}"#),
            ],
        )
    };
    assert_eq!(runtime.handle(edit()).status, D7OK);
    assert_eq!(
        runtime
            .handle(request(4, "MSI", vec![Field::raw("index", "1")]))
            .field("image_count"),
        Some("2")
    );
    runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_UPDATE","d":{"channel_id":"20","id":"30","content":"old gateway edit","attachments":[{"filename":"old.png"}],"edited_timestamp":"1970-01-01T00:00:01Z"}}"#, now).unwrap();
    assert_eq!(runtime.handle(search()).field("count"), Some("0"));
    runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"MESSAGE_UPDATE","d":{"channel_id":"20","id":"30","content":"new gateway edit","attachments":[{"filename":"latest.png"}],"edited_timestamp":"1970-01-01T00:00:03Z"}}"#, now).unwrap();
    assert_eq!(runtime.handle(edit()).status, D7OK);
    assert_eq!(
        runtime
            .handle(request(5, "RBD", vec![Field::raw("buf_len", "60000")]))
            .field("body"),
        Some(old_reply)
    );
    assert_eq!(runtime.handle(search()).field("count"), Some("1"));
    assert_eq!(
        runtime
            .handle(request(6, "MSI", vec![Field::raw("index", "1")]))
            .field("image_count"),
        Some("1")
    );
    runtime.ingest_gateway_json(r#"{"op":0,"s":4,"t":"MESSAGE_UPDATE","d":{"channel_id":"20","id":"30","content":"uncertain"}}"#, now).unwrap();
    assert_eq!(runtime.handle(search()).field("count"), Some("0"));
    assert_eq!(
        runtime
            .handle(request(
                7,
                "RST",
                vec![
                    Field::raw("method", "GET"),
                    Field::raw("path", "/api/v10/channels/20/messages/30")
                ]
            ))
            .status,
        D7OK
    );
    assert_eq!(runtime.handle(search()).field("count"), Some("1"));
    for method in ["PATCH", "PATCH", "GET"] {
        assert!(received
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .starts_with(&format!(
                "{method} /api/v10/channels/20/messages/30 HTTP/1.1"
            )));
    }
    server.join().unwrap();
}

#[test]
fn standard_and_extension_bots_read_scoped_rate_limits_and_legacy_notifications() {
    let _guard = integration_test_lock();
    let program = r#"      CHARACTER*256 TXT
      CHARACTER*32 GID
      INTEGER S, EV, OP, RETRY, L, TR
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
   10 CALL D7NXT(EV,S)
      IF (S.NE.D7OK) STOP 2
      IF (EV.NE.D7GRL) GOTO 10
      CALL D7GOP(OP,S)
      IF (S.NE.D7OK.OR.OP.NE.8) STOP 3
      CALL D7GRT(RETRY,S)
      IF (S.NE.D7OK.OR.RETRY.NE.1501) STOP 4
      CALL D7GID(GID,32,L,TR,S)
      IF (S.NE.D7OK.OR.GID.NE.'10') STOP 5
      CALL D7TXT(TXT,256,L,TR,S)
      IF (S.NE.D7OK.OR.TR.NE.0) STOP 6
      IF (INDEX(TXT,'"nonce":"fixture-rate-one"').EQ.0) STOP 7
      CALL D7NXT(EV,S)
      IF (S.NE.D7OK.OR.EV.NE.D7GRL) STOP 8
      CALL D7GID(GID,32,L,TR,S)
      IF (S.NE.D7OK.OR.GID.NE.'11') STOP 9
      CALL D7GRT(RETRY,S)
      IF (S.NE.D7OK.OR.RETRY.NE.2250) STOP 10
      CALL D7NXT(EV,S)
      IF (S.NE.D7OK.OR.EV.NE.D7GRL) STOP 11
      CALL D7GOP(OP,S)
      IF (S.NE.D7OK.OR.OP.NE.255) STOP 12
      CALL D7GRT(RETRY,S)
      IF (S.NE.D7OK.OR.RETRY.NE.50) STOP 13
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 14
      END
"#;
    let frames = [
        r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"fixture","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#,
        r#"{"op":0,"s":2,"t":"RATE_LIMITED","d":{"opcode":8,"retry_after":1.5001,"meta":{"guild_id":"10","nonce":"fixture-rate-one"}}}"#,
        r#"{"op":0,"s":3,"t":"RATE_LIMITED","d":{"opcode":8,"retry_after":2.25,"meta":{"guild_id":"11","nonce":"fixture-rate-two"}}}"#,
        r#"{"op":0,"s":2,"t":"RATE_LIMITED","d":{"opcode":8,"retry_after":1.5001,"meta":{"guild_id":"10","nonce":"fixture-rate-one"}}}"#,
        r#"{"op":0,"s":null,"t":"GATEWAY_RATE_LIMITED","d":{"opcode":255,"retry_ms":50}}"#,
    ];
    for extension in [false, true] {
        let mut fixture = FortranFixture::new("gateway-rate-limits");
        let declarations = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            "      INTEGER D7OK, D7GRL\n      PARAMETER (D7OK=0,D7GRL=104)\n"
        };
        let binary = fixture.compile(
            &format!("      PROGRAM T\n{declarations}{program}"),
            extension,
        );
        let gateway = fixture.directory.join("gateway.jsonl");
        std::fs::write(&gateway, frames.join("\n")).unwrap();
        fixture.process = Some(
            Command::new(env!("CARGO_BIN_EXE_dfor77"))
                .current_dir(&fixture.directory)
                .arg("--mock-gateway")
                .arg(gateway)
                .arg("--audit-log")
                .arg(fixture.directory.join("audit.jsonl"))
                .arg("--bot")
                .arg(binary)
                .env("DISCORD_BOT_TOKEN", "local-rate-limits-token")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = fixture.wait();
        assert!(!stderr.contains("fixture-rate-one"));
        assert!(!stderr.contains("fixture-rate-two"));
        assert!(!stderr.contains("local-rate-limits-token"));
    }
}

#[test]
fn standard_and_extension_bots_read_member_chunks_and_use_refreshed_permissions() {
    let _guard = integration_test_lock();
    let program = r#"      CHARACTER*256 TXT
      CHARACTER*32 GID
      INTEGER S, EV, VAL, L, TR
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
   10 CALL D7NXT(EV,S)
      IF (S.NE.D7OK) STOP 2
      IF (EV.NE.D7MSG) GOTO 10
      CALL D7CAD(VAL,S)
      IF (S.NE.D7OK.OR.VAL.NE.0) STOP 3
      CALL D7NXT(EV,S)
      IF (S.NE.D7OK.OR.EV.NE.D7GEV) STOP 4
      CALL D7GID(GID,32,L,TR,S)
      IF (S.NE.D7OK.OR.GID.NE.'10') STOP 5
      CALL D7TXT(TXT,256,L,TR,S)
      IF (S.NE.D7OK.OR.TR.NE.0) STOP 6
      IF (INDEX(TXT,'"chunk_index":0').EQ.0) STOP 7
      IF (INDEX(TXT,'"chunk_count":2').EQ.0) STOP 8
      IF (INDEX(TXT,'"member_ids":["40"]').EQ.0) STOP 9
      IF (INDEX(TXT,'"nonce":"chunk-request"').EQ.0) STOP 10
      CALL D7NXT(EV,S)
      IF (S.NE.D7OK.OR.EV.NE.D7GEV) STOP 11
      CALL D7TXT(TXT,8,L,TR,S)
      IF (S.NE.D7TRN.OR.TR.NE.1.OR.L.LE.8) STOP 12
      CALL D7TXT(TXT,256,L,TR,S)
      IF (S.NE.D7OK.OR.TR.NE.0) STOP 13
      IF (INDEX(TXT,'"chunk_index":1').EQ.0) STOP 14
      IF (INDEX(TXT,'"member_ids":["41"]').EQ.0) STOP 15
      CALL D7ADM('10',2,'41',2,VAL,S)
      IF (S.NE.D7OK.OR.VAL.NE.1) STOP 16
      CALL D7ADM('10',2,'40',2,VAL,S)
      IF (S.NE.D7OK.OR.VAL.NE.0) STOP 17
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 18
      END
"#;
    let frames = [
        r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}],"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null},{"user":{"id":"41"},"roles":[],"communication_disabled_until":null}]}}"#,
        r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"8"}}}"#,
        r#"{"op":0,"s":3,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","chunk_index":0,"chunk_count":2,"nonce":"chunk-request","members":[{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}]}}"#,
        r#"{"op":0,"s":4,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","chunk_index":1,"chunk_count":2,"nonce":"chunk-request","members":[{"user":{"id":"41"},"roles":["50"],"communication_disabled_until":null}]}}"#,
    ];
    for extension in [false, true] {
        let mut fixture = FortranFixture::new("member-chunks");
        let declarations = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            "      INTEGER D7OK, D7MSG, D7GEV, D7TRN\n      PARAMETER (D7OK=0,D7MSG=101,D7GEV=106,D7TRN=10)\n"
        };
        let binary = fixture.compile(
            &format!("      PROGRAM T\n{declarations}{program}"),
            extension,
        );
        let gateway = fixture.directory.join("gateway.jsonl");
        std::fs::write(&gateway, frames.join("\n")).unwrap();
        fixture.process = Some(
            Command::new(env!("CARGO_BIN_EXE_dfor77"))
                .current_dir(&fixture.directory)
                .arg("--mock-gateway")
                .arg(gateway)
                .arg("--audit-log")
                .arg(fixture.directory.join("audit.jsonl"))
                .arg("--bot")
                .arg(binary)
                .env("DISCORD_BOT_TOKEN", "local-member-chunks-token")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = fixture.wait();
        assert!(!stderr.contains("chunk-request"));
        assert!(!stderr.contains("local-member-chunks-token"));
    }
}

#[test]
fn standard_and_extension_gateway_calls_protect_session_control_and_keep_application_events() {
    let _guard = integration_test_lock();
    let program = r#"      CHARACTER*256 PAY
      INTEGER S, I, OPS(7)
      DATA OPS /0,2,6,7,9,10,11/
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
      DO 10 I=1,7
        WRITE(PAY,'(A,I2,A)') '{"op":',OPS(I),
     1     ',"d":{"token":"private-control-fixture"}}'
        CALL D7GWS(PAY,LEN_TRIM(PAY),S)
        IF (S.NE.D7ARG) STOP 2
   10 CONTINUE
      PAY='{"op":3,"d":null}'
      CALL D7GWS(PAY,LEN_TRIM(PAY),S)
      IF (S.NE.D7ARG) STOP 20
      PAY='{"since":null,"activities":[],'//
     1    '"status":"online","afk":0}'
      CALL D7PRS(PAY,LEN_TRIM(PAY),S)
      IF (S.NE.D7ARG) STOP 21
      PAY='{"op":4,"d":{"guild_id":"10","channel_id":null,'//
     1    '"self_mute":1,"self_deaf":false}}'
      CALL D7GWS(PAY,LEN_TRIM(PAY),S)
      IF (S.NE.D7ARG) STOP 22
      PAY='{"op":8,"d":{"guild_id":"10","query":""}}'
      CALL D7GWS(PAY,LEN_TRIM(PAY),S)
      IF (S.NE.D7ARG) STOP 23
      PAY='{"op":8,"d":{"guild_id":"10","user_ids":["40"]}}'
      CALL D7GWS(PAY,LEN_TRIM(PAY),S)
      IF (S.NE.D7OK) STOP 3
      PAY='{"op":31,"d":{"guild_ids":["10"]}}'
      CALL D7GWS(PAY,LEN_TRIM(PAY),S)
      IF (S.NE.D7OK) STOP 4
      PAY='{"op":43,"d":{"guild_id":"10","fields":["status"]}}'
      CALL D7GWS(PAY,LEN_TRIM(PAY),S)
      IF (S.NE.D7OK) STOP 5
      PAY='{"op":255,"d":null}'
      CALL D7GWS(PAY,LEN_TRIM(PAY),S)
      IF (S.NE.D7OK) STOP 6
      PAY='{"op":1,"d":null}'
      CALL D7GWS(PAY,LEN_TRIM(PAY),S)
      IF (S.NE.D7OK) STOP 7
      PAY='{"since":null,"activities":[],'//
     1    '"status":"online","afk":false}'
      CALL D7PRS(PAY,LEN_TRIM(PAY),S)
      IF (S.NE.D7OK) STOP 8
      CALL D7VST('10',2,'20',2,0,0,S)
      IF (S.NE.D7OK) STOP 9
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 10
      END
"#;
    for extension in [false, true] {
        let mut fixture = FortranFixture::new("gateway-control");
        let declarations = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            "      INTEGER D7OK, D7ARG\n      PARAMETER (D7OK=0,D7ARG=30)\n"
        };
        let binary = fixture.compile(
            &format!("      PROGRAM T\n{declarations}{program}"),
            extension,
        );
        let gateway = fixture.directory.join("gateway.jsonl");
        std::fs::write(&gateway, r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"fixture","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#).unwrap();
        let audit = fixture.directory.join("audit.jsonl");
        fixture.process = Some(
            Command::new(env!("CARGO_BIN_EXE_dfor77"))
                .current_dir(&fixture.directory)
                .arg("--mock-gateway")
                .arg(gateway)
                .arg("--audit-log")
                .arg(&audit)
                .arg("--bot")
                .arg(binary)
                .env("DISCORD_BOT_TOKEN", "local-gateway-control-token")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = fixture.wait();
        let audit = std::fs::read_to_string(audit).unwrap();
        let statuses = audit
            .lines()
            .filter_map(|line| {
                let value = dfor77::parse_json(line).unwrap();
                (value
                    .get("commit_state_source")
                    .and_then(dfor77::JsonValue::as_str)
                    == Some("gateway_queue"))
                .then(|| value.get("status").unwrap().as_i64().unwrap())
            })
            .collect::<Vec<_>>();
        assert_eq!(statuses, [vec![30; 11], vec![0; 7]].concat());
        for secret in ["private-control-fixture", "local-gateway-control-token"] {
            assert!(!audit.contains(secret));
            assert!(!stderr.contains(secret));
        }
    }
}

#[test]
fn standard_and_extension_raw_paths_reject_ambiguous_routes_and_preserve_emoji_queries() {
    let _guard = integration_test_lock();
    let program = r#"      CHARACTER*256 PATH, BODY
      INTEGER S, L, TR, I
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
      DO 10 I=1,4
        IF (I.EQ.1) PATH='/api/v10/../v9/gateway'
        IF (I.EQ.2) PATH='/api/v10/%2e%2e/v9/gateway'
        IF (I.EQ.3) PATH='/api/v10/channels/20%2fmessages/30'
        IF (I.EQ.4) PATH='/api/v10/%252e%252e/v9/gateway'
        CALL D7RST(1,PATH,LEN_TRIM(PATH),' ',0,' ',0,S)
        IF (S.NE.D7ARG) STOP 2
        CALL D7RBD(BODY,256,L,TR,S)
        IF (S.NE.D7STA.OR.L.NE.0.OR.TR.NE.0) STOP 3
   10 CONTINUE
      PATH='/api/v10/%63hannels/%32%30/messages/30/reactions/'//
     1     '%23%ef%b8%8f%e2%83%a3?type=0&limit=2&after=31'
      CALL D7RST(1,PATH,LEN_TRIM(PATH),' ',0,' ',0,S)
      IF (S.NE.D7OK) STOP 4
      CALL D7RBD(BODY,256,L,TR,S)
      IF (S.NE.D7OK.OR.TR.NE.0) STOP 5
      IF (BODY.NE.'[{"id":"40"}]') STOP 6
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 7
      END
"#;
    for extension in [false, true] {
        let mut fixture = FortranFixture::new("raw-paths");
        let declarations = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            "      INTEGER D7OK, D7ARG, D7STA\n      PARAMETER (D7OK=0,D7ARG=30,D7STA=31)\n"
        };
        let binary = fixture.compile(
            &format!("      PROGRAM T\n{declarations}{program}"),
            extension,
        );
        let Some((base_url, received, server)) = serve_once(200, r#"[{"id":"40"}]"#) else {
            return;
        };
        fixture.process = Some(
            Command::new(env!("CARGO_BIN_EXE_dfor77"))
                .current_dir(&fixture.directory)
                .arg("--mock-rest")
                .arg(base_url)
                .arg("--audit-log")
                .arg(fixture.directory.join("audit.jsonl"))
                .arg("--bot")
                .arg(binary)
                .env("DISCORD_BOT_TOKEN", "local-raw-path-token")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = fixture.wait();
        let request = received
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(request.starts_with(concat!(
            "GET /api/v10/%63hannels/%32%30/messages/30/reactions/",
            "%23%ef%b8%8f%e2%83%a3?type=0&limit=2&after=31 HTTP/1.1"
        )));
        server.join().unwrap();
        assert_eq!(received.try_iter().count(), 0);
        let audit = std::fs::read_to_string(fixture.directory.join("audit.jsonl")).unwrap();
        assert!(!audit.contains("local-raw-path-token"));
        assert!(!stderr.contains("local-raw-path-token"));
    }
}

#[test]
fn standard_fortran_bot_reads_a_created_resource_body_without_extension() {
    let _guard = integration_test_lock();
    let mut fixture = FortranFixture::new("created-body");
    let source = r#"      PROGRAM T
      CHARACTER*256 BODY
      CHARACTER*32 PATH, PAY
      INTEGER S, L, TR
      PATH = '/api/v10/users/@me/channels'
      PAY = '{"recipient_id":"40"}'
      CALL D7INI(S)
      IF (S.NE.0) STOP 1
      CALL D7RST(2,PATH,27,PAY,21,' ',0,S)
      IF (S.NE.0) STOP 2
      CALL D7RBD(BODY,8,L,TR,S)
      IF (S.NE.10.OR.TR.NE.1) STOP 3
      IF (L.NE.LEN('{"id":"20","type":1}')) STOP 4
      CALL D7RBD(BODY,256,L,TR,S)
      IF (S.NE.0.OR.TR.NE.0) STOP 5
      IF (BODY.NE.'{"id":"20","type":1}') STOP 6
      CALL D7END(S)
      IF (S.NE.0) STOP 7
      END
"#;
    let binary = fixture.compile(source, false);
    let Some((base_url, received, server)) = serve_once(201, r#"{"id":"20","type":1}"#) else {
        return;
    };
    let audit = fixture.directory.join("audit.jsonl");
    fixture.process = Some(
        Command::new(env!("CARGO_BIN_EXE_dfor77"))
            .current_dir(&fixture.directory)
            .arg("--mock-rest")
            .arg(base_url)
            .arg("--audit-log")
            .arg(&audit)
            .arg("--bot")
            .arg(binary)
            .env("DISCORD_BOT_TOKEN", "local-created-resource-token")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = fixture.wait();
    let request = received
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    assert!(request.starts_with("POST /api/v10/users/@me/channels HTTP/1.1"));
    assert!(request.contains(r#"{"recipient_id":"40"}"#));
    server.join().unwrap();
    let audit = std::fs::read_to_string(audit).unwrap();
    assert!(!audit.contains("recipient_id"));
    assert!(!audit.contains("local-created-resource-token"));
    assert!(!stderr.contains("local-created-resource-token"));
}

#[test]
fn standard_and_extension_edits_refresh_searches_without_reindexing_previous_results() {
    let _guard = integration_test_lock();
    let program = r#"      CHARACTER*32 CH, MID
      CHARACTER*256 BODY
      INTEGER S, N, TR, L, CL, ML, CT, MT, IMG, DELTA
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
      CALL D7MSF('10',2,'40',2,'40',2,60,60,1,10,N,TR,S)
      IF (S.NE.D7OK.OR.N.NE.1) STOP 2
      CALL D7EMS('20',2,'30',2,'{"attachments":[]}',
     1           18,' ',0,S)
      IF (S.NE.D7OK) STOP 3
      CALL D7RBD(BODY,256,L,TR,S)
      IF (S.NE.D7OK.OR.TR.NE.0) STOP 4
      IF (INDEX(BODY,'server edit').EQ.0) STOP 5
      CALL D7MSI(1,CH,32,CL,CT,MID,32,ML,MT,IMG,DELTA,S)
      IF (S.NE.D7OK.OR.IMG.NE.2.OR.MID.NE.'30') STOP 6
      CALL D7MSF('10',2,'40',2,'40',2,60,60,1,10,N,TR,S)
      IF (S.NE.D7OK.OR.N.NE.0) STOP 7
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 8
      END
"#;
    let frame = r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"content":"before edit","author":{"id":"40"},"attachments":[{"filename":"one.png"},{"filename":"two.webp"}],"edited_timestamp":null}}"#;
    let reply = r#"{"id":"30","channel_id":"20","author":{"id":"40"},"content":"server edit","type":0,"attachments":[],"edited_timestamp":"1970-01-01T00:00:02Z"}"#;
    for extension in [false, true] {
        let mut fixture = FortranFixture::new("message-edit");
        let declarations = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            "      INTEGER D7OK\n      PARAMETER (D7OK=0)\n"
        };
        let binary = fixture.compile(
            &format!("      PROGRAM T\n{declarations}{program}"),
            extension,
        );
        let gateway = fixture.directory.join("gateway.jsonl");
        std::fs::write(&gateway, frame).unwrap();
        let Some((base_url, received, server)) = serve_once(200, reply) else {
            return;
        };
        fixture.process = Some(
            Command::new(env!("CARGO_BIN_EXE_dfor77"))
                .current_dir(&fixture.directory)
                .arg("--mock-gateway")
                .arg(&gateway)
                .arg("--mock-rest")
                .arg(base_url)
                .arg("--audit-log")
                .arg(fixture.directory.join("audit.jsonl"))
                .arg("--bot")
                .arg(binary)
                .env("DISCORD_BOT_TOKEN", "local-message-edit-token")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = fixture.wait();
        let request = received
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(request.starts_with("PATCH /api/v10/channels/20/messages/30 HTTP/1.1"));
        assert!(request.ends_with(r#"{"attachments":[]}"#));
        server.join().unwrap();
        assert_eq!(received.try_iter().count(), 0);
        let audit = std::fs::read_to_string(fixture.directory.join("audit.jsonl")).unwrap();
        assert!(!audit.contains("server edit"));
        assert!(!audit.contains("local-message-edit-token"));
        assert!(!stderr.contains("local-message-edit-token"));
    }
}

#[test]
fn standard_and_extension_webhook_edits_and_original_deletes_reconcile_searches() {
    let _guard = integration_test_lock();
    for original in [false, true] {
        let (edit, delete, suffix, webhook) = if original {
            (
                "      CALL D7IFU(3,'50',2,'fixture-token',13,' ',0,\n     1           '{\"attachments\":[]}',18,S)\n",
                "      CALL D7IFU(4,'50',2,'fixture-token',13,' ',0,\n     1           ' ',0,S)\n",
                "@original",
                "51",
            )
        } else {
            (
                "      CALL D7WHM(2,'50',2,'fixture-token',13,'30',2,\n     1           '{\"attachments\":[]}',18,S)\n",
                "      CALL D7WHM(3,'50',2,'fixture-token',13,'30',2,\n     1           ' ',0,S)\n",
                "30",
                "50",
            )
        };
        let program = format!(
            r#"      CHARACTER*32 CH, MID
      CHARACTER*64 WHY
      CHARACTER*512 BODY
      INTEGER S, N, TR, L, CL, ML, CT, MT, IMG, DELTA, CAN
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
      CALL D7MSF('10',2,'40',2,'40',2,60,60,1,10,N,TR,S)
      IF (S.NE.D7OK.OR.N.NE.1) STOP 2
{edit}      IF (S.NE.D7OK) STOP 3
      CALL D7RBD(BODY,512,L,TR,S)
      IF (S.NE.D7OK.OR.INDEX(BODY,'server edit').EQ.0) STOP 4
      CALL D7MSI(1,CH,32,CL,CT,MID,32,ML,MT,IMG,DELTA,S)
      IF (S.NE.D7OK.OR.IMG.NE.2.OR.MID.NE.'30') STOP 5
      CALL D7MSF('10',2,'40',2,'40',2,60,60,1,10,N,TR,S)
      IF (S.NE.D7OK.OR.N.NE.0) STOP 6
{delete}      IF (S.NE.D7OK) STOP 7
      CALL D7CDL('10',2,'20',2,'30',2,CAN,WHY,64,L,TR,S)
      IF (S.NE.D7NFD.OR.CAN.NE.0) STOP 8
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 9
      END
"#
        );
        let frame = format!(
            r#"{{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{{"guild_id":"10","channel_id":"20","id":"30","webhook_id":"{webhook}","application_id":"50","type":0,"content":"before edit","author":{{"id":"40","bot":false}},"attachments":[{{"filename":"one.png"}},{{"filename":"two.webp"}}]}}}}"#
        );
        let reply = format!(
            r#"{{"id":"30","channel_id":"20","webhook_id":"{webhook}","application_id":"50","author":{{"id":"40","bot":false}},"content":"server edit","type":0,"attachments":[]}}"#
        );
        for extension in [false, true] {
            let mut fixture = FortranFixture::new("webhook-cache");
            let declarations = if extension {
                "      INCLUDE 'dfor77.inc'\n"
            } else {
                "      INTEGER D7OK, D7NFD\n      PARAMETER (D7OK=0,D7NFD=41)\n"
            };
            let binary = fixture.compile(
                &format!("      PROGRAM T\n{declarations}{program}"),
                extension,
            );
            let gateway = fixture.directory.join("gateway.jsonl");
            std::fs::write(&gateway, &frame).unwrap();
            let Some((base_url, received, server)) =
                serve_sequence(vec![(200, reply.clone()), (204, String::new())])
            else {
                return;
            };
            fixture.process = Some(
                Command::new(env!("CARGO_BIN_EXE_dfor77"))
                    .current_dir(&fixture.directory)
                    .arg("--mock-gateway")
                    .arg(&gateway)
                    .arg("--mock-rest")
                    .arg(base_url)
                    .arg("--audit-log")
                    .arg(fixture.directory.join("audit.jsonl"))
                    .arg("--bot")
                    .arg(binary)
                    .env("DISCORD_BOT_TOKEN", "local-webhook-cache-token")
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stderr = fixture.wait();
            let requests = [
                received
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap(),
                received
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap(),
            ];
            assert!(requests[0].starts_with(&format!(
                "PATCH /api/v10/webhooks/50/fixture-token/messages/{suffix} HTTP/1.1"
            )));
            assert!(requests[1].starts_with(&format!(
                "DELETE /api/v10/webhooks/50/fixture-token/messages/{suffix} HTTP/1.1"
            )));
            server.join().unwrap();
            assert_eq!(received.try_iter().count(), 0);
            let audit = std::fs::read_to_string(fixture.directory.join("audit.jsonl")).unwrap();
            assert!(!audit.contains("fixture-token"));
            assert!(!audit.contains("local-webhook-cache-token"));
            assert!(!stderr.contains("fixture-token"));
            assert!(!stderr.contains("local-webhook-cache-token"));
        }
    }
}

#[test]
fn standard_and_extension_bots_resolve_and_protect_owners_without_guild_create() {
    let _guard = integration_test_lock();
    let program = r#"      CHARACTER*64 WHY
      INTEGER S, EV, VAL, CAN, L, TR
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
   10 CALL D7NXT(EV,S)
      IF (S.NE.D7OK) STOP 2
      IF (EV.NE.D7MSG) GOTO 10
      CALL D7CAD(VAL,S)
      IF (S.NE.D7OK.OR.VAL.NE.1) STOP 3
      CALL D7ADM('10',2,'40',2,VAL,S)
      IF (S.NE.D7OK.OR.VAL.NE.1) STOP 4
      CALL D7CTM('10',2,'40',2,CAN,WHY,64,L,TR,S)
      IF (S.NE.D7OK.OR.CAN.NE.0) STOP 5
      IF (WHY.NE.'TARGET_IS_OWNER') STOP 6
      CALL D7TMO('10',2,'40',2,60,' ',0,S)
      IF (S.NE.D7FOR) STOP 7
      CALL D7WHY(1,WHY,64,L,TR,S)
      IF (S.NE.D7OK.OR.WHY.NE.'TARGET_IS_OWNER') STOP 8
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 9
      END
"#;
    let frames = [
        r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"cold","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#,
        r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"0"}}}"#,
    ].join("\n");
    for extension in [false, true] {
        let mut fixture = FortranFixture::new("owner-without-guild");
        let declarations = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            "      INTEGER D7OK, D7FOR, D7MSG\n      PARAMETER (D7OK=0,D7FOR=40,D7MSG=101)\n"
        };
        let binary = fixture.compile(
            &format!("      PROGRAM T\n{declarations}{program}"),
            extension,
        );
        let gateway = fixture.directory.join("gateway.jsonl");
        std::fs::write(&gateway, &frames).unwrap();
        let Some((base_url, received, server)) = serve_once(200, r#"{"id":"10","owner_id":"40"}"#)
        else {
            return;
        };
        fixture.process = Some(
            Command::new(env!("CARGO_BIN_EXE_dfor77"))
                .current_dir(&fixture.directory)
                .arg("--mock-gateway")
                .arg(&gateway)
                .arg("--mock-rest")
                .arg(base_url)
                .arg("--audit-log")
                .arg(fixture.directory.join("audit.jsonl"))
                .arg("--bot")
                .arg(binary)
                .env("DISCORD_BOT_TOKEN", "local-cold-owner-token")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = fixture.wait();
        let request = received
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(request.starts_with("GET /api/v10/guilds/10 HTTP/1.1"));
        server.join().unwrap();
        assert_eq!(received.try_iter().count(), 0);
        let audit = std::fs::read_to_string(fixture.directory.join("audit.jsonl")).unwrap();
        assert!(!audit.contains("local-cold-owner-token"));
        assert!(!stderr.contains("local-cold-owner-token"));
    }
}

#[test]
fn standard_and_extension_malformed_guild_children_revalidate_permissions() {
    let _guard = integration_test_lock();
    for (update, preflight, path) in [
        (
            r#""channels":[{"id":"20","type":"bad"},{"id":"21","type":0}]"#,
            "CSP",
            "/api/v10/channels/20",
        ),
        (
            r#""members":[{"user":{"id":"99"},"roles":[],"communication_disabled_until":null},{"roles":[]}]"#,
            "CAD",
            "/api/v10/guilds/10/members/40",
        ),
    ] {
        let frames = format!(
            r#"{{"op":0,"s":1,"t":"READY","d":{{"session_id":"fixture","resume_gateway_url":"wss://resume","user":{{"id":"99"}}}}}}
{{"op":0,"s":2,"t":"GUILD_CREATE","d":{{"id":"10","owner_id":"70","roles":[{{"id":"10","permissions":"3072","position":0}},{{"id":"50","permissions":"8","position":1}}],"members":[{{"user":{{"id":"99"}},"roles":[],"communication_disabled_until":null}},{{"user":{{"id":"40"}},"roles":["50"],"communication_disabled_until":null}}],"channels":[{{"id":"20","type":0}}]}}}}
{{"op":0,"s":3,"t":"GUILD_CREATE","d":{{"id":"10",{update}}}}}
{{"op":0,"s":4,"t":"MESSAGE_CREATE","d":{{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{{"id":"40"}},"member":{{"roles":["50"]}}}}}}"#
        );
        for extension in [false, true] {
            let mut fixture = FortranFixture::new("guild-context");
            let declarations = if extension {
                "      INCLUDE 'dfor77.inc'\n"
            } else {
                "      INTEGER D7OK, D7MSG, D7FOR\n      PARAMETER (D7OK=0,D7MSG=101,D7FOR=40)\n"
            };
            let action = if preflight == "CAD" {
                "      CALL D7CSP(CAN,WHY,80,L,TR,S)\n      IF (S.NE.D7OK.OR.CAN.NE.1) STOP 3\n      CALL D7CAD(CAN,S)\n"
            } else {
                "      CALL D7CSP(CAN,WHY,80,L,TR,S)\n"
            };
            let program = format!(
                r#"      PROGRAM T
{declarations}      INTEGER S, EV, CAN, L, TR
      CHARACTER*80 WHY
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
   10 CALL D7NXT(EV,S)
      IF (S.NE.D7OK) STOP 2
      IF (EV.NE.D7MSG) GOTO 10
{action}      IF (S.NE.D7FOR.OR.CAN.NE.0) STOP 4
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 5
      END
"#
            );
            let binary = fixture.compile(&program, extension);
            let gateway = fixture.directory.join("gateway.jsonl");
            std::fs::write(&gateway, &frames).unwrap();
            let Some((base_url, received, server)) =
                serve_once(403, r#"{"code":50001,"message":"Missing access"}"#)
            else {
                return;
            };
            fixture.process = Some(
                Command::new(env!("CARGO_BIN_EXE_dfor77"))
                    .current_dir(&fixture.directory)
                    .arg("--mock-gateway")
                    .arg(&gateway)
                    .arg("--mock-rest")
                    .arg(base_url)
                    .arg("--audit-log")
                    .arg(fixture.directory.join("audit.jsonl"))
                    .arg("--bot")
                    .arg(binary)
                    .env("DISCORD_BOT_TOKEN", "local-guild-context-token")
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stderr = fixture.wait();
            assert!(received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .starts_with(&format!("GET {path} HTTP/1.1")));
            server.join().unwrap();
            assert_eq!(received.try_iter().count(), 0);
            assert!(!stderr.contains("local-guild-context-token"));
        }
    }
}

#[test]
fn standard_and_extension_thread_context_changes_revalidate_before_sending() {
    let _guard = integration_test_lock();
    for update in [
        r#"{"op":0,"s":3,"t":"THREAD_UPDATE","d":{"id":"21","guild_id":"10","type":12,"parent_id":"20","permission_overwrites":[],"flags":0,"thread_metadata":{"archived":false,"locked":false},"member":{"id":"21","user_id":"40"}}}"#,
        r#"{"op":0,"s":3,"t":"THREAD_LIST_SYNC","d":{"guild_id":"10","channel_ids":["20"],"threads":[],"members":[]}}"#,
    ] {
        let permissions = (1_u64 << 10) | (1_u64 << 38);
        let frames = format!(
            r#"{{"op":0,"s":1,"t":"READY","d":{{"session_id":"fixture","resume_gateway_url":"wss://resume","user":{{"id":"99"}}}}}}
{{"op":0,"s":2,"t":"GUILD_CREATE","d":{{"id":"10","owner_id":"70","roles":[{{"id":"10","permissions":"{permissions}","position":0}}],"members":[{{"user":{{"id":"99"}},"roles":[],"communication_disabled_until":null}}],"channels":[{{"id":"20","type":0}}],"threads":[{{"id":"21","type":12,"parent_id":"20","joined":true,"thread_metadata":{{"archived":false,"locked":false}}}}]}}}}
{update}
{{"op":0,"s":4,"t":"MESSAGE_CREATE","d":{{"guild_id":"10","channel_id":"21","id":"30","type":0,"author":{{"id":"40"}}}}}}"#
        );
        for extension in [false, true] {
            let mut fixture = FortranFixture::new("thread-context");
            let declarations = if extension {
                "      INCLUDE 'dfor77.inc'\n"
            } else {
                "      INTEGER D7OK, D7MSG, D7FOR\n      PARAMETER (D7OK=0,D7MSG=101,D7FOR=40)\n"
            };
            let program = format!(
                r#"      PROGRAM T
{declarations}      INTEGER S, EV, CAN, L, TR
      CHARACTER*80 WHY
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
   10 CALL D7NXT(EV,S)
      IF (S.NE.D7OK) STOP 2
      IF (EV.NE.D7MSG) GOTO 10
      CALL D7CSP(CAN,WHY,80,L,TR,S)
      IF (S.NE.D7FOR.OR.CAN.NE.0) STOP 3
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 4
      END
"#
            );
            let binary = fixture.compile(&program, extension);
            let gateway = fixture.directory.join("gateway.jsonl");
            std::fs::write(&gateway, &frames).unwrap();
            let Some((base_url, received, server)) =
                serve_once(403, r#"{"code":50001,"message":"Missing access"}"#)
            else {
                return;
            };
            fixture.process = Some(
                Command::new(env!("CARGO_BIN_EXE_dfor77"))
                    .current_dir(&fixture.directory)
                    .arg("--mock-gateway")
                    .arg(&gateway)
                    .arg("--mock-rest")
                    .arg(base_url)
                    .arg("--audit-log")
                    .arg(fixture.directory.join("audit.jsonl"))
                    .arg("--bot")
                    .arg(binary)
                    .env("DISCORD_BOT_TOKEN", "local-thread-context-token")
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stderr = fixture.wait();
            assert!(received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .starts_with("GET /api/v10/channels/21 HTTP/1.1"));
            server.join().unwrap();
            assert_eq!(received.try_iter().count(), 0);
            assert!(!stderr.contains("local-thread-context-token"));
        }
    }
}

#[test]
fn standard_and_extension_thread_member_reads_revalidate_send_preflights() {
    let _guard = integration_test_lock();
    for (mode, status, response, initial, expected) in [
        (7, 200, r#"{"id":"21","user_id":"99"}"#, false, true),
        (
            7,
            404,
            r#"{"code":10007,"message":"Unknown member"}"#,
            true,
            false,
        ),
        (8, 200, r#"[{"id":"21","user_id":"99"}]"#, false, true),
        (8, 200, r#"[{"id":"21","user_id":"40"}]"#, true, true),
    ] {
        let permissions = (1_u64 << 10) | (1_u64 << 38);
        let frames = format!(
            r#"{{"op":0,"s":1,"t":"READY","d":{{"session_id":"fixture","resume_gateway_url":"wss://resume","user":{{"id":"99"}}}}}}
{{"op":0,"s":2,"t":"GUILD_CREATE","d":{{"id":"10","owner_id":"70","roles":[{{"id":"10","permissions":"{permissions}","position":0}}],"members":[{{"user":{{"id":"99"}},"roles":[],"communication_disabled_until":null}}],"channels":[{{"id":"20","type":0}}],"threads":[{{"id":"21","type":12,"parent_id":"20","joined":{initial},"thread_metadata":{{"archived":false,"locked":false}}}}]}}}}
{{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{{"guild_id":"10","channel_id":"21","id":"30","type":0,"author":{{"id":"40"}}}}}}"#
        );
        for extension in [false, true] {
            let mut fixture = FortranFixture::new("thread-member-reads");
            let declarations = if extension {
                "      INCLUDE 'dfor77.inc'\n"
            } else {
                "      INTEGER D7OK, D7MSG\n      PARAMETER (D7OK=0,D7MSG=101)\n"
            };
            let (user, query) = if mode == 7 {
                ("99", "")
            } else {
                ("", "?with_member=true&limit=1")
            };
            let raw_status = if status == 404 {
                dfor77::constants::D7NFD
            } else {
                D7OK
            };
            let program = format!(
                r#"      PROGRAM T
{declarations}      INTEGER S, EV, CAN, L, TR
      CHARACTER*80 WHY
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
   10 CALL D7NXT(EV,S)
      IF (S.NE.D7OK) STOP 2
      IF (EV.NE.D7MSG) GOTO 10
      CALL D7CSP(CAN,WHY,80,L,TR,S)
      IF (S.NE.D7OK.OR.CAN.NE.{before}) STOP 3
      CALL D7THR({mode},'21',2,'{user_field}',{user_len},
     1 '{query_field}',{query_len},' ',0,S)
      IF (S.NE.{raw_status}) STOP 4
      CALL D7CSP(CAN,WHY,80,L,TR,S)
      IF (S.NE.D7OK.OR.CAN.NE.{after}) STOP 5
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 6
      END
"#,
                before = u8::from(initial),
                after = u8::from(expected),
                user_len = user.len(),
                query_len = query.len(),
                user_field = if user.is_empty() { " " } else { user },
                query_field = if query.is_empty() { " " } else { query }
            );
            let binary = fixture.compile(&program, extension);
            let gateway = fixture.directory.join("gateway.jsonl");
            std::fs::write(&gateway, &frames).unwrap();
            let Some((base_url, received, server)) = serve_once(status, response) else {
                return;
            };
            fixture.process = Some(
                Command::new(env!("CARGO_BIN_EXE_dfor77"))
                    .current_dir(&fixture.directory)
                    .arg("--mock-gateway")
                    .arg(&gateway)
                    .arg("--mock-rest")
                    .arg(base_url)
                    .arg("--audit-log")
                    .arg(fixture.directory.join("audit.jsonl"))
                    .arg("--bot")
                    .arg(binary)
                    .env("DISCORD_BOT_TOKEN", "local-thread-member-token")
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stderr = fixture.wait();
            let path = if mode == 7 {
                "/api/v10/channels/21/thread-members/99".to_owned()
            } else {
                format!("/api/v10/channels/21/thread-members{query}")
            };
            assert!(received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .starts_with(&format!("GET {path} HTTP/1.1")));
            server.join().unwrap();
            assert_eq!(received.try_iter().count(), 0);
            assert!(!stderr.contains("local-thread-member-token"));
        }
    }
}

#[test]
fn standard_and_extension_direct_channel_reads_update_action_preflights() {
    let _guard = integration_test_lock();
    let frames = [
        r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"fixture","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#,
        r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"11264","position":0}],"members":[{"user":{"id":"99"},"roles":[],"communication_disabled_until":null}],"channels":[{"id":"20","type":0}]}}"#,
        r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"}}}"#,
    ].join("\n");
    for (path, status, response, preflight) in [
        (
            "/api/v10/channels/20",
            200,
            r#"{"id":"20","guild_id":"10","type":0,"permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"2048"}]}"#,
            "D7CSP",
        ),
        (
            "/api/v10/channels/20",
            404,
            r#"{"code":10003,"message":"Unknown channel"}"#,
            "D7CSP",
        ),
        (
            "/api/v10/channels/20/messages/30",
            404,
            r#"{"code":10008,"message":"Unknown message"}"#,
            "D7CDP",
        ),
    ] {
        for extension in [false, true] {
            let mut fixture = FortranFixture::new("channel-reads");
            let declarations = if extension {
                "      INCLUDE 'dfor77.inc'\n"
            } else {
                "      INTEGER D7OK, D7MSG\n      PARAMETER (D7OK=0,D7MSG=101)\n"
            };
            let expected_status = if status == 404 {
                dfor77::constants::D7NFD
            } else {
                D7OK
            };
            let program = format!(
                r#"      PROGRAM T
{declarations}      INTEGER S, EV, CAN, L, TR
      CHARACTER*80 WHY
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
   10 CALL D7NXT(EV,S)
      IF (S.NE.D7OK) STOP 2
      IF (EV.NE.D7MSG) GOTO 10
      CALL {preflight}(CAN,WHY,80,L,TR,S)
      IF (S.NE.D7OK.OR.CAN.NE.1) STOP 3
      CALL D7RST(1,'{path}',{length},
     1 ' ',0,' ',0,S)
      IF (S.NE.{expected_status}) STOP 4
      CALL {preflight}(CAN,WHY,80,L,TR,S)
      IF (S.NE.{expected_status}.OR.CAN.NE.0) STOP 5
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 6
      END
"#,
                length = path.len()
            );
            let binary = fixture.compile(&program, extension);
            let gateway = fixture.directory.join("gateway.jsonl");
            std::fs::write(&gateway, &frames).unwrap();
            let Some((base_url, received, server)) = serve_once(status, response) else {
                return;
            };
            fixture.process = Some(
                Command::new(env!("CARGO_BIN_EXE_dfor77"))
                    .current_dir(&fixture.directory)
                    .arg("--mock-gateway")
                    .arg(&gateway)
                    .arg("--mock-rest")
                    .arg(base_url)
                    .arg("--audit-log")
                    .arg(fixture.directory.join("audit.jsonl"))
                    .arg("--bot")
                    .arg(binary)
                    .env("DISCORD_BOT_TOKEN", "local-channel-read-token")
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stderr = fixture.wait();
            assert!(received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .starts_with(&format!("GET {path} HTTP/1.1")));
            server.join().unwrap();
            assert_eq!(received.try_iter().count(), 0);
            assert!(!stderr.contains("local-channel-read-token"));
        }
    }
}

#[test]
fn standard_and_extension_direct_permission_reads_revoke_old_administrator_hints() {
    let _guard = integration_test_lock();
    for (path, status, response, owner) in [
        (
            "/api/v10/guilds/10/members/40",
            200,
            r#"{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}"#,
            false,
        ),
        (
            "/api/v10/guilds/10/roles",
            200,
            r#"[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"0","position":1}]"#,
            false,
        ),
        (
            "/api/v10/guilds/10",
            200,
            r#"{"id":"10","owner_id":"70"}"#,
            true,
        ),
        (
            "/api/v10/guilds/10/members/40",
            404,
            r#"{"code":10007,"message":"Unknown member"}"#,
            false,
        ),
    ] {
        let owner_id = if owner { "40" } else { "70" };
        let roles = if owner { "[]" } else { r#"["50"]"# };
        let frames = format!(
            r#"{{"op":0,"s":1,"t":"GUILD_CREATE","d":{{"id":"10","owner_id":"{owner_id}","roles":[{{"id":"10","permissions":"0","position":0}},{{"id":"50","permissions":"8","position":1}}],"members":[{{"user":{{"id":"40"}},"roles":{roles},"communication_disabled_until":null}}]}}}}
{{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{{"id":"40"}},"member":{{"permissions":"8"}}}}}}"#
        );
        for extension in [false, true] {
            let mut fixture = FortranFixture::new("permission-reads");
            let declarations = if extension {
                "      INCLUDE 'dfor77.inc'\n"
            } else {
                "      INTEGER D7OK, D7MSG\n      PARAMETER (D7OK=0,D7MSG=101)\n"
            };
            let expected_status = if status == 404 {
                dfor77::constants::D7NFD
            } else {
                D7OK
            };
            let program = format!(
                r#"      PROGRAM T
{declarations}      INTEGER S, EV, VAL
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
   10 CALL D7NXT(EV,S)
      IF (S.NE.D7OK) STOP 2
      IF (EV.NE.D7MSG) GOTO 10
      CALL D7CAD(VAL,S)
      IF (S.NE.D7OK.OR.VAL.NE.1) STOP 3
      CALL D7RST(1,'{path}',{length},
     1 ' ',0,' ',0,S)
      IF (S.NE.{expected_status}) STOP 4
      CALL D7CAD(VAL,S)
      IF (S.NE.{expected_status}.OR.VAL.NE.0) STOP 5
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 6
      END
"#,
                length = path.len()
            );
            let binary = fixture.compile(&program, extension);
            let gateway = fixture.directory.join("gateway.jsonl");
            std::fs::write(&gateway, &frames).unwrap();
            let Some((base_url, received, server)) = serve_once(status, response) else {
                return;
            };
            fixture.process = Some(
                Command::new(env!("CARGO_BIN_EXE_dfor77"))
                    .current_dir(&fixture.directory)
                    .arg("--mock-gateway")
                    .arg(&gateway)
                    .arg("--mock-rest")
                    .arg(base_url)
                    .arg("--audit-log")
                    .arg(fixture.directory.join("audit.jsonl"))
                    .arg("--bot")
                    .arg(binary)
                    .env("DISCORD_BOT_TOKEN", "local-permission-read-token")
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stderr = fixture.wait();
            let request = received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert!(request.starts_with(&format!("GET {path} HTTP/1.1")));
            server.join().unwrap();
            assert_eq!(received.try_iter().count(), 0);
            assert!(!stderr.contains("local-permission-read-token"));
        }
    }
}

#[test]
fn standard_and_extension_bots_resolve_current_admin_without_event_permissions() {
    let _guard = integration_test_lock();
    let body = r#"      INTEGER S, EV, VAL
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
   10 CALL D7NXT(EV,S)
      IF (S.NE.D7OK) STOP 2
      IF (EV.NE.D7MSG) GOTO 10
      CALL D7CAD(VAL,S)
      IF (S.NE.D7OK.OR.VAL.NE.1) STOP 3
      CALL D7CAD(VAL,S)
      IF (S.NE.D7OK.OR.VAL.NE.1) STOP 4
      CALL D7NXT(EV,S)
      IF (S.NE.D7OK.OR.EV.NE.D7MSG) STOP 5
      CALL D7CAD(VAL,S)
      IF (S.NE.D7OK.OR.VAL.NE.0) STOP 6
      CALL D7NXT(EV,S)
      IF (S.NE.D7OK.OR.EV.NE.D7MSG) STOP 7
      CALL D7CAD(VAL,S)
      IF (S.NE.D7OK.OR.VAL.NE.1) STOP 8
      CALL D7NXT(EV,S)
      IF (S.NE.D7OK.OR.EV.NE.D7MSG) STOP 9
      CALL D7CAD(VAL,S)
      IF (S.NE.D7STA.OR.VAL.NE.0) STOP 10
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 11
      END
"#;
    let frames = [
        r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}],"members":[{"user":{"id":"41"},"roles":[],"communication_disabled_until":null},{"user":{"id":"70"},"roles":[],"communication_disabled_until":null}]}}"#,
        r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"roles":["50"]}}}"#,
        r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"31","type":0,"author":{"id":"41"},"member":{"roles":[]}}}"#,
        r#"{"op":0,"s":4,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"32","type":0,"author":{"id":"70"},"member":{"roles":[]}}}"#,
        r#"{"op":0,"s":5,"t":"MESSAGE_CREATE","d":{"channel_id":"21","id":"33","type":0,"author":{"id":"70"}}}"#,
    ].join("\n");
    for extension in [false, true] {
        let mut fixture = FortranFixture::new("current-admin");
        let declarations = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            "      INTEGER D7OK, D7STA, D7MSG\n      PARAMETER (D7OK=0, D7STA=31, D7MSG=101)\n"
        };
        let binary = fixture.compile(&format!("      PROGRAM T\n{declarations}{body}"), extension);
        let gateway = fixture.directory.join("gateway.jsonl");
        std::fs::write(&gateway, &frames).unwrap();
        let Some((base_url, received, server)) =
            serve_once(200, r#"{"user":{"id":"40"},"roles":["50"]}"#)
        else {
            return;
        };
        fixture.process = Some(
            Command::new(env!("CARGO_BIN_EXE_dfor77"))
                .current_dir(&fixture.directory)
                .arg("--mock-gateway")
                .arg(&gateway)
                .arg("--mock-rest")
                .arg(base_url)
                .arg("--audit-log")
                .arg(fixture.directory.join("audit.jsonl"))
                .arg("--bot")
                .arg(binary)
                .env("DISCORD_BOT_TOKEN", "local-current-admin-token")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = fixture.wait();
        let request = received
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(request.starts_with("GET /api/v10/guilds/10/members/40 HTTP/1.1"));
        server.join().unwrap();
        assert_eq!(received.try_iter().count(), 0);
        assert!(!stderr.contains("local-current-admin-token"));
    }
}

#[test]
fn standard_and_extension_bots_obey_thread_parent_permissions_over_http() {
    let _guard = integration_test_lock();
    let body = r#"      CHARACTER*64 WHY
      INTEGER S, CAN, L, TR
      CALL D7INI(S)
      IF (S.NE.D7OK) STOP 1
      CALL D7CSN('10',2,'20',2,CAN,WHY,64,L,TR,S)
      IF (S.NE.D7OK.OR.CAN.NE.1) STOP 2
      CALL D7CDL('10',2,'20',2,'50',2,CAN,WHY,64,L,TR,S)
      IF (S.NE.D7OK.OR.CAN.NE.1) STOP 3
      CALL D7SND('20',2,'allowed',7,S)
      IF (S.NE.D7OK) STOP 4
      CALL D7DEL('20',2,'50',2,' ',0,S)
      IF (S.NE.D7OK) STOP 5
      CALL D7CSN('10',2,'21',2,CAN,WHY,64,L,TR,S)
      IF (S.NE.D7OK.OR.CAN.NE.0) STOP 6
      IF (WHY.NE.'MISSING_VIEW_CHANNEL') STOP 7
      CALL D7CDL('10',2,'21',2,'51',2,CAN,WHY,64,L,TR,S)
      IF (S.NE.D7OK.OR.CAN.NE.0) STOP 8
      CALL D7SND('21',2,'blocked',7,S)
      IF (S.NE.D7FOR) STOP 9
      CALL D7DEL('21',2,'51',2,' ',0,S)
      IF (S.NE.D7FOR) STOP 10
      CALL D7CDL('10',2,'22',2,'52',2,CAN,WHY,64,L,TR,S)
      IF (S.NE.D7OK.OR.CAN.NE.1) STOP 11
      CALL D7DEL('22',2,'52',2,' ',0,S)
      IF (S.NE.D7OK) STOP 12
      CALL D7END(S)
      IF (S.NE.D7OK) STOP 13
      END
"#;
    let frames = [
        r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#,
        r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","channels":[{"id":"30","type":0,"permission_overwrites":[]},{"id":"40","type":0,"permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"1024"}]}],"threads":[{"id":"20","type":12,"parent_id":"30","current_user_joined":false,"thread_metadata":{"archived":false,"locked":false}},{"id":"21","type":11,"parent_id":"40","thread_metadata":{"archived":false,"locked":false}},{"id":"22","type":12,"parent_id":"30","thread_metadata":{"archived":true,"locked":false}}],"roles":[{"id":"10","permissions":"292057850880","position":0}],"members":[{"user":{"id":"99"},"roles":[],"communication_disabled_until":null}]}}"#,
        r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"50","type":0,"content":"allowed","author":{"id":"70"}}}"#,
        r#"{"op":0,"s":4,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"21","id":"51","type":0,"content":"blocked","author":{"id":"70"}}}"#,
        r#"{"op":0,"s":5,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"22","id":"52","type":0,"content":"archived","author":{"id":"70"}}}"#,
    ].join("\n");
    for extension in [false, true] {
        let mut fixture = FortranFixture::new("thread-parents");
        let declarations = if extension {
            "      INCLUDE 'dfor77.inc'\n"
        } else {
            "      INTEGER D7OK, D7FOR\n      PARAMETER (D7OK=0, D7FOR=40)\n"
        };
        let source = format!("      PROGRAM T\n{declarations}{body}");
        let binary = fixture.compile(&source, extension);
        let gateway = fixture.directory.join("gateway.jsonl");
        std::fs::write(&gateway, &frames).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = finished.clone();
        let server = thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut requests = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed)
                && std::time::Instant::now() < deadline
            {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("thread fixture accept failed: {error}"),
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_millis(500)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(std::time::Duration::from_millis(500)))
                    .unwrap();
                let request = read_http_request(&mut stream);
                let line = request.lines().next().unwrap().to_string();
                let (status, body) = match line.as_str() {
                    "POST /api/v10/channels/20/messages HTTP/1.1" => (200, r#"{"id":"60"}"#),
                    "DELETE /api/v10/channels/20/messages/50 HTTP/1.1" => (204, ""),
                    "GET /api/v10/channels/30/users/@me/threads/archived/private HTTP/1.1" => (200, r#"{"threads":[{"id":"80","guild_id":"10","parent_id":"30","type":12,"thread_metadata":{"archived":true,"locked":false}}],"members":[],"has_more":true}"#),
                    "GET /api/v10/channels/30/users/@me/threads/archived/private?limit=1&before=23 HTTP/1.1" => (200, r#"{"threads":[{"id":"22","guild_id":"10","parent_id":"30","type":12,"thread_metadata":{"archived":true,"locked":false}}],"members":[{"id":"22","user_id":"99"}],"has_more":true}"#),
                    "DELETE /api/v10/channels/22/messages/52 HTTP/1.1" => (204, ""),
                    _ => (
                        403,
                        r#"{"code":50013,"message":"unexpected fixture request"}"#,
                    ),
                };
                requests.push(line);
                write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });
        fixture.process = Some(
            Command::new(env!("CARGO_BIN_EXE_dfor77"))
                .current_dir(&fixture.directory)
                .arg("--mock-gateway")
                .arg(&gateway)
                .arg("--mock-rest")
                .arg(format!("http://{address}"))
                .arg("--audit-log")
                .arg(fixture.directory.join("audit.jsonl"))
                .arg("--bot")
                .arg(binary)
                .env("DISCORD_BOT_TOKEN", "local-thread-fixture-token")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = fixture.wait();
        finished.store(true, std::sync::atomic::Ordering::Relaxed);
        let requests = server.join().unwrap();
        assert_eq!(
            requests,
            vec![
                "POST /api/v10/channels/20/messages HTTP/1.1",
                "DELETE /api/v10/channels/20/messages/50 HTTP/1.1",
                "GET /api/v10/channels/30/users/@me/threads/archived/private HTTP/1.1",
                "GET /api/v10/channels/30/users/@me/threads/archived/private?limit=1&before=23 HTTP/1.1",
                "DELETE /api/v10/channels/22/messages/52 HTTP/1.1",
            ]
        );
        assert!(!stderr.contains("local-thread-fixture-token"));
    }
}

#[test]
fn channel_administrator_overwrites_do_not_send_mutating_http_requests() {
    let _guard = integration_test_lock();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let backend = HttpRestBackend::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        "secret-token",
        "dfor77-overwrite-test",
        100,
    )
    .unwrap();
    let mut runtime = DforRuntime::new(false, false);
    runtime.use_http_rest_backend(backend);
    let now = current_test_ms();
    runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, now).unwrap();
    runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"98","roles":[{"id":"10","permissions":"66560","position":0},{"id":"20","permissions":"0","position":1}],"members":[{"user":{"id":"99"},"roles":["20"],"communication_disabled_until":null}],"channels":[{"id":"30","type":0,"permission_overwrites":[{"id":"99","type":1,"allow":"8","deny":"0"}]}]}}"#, now).unwrap();
    runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"id":"40","channel_id":"30","guild_id":"10","type":0,"content":"message","author":{"id":"42"}}}"#, now).unwrap();
    assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
    let send = runtime.handle(request(
        2,
        "SND",
        vec![
            Field::raw("channel_id", "30"),
            Field::percent("content", "test"),
        ],
    ));
    assert_eq!(send.status, dfor77::constants::D7FOR);
    let delete = runtime.handle(request(
        3,
        "DEL",
        vec![
            Field::raw("channel_id", "30"),
            Field::raw("message_id", "40"),
            Field::percent("reason", "test"),
        ],
    ));
    assert_eq!(delete.status, dfor77::constants::D7FOR);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn runtime_refreshes_expired_ownership_over_http_before_authorising_send() {
    let _guard = integration_test_lock();
    let Some((base_url, received, server)) = serve_once(200, r#"{"id":"10","owner_id":"98"}"#)
    else {
        return;
    };
    let backend =
        HttpRestBackend::new(&base_url, "secret-token", "dfor77-owner-test", 1000).unwrap();
    let mut runtime = DforRuntime::new(false, false);
    runtime.use_http_rest_backend(backend);
    runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
    runtime
        .ingest_gateway_json(
            r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"99"}}"#,
            0,
        )
        .unwrap();
    runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"GUILD_CREATE","d":{"id":"10","roles":[{"id":"10","permissions":"0","position":0}],"members":[{"user":{"id":"99"},"roles":[],"communication_disabled_until":null}],"channels":[{"id":"30","type":0}]}}"#, current_test_ms()).unwrap();
    assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
    let result = runtime.handle(request(
        2,
        "CSN",
        vec![Field::raw("guild_id", "10"), Field::raw("channel_id", "30")],
    ));
    assert_eq!(result.status, D7OK);
    assert_eq!(result.field("can"), Some("0"));
    assert_eq!(result.field("why"), Some("MISSING_VIEW_CHANNEL"));
    let wire = received
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    assert!(wire.starts_with("GET /api/v10/guilds/10 HTTP/1.1\r\n"));
    assert!(wire.contains("\r\nAuthorization: Bot secret-token\r\n"));
    server.join().unwrap();
}

#[test]
fn runtime_sends_action_through_http_backend_and_audits_commit() {
    let _guard = integration_test_lock();
    let Some((base_url, received, server)) = serve_once(200, r#"{"id":"100000000000000099"}"#)
    else {
        return;
    };
    let audit_path = std::env::temp_dir().join(format!(
        "dfor77-runtime-http-audit-{}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&audit_path);

    let backend = HttpRestBackend::new(&base_url, "secret-token", "dfor77-runtime-test", 1000)
        .expect("failed to create HTTP backend");
    let mut runtime = DforRuntime::new(false, false);
    runtime.use_http_rest_backend(backend);
    runtime
        .enable_audit(audit_path.to_str().unwrap())
        .expect("failed to open audit log");
    assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
    prime_send_permissions(&mut runtime);
    let response = runtime.handle(request(
        2,
        "SND",
        vec![
            Field::raw("channel_id", "100000000000000001"),
            Field::percent("content", "runtime http message"),
        ],
    ));
    if response.status != D7OK {
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        panic!("send failed: {response:?} {why:?}");
    }

    let request_text = received.recv().unwrap();
    assert!(request_text.starts_with("POST /api/v10/channels/100000000000000001/messages HTTP/1.1"));
    assert!(request_text.contains("\r\nAuthorization: Bot secret-token\r\n"));
    assert!(request_text.contains("\r\nUser-Agent: dfor77-runtime-test\r\n"));
    assert!(request_text.contains("\"content\":\"runtime http message\""));
    server.join().unwrap();

    let audit = std::fs::read_to_string(&audit_path).unwrap();
    assert!(audit.contains("\"action\":\"D7SND\""));
    assert!(audit.contains("\"status\":0"));
    assert!(audit.contains("\"committed\":true"));
    assert!(audit.contains("\"commit_state\":\"committed\""));
    assert!(audit.contains("\"created_message_id\":\"100000000000000099\""));
    assert!(audit.contains("\"nonce\":"));
    assert!(!audit.contains("secret-token"));
    assert!(!audit.contains("runtime http message"));
    let _ = std::fs::remove_file(audit_path);
}

#[test]
fn runtime_prepares_gateway_identify_through_http_backend() {
    let _guard = integration_test_lock();
    let Some((base_url, received, server)) = serve_once(
        200,
        r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":3,"reset_after":60000,"max_concurrency":2}}"#,
    ) else {
        return;
    };
    let backend = HttpRestBackend::new(&base_url, "secret-token", "dfor77-runtime-test", 1000)
        .expect("failed to create HTTP backend");
    let mut runtime = DforRuntime::new(false, false);
    runtime.use_http_rest_backend(backend);
    let startup = runtime.prepare_gateway_identify("secret-token", 0);
    assert_eq!(startup.status, D7OK);
    assert_eq!(startup.action, GatewayAction::SendIdentify);
    assert_eq!(
        startup.gateway_url.as_deref(),
        Some("wss://gateway.discord.gg")
    );
    assert_eq!(startup.identify_bucket, Some(0));
    assert_eq!(startup.identify_remaining, Some(3));
    assert!(startup
        .identify_payload
        .as_deref()
        .is_some_and(|payload| payload.contains("\"op\":2")));

    let request_text = received.recv().unwrap();
    assert!(request_text.starts_with("GET /api/v10/gateway/bot HTTP/1.1"));
    assert!(request_text.contains("\r\nAuthorization: Bot secret-token\r\n"));
    assert!(request_text.contains("\r\nContent-Length: 0\r\n"));
    server.join().unwrap();
}

#[test]
fn runtime_connects_gateway_through_local_websocket_transport() {
    let _guard = integration_test_lock();
    let Some((gateway_url, gateway_received, gateway_server)) = serve_websocket_gateway() else {
        return;
    };
    let rest_body = format!(
        r#"{{"url":"{}","session_start_limit":{{"total":1000,"remaining":1,"reset_after":60000,"max_concurrency":1}}}}"#,
        gateway_url
    );
    let Some((base_url, rest_received, rest_server)) = serve_once(200, &rest_body) else {
        return;
    };
    let backend = HttpRestBackend::new(&base_url, "secret-token", "dfor77-runtime-test", 1000)
        .expect("failed to create HTTP backend");
    let mut runtime = DforRuntime::new(false, false);
    runtime.use_http_rest_backend(backend);
    let mut transport = WsGatewayTransport::new(1000);
    let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
    assert_eq!(result.status, D7OK);
    assert!(result.connected);
    assert!(result.identify_sent);
    assert_eq!(result.received_frames, 2);
    assert_eq!(
        result.startup.gateway_url.as_deref(),
        Some(gateway_url.as_str())
    );

    let rest_request = rest_received.recv().unwrap();
    assert!(rest_request.starts_with("GET /api/v10/gateway/bot HTTP/1.1"));
    rest_server.join().unwrap();

    let ws_request = gateway_received.recv().unwrap();
    assert!(ws_request.starts_with("GET /gateway HTTP/1.1"));
    let identify = gateway_received.recv().unwrap();
    assert!(identify.contains("\"op\":2"));
    assert!(identify.contains("\"token\":\"secret-token\""));
    gateway_server.join().unwrap();

    assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
    let next = runtime.handle(request(2, "NXT", vec![]));
    assert_eq!(next.status, D7OK);
    assert_eq!(next.field("event_type"), Some("100"));
}

#[test]
fn runtime_connects_gateway_through_tungstenite_transport() {
    let _guard = integration_test_lock();
    let Some((gateway_url, gateway_received, gateway_server)) = serve_websocket_gateway() else {
        return;
    };
    let rest_body = format!(
        r#"{{"url":"{}","session_start_limit":{{"total":1000,"remaining":1,"reset_after":60000,"max_concurrency":1}}}}"#,
        gateway_url
    );
    let Some((base_url, rest_received, rest_server)) = serve_once(200, &rest_body) else {
        return;
    };
    let backend = HttpRestBackend::new(&base_url, "secret-token", "dfor77-runtime-test", 1000)
        .expect("failed to create HTTP backend");
    let mut runtime = DforRuntime::new(false, false);
    runtime.use_http_rest_backend(backend);
    let mut transport = TungsteniteGatewayTransport::new(1000);
    let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
    assert_eq!(result.status, D7OK);
    assert!(result.connected);
    assert!(result.identify_sent);
    assert_eq!(result.received_frames, 2);
    assert_eq!(
        result.startup.gateway_url.as_deref(),
        Some(gateway_url.as_str())
    );

    let rest_request = rest_received.recv().unwrap();
    assert!(rest_request.starts_with("GET /api/v10/gateway/bot HTTP/1.1"));
    rest_server.join().unwrap();

    let ws_request = gateway_received.recv().unwrap();
    assert!(ws_request.starts_with("GET /gateway HTTP/1.1"));
    let identify = gateway_received.recv().unwrap();
    assert!(identify.contains("\"op\":2"));
    assert!(identify.contains("\"token\":\"secret-token\""));
    transport.close_gracefully().unwrap();
    let close = gateway_received.recv().unwrap();
    assert_eq!(close, "close:1000");
    gateway_server.join().unwrap();
}

#[test]
fn cli_gateway_connect_once_uses_http_rest_and_websocket_transport() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI gateway connect test");
        return;
    };
    let Some((gateway_url, gateway_received, gateway_server)) = serve_websocket_gateway() else {
        return;
    };
    let rest_body = format!(
        r#"{{"url":"{}","session_start_limit":{{"total":1000,"remaining":1,"reset_after":60000,"max_concurrency":1}}}}"#,
        gateway_url
    );
    let Some((base_url, rest_received, rest_server)) = serve_once(200, &rest_body) else {
        return;
    };
    let output = Command::new(runtime)
        .args(["--gateway-connect-once", "--rest-url"])
        .arg(&base_url)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 gateway connect once");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"status\":0"));
    assert!(stdout.contains("\"connected\":true"));
    assert!(stdout.contains("\"identify_sent\":true"));
    assert!(stdout.contains("\"received_frames\":2"));
    assert!(stdout.contains("\"gateway_url\":\"ws://"));

    let rest_request = rest_received.recv().unwrap();
    assert!(rest_request.starts_with("GET /api/v10/gateway/bot HTTP/1.1"));
    rest_server.join().unwrap();

    let ws_request = gateway_received.recv().unwrap();
    assert!(ws_request.starts_with("GET /gateway HTTP/1.1"));
    let identify = gateway_received.recv().unwrap();
    assert!(identify.contains("\"op\":2"));
    assert!(identify.contains("\"token\":\"secret-token\""));
    gateway_server.join().unwrap();
}

#[test]
fn cli_gateway_connect_once_maps_auth_failure_to_exit_14() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI gateway auth test");
        return;
    };
    let Some((base_url, received, server)) = serve_once(401, r#"{"message":"401: Unauthorized"}"#)
    else {
        return;
    };
    let output = Command::new(runtime)
        .args(["--gateway-connect-once", "--rest-url"])
        .arg(&base_url)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 gateway auth test");
    assert_eq!(output.status.code(), Some(14));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("\"status\":40"));
    assert!(stdout.contains("\"why\":\"AUTH_INVALID\""));
    assert!(!stdout.contains("secret-token"));
    assert!(!stderr.contains("secret-token"));
    let request = received.recv().unwrap();
    assert!(request.starts_with("GET /api/v10/gateway/bot HTTP/1.1"));
    server.join().unwrap();
}

#[test]
fn cli_gateway_connect_once_rejects_empty_token_before_network() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI token validation test");
        return;
    };
    let output = Command::new(runtime)
        .args(["--gateway-connect-once", "--rest-url", "http://127.0.0.1:9"])
        .env("DISCORD_BOT_TOKEN", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 token validation test");
    assert_eq!(output.status.code(), Some(12));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.is_empty());
    assert!(stderr.contains("bot token must not be empty"));
}

#[test]
fn cli_help_lists_gateway_mock_rest_option() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI help test");
        return;
    };
    let output = Command::new(runtime)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 help test");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
    assert!(stdout.contains("dfor77 --version"));
    assert!(stdout.contains("dfor77 --validate-config --config PATH"));
    assert!(stdout.contains(
        "dfor77 --validate-audit-log PATH [--audit-require-action ACTION]... [--audit-forbid-action ACTION]... [--audit-require-live-actions]"
    ));
    assert!(stdout.contains("dfor77 --validate-runtime-log PATH"));
    assert!(stdout.contains("dfor77 --gateway-connect-once"));
    assert!(stdout.contains("[--mock-rest URL]"));
}

#[test]
fn cli_version_prints_crate_version() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI version test");
        return;
    };
    let output = Command::new(runtime)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 version test");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("dfor77 {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
}

#[test]
fn cli_rejects_invalid_arguments_as_config_errors() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI argument error test");
        return;
    };

    let unknown = Command::new(runtime)
        .arg("--definitely-not-a-dfor77-option")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 unknown argument test");
    assert_eq!(unknown.status.code(), Some(12));
    assert!(String::from_utf8_lossy(&unknown.stdout).is_empty());
    assert!(String::from_utf8_lossy(&unknown.stderr)
        .contains("unknown argument: --definitely-not-a-dfor77-option"));

    let missing_bot = Command::new(runtime)
        .arg("--bot")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 missing bot command test");
    assert_eq!(missing_bot.status.code(), Some(12));
    assert!(String::from_utf8_lossy(&missing_bot.stdout).is_empty());
    assert!(String::from_utf8_lossy(&missing_bot.stderr).contains("--bot requires a command"));

    let missing_config = Command::new(runtime)
        .arg("--validate-config")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 validate config argument test");
    assert_eq!(missing_config.status.code(), Some(12));
    assert!(String::from_utf8_lossy(&missing_config.stdout).is_empty());
    assert!(String::from_utf8_lossy(&missing_config.stderr)
        .contains("--validate-config requires --config PATH"));
}

#[test]
fn cli_gateway_connect_once_maps_gateway_failure_to_exit_13() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI gateway fatal test");
        return;
    };
    let Some((base_url, received, server)) = serve_once(500, r#"{"message":"upstream"}"#) else {
        return;
    };
    let output = Command::new(runtime)
        .args(["--gateway-connect-once", "--rest-url"])
        .arg(&base_url)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 gateway fatal test");
    assert_eq!(output.status.code(), Some(13));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("\"status\":50"));
    assert!(!stdout.contains("secret-token"));
    assert!(!stderr.contains("secret-token"));
    let request = received.recv().unwrap();
    assert!(request.starts_with("GET /api/v10/gateway/bot HTTP/1.1"));
    server.join().unwrap();
}

#[test]
fn cli_bot_supervisor_gateway_live_feeds_ready_event_to_bot() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI gateway live test");
        return;
    };
    let Some((gateway_url, gateway_received, gateway_server)) = serve_websocket_gateway_hold(500)
    else {
        return;
    };
    let rest_body = format!(
        r#"{{"url":"{}","session_start_limit":{{"total":1000,"remaining":1,"reset_after":60000,"max_concurrency":1}}}}"#,
        gateway_url
    );
    let Some((base_url, rest_received, rest_server)) = serve_once(200, &rest_body) else {
        return;
    };
    let script = concat!(
        "printf '@D7/1 REQ 1 INI 0\n@D7/1 END 1\n@D7/1 REQ 2 NXT 0\n@D7/1 END 2\n'\n",
        "while IFS= read -r line; do echo \"$line\" >&2; if [ \"$line\" = '@D7/1 END 2' ]; then break; fi; done\n"
    );
    let output = Command::new(runtime)
        .args(["--gateway-live", "--rest-url"])
        .arg(&base_url)
        .arg("--bot")
        .arg("/bin/sh")
        .arg("-c")
        .arg(script)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 gateway live supervisor");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("@D7/1 RES 2 0"));
    assert!(stderr.contains("K event_type raw 3 100"));

    let rest_request = rest_received.recv().unwrap();
    assert!(rest_request.starts_with("GET /api/v10/gateway/bot HTTP/1.1"));
    rest_server.join().unwrap();

    let ws_request = gateway_received.recv().unwrap();
    assert!(ws_request.starts_with("GET /gateway HTTP/1.1"));
    let identify = gateway_received.recv().unwrap();
    assert!(identify.contains("\"op\":2"));
    assert!(identify.contains("\"token\":\"secret-token\""));
    gateway_server.join().unwrap();
}

#[test]
fn cli_stdio_gateway_live_closes_on_d7end_without_stdin_eof() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI stdio gateway close test");
        return;
    };
    let Some((gateway_url, gateway_received, gateway_server)) = serve_websocket_gateway_hold(1000)
    else {
        return;
    };
    let rest_body = format!(
        r#"{{"url":"{}","session_start_limit":{{"total":1000,"remaining":1,"reset_after":60000,"max_concurrency":1}}}}"#,
        gateway_url
    );
    let Some((base_url, rest_received, rest_server)) = serve_once(200, &rest_body) else {
        return;
    };
    let mut child = Command::new(runtime)
        .args(["--stdio-server", "--gateway-live", "--rest-url"])
        .arg(&base_url)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn dfor77 stdio gateway close test");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    stdin
        .write_all(
            concat!(
                "@D7/1 REQ 1 INI 0\n",
                "@D7/1 END 1\n",
                "@D7/1 REQ 2 END 0\n",
                "@D7/1 END 2\n"
            )
            .as_bytes(),
        )
        .unwrap();
    stdin.flush().unwrap();

    let mut stdout = BufReader::new(stdout);
    let init_response = read_ipc_response(&mut stdout, 1);
    assert!(init_response.contains("@D7/1 RES 1 0 0"));
    let end_response = read_ipc_response(&mut stdout, 2);
    assert!(end_response.contains("@D7/1 RES 2 0 0"));

    let rest_request = rest_received.recv().unwrap();
    assert!(rest_request.starts_with("GET /api/v10/gateway/bot HTTP/1.1"));
    rest_server.join().unwrap();

    let ws_request = gateway_received.recv().unwrap();
    assert!(ws_request.starts_with("GET /gateway HTTP/1.1"));
    let identify = gateway_received.recv().unwrap();
    assert!(identify.contains("\"op\":2"));
    let close = gateway_received
        .recv_timeout(std::time::Duration::from_millis(1500))
        .unwrap();
    assert_eq!(close, "close:1000");

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());
    gateway_server.join().unwrap();
}

#[test]
fn cli_stdio_exits_14_after_rest_auth_fatal_response() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI REST auth fatal test");
        return;
    };
    let Some((base_url, received, server)) = serve_once(401, r#"{"message":"401: Unauthorized"}"#)
    else {
        return;
    };
    let mut child = Command::new(runtime)
        .args(["--stdio-server", "--mock-rest"])
        .arg(&base_url)
        .arg("--mock-gateway")
        .arg("examples/sample_moderation/mock_gateway.jsonl")
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn dfor77 stdio auth test");
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(
                concat!(
                    "@D7/1 REQ 1 INI 0\n",
                    "@D7/1 END 1\n",
                    "@D7/1 REQ 2 SND 2\n",
                    "K channel_id raw 18 100000000000000002\n",
                    "K content raw 4 test\n",
                    "@D7/1 END 2\n"
                )
                .as_bytes(),
            )
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(14));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("@D7/1 RES 1 0 0"));
    assert!(stdout.contains("@D7/1 RES 2 40 0"));
    assert!(stderr.contains("REST auth fatal"));
    assert!(!stdout.contains("secret-token"));
    assert!(!stderr.contains("secret-token"));
    let request = received.recv().unwrap();
    assert!(request.starts_with("POST /api/v10/channels/100000000000000002/messages HTTP/1.1"));
    server.join().unwrap();
}

#[test]
fn cli_stdio_server_uses_mock_rest_backend() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI mock REST test");
        return;
    };
    let Some((base_url, received, server)) = serve_once(200, r#"{"id":"100000000000000088"}"#)
    else {
        return;
    };
    let audit_path = std::env::temp_dir().join(format!(
        "dfor77-cli-http-audit-{}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&audit_path);

    let mut child = Command::new(runtime)
        .args(["--stdio-server", "--mock-rest"])
        .arg(&base_url)
        .arg("--mock-gateway")
        .arg("examples/sample_moderation/mock_gateway.jsonl")
        .arg("--audit-log")
        .arg(&audit_path)
        .env("DISCORD_BOT_TOKEN", "secret-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn dfor77 stdio server");
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(
                concat!(
                    "@D7/1 REQ 1 INI 0\n",
                    "@D7/1 END 1\n",
                    "@D7/1 REQ 2 SND 2\n",
                    "K channel_id raw 18 100000000000000002\n",
                    "K content raw 10 climessage\n",
                    "@D7/1 END 2\n"
                )
                .as_bytes(),
            )
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("@D7/1 RES 1 0"));
    assert!(stdout.contains("@D7/1 RES 2 0"));

    let request_text = received.recv().unwrap();
    assert!(request_text.contains("\r\nAuthorization: Bot secret-token\r\n"));
    assert!(request_text.contains(&format!(
        "\r\nUser-Agent: dfor77/{} ",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(request_text.contains("\"content\":\"climessage\""));
    server.join().unwrap();

    let audit = std::fs::read_to_string(&audit_path).unwrap();
    assert!(audit.contains("\"created_message_id\":\"100000000000000088\""));
    assert!(!audit.contains("secret-token"));
    let _ = std::fs::remove_file(audit_path);
}

#[test]
fn cli_stdio_log_writes_sanitized_stderr_without_stdout_pollution() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI log test");
        return;
    };
    let token = fake_discord_token();
    let message = format!("Authorization: Bot {}\nnext\u{1b}[31m", token);
    let encoded = percent_encode(&message);
    let input = format!(
        concat!(
            "@D7/1 REQ 1 LOG 2\n",
            "K level raw 1 1\n",
            "K message pct {} {}\n",
            "@D7/1 END 1\n",
            "@D7/1 REQ 2 INI 0\n",
            "@D7/1 END 2\n",
            "@D7/1 REQ 3 LOG 2\n",
            "K level raw 1 2\n",
            "K message pct {} {}\n",
            "@D7/1 END 3\n"
        ),
        encoded.len(),
        encoded,
        encoded.len(),
        encoded
    );
    let output = Command::new(runtime)
        .arg("--stdio-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(input.as_bytes())?;
            child.wait_with_output()
        })
        .expect("failed to run dfor77 stdio log test");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("@D7/1 RES 1 31 0"));
    assert!(stdout.contains("@D7/1 RES 2 0 0"));
    assert!(stdout.contains("@D7/1 RES 3 0 0"));
    assert!(!stdout.contains("Authorization"));
    assert!(!stdout.contains("abcdefghijkl"));
    assert!(stderr.contains("dfor77 warn [REDACTED][CTRL]next[CTRL]"));
    assert!(!stderr.contains("Authorization"));
    assert!(!stderr.contains("Bot "));
    assert!(!stderr.contains("abcdefghijkl"));
    assert!(!stderr.contains('\u{1b}'));
}

#[test]
fn cli_validate_audit_log_accepts_schema_records() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI audit validation test");
        return;
    };
    let path = std::env::temp_dir().join(format!(
        "dfor77-validate-audit-ok-{}.jsonl",
        std::process::id()
    ));
    let mut planner = RestPlanner::new();
    let outcome = planner.send_message("100000000000000001", "hello");
    let record = AuditRecord::action_with_content_policy(
        "D7SND",
        &outcome,
        false,
        false,
        &AuditContentPolicy::Hash {
            key: b"key".to_vec(),
        },
    );
    std::fs::write(&path, record.to_json_line()).unwrap();
    let output = Command::new(runtime)
        .arg("--validate-audit-log")
        .arg(&path)
        .args(["--audit-require-action", "D7SND"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 audit validation test");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "dfor77 audit log ok: 1 records\n"
    );
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
    let _ = std::fs::remove_file(path);
}

#[test]
fn cli_validate_runtime_log_accepts_schema_records() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI runtime validation test");
        return;
    };
    let path = std::env::temp_dir().join(format!(
        "dfor77-validate-runtime-ok-{}.jsonl",
        std::process::id()
    ));
    std::fs::write(
        &path,
        concat!(
            "{\"schema\":\"dfor77.runtime.v1\",\"time_unix_ms\":1,",
            "\"event\":\"runtime\",\"kind\":\"gateway_transition\",",
            "\"phase\":\"start\",\"shard_id\":0}\n"
        ),
    )
    .unwrap();
    let output = Command::new(runtime)
        .arg("--validate-runtime-log")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 runtime validation test");
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("dfor77 runtime log ok: 1 records"));
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
    let _ = std::fs::remove_file(path);
}

#[test]
fn cli_validate_audit_log_rejects_missing_required_action() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI audit required action test");
        return;
    };
    let path = std::env::temp_dir().join(format!(
        "dfor77-validate-audit-missing-action-{}.jsonl",
        std::process::id()
    ));
    let mut planner = RestPlanner::new();
    let outcome = planner.send_message("100000000000000001", "hello");
    let record = AuditRecord::action("D7SND", &outcome, false, false);
    std::fs::write(&path, record.to_json_line()).unwrap();
    let output = Command::new(runtime)
        .arg("--validate-audit-log")
        .arg(&path)
        .args(["--audit-require-action", "D7DEL"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 audit required action test");
    assert_eq!(output.status.code(), Some(12));
    assert!(String::from_utf8_lossy(&output.stdout).is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("required action D7DEL not found"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn cli_validate_audit_log_rejects_forbidden_action() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI audit forbidden action test");
        return;
    };
    let path = std::env::temp_dir().join(format!(
        "dfor77-validate-audit-forbidden-action-{}.jsonl",
        std::process::id()
    ));
    let mut planner = RestPlanner::new();
    let outcome = planner.send_message("100000000000000001", "hello");
    let record = AuditRecord::action("D7UNC", &outcome, false, true);
    std::fs::write(&path, record.to_json_line()).unwrap();
    let output = Command::new(runtime)
        .arg("--validate-audit-log")
        .arg(&path)
        .args(["--audit-forbid-action", "D7UNC"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 audit forbidden action test");
    assert_eq!(output.status.code(), Some(12));
    assert!(String::from_utf8_lossy(&output.stdout).is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("forbidden action D7UNC found"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn cli_validate_audit_log_requires_live_committed_actions() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI audit live action test");
        return;
    };
    let path = std::env::temp_dir().join(format!(
        "dfor77-validate-audit-live-action-{}.jsonl",
        std::process::id()
    ));
    let mut planner = RestPlanner::new();
    let dry_outcome = planner.send_message("100000000000000001", "hello");
    let dry_record = AuditRecord::action("D7SND", &dry_outcome, true, false);
    std::fs::write(&path, dry_record.to_json_line()).unwrap();
    let output = Command::new(runtime)
        .arg("--validate-audit-log")
        .arg(&path)
        .args(["--audit-require-action", "D7SND"])
        .arg("--audit-require-live-actions")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 audit live action test");
    assert_eq!(output.status.code(), Some(12));
    assert!(String::from_utf8_lossy(&output.stdout).is_empty());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("required action D7SND has no live committed record"));
    let mut live_outcome = dry_outcome;
    live_outcome.status = 0;
    live_outcome.http_status = 200;
    live_outcome.commit_state = CommitState::Committed;
    let live_record = AuditRecord::action("D7SND", &live_outcome, false, false);
    std::fs::write(&path, live_record.to_json_line()).unwrap();
    let output = Command::new(runtime)
        .arg("--validate-audit-log")
        .arg(&path)
        .args(["--audit-require-action", "D7SND"])
        .arg("--audit-require-live-actions")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 audit live action success test");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "dfor77 audit log ok: 1 records\n"
    );
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
    let _ = std::fs::remove_file(path);
}

#[test]
fn cli_audit_required_action_requires_audit_validation_mode() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI audit option mode test");
        return;
    };
    let output = Command::new(runtime)
        .args(["--audit-require-action", "D7SND"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 audit option mode test");
    assert_eq!(output.status.code(), Some(12));
    assert!(String::from_utf8_lossy(&output.stdout).is_empty());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--audit-require-action requires --validate-audit-log"));
    let output = Command::new(runtime)
        .args(["--audit-forbid-action", "D7UNC"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 audit option mode test");
    assert_eq!(output.status.code(), Some(12));
    assert!(String::from_utf8_lossy(&output.stdout).is_empty());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--audit-forbid-action requires --validate-audit-log"));
    let output = Command::new(runtime)
        .arg("--audit-require-live-actions")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 audit option mode test");
    assert_eq!(output.status.code(), Some(12));
    assert!(String::from_utf8_lossy(&output.stdout).is_empty());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--audit-require-live-actions requires --validate-audit-log"));
}

#[test]
fn cli_validate_audit_log_rejects_sensitive_values() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI audit validation failure test");
        return;
    };
    let path = std::env::temp_dir().join(format!(
        "dfor77-validate-audit-bad-{}.jsonl",
        std::process::id()
    ));
    let mut planner = RestPlanner::new();
    let token = fake_discord_token();
    let outcome = planner.delete_message(
        "100000000000000001",
        "100000000000000002",
        &format!("Authorization: Bot {token}"),
    );
    let record = AuditRecord::action("D7DEL", &outcome, false, false);
    std::fs::write(&path, record.to_json_line()).unwrap();
    let output = Command::new(runtime)
        .arg("--validate-audit-log")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 audit validation failure test");
    assert_eq!(output.status.code(), Some(12));
    assert!(String::from_utf8_lossy(&output.stdout).is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("audit log validation failed"));
    assert!(stderr.contains("field reason contains sensitive text"));
    assert!(!stderr.contains(&token));
    let _ = std::fs::remove_file(path);
}

#[test]
fn cli_config_supports_literal_strings_multiline_arrays_and_strict_value_boundaries() {
    let _guard = integration_test_lock();
    let fixture = FortranFixture::new("config-grammar");
    std::fs::create_dir(fixture.directory.join("logs")).unwrap();
    let config_path = fixture.directory.join("config.toml");
    let config = include_str!("../config/dfor77.sample.toml")
        .replace('"', "'")
        .replace("token_env = 'DISCORD_BOT_TOKEN'", "token_env = 'DFOR77_CONFIG_GRAMMAR_TOKEN'")
        .replace("hash_key_env = 'DFOR77_AUDIT_HASH_KEY'", "hash_key_env = 'DFOR77_CONFIG_GRAMMAR_KEY'")
        .replace("intents = ['GUILDS', 'GUILD_MESSAGES', 'MESSAGE_CONTENT', 'GUILD_MEMBERS']", "intents = [\n 'GUILDS', # guilds\n \"DIRECT_MESSAGES\", # messages\n 'GUILD_MEMBERS',\n]")
        .replace("path = './dfor77-audit.jsonl'", r#"path = 'logs\#audit.jsonl'"#);
    let run = |text: &str| {
        std::fs::write(&config_path, text).unwrap();
        Command::new(env!("CARGO_BIN_EXE_dfor77"))
            .current_dir(&fixture.directory)
            .args(["--validate-config", "--config"])
            .arg(&config_path)
            .env("DFOR77_CONFIG_GRAMMAR_TOKEN", "local-config-grammar-token")
            .env("DFOR77_CONFIG_GRAMMAR_KEY", "local-config-grammar-key")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let output = run(&config);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"dfor77 config ok\n");
    for invalid in [
        r#"'safe' path = 'private-config-fragment'"#,
        r#""safe" "private-config-fragment""#,
        "'private-config-fragment\u{7f}'",
    ] {
        let output = run(&config.replace(
            r#"path = 'logs\#audit.jsonl'"#,
            &format!("path = {invalid}"),
        ));
        assert_eq!(output.status.code(), Some(12));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("private-config-fragment"));
        assert!(!stderr.contains("local-config-grammar-token"));
        assert!(!stderr.contains("local-config-grammar-key"));
    }
    assert!(!fixture.directory.join(r#"logs\#audit.jsonl"#).exists());
    assert!(!fixture.directory.join("dfor77-runtime.jsonl").exists());
}

#[test]
fn cli_validate_config_checks_config_without_starting_runtime() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI validate config test");
        return;
    };
    let token_env = format!("DFOR77_VALIDATE_TOKEN_{}", std::process::id());
    let config_path =
        std::env::temp_dir().join(format!("dfor77-validate-{}.toml", std::process::id()));
    let audit_path = cli_config_audit_path("validate");
    let session_dir =
        std::env::temp_dir().join(format!("dfor77-validate-sessions-{}", std::process::id()));
    let restart_file =
        std::env::temp_dir().join(format!("dfor77-validate-restart-{}", std::process::id()));
    let _ = std::fs::remove_file(&audit_path);
    let _ = std::fs::remove_dir_all(&session_dir);
    let _ = std::fs::remove_file(&restart_file);
    std::fs::write(&config_path, cli_warning_config(&token_env, &audit_path)).unwrap();

    let output = Command::new(runtime)
        .env(&token_env, "secret-token")
        .args(["--validate-config", "--config"])
        .arg(&config_path)
        .arg("--gateway-session-dir")
        .arg(&session_dir)
        .arg("--hot-restart-file")
        .arg(&restart_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 validate config test");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stdout, "dfor77 config ok\n");
    assert!(stderr.contains("dfor77 warn config: MESSAGE_CONTENT intent is not configured"));
    assert!(stderr.contains("dry_run takes precedence"));
    assert!(!stdout.contains("secret-token"));
    assert!(!stderr.contains("secret-token"));
    assert!(!audit_path.exists());
    assert!(!session_dir.exists());
    assert!(!restart_file.exists());

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(audit_path);
    let _ = std::fs::remove_dir_all(session_dir);
    let _ = std::fs::remove_file(restart_file);
}

#[test]
fn cli_validate_config_rejects_bot_mode_without_starting_runtime() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI validate bot conflict test");
        return;
    };
    let token_env = format!("DFOR77_VALIDATE_BOT_TOKEN_{}", std::process::id());
    let config_path = std::env::temp_dir().join(format!(
        "dfor77-validate-bot-conflict-{}.toml",
        std::process::id()
    ));
    let audit_path = cli_config_audit_path("validate-bot-conflict");
    let _ = std::fs::remove_file(&audit_path);
    std::fs::write(&config_path, cli_warning_config(&token_env, &audit_path)).unwrap();

    let output = Command::new(runtime)
        .env(&token_env, "secret-token")
        .args(["--validate-config", "--config"])
        .arg(&config_path)
        .arg("--bot")
        .arg(runtime)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 validate bot conflict test");

    assert_eq!(output.status.code(), Some(12));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.is_empty());
    assert!(stderr.contains("--bot conflicts with --validate-config"));
    assert!(!stderr.contains("secret-token"));
    assert!(!audit_path.exists());

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(audit_path);
}

#[test]
fn cli_validate_config_rejects_missing_audit_parent() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI validate config filesystem test");
        return;
    };
    let token_env = format!("DFOR77_VALIDATE_BAD_AUDIT_TOKEN_{}", std::process::id());
    let config_path = std::env::temp_dir().join(format!(
        "dfor77-validate-bad-audit-{}.toml",
        std::process::id()
    ));
    let missing_parent = std::env::temp_dir().join(format!(
        "dfor77-missing-audit-parent-{}",
        std::process::id()
    ));
    let audit_path = missing_parent.join("audit.jsonl");
    let _ = std::fs::remove_dir_all(&missing_parent);
    std::fs::write(
        &config_path,
        cli_timeout_config(&token_env, 100, &audit_path),
    )
    .unwrap();

    let output = Command::new(runtime)
        .env(&token_env, "secret-token")
        .args(["--validate-config", "--config"])
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 validate config missing audit parent test");

    assert_eq!(output.status.code(), Some(12));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.is_empty());
    assert!(stderr.contains("audit.path parent does not exist"));
    assert!(!stderr.contains("secret-token"));
    assert!(!missing_parent.exists());

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_dir_all(missing_parent);
}

#[test]
fn cli_validate_config_rejects_missing_runtime_log_parent() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!(
            "CARGO_BIN_EXE_dfor77 is not set; skipping CLI validate runtime log filesystem test"
        );
        return;
    };
    let token_env = format!("DFOR77_VALIDATE_BAD_RUNTIME_TOKEN_{}", std::process::id());
    let config_path = std::env::temp_dir().join(format!(
        "dfor77-validate-bad-runtime-log-{}.toml",
        std::process::id()
    ));
    let audit_path = cli_config_audit_path("bad-runtime-log");
    let missing_parent = std::env::temp_dir().join(format!(
        "dfor77-missing-runtime-log-parent-{}",
        std::process::id()
    ));
    let runtime_log_path = missing_parent.join("runtime.jsonl");
    let _ = std::fs::remove_file(&audit_path);
    let _ = std::fs::remove_dir_all(&missing_parent);
    let config = format!(
        "{}\n[runtime]\nlog_path = \"{}\"\n",
        cli_timeout_config(&token_env, 100, &audit_path),
        runtime_log_path.display()
    );
    std::fs::write(&config_path, config).unwrap();

    let output = Command::new(runtime)
        .env(&token_env, "secret-token")
        .args(["--validate-config", "--config"])
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 validate config missing runtime log parent test");

    assert_eq!(output.status.code(), Some(12));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.is_empty());
    assert!(stderr.contains("runtime.log_path parent does not exist"));
    assert!(!stderr.contains("secret-token"));
    assert!(!missing_parent.exists());
    assert!(!audit_path.exists());

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(audit_path);
    let _ = std::fs::remove_dir_all(missing_parent);
}

#[test]
fn cli_validate_config_rejects_missing_session_parent() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI validate session dir test");
        return;
    };
    let token_env = format!("DFOR77_VALIDATE_BAD_SESSION_TOKEN_{}", std::process::id());
    let config_path = std::env::temp_dir().join(format!(
        "dfor77-validate-bad-session-{}.toml",
        std::process::id()
    ));
    let audit_path = cli_config_audit_path("bad-session");
    let missing_parent = std::env::temp_dir().join(format!(
        "dfor77-missing-session-parent-{}",
        std::process::id()
    ));
    let session_dir = missing_parent.join("sessions");
    let _ = std::fs::remove_file(&audit_path);
    let _ = std::fs::remove_dir_all(&missing_parent);
    std::fs::write(
        &config_path,
        cli_timeout_config(&token_env, 100, &audit_path),
    )
    .unwrap();

    let output = Command::new(runtime)
        .env(&token_env, "secret-token")
        .args(["--validate-config", "--config"])
        .arg(&config_path)
        .arg("--gateway-session-dir")
        .arg(&session_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 validate config missing session parent test");

    assert_eq!(output.status.code(), Some(12));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.is_empty());
    assert!(stderr.contains("--gateway-session-dir parent does not exist"));
    assert!(!stderr.contains("secret-token"));
    assert!(!missing_parent.exists());
    assert!(!audit_path.exists());

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(audit_path);
    let _ = std::fs::remove_dir_all(missing_parent);
}

#[test]
fn cli_config_emits_startup_warnings_without_failing() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI config warning test");
        return;
    };
    let token_env = format!("DFOR77_CONFIG_WARNING_TOKEN_{}", std::process::id());
    let config_path =
        std::env::temp_dir().join(format!("dfor77-config-warning-{}.toml", std::process::id()));
    let audit_path = cli_config_audit_path("warning");
    let _ = std::fs::remove_file(&audit_path);
    std::fs::write(&config_path, cli_warning_config(&token_env, &audit_path)).unwrap();

    let output = Command::new(runtime)
        .env(&token_env, "secret-token")
        .args(["--config"])
        .arg(&config_path)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run dfor77 config warning test");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("Usage:"));
    assert!(stderr.contains("dfor77 warn config: MESSAGE_CONTENT intent is not configured"));
    assert!(stderr.contains("dfor77 warn config: GUILD_MEMBERS intent is not configured"));
    assert!(stderr.contains("dry_run takes precedence"));
    assert!(!stdout.contains("secret-token"));
    assert!(!stderr.contains("secret-token"));

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(audit_path);
}

#[test]
fn cli_stdio_next_event_waits_for_configured_timeout() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI next event timeout test");
        return;
    };
    let token_env = format!("DFOR77_NXT_TIMEOUT_TOKEN_{}", std::process::id());
    let config_path =
        std::env::temp_dir().join(format!("dfor77-nxt-timeout-{}.toml", std::process::id()));
    let audit_path = cli_config_audit_path("nxt-timeout");
    let _ = std::fs::remove_file(&audit_path);
    std::fs::write(
        &config_path,
        cli_timeout_config(&token_env, 60, &audit_path),
    )
    .unwrap();
    let input = concat!(
        "@D7/1 REQ 1 INI 0\n",
        "@D7/1 END 1\n",
        "@D7/1 REQ 2 NXT 0\n",
        "@D7/1 END 2\n"
    );
    let start = std::time::Instant::now();
    let output = Command::new(runtime)
        .env(&token_env, "secret-token")
        .args(["--stdio-server", "--config"])
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(input.as_bytes())?;
            child.wait_with_output()
        })
        .expect("failed to run dfor77 next event timeout test");
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("@D7/1 RES 1 0 0"));
    assert!(stdout.contains("@D7/1 RES 2 51 0"));
    assert!(
        elapsed >= std::time::Duration::from_millis(40),
        "D7NXT returned too early after {:?}",
        elapsed
    );
    assert!(!stdout.contains("secret-token"));
    assert!(!stderr.contains("secret-token"));

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(audit_path);
}

#[test]
fn cli_bot_supervisor_hot_restart_file_restarts_idle_bot() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI hot restart test");
        return;
    };
    let token_env = format!("DFOR77_HOT_RESTART_TOKEN_{}", std::process::id());
    let base = format!("dfor77-hot-restart-{}", std::process::id());
    let config_path = std::env::temp_dir().join(format!("{base}.toml"));
    let audit_path = cli_config_audit_path("hot-restart");
    let marker_path = std::env::temp_dir().join(format!("{base}.marker"));
    let restart_path = std::env::temp_dir().join(format!("{base}.ctl"));
    let _ = std::fs::remove_file(&config_path);
    let _ = std::fs::remove_file(&audit_path);
    let _ = std::fs::remove_file(&marker_path);
    let _ = std::fs::remove_file(&restart_path);
    std::fs::write(&config_path, cli_timeout_config(&token_env, 0, &audit_path)).unwrap();

    let script = concat!(
        "marker=\"$1\"\n",
        "echo START >&2\n",
        "printf '@D7/1 REQ 1 INI 0\n@D7/1 END 1\n'\n",
        "while IFS= read -r line; do echo \"$line\" >&2; if [ \"$line\" = '@D7/1 END 1' ]; then break; fi; done\n",
        "if [ -f \"$marker\" ]; then\n",
        "  printf '@D7/1 REQ 2 END 0\n@D7/1 END 2\n'\n",
        "  while IFS= read -r line; do echo \"$line\" >&2; if [ \"$line\" = '@D7/1 END 2' ]; then break; fi; done\n",
        "  exit 0\n",
        "fi\n",
        "touch \"$marker\"\n",
        "while IFS= read -r dummy; do :; done\n",
        "exit 0\n"
    );

    let mut child = Command::new(runtime)
        .args(["--config"])
        .arg(&config_path)
        .arg("--hot-restart-file")
        .arg(&restart_path)
        .arg("--bot")
        .arg("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("hot-restart-bot")
        .arg(&marker_path)
        .env(&token_env, "secret-token")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn dfor77 hot restart supervisor");

    let marker_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !marker_path.exists() {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            panic!(
                "hot restart supervisor exited before marker\nstderr:\n{}\nstdout:\n{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
        }
        assert!(
            std::time::Instant::now() < marker_deadline,
            "first bot did not reach hot restart marker"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::write(&restart_path, "restart").unwrap();

    let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= exit_deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stderr:\n{}\nstdout:\n{}",
        stderr,
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.matches("START").count() >= 2, "{stderr}");
    assert!(stderr.contains("hot restart requested"), "{stderr}");
    assert!(!stderr.contains("secret-token"));

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(audit_path);
    let _ = std::fs::remove_file(marker_path);
    let _ = std::fs::remove_file(restart_path);
}

#[test]
fn cli_mock_rest_server_serves_gateway_and_message_routes() {
    let _guard = integration_test_lock();
    let Some(runtime) = option_env!("CARGO_BIN_EXE_dfor77") else {
        eprintln!("CARGO_BIN_EXE_dfor77 is not set; skipping CLI mock REST server test");
        return;
    };
    let Some(addr) = reserve_local_addr() else {
        return;
    };
    let child = Command::new(runtime)
        .args([
            "--mock-rest-server",
            &addr,
            "--mock-gateway-url",
            "ws://127.0.0.1:65535/gateway",
            "--mock-rest-max-requests",
            "9",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn dfor77 mock REST server");

    let gateway = send_raw_http(
        &addr,
        "GET /api/v10/gateway/bot HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(gateway.contains("HTTP/1.1 200 OK"));
    assert!(gateway.contains("\"url\":\"ws://127.0.0.1:65535/gateway\""));

    let current_user = send_raw_http(
        &addr,
        "GET /api/v10/users/@me HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(current_user.contains("HTTP/1.1 200 OK"));
    assert!(current_user.contains("\"id\":\"100000000000000009\""));

    let channel = send_raw_http(
        &addr,
        "GET /api/v10/channels/100000000000000001 HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(channel.contains("HTTP/1.1 200 OK"));
    assert!(channel.contains("\"type\":0"));

    let member = send_raw_http(
        &addr,
        "GET /api/v10/guilds/100000000000000001/members/100000000000000042 HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(member.contains("HTTP/1.1 200 OK"));
    assert!(member.contains("\"id\":\"100000000000000042\""));

    let roles = send_raw_http(
        &addr,
        "GET /api/v10/guilds/100000000000000001/roles HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(roles.contains("HTTP/1.1 200 OK"));
    assert!(roles.contains("\"permissions\":\"1099511702528\""));

    let fetched_message = send_raw_http(
        &addr,
        "GET /api/v10/channels/100000000000000001/messages/100000000000000002 HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(fetched_message.contains("HTTP/1.1 200 OK"));
    assert!(fetched_message.contains("\"id\":\"100000000000000002\""));
    assert!(fetched_message.contains("\"channel_id\":\"100000000000000001\""));

    let thread_member = send_raw_http(
        &addr,
        "GET /api/v10/channels/100000000000000010/thread-members/100000000000000009 HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(thread_member.contains("HTTP/1.1 200 OK"));
    assert!(thread_member.contains("\"user_id\":\"100000000000000009\""));

    let archived_threads = send_raw_http(
        &addr,
        "GET /api/v10/channels/100000000000000010/users/@me/threads/archived/private?limit=50 HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(archived_threads.contains("HTTP/1.1 200 OK"));
    assert!(archived_threads.contains("\"threads\""));
    assert!(archived_threads.contains("\"parent_id\":\"100000000000000010\""));

    let message = send_raw_http(
        &addr,
        concat!(
            "POST /api/v10/channels/100000000000000001/messages HTTP/1.1\r\n",
            "Host: local\r\n",
            "Content-Type: application/json\r\n",
            "Content-Length: 19\r\n",
            "Connection: close\r\n\r\n",
            "{\"content\":\"hello\"}"
        ),
    );
    assert!(message.contains("HTTP/1.1 200 OK"));
    assert!(message.contains("\"id\":\"100000000000000099\""));

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn request(id: u64, verb: &str, fields: Vec<Field>) -> Request {
    Request {
        id,
        verb: verb.to_string(),
        fields,
    }
}

fn reserve_local_addr() -> Option<String> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("cannot bind local TCP listener; skipping CLI mock REST server test");
            return None;
        }
        Err(error) => panic!("failed to reserve local addr: {error}"),
    };
    Some(listener.local_addr().unwrap().to_string())
}

fn send_raw_http(addr: &str, request: &str) -> String {
    let mut stream = connect_with_retry(addr);
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    String::from_utf8_lossy(&bytes).to_string()
}

fn connect_with_retry(addr: &str) -> TcpStream {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => return stream,
            Err(error) if std::time::Instant::now() < deadline => {
                if error.kind() != std::io::ErrorKind::ConnectionRefused {
                    panic!("failed to connect to mock REST server: {error}");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("failed to connect to mock REST server: {error}"),
        }
    }
}

fn cli_warning_config(token_env: &str, audit_path: &std::path::Path) -> String {
    cli_config(
        token_env,
        r#""GUILDS", "GUILD_MESSAGES""#,
        100,
        true,
        true,
        audit_path,
    )
}

fn cli_timeout_config(
    token_env: &str,
    next_event_timeout_ms: u64,
    audit_path: &std::path::Path,
) -> String {
    cli_config(
        token_env,
        r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        next_event_timeout_ms,
        false,
        false,
        audit_path,
    )
}

fn cli_config(
    token_env: &str,
    intents: &str,
    next_event_timeout_ms: u64,
    dry_run: bool,
    log_only: bool,
    audit_path: &std::path::Path,
) -> String {
    format!(
        r#"
[discord]
token_env = "{}"
api_version = 10
intents = [{}]

[gateway]
encoding = "json"
compression = "none"
shard_id = 0
shard_count = 1
identify_max_wait_ms = 600000
daily_identify_warn = 800
daily_identify_stop = 950
live = false
next_event_timeout_ms = {}

[rest]
request_timeout_ms = 30000
http_timeout_ms = 10000
timeout_margin_ms = 1000
delete_max_wait_ms = 10000
timeout_max_wait_ms = 10000
send_max_wait_ms = 10000
read_max_wait_ms = 10000
base_url = "https://discord.com"
global_max_wait_ms = 10000
invalid_budget_warn = 6000
invalid_budget_recover = 7000
invalid_budget_stop = 8000

[snapshot]
event_queue_capacity = 1024
message_snapshot_ttl_ms = 3600000
message_snapshot_max_entries = 100000
message_tombstone_ttl_ms = 3600000
message_tombstone_max_entries = 100000
member_snapshot_ttl_ms = 3600000
member_snapshot_max_entries = 100000
member_tombstone_ttl_ms = 3600000
member_tombstone_max_entries = 100000
bot_member_snapshot_ttl_ms = 3600000
thread_member_snapshot_ttl_ms = 3600000
resource_snapshot_max_entries = 100000
resource_tombstone_ttl_ms = 3600000
resource_tombstone_max_entries = 100000

[audit]
path = "{}"
content_mode = "none"
content_preview_bytes = 0
hash_key_env = ""

[mode]
dry_run = {}
log_only = {}
"#,
        token_env,
        intents,
        next_event_timeout_ms,
        audit_path.display(),
        dry_run,
        log_only
    )
}

fn cli_config_audit_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("dfor77-{label}-audit-{}.jsonl", std::process::id()))
}

fn serve_once(
    status: u16,
    body: &str,
) -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    serve_sequence(vec![(status, body.to_string())])
}

fn serve_sequence(
    responses: Vec<(u16, String)>,
) -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("cannot bind local TCP listener; skipping runtime REST integration test");
            return None;
        }
        Err(error) => panic!("failed to bind mock REST listener: {error}"),
    };
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for (status, body) in responses {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "REST fixture timed out"
                        );
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("REST fixture accept failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(1)))
                .unwrap();
            let request = read_http_request(&mut stream);
            tx.send(request).unwrap();
            write!(
                stream,
                "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    Some((format!("http://{}", addr), rx, handle))
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

fn serve_websocket_gateway() -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    serve_websocket_gateway_hold(0)
}

fn serve_websocket_gateway_hold(
    hold_ms: u64,
) -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("cannot bind local TCP listener; skipping websocket gateway test");
            return None;
        }
        Err(error) => panic!("failed to bind websocket gateway listener: {error}"),
    };
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let accept = websocket_accept_key(&request);
        tx.send(request).unwrap();
        let response = format!(
            concat!(
                "HTTP/1.1 101 Switching Protocols\r\n",
                "Upgrade: websocket\r\n",
                "Connection: Upgrade\r\n",
                "Sec-WebSocket-Accept: {}\r\n\r\n"
            ),
            accept
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream
            .write_all(&server_text_frame(
                r#"{"op":10,"d":{"heartbeat_interval":1000}}"#,
            ))
            .unwrap();
        let identify = read_client_ws_text(&mut stream);
        tx.send(identify).unwrap();
        stream
            .write_all(&server_text_frame(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
            ))
            .unwrap();
        if hold_ms > 0 {
            thread::sleep(std::time::Duration::from_millis(hold_ms));
        }
        if let Some(code) = read_client_ws_close_code(&mut stream) {
            let _ = tx.send(format!("close:{code}"));
        }
    });
    Some((format!("ws://{addr}/gateway"), rx, handle))
}

fn websocket_accept_key(request: &str) -> String {
    let key = request
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("sec-websocket-key"))
        .map(|(_, value)| value.trim())
        .unwrap_or("");
    tungstenite::handshake::derive_accept_key(key.as_bytes())
}

fn server_text_frame(payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    let mut out = Vec::new();
    out.push(0x81);
    if bytes.len() <= 125 {
        out.push(bytes.len() as u8);
    } else {
        out.push(126);
        out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(bytes);
    out
}

fn read_client_ws_text(stream: &mut TcpStream) -> String {
    let mut head = [0_u8; 2];
    stream.read_exact(&mut head).unwrap();
    let opcode = head[0] & 0x0F;
    let masked = head[1] & 0x80 != 0;
    let mut len = usize::from(head[1] & 0x7F);
    if len == 126 {
        let mut bytes = [0_u8; 2];
        stream.read_exact(&mut bytes).unwrap();
        len = usize::from(u16::from_be_bytes(bytes));
    }
    let mut mask = [0_u8; 4];
    if masked {
        stream.read_exact(&mut mask).unwrap();
    }
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).unwrap();
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    assert_eq!(opcode, 0x1);
    String::from_utf8(payload).unwrap()
}

fn read_client_ws_close_code(stream: &mut TcpStream) -> Option<u16> {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(50)));
    let mut head = [0_u8; 2];
    stream.read_exact(&mut head).ok()?;
    let opcode = head[0] & 0x0F;
    let masked = head[1] & 0x80 != 0;
    let mut len = usize::from(head[1] & 0x7F);
    if len == 126 {
        let mut bytes = [0_u8; 2];
        stream.read_exact(&mut bytes).ok()?;
        len = usize::from(u16::from_be_bytes(bytes));
    }
    let mut mask = [0_u8; 4];
    if masked {
        stream.read_exact(&mut mask).ok()?;
    }
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).ok()?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    if opcode != 0x8 || payload.len() < 2 {
        return None;
    }
    Some(u16::from_be_bytes([payload[0], payload[1]]))
}

fn read_ipc_response<R: BufRead>(reader: &mut R, id: u64) -> String {
    let mut out = String::new();
    let expected_end = format!("@D7/1 END {id}");
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).unwrap();
        if read == 0 {
            break;
        }
        let trimmed = line.trim_end().to_string();
        out.push_str(&line);
        if trimmed == expected_end {
            break;
        }
    }
    out
}
