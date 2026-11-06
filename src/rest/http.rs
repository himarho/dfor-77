use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};

use super::{extract_discord_error_code, RestResponse, RestTransportError};

pub(super) const MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_HTTP_HEAD_BYTES: usize = 64 * 1024;

pub(super) fn read_response(stream: &mut impl Read) -> Result<RestResponse, RestTransportError> {
    let mut reader = ResponseReader {
        stream: BufReader::new(stream),
        line: Vec::new(),
        consumed: 0,
    };
    let mut head_bytes = 0;
    let (status, headers) = loop {
        let (status, headers) = reader.read_head(MAX_HTTP_HEAD_BYTES + 4 - head_bytes)?;
        head_bytes = reader.consumed;
        if !(100..200).contains(&status) {
            break (status, headers);
        }
        if status == 101
            || headers.contains_key("content-length")
            || headers.contains_key("transfer-encoding")
        {
            return Err(RestTransportError::ConnectionResetAfterSend);
        }
    };
    let mut body = Vec::new();
    if !matches!(status, 204 | 304) {
        if headers.contains_key("transfer-encoding") {
            reader.read_chunks(&mut body)?;
        } else if let Some(length) = headers.get("content-length") {
            let length = length
                .parse::<usize>()
                .map_err(|_| RestTransportError::ConnectionResetAfterSend)?;
            reader.read_body(length, &mut body)?;
        } else {
            reader.read_until_close(&mut body)?;
        }
    }
    let body = String::from_utf8(body).map_err(|_| RestTransportError::ConnectionResetAfterSend)?;
    Ok(RestResponse {
        http_status: status,
        discord_error_code: extract_discord_error_code(&body),
        body,
        headers,
    })
}

struct ResponseReader<R> {
    stream: BufReader<R>,
    line: Vec<u8>,
    consumed: usize,
}

impl<R: Read> ResponseReader<R> {
    fn read_line(&mut self, limit: usize) -> Result<&[u8], RestTransportError> {
        self.line.clear();
        loop {
            let buffer = match self.stream.fill_buf() {
                Ok(buffer) => buffer,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(RestTransportError::TimeoutAfterSend),
            };
            if buffer.is_empty() {
                return Err(RestTransportError::ConnectionResetAfterSend);
            }
            let count = buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|position| position + 1)
                .unwrap_or(buffer.len());
            if count > limit.saturating_sub(self.line.len())
                || count > MAX_HTTP_RESPONSE_BYTES.saturating_sub(self.consumed)
            {
                return Err(RestTransportError::ConnectionResetAfterSend);
            }
            let complete = buffer[count - 1] == b'\n';
            self.line.extend_from_slice(&buffer[..count]);
            self.stream.consume(count);
            self.consumed += count;
            if complete {
                if !self.line.ends_with(b"\r\n") {
                    return Err(RestTransportError::ConnectionResetAfterSend);
                }
                self.line.truncate(self.line.len() - 2);
                return Ok(&self.line);
            }
        }
    }

    fn read_head(
        &mut self,
        limit: usize,
    ) -> Result<(u16, HashMap<String, String>), RestTransportError> {
        let start = self.consumed;
        let status = parse_status_line(self.read_line(limit)?)?;
        let mut headers = HashMap::new();
        loop {
            let remaining = limit.saturating_sub(self.consumed - start);
            let line = self.read_line(remaining)?;
            if line.is_empty() {
                break;
            }
            let (name, value) = parse_header_line(line)?;
            if matches!(name.as_str(), "content-length" | "transfer-encoding")
                && headers.contains_key(&name)
            {
                return Err(RestTransportError::ConnectionResetAfterSend);
            }
            headers.insert(name, value);
        }
        if let Some(encoding) = headers.get("transfer-encoding") {
            if headers.contains_key("content-length") || !encoding.eq_ignore_ascii_case("chunked") {
                return Err(RestTransportError::ConnectionResetAfterSend);
            }
        }
        if let Some(length) = headers.get("content-length") {
            if length.is_empty()
                || !length.bytes().all(|byte| byte.is_ascii_digit())
                || length.parse::<usize>().map_or(true, |length| {
                    length > MAX_HTTP_RESPONSE_BYTES.saturating_sub(self.consumed)
                })
            {
                return Err(RestTransportError::ConnectionResetAfterSend);
            }
        }
        Ok((status, headers))
    }

    fn read_body(
        &mut self,
        mut remaining: usize,
        body: &mut Vec<u8>,
    ) -> Result<(), RestTransportError> {
        if remaining > MAX_HTTP_RESPONSE_BYTES.saturating_sub(self.consumed) {
            return Err(RestTransportError::ConnectionResetAfterSend);
        }
        while remaining > 0 {
            let buffer = match self.stream.fill_buf() {
                Ok(buffer) => buffer,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(RestTransportError::TimeoutAfterSend),
            };
            if buffer.is_empty() {
                return Err(RestTransportError::ConnectionResetAfterSend);
            }
            let count = remaining.min(buffer.len());
            body.extend_from_slice(&buffer[..count]);
            self.stream.consume(count);
            self.consumed += count;
            remaining -= count;
        }
        Ok(())
    }

    fn read_until_close(&mut self, body: &mut Vec<u8>) -> Result<(), RestTransportError> {
        loop {
            let buffer = match self.stream.fill_buf() {
                Ok(buffer) => buffer,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(RestTransportError::TimeoutAfterSend),
            };
            let count = buffer.len();
            if count == 0 {
                return Ok(());
            }
            if count > MAX_HTTP_RESPONSE_BYTES.saturating_sub(self.consumed) {
                return Err(RestTransportError::ConnectionResetAfterSend);
            }
            body.extend_from_slice(buffer);
            self.stream.consume(count);
            self.consumed += count;
        }
    }

    fn read_chunks(&mut self, body: &mut Vec<u8>) -> Result<(), RestTransportError> {
        loop {
            let size = parse_chunk_size(self.read_line(MAX_HTTP_HEAD_BYTES)?)?;
            if size == 0 {
                let start = self.consumed;
                loop {
                    let remaining = MAX_HTTP_HEAD_BYTES.saturating_sub(self.consumed - start);
                    let line = self.read_line(remaining)?;
                    if line.is_empty() {
                        return Ok(());
                    }
                    let (name, _) = parse_header_line(line)?;
                    if matches!(name.as_str(), "content-length" | "transfer-encoding") {
                        return Err(RestTransportError::ConnectionResetAfterSend);
                    }
                }
            }
            self.read_body(size, body)?;
            if !self.read_line(2)?.is_empty() {
                return Err(RestTransportError::ConnectionResetAfterSend);
            }
        }
    }
}

