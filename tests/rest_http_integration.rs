use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use dfor77::constants::D7UNC;
use dfor77::rest::{CommitState, HttpMethod, RestBackend, RestPlanner, RestTransportError};
use dfor77::{HttpRestBackend, InvalidRequestBudget, RestExecutor, D7FOR, D7OK};

#[test]
fn manually_constructed_requests_cannot_bypass_the_backend_path_boundary() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut backend = HttpRestBackend::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        "local-path-test-token",
        "path-test",
        100,
    )
    .unwrap();
    let mut request = RestPlanner::new()
        .raw_json_request(HttpMethod::Get, "/api/v10/gateway", None, "")
        .request
        .unwrap();
    for path in [
        "/api/v10/../v9/gateway",
        "/api/v10/%2e%2e/v9/gateway",
        "/api/v10/webhooks/50/secret%2Fmessages%2F30",
        "/api/v10/%252e%252e/v9/gateway",
    ] {
        request.path = path.into();
        assert_eq!(
            backend.execute(&request).unwrap_err(),
            RestTransportError::NetworkBeforeSend
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn http_backend_sends_common_headers_and_json_body() {
    let Some((base_url, received, server)) = serve_once(
        200,
        &[("X-RateLimit-Bucket", "bucket-a")],
        r#"{"id":"100000000000000099"}"#,
    ) else {
        return;
    };
    let backend = HttpRestBackend::new(&base_url, "secret-token", "dfor77-test", 1000).unwrap();
    let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
    let mut executor = RestExecutor::new(backend, budget);

    let outcome = executor.execute_send_message("100000000000000001", "hello", 0);
    assert_eq!(outcome.status, D7OK);

    let request = received.recv().unwrap();
    assert!(request.starts_with("POST /api/v10/channels/100000000000000001/messages HTTP/1.1"));
    assert!(request.contains("\r\nAuthorization: Bot secret-token\r\n"));
    assert!(request.contains("\r\nUser-Agent: dfor77-test\r\n"));
    assert!(request.contains("\r\nContent-Type: application/json\r\n"));
    assert!(request.contains("\"allowed_mentions\":{\"parse\":[]}"));
    assert!(request.contains("\"enforce_nonce\":true"));
    server.join().unwrap();
}

#[test]
fn http_backend_maps_discord_error_response() {
    let Some((base_url, received, server)) = serve_once(
        403,
        &[],
        r#"{"code":50013,"message":"Missing Permissions"}"#,
    ) else {
        return;
    };
    let backend = HttpRestBackend::new(&base_url, "secret-token", "dfor77-test", 1000).unwrap();
    let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
    let mut executor = RestExecutor::new(backend, budget);

    let outcome =
        executor.execute_delete_message("100000000000000001", "100000000000000002", "because", 0);
    assert_eq!(outcome.status, D7FOR);
    assert_eq!(outcome.why, "MISSING_PERMISSION");

    let request = received.recv().unwrap();
    assert!(request.starts_with(
        "DELETE /api/v10/channels/100000000000000001/messages/100000000000000002 HTTP/1.1"
    ));
    assert!(request.contains("\r\nX-Audit-Log-Reason: because\r\n"));
    server.join().unwrap();
}

#[test]
fn http_backend_maps_chunked_discord_error_response() {
    let Some((base_url, received, server)) = serve_chunked_once(
        403,
        &[r#"{"code":50013,"#, r#""message":"Missing Permissions"}"#],
    ) else {
        return;
    };
    let backend = HttpRestBackend::new(&base_url, "secret-token", "dfor77-test", 1000).unwrap();
    let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
    let mut executor = RestExecutor::new(backend, budget);

    let outcome =
        executor.execute_delete_message("100000000000000001", "100000000000000002", "", 0);
    assert_eq!(outcome.status, D7FOR);
    assert_eq!(outcome.why, "MISSING_PERMISSION");

    let request = received.recv().unwrap();
    assert!(request.starts_with(
        "DELETE /api/v10/channels/100000000000000001/messages/100000000000000002 HTTP/1.1"
    ));
    server.join().unwrap();
}

#[test]
fn http_backend_sends_percent_encoded_audit_reason() {
    let Some((base_url, received, server)) = serve_once(204, &[], "") else {
        return;
    };
    let backend = HttpRestBackend::new(&base_url, "secret-token", "dfor77-test", 1000).unwrap();
    let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
    let mut executor = RestExecutor::new(backend, budget);

    let outcome = executor.execute_untimeout_member(
        "100000000000000001",
        "100000000000000002",
        "hello world",
        0,
    );
    assert_eq!(outcome.status, D7OK);

    let request = received.recv().unwrap();
    assert!(request.starts_with(
        "PATCH /api/v10/guilds/100000000000000001/members/100000000000000002 HTTP/1.1"
    ));
    assert!(request.contains("\r\nX-Audit-Log-Reason: hello%20world\r\n"));
    assert!(request.contains("\"communication_disabled_until\":null"));
    server.join().unwrap();
}

#[test]
fn http_backend_uses_the_final_response_after_informational_heads() {
    let wire = concat!(
        "HTTP/1.1 100 Continue\r\n\r\n",
        "HTTP/1.1 103 Early Hints\r\nX-RateLimit-Remaining: 999\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 14\r\nX-RateLimit-Remaining: 1\r\n\r\n",
        "{\"id\":\"12345\"}"
    );
    let Some((base_url, received, server)) = serve_raw_once(wire.as_bytes().to_vec()) else {
        return;
    };
    let backend = HttpRestBackend::new(&base_url, "secret-token", "dfor77-test", 1000).unwrap();
    let mut executor = RestExecutor::new(backend, InvalidRequestBudget::new(2, 3, 4).unwrap());
    let outcome = executor.execute_send_message("100000000000000001", "hello", 0);
    assert_eq!(outcome.status, D7OK);
    assert_eq!(outcome.http_status, 200);
    assert_eq!(outcome.created_message_id.as_deref(), Some("12345"));
    assert_eq!(outcome.commit_state, CommitState::Committed);
    assert!(received
        .recv()
        .unwrap()
        .starts_with("POST /api/v10/channels/"));
    server.join().unwrap();
}

#[test]
fn malformed_chunked_response_to_a_mutation_remains_an_unknown_commit() {
    let wire = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n+1\r\nx\r\n0\r\n\r\n";
    let Some((base_url, received, server)) = serve_raw_once(wire.as_bytes().to_vec()) else {
        return;
    };
    let backend = HttpRestBackend::new(&base_url, "secret-token", "dfor77-test", 1000).unwrap();
    let mut executor = RestExecutor::new(backend, InvalidRequestBudget::new(2, 3, 4).unwrap());
    let outcome = executor.execute_send_message("100000000000000001", "hello", 0);
    assert_eq!(outcome.status, D7UNC);
    assert_eq!(outcome.commit_state, CommitState::Unknown);
    assert!(received
        .recv()
        .unwrap()
        .starts_with("POST /api/v10/channels/"));
    server.join().unwrap();
}

fn serve_once(
    status: u16,
    headers: &[(&str, &str)],
    body: &str,
) -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let mut response = format!("HTTP/1.1 {status} Test\r\n");
    for (key, value) in headers {
        response.push_str(&format!("{key}: {value}\r\n"));
    }
    response.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    serve_raw_once(response.into_bytes())
}

fn serve_chunked_once(
    status: u16,
    chunks: &[&str],
) -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let mut response = format!(
        "HTTP/1.1 {status} Test\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    for chunk in chunks {
        response.push_str(&format!("{:X}\r\n{chunk}\r\n", chunk.len()));
    }
    response.push_str("0\r\n\r\n");
    serve_raw_once(response.into_bytes())
}

fn serve_raw_once(
    response: Vec<u8>,
) -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("cannot bind local TCP listener; skipping HTTP integration test");
            return None;
        }
        Err(error) => panic!("failed to bind mock REST listener: {error}"),
    };
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        tx.send(request).unwrap();
        stream.write_all(&response).unwrap();
    });
    Some((format!("http://{addr}"), rx, handle))
}

fn read_http_request(stream: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    let mut content_length = None;
    loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if content_length.is_none() {
            if let Some((head, _)) = split_head_body(&bytes) {
                content_length = parse_content_length(head);
            }
        }
        if let Some((head, body_start)) = split_head_body(&bytes) {
            let needed = parse_content_length(head).unwrap_or(content_length.unwrap_or(0));
            if bytes.len().saturating_sub(body_start) >= needed {
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
    let headers = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect::<HashMap<_, _>>();
    headers.get("content-length")?.parse().ok()
}
