use std::fmt;
use std::io::{self, BufRead, Write};

use crate::validation::is_unreserved;

pub const MAX_FRAME_BYTES: usize = 65_536;
pub const MAX_FIELDS: usize = 64;
pub const MAX_KEY_BYTES: usize = 32;
pub const MAX_VALUE_ENCODED_BYTES: usize = 60_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Encoding {
    Raw,
    Percent,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Field {
    pub key: String,
    pub encoding: Encoding,
    pub value: String,
}

impl fmt::Debug for Field {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Field")
            .field("key", &self.key)
            .field("encoding", &self.encoding)
            .field("value_bytes", &self.value.len())
            .finish()
    }
}

impl Field {
    pub fn raw(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            encoding: Encoding::Raw,
            value: value.into(),
        }
    }

    pub fn percent(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            encoding: Encoding::Percent,
            value: value.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    pub id: u64,
    pub verb: String,
    pub fields: Vec<Field>,
}

impl Request {
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|field| field.key == key)
            .map(|field| field.value.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    pub id: u64,
    pub status: i32,
    pub fields: Vec<Field>,
}

impl Response {
    pub fn empty(id: u64, status: i32) -> Self {
        Self {
            id,
            status,
            fields: Vec::new(),
        }
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|field| field.key == key)
            .map(|field| field.value.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IpcError {
    message: String,
    io_kind: Option<io::ErrorKind>,
}

impl IpcError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            io_kind: None,
        }
    }

    fn io_failed(operation: &str, error: io::Error) -> Self {
        Self {
            message: format!("{operation} failed: {error}"),
            io_kind: Some(error.kind()),
        }
    }

    pub fn io_error_kind(&self) -> Option<io::ErrorKind> {
        self.io_kind
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for IpcError {}

pub fn read_request<R: BufRead>(reader: &mut R) -> Result<Option<Request>, IpcError> {
    let mut header = String::new();
    let mut frame_bytes;
    loop {
        let read = read_bounded_line(reader, &mut header, MAX_FRAME_BYTES)?;
        if read == 0 {
            return Ok(None);
        }
        trim_newline(&mut header);
        if !header.is_empty() {
            frame_bytes = read;
            break;
        }
    }

    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() != 5 || parts[0] != "@D7/1" || parts[1] != "REQ" {
        return Err(IpcError::new("invalid request header"));
    }
    let id = parse_id(parts[2])?;
    let verb = parts[3].to_string();
    let field_count = parse_usize(parts[4], "field count")?;
    if field_count > MAX_FIELDS {
        return Err(IpcError::new("too many fields"));
    }

    let mut fields = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let mut line = String::new();
        let read = read_bounded_line(
            reader,
            &mut line,
            MAX_FRAME_BYTES.saturating_sub(frame_bytes),
        )?;
        if read == 0 {
            return Err(IpcError::new("unexpected EOF in fields"));
        }
        frame_bytes += read;
        trim_newline(&mut line);
        let field = parse_field_line(&line)?;
        if fields
            .iter()
            .any(|existing: &Field| existing.key == field.key)
        {
            return Err(IpcError::new("duplicate field key"));
        }
        fields.push(field);
    }

    let mut end = String::new();
    let read = read_bounded_line(
        reader,
        &mut end,
        MAX_FRAME_BYTES.saturating_sub(frame_bytes),
    )?;
    if read == 0 {
        return Err(IpcError::new("unexpected EOF before END"));
    }
    trim_newline(&mut end);
    let end_parts: Vec<&str> = end.split_whitespace().collect();
    if end_parts.len() != 3 || end_parts[0] != "@D7/1" || end_parts[1] != "END" {
        return Err(IpcError::new("invalid END marker"));
    }
    let end_id = parse_id(end_parts[2])?;
    if end_id != id {
        return Err(IpcError::new("invalid END marker"));
    }

    Ok(Some(Request { id, verb, fields }))
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    out: &mut String,
    max_bytes: usize,
) -> Result<usize, IpcError> {
    out.clear();
    let mut bytes = Vec::new();
    let mut total = 0;
    loop {
        let buffer = reader
            .fill_buf()
            .map_err(|error| IpcError::io_failed("read", error))?;
        if buffer.is_empty() {
            if total == 0 {
                return Ok(0);
            }
            *out = String::from_utf8(bytes)
                .map_err(|_| IpcError::new("read failed: stream did not contain valid UTF-8"))?;
            return Ok(total);
        }
        let take = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(buffer.len());
        if take > max_bytes.saturating_sub(total) {
            return Err(IpcError::new("frame too large"));
        }
        let has_newline = buffer[..take].ends_with(b"\n");
        bytes.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        total += take;
        if has_newline {
            *out = String::from_utf8(bytes)
                .map_err(|_| IpcError::new("read failed: stream did not contain valid UTF-8"))?;
            return Ok(total);
        }
    }
}