fn parse_status_line(line: &[u8]) -> Result<u16, RestTransportError> {
    let code = line
        .strip_prefix(b"HTTP/1.1 ")
        .or_else(|| line.strip_prefix(b"HTTP/1.0 "))
        .ok_or(RestTransportError::ConnectionResetAfterSend)?;
    if code.len() < 3
        || !code[..3].iter().all(u8::is_ascii_digit)
        || (code.len() > 3 && code[3] != b' ')
        || !valid_header_value_bytes(code.get(4..).unwrap_or_default())
    {
        return Err(RestTransportError::ConnectionResetAfterSend);
    }
    let status = u16::from(code[0] - b'0') * 100
        + u16::from(code[1] - b'0') * 10
        + u16::from(code[2] - b'0');
    if !(100..=599).contains(&status) {
        return Err(RestTransportError::ConnectionResetAfterSend);
    }
    Ok(status)
}

fn parse_header_line(line: &[u8]) -> Result<(String, String), RestTransportError> {
    let colon = line
        .iter()
        .position(|byte| *byte == b':')
        .ok_or(RestTransportError::ConnectionResetAfterSend)?;
    let name = &line[..colon];
    let value = &line[colon + 1..];
    if name.is_empty()
        || !name.iter().all(|byte| is_token(*byte))
        || !valid_header_value_bytes(value)
    {
        return Err(RestTransportError::ConnectionResetAfterSend);
    }
    let name = String::from_utf8(name.to_ascii_lowercase())
        .map_err(|_| RestTransportError::ConnectionResetAfterSend)?;
    let value = std::str::from_utf8(value)
        .map_err(|_| RestTransportError::ConnectionResetAfterSend)?
        .trim_matches([' ', '\t'])
        .to_string();
    Ok((name, value))
}

pub(super) fn valid_header_value(value: &str) -> bool {
    valid_header_value_bytes(value.as_bytes())
}

fn valid_header_value_bytes(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| *byte == b'\t' || (*byte >= b' ' && *byte != 0x7F))
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn parse_chunk_size(line: &[u8]) -> Result<usize, RestTransportError> {
    let digits = line
        .iter()
        .take_while(|byte| byte.is_ascii_hexdigit())
        .count();
    if digits == 0 {
        return Err(RestTransportError::ConnectionResetAfterSend);
    }
    let mut size = 0_usize;
    for byte in &line[..digits] {
        let value = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => byte - b'A' + 10,
        };
        size = size
            .checked_mul(16)
            .and_then(|size| size.checked_add(usize::from(value)))
            .ok_or(RestTransportError::ConnectionResetAfterSend)?;
    }
    let mut rest = &line[digits..];
    while !rest.is_empty() {
        rest = trim_ows(rest);
        if rest.is_empty() {
            break;
        }
        rest = rest
            .strip_prefix(b";")
            .ok_or(RestTransportError::ConnectionResetAfterSend)?;
        rest = trim_ows(rest);
        let name_len = rest.iter().take_while(|byte| is_token(**byte)).count();
        if name_len == 0 {
            return Err(RestTransportError::ConnectionResetAfterSend);
        }
        rest = trim_ows(&rest[name_len..]);
        if let Some(value) = rest.strip_prefix(b"=") {
            rest = trim_ows(value);
            if rest.first() == Some(&b'"') {
                rest = quoted_extension(rest)?;
            } else {
                let value_len = rest.iter().take_while(|byte| is_token(**byte)).count();
                if value_len == 0 {
                    return Err(RestTransportError::ConnectionResetAfterSend);
                }
                rest = &rest[value_len..];
            }
        }
    }
    Ok(size)
}

fn trim_ows(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b' ' | b'\t')) {
        bytes = &bytes[1..];
    }
    bytes
}