pub fn write_response<W: Write>(writer: &mut W, response: &Response) -> Result<(), IpcError> {
    let wire = serialize_response(response)?;
    writer
        .write_all(wire.as_bytes())
        .map_err(|error| IpcError::io_failed("write", error))?;
    writer
        .flush()
        .map_err(|error| IpcError::io_failed("flush", error))
}

pub fn serialize_response(response: &Response) -> Result<String, IpcError> {
    if response.id == 0 || response.id > i64::MAX as u64 {
        return Err(IpcError::new("response id out of range"));
    }
    if response.status < 0 {
        return Err(IpcError::new("response status out of range"));
    }
    if response.fields.len() > MAX_FIELDS {
        return Err(IpcError::new("too many response fields"));
    }

    let mut out = format!(
        "@D7/1 RES {} {} {}\n",
        response.id,
        response.status,
        response.fields.len()
    );
    if out.len() > MAX_FRAME_BYTES {
        return Err(IpcError::new("response frame too large"));
    }

    let mut seen_keys: Vec<&str> = Vec::with_capacity(response.fields.len());
    for field in &response.fields {
        if !valid_field_key(&field.key) {
            return Err(IpcError::new("invalid response field key"));
        }
        if seen_keys.iter().any(|key| *key == field.key) {
            return Err(IpcError::new("duplicate response field key"));
        }
        seen_keys.push(&field.key);

        let (encoding, encoded) = response_field_wire(field);
        if encoded.len() > MAX_VALUE_ENCODED_BYTES {
            return Err(IpcError::new("response field value too large"));
        }
        let line = if encoded.is_empty() {
            format!("K {} {} 0\n", field.key, encoding)
        } else {
            format!(
                "K {} {} {} {}\n",
                field.key,
                encoding,
                encoded.len(),
                encoded
            )
        };
        if out.len() > MAX_FRAME_BYTES - line.len() {
            return Err(IpcError::new("response frame too large"));
        }
        out.push_str(&line);
    }
    let end = format!("@D7/1 END {}\n", response.id);
    if out.len() > MAX_FRAME_BYTES - end.len() {
        return Err(IpcError::new("response frame too large"));
    }
    out.push_str(&end);
    Ok(out)
}

fn response_field_wire(field: &Field) -> (&'static str, String) {
    match field.encoding {
        Encoding::Raw if raw_response_value_safe(&field.value) => ("raw", field.value.clone()),
        Encoding::Raw | Encoding::Percent => ("pct", percent_encode(&field.value)),
    }
}

fn raw_response_value_safe(value: &str) -> bool {
    value.bytes().all(|byte| byte > b' ' && byte < 0x7F)
}

pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if is_unreserved(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex_char(byte >> 4));
            out.push(hex_char(byte & 0x0F));
        }
    }
    out
}

pub fn percent_decode(value: &str) -> Result<String, IpcError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(IpcError::new("truncated percent escape"));
                }
                let hi = hex_value(bytes[index + 1])?;
                let lo = hex_value(bytes[index + 2])?;
                out.push((hi << 4) | lo);
                index += 3;
            }
            b'+' => {
                out.push(b'+');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| IpcError::new("decoded field is not UTF-8"))
}

fn parse_field_line(line: &str) -> Result<Field, IpcError> {
    let mut spaces = Vec::with_capacity(4);
    for (index, byte) in line.bytes().enumerate() {
        if byte == b' ' {
            spaces.push(index);
            if spaces.len() == 4 {
                break;
            }
        }
    }
    if spaces.len() < 3 || &line[..spaces[0]] != "K" {
        return Err(IpcError::new("invalid field line"));
    }

    let key = &line[spaces[0] + 1..spaces[1]];
    if !valid_field_key(key) {
        return Err(IpcError::new("invalid field key"));
    }
    let encoding_text = &line[spaces[1] + 1..spaces[2]];
    let encoding = match encoding_text {
        "raw" => Encoding::Raw,
        "pct" => Encoding::Percent,
        _ => return Err(IpcError::new("invalid field encoding")),
    };
    let (length_text, encoded_value) = if spaces.len() >= 4 {
        (&line[spaces[2] + 1..spaces[3]], &line[spaces[3] + 1..])
    } else {
        (&line[spaces[2] + 1..], "")
    };
    let length = parse_usize(length_text, "field length")?;
    if length > MAX_VALUE_ENCODED_BYTES {
        return Err(IpcError::new("field value too large"));
    }
    if encoded_value.len() != length {
        return Err(IpcError::new("field length mismatch"));
    }
    let value = match encoding {
        Encoding::Raw => encoded_value.to_string(),
        Encoding::Percent => percent_decode(encoded_value)?,
    };
    Ok(Field {
        key: key.to_string(),
        encoding,
        value,
    })
}