fn quoted_extension(bytes: &[u8]) -> Result<&[u8], RestTransportError> {
    let mut index = 1;
    while let Some(byte) = bytes.get(index) {
        match byte {
            b'"' => return Ok(&bytes[index + 1..]),
            b'\\' => {
                index += 1;
                if !bytes
                    .get(index)
                    .is_some_and(|byte| *byte == b'\t' || (*byte >= b' ' && *byte != 0x7F))
                {
                    return Err(RestTransportError::ConnectionResetAfterSend);
                }
            }
            b'\t' | b' ' | 0x21 | 0x23..=0x5B | 0x5D..=0x7E | 0x80..=0xFF => {}
            _ => return Err(RestTransportError::ConnectionResetAfterSend),
        }
        index += 1;
    }
    Err(RestTransportError::ConnectionResetAfterSend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    struct Segmented<'a> {
        bytes: &'a [u8],
        maximum: usize,
        interrupt: bool,
    }

    impl Read for Segmented<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self.bytes.is_empty() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let count = buffer.len().min(self.maximum).min(self.bytes.len());
            buffer[..count].copy_from_slice(&self.bytes[..count]);
            self.bytes = &self.bytes[count..];
            Ok(count)
        }
    }

    #[test]
    fn informational_responses_and_chunk_extensions_survive_arbitrary_fragmentation() {
        let wire = concat!(
            "HTTP/1.1 100 Continue\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\nX-RateLimit-Remaining: 999\r\n\r\n",
            "HTTP/1.1 403 Forbidden\r\nTransfer-Encoding: chunked\r\n",
            "X-RateLimit-Remaining: 0\r\n\r\n",
            "e ; name = \"a;\\\"b\\\\c\";flag\r\n{\"code\":50013}\r\n",
            "0;last\r\nX-Note: ignored\r\n\r\n"
        );
        for maximum in 1..=wire.len() {
            let response = read_response(&mut Segmented {
                bytes: wire.as_bytes(),
                maximum,
                interrupt: false,
            })
            .unwrap();
            assert_eq!(response.http_status, 403);
            assert_eq!(response.body, r#"{"code":50013}"#);
            assert_eq!(response.discord_error_code, Some(50013));
            assert_eq!(
                response
                    .headers
                    .get("x-ratelimit-remaining")
                    .map(String::as_str),
                Some("0")
            );
            assert!(!response.headers.contains_key("x-note"));
        }
    }

    #[test]
    fn premature_eof_is_rejected_at_every_byte_of_a_chunked_response() {
        let wire = concat!(
            "HTTP/1.1 103 Early Hints\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            "1\r\nx\r\n0\r\nX-Trailer: ignored\r\n\r\n"
        );
        for end in 0..wire.len() {
            assert!(
                read_response(&mut Cursor::new(&wire.as_bytes()[..end])).is_err(),
                "{end}"
            );
        }
        assert_eq!(read_response(&mut Cursor::new(wire)).unwrap().body, "x");
    }

    #[test]
    fn malformed_chunks_and_trailers_are_rejected() {
        for body in [
            "+1\r\nx\r\n0\r\n\r\n",
            " 1\r\nx\r\n0\r\n\r\n",
            "1z\r\nx\r\n0\r\n\r\n",
            "1;\r\nx\r\n0\r\n\r\n",
            "1;n=\r\nx\r\n0\r\n\r\n",
            "1;n=\"unterminated\r\nx\r\n0\r\n\r\n",
            "1;n=\"bad\u{7f}\"\r\nx\r\n0\r\n\r\n",
            "1\r\nx\n0\r\n\r\n",
            "ffffffffffffffffffffffff\r\n",
            "0\r\nMissingColon\r\n\r\n",
            "0\r\nContent-Length: 0\r\n\r\n",
            "0\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            let wire = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{body}");
            assert_eq!(
                read_response(&mut Cursor::new(wire)).unwrap_err(),
                RestTransportError::ConnectionResetAfterSend,
                "{body:?}"
            );
        }
    }

    #[test]
    fn malformed_headers_and_unsolicited_protocol_switches_are_rejected() {
        for head in [
            "HTTP/1.1 200 OK\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1\t200 OK\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\n Content-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length : 0\r\n\r\n",
            "HTTP/1.1 200 Bad\rReason\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 101 Switching Protocols\r\n\r\n",
            "HTTP/1.1 100 Continue\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            assert_eq!(
                read_response(&mut Cursor::new(head)).unwrap_err(),
                RestTransportError::ConnectionResetAfterSend,
                "{head:?}"
            );
        }
    }

    #[test]
    fn empty_chunk_ends_before_any_subsequent_message() {
        let wire = concat!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            "HTTP/1.1 500 Unexpected\r\nContent-Length: 0\r\n\r\n"
        );
        assert!(read_response(&mut Cursor::new(wire))
            .unwrap()
            .body
            .is_empty());
        for status in [204, 304] {
            let wire = format!("HTTP/1.1 {status}\r\n\r\n");
            assert!(read_response(&mut Segmented {
                bytes: wire.as_bytes(),
                maximum: 1,
                interrupt: false,
            })
            .unwrap()
            .body
            .is_empty());
        }
    }

    #[test]
    fn informational_heads_chunk_lines_and_trailers_have_separate_bounded_work() {
        let informational = "HTTP/1.1 103 Early Hints\r\n\r\n".repeat(3000);
        let long = "x".repeat(MAX_HTTP_HEAD_BYTES + 1);
        for wire in [
            informational,
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;{long}\r\n"),
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nX-Trailer: {long}\r\n\r\n"),
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n", MAX_HTTP_RESPONSE_BYTES),
        ] {
            assert_eq!(read_response(&mut Segmented {
                bytes: wire.as_bytes(), maximum: 4096, interrupt: false,
            }).unwrap_err(), RestTransportError::ConnectionResetAfterSend);
        }
    }

    #[test]
    fn many_small_chunks_preserve_every_body_byte() {
        let mut wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for _ in 0..100_000 {
            wire.extend_from_slice(b"1\r\nx\r\n");
        }
        wire.extend_from_slice(b"0\r\n\r\n");
        let response = read_response(&mut Cursor::new(wire)).unwrap();
        assert_eq!(response.body.as_bytes(), vec![b'x'; 100_000]);
    }

    #[test]
    fn close_delimited_and_content_length_bodies_keep_the_total_wire_limit() {
        for mode in ["", "Content-Length: LENGTH\r\n"] {
            let mut length = MAX_HTTP_RESPONSE_BYTES;
            let head = loop {
                let head = format!(
                    "HTTP/1.1 200 OK\r\n{}\r\n",
                    mode.replace("LENGTH", &length.to_string())
                );
                let next = MAX_HTTP_RESPONSE_BYTES - head.len();
                if next == length {
                    break head;
                }
                length = next;
            };
            let mut wire = head.into_bytes();
            wire.resize(MAX_HTTP_RESPONSE_BYTES, b'x');
            assert_eq!(
                read_response(&mut Cursor::new(&wire)).unwrap().body.len(),
                length
            );
            if mode.is_empty() {
                wire.push(b'x');
                assert_eq!(
                    read_response(&mut Cursor::new(wire)).unwrap_err(),
                    RestTransportError::ConnectionResetAfterSend
                );
            }
        }
    }
}