fn valid_field_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && key
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn parse_id(value: &str) -> Result<u64, IpcError> {
    let id = value
        .parse::<u64>()
        .map_err(|_| IpcError::new("invalid request id"))?;
    if id == 0 || id > i64::MAX as u64 {
        return Err(IpcError::new("request id out of range"));
    }
    Ok(id)
}

fn parse_usize(value: &str, name: &str) -> Result<usize, IpcError> {
    value
        .parse::<usize>()
        .map_err(|_| IpcError::new(format!("invalid {}", name)))
}

fn trim_newline(value: &mut String) {
    while value.ends_with('\n') || value.ends_with('\r') {
        value.pop();
    }
}

fn hex_char(nibble: u8) -> char {
    char::from(match nibble {
        0..=9 => b'0' + nibble,
        10..=15 => b'A' + (nibble - 10),
        _ => b'0',
    })
}

fn hex_value(byte: u8) -> Result<u8, IpcError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(IpcError::new("percent escape must use uppercase hex")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn diagnostics_hide_field_values_while_the_wire_and_accessors_preserve_them() {
        let secret = "private-ipc-secret";
        let field = Field::percent("token", secret);
        let request = Request {
            id: 1,
            verb: "ICB".into(),
            fields: vec![field.clone()],
        };
        let response = Response {
            id: 1,
            status: 0,
            fields: vec![field],
        };
        let output = format!("{request:#?} {response:#?}");
        assert!(!output.contains(secret));
        assert!(output.contains("token"));
        assert!(output.len() < 1024);
        assert_eq!(request.field("token"), Some(secret));
        assert_eq!(response.field("token"), Some(secret));
        assert!(serialize_response(&response).unwrap().contains(secret));
    }

    #[test]
    fn response_write_errors_retain_their_io_kind() {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::TimedOut, "test deadline"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let response = Response::empty(1, 0);
        let error = write_response(&mut BrokenWriter, &response).unwrap_err();
        assert_eq!(error.io_error_kind(), Some(io::ErrorKind::TimedOut));
        assert!(error.to_string().contains("write failed"));
        assert_eq!(
            serialize_response(&Response::empty(0, 0))
                .unwrap_err()
                .io_error_kind(),
            None
        );
    }

    #[test]
    fn parses_percent_encoded_request() {
        let wire = concat!(
            "@D7/1 REQ 1 SND 2\n",
            "K channel_id raw 3 123\n",
            "K content pct 11 hello%20bot\n",
            "@D7/1 END 1\n"
        );
        let request = read_request(&mut Cursor::new(wire)).unwrap().unwrap();
        assert_eq!(request.id, 1);
        assert_eq!(request.verb, "SND");
        assert_eq!(request.field("content"), Some("hello bot"));
    }

    #[test]
    fn parses_request_across_tiny_reader_buffer() {
        let wire = concat!(
            "@D7/1 REQ 1 LOG 1\n",
            "K message pct 11 hello%20bot\n",
            "@D7/1 END 1\n"
        );
        let mut reader = BufReader::with_capacity(3, Cursor::new(wire.as_bytes()));
        let request = read_request(&mut reader).unwrap().unwrap();
        assert_eq!(request.verb, "LOG");
        assert_eq!(request.field("message"), Some("hello bot"));
    }

    #[test]
    fn rejects_lowercase_percent_escape() {
        let wire = concat!(
            "@D7/1 REQ 1 LOG 2\n",
            "K level raw 1 1\n",
            "K message pct 3 %2f\n",
            "@D7/1 END 1\n"
        );
        assert!(read_request(&mut Cursor::new(wire)).is_err());
    }

    #[test]
    fn rejects_duplicate_request_field_keys() {
        let wire = concat!(
            "@D7/1 REQ 1 SND 2\n",
            "K channel_id raw 3 123\n",
            "K channel_id raw 3 456\n",
            "@D7/1 END 1\n"
        );
        let error = read_request(&mut Cursor::new(wire)).unwrap_err();
        assert_eq!(error.to_string(), "duplicate field key");
    }

    #[test]
    fn rejects_non_ascii_request_field_keys() {
        for wire in [
            concat!(
                "@D7/1 REQ 1 LOG 1\n",
                "K message-id raw 1 x\n",
                "@D7/1 END 1\n"
            ),
            concat!("@D7/1 REQ 1 LOG 1\n", "K メ raw 1 x\n", "@D7/1 END 1\n"),
        ] {
            let error = read_request(&mut Cursor::new(wire)).unwrap_err();
            assert_eq!(error.to_string(), "invalid field key");
        }
    }

    #[test]
    fn rejects_oversized_header() {
        let wire = format!(
            "@D7/1 REQ 1 {} 0\n@D7/1 END 1\n",
            "A".repeat(MAX_FRAME_BYTES)
        );
        let error = read_request(&mut Cursor::new(wire)).unwrap_err();
        assert_eq!(error.to_string(), "frame too large");
    }

    #[test]
    fn rejects_oversized_unterminated_line() {
        let wire = "A".repeat(MAX_FRAME_BYTES + 1);
        let error = read_request(&mut Cursor::new(wire)).unwrap_err();
        assert_eq!(error.to_string(), "frame too large");
    }

    #[test]
    fn rejects_oversized_end_marker() {
        let wire = format!(
            "@D7/1 REQ 1 INI 0\n@D7/1 END 1{}\n",
            " ".repeat(MAX_FRAME_BYTES)
        );
        let error = read_request(&mut Cursor::new(wire)).unwrap_err();
        assert_eq!(error.to_string(), "frame too large");
    }

    #[test]
    fn serializes_response() {
        let response = Response {
            id: 7,
            status: 0,
            fields: vec![Field::percent("value", "a b")],
        };
        assert_eq!(
            serialize_response(&response).unwrap(),
            "@D7/1 RES 7 0 1\nK value pct 5 a%20b\n@D7/1 END 7\n"
        );
    }

    #[test]
    fn serializes_safe_raw_response_value_as_raw() {
        let response = Response {
            id: 7,
            status: 0,
            fields: vec![Field::raw("count", "12")],
        };
        assert_eq!(
            serialize_response(&response).unwrap(),
            "@D7/1 RES 7 0 1\nK count raw 2 12\n@D7/1 END 7\n"
        );
    }

    #[test]
    fn serializes_unsafe_raw_response_value_as_percent() {
        let response = Response {
            id: 7,
            status: 0,
            fields: vec![Field::raw("message", "bad value\n")],
        };
        assert_eq!(
            serialize_response(&response).unwrap(),
            "@D7/1 RES 7 0 1\nK message pct 14 bad%20value%0A\n@D7/1 END 7\n"
        );
    }

    #[test]
    fn rejects_invalid_response_before_write() {
        let too_many_fields = Response {
            id: 1,
            status: 0,
            fields: (0..=MAX_FIELDS)
                .map(|index| Field::raw(format!("k{}", index), "x"))
                .collect(),
        };
        assert_eq!(
            write_response(&mut Vec::new(), &too_many_fields)
                .unwrap_err()
                .to_string(),
            "too many response fields"
        );

        let invalid_key = Response {
            id: 1,
            status: 0,
            fields: vec![Field::raw("bad-key", "x")],
        };
        assert_eq!(
            write_response(&mut Vec::new(), &invalid_key)
                .unwrap_err()
                .to_string(),
            "invalid response field key"
        );

        let duplicate_key = Response {
            id: 1,
            status: 0,
            fields: vec![Field::raw("value", "x"), Field::raw("value", "y")],
        };
        assert_eq!(
            write_response(&mut Vec::new(), &duplicate_key)
                .unwrap_err()
                .to_string(),
            "duplicate response field key"
        );

        let too_large_value = Response {
            id: 1,
            status: 0,
            fields: vec![Field::raw("value", "a".repeat(MAX_VALUE_ENCODED_BYTES + 1))],
        };
        assert_eq!(
            write_response(&mut Vec::new(), &too_large_value)
                .unwrap_err()
                .to_string(),
            "response field value too large"
        );

        let oversized_frame = Response {
            id: 1,
            status: 0,
            fields: vec![
                Field::raw("first", "a".repeat(60_000)),
                Field::raw("second", "b".repeat(6_000)),
            ],
        };
        assert_eq!(
            write_response(&mut Vec::new(), &oversized_frame)
                .unwrap_err()
                .to_string(),
            "response frame too large"
        );
    }
}
