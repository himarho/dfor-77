use std::collections::HashMap;

pub(crate) const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_VALUES: usize = 262_144;

#[derive(Clone, Debug, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<JsonValue>),
    Object(HashMap<String, JsonValue>),
}

impl JsonValue {
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(values) => values.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Number(value) => value.parse().ok(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            JsonValue::Number(value) => value.parse().ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(values) => Some(values),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonError {
    message: String,
}

impl JsonError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for JsonError {}

pub fn parse_json(input: &str) -> Result<JsonValue, JsonError> {
    if input.len() > MAX_JSON_BYTES {
        return Err(JsonError::new("JSON input too large"));
    }
    let mut parser = Parser {
        input: input.as_bytes(),
        index: 0,
        depth: 0,
        remaining_values: MAX_JSON_VALUES,
    };
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.index != parser.input.len() {
        return Err(JsonError::new("trailing JSON input"));
    }
    Ok(value)
}

struct Parser<'a> {
    input: &'a [u8],
    index: usize,
    depth: usize,
    remaining_values: usize,
}

impl Parser<'_> {
    fn parse_value(&mut self) -> Result<JsonValue, JsonError> {
        if self.remaining_values == 0 {
            return Err(JsonError::new("too many JSON values"));
        }
        self.remaining_values -= 1;
        self.skip_ws();
        match self.peek() {
            Some(b'n') => self.parse_literal(b"null", JsonValue::Null),
            Some(b't') => self.parse_literal(b"true", JsonValue::Bool(true)),
            Some(b'f') => self.parse_literal(b"false", JsonValue::Bool(false)),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b'[' | b'{') => {
                if self.depth >= MAX_JSON_DEPTH {
                    return Err(JsonError::new("JSON nesting too deep"));
                }
                self.depth += 1;
                let result = if self.peek() == Some(b'[') {
                    self.parse_array()
                } else {
                    self.parse_object()
                };
                self.depth -= 1;
                result
            }
            Some(b'-' | b'0'..=b'9') => self.parse_number().map(JsonValue::Number),
            _ => Err(JsonError::new("unexpected JSON token")),
        }
    }

    fn parse_literal(&mut self, literal: &[u8], value: JsonValue) -> Result<JsonValue, JsonError> {
        if self.input.get(self.index..self.index + literal.len()) == Some(literal) {
            self.index += literal.len();
            Ok(value)
        } else {
            Err(JsonError::new("invalid JSON literal"))
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.expect(b'"')?;
        let mut out = String::new();
        while let Some(byte) = self.next() {
            match byte {
                b'"' => return Ok(out),
                b'\\' => out.push(self.parse_escape()?),
                0x00..=0x1F => return Err(JsonError::new("control character in JSON string")),
                _ => {
                    let start = self.index - 1;
                    let width = utf8_width(byte)?;
                    let end = start + width;
                    let slice = self
                        .input
                        .get(start..end)
                        .ok_or_else(|| JsonError::new("truncated UTF-8 in JSON string"))?;
                    let text = std::str::from_utf8(slice)
                        .map_err(|_| JsonError::new("invalid UTF-8 in JSON string"))?;
                    out.push_str(text);
                    self.index = end;
                }
            }
        }
        Err(JsonError::new("unterminated JSON string"))
    }

    fn parse_escape(&mut self) -> Result<char, JsonError> {
        match self.next() {
            Some(b'"') => Ok('"'),
            Some(b'\\') => Ok('\\'),
            Some(b'/') => Ok('/'),
            Some(b'b') => Ok('\u{08}'),
            Some(b'f') => Ok('\u{0C}'),
            Some(b'n') => Ok('\n'),
            Some(b'r') => Ok('\r'),
            Some(b't') => Ok('\t'),
            Some(b'u') => self.parse_unicode_escape(),
            _ => Err(JsonError::new("invalid JSON string escape")),
        }
    }

    fn parse_unicode_escape(&mut self) -> Result<char, JsonError> {
        let code = self.parse_hex4()?;
        if (0xD800..=0xDBFF).contains(&code) {
            if !self.consume(b'\\') || !self.consume(b'u') {
                return Err(JsonError::new("invalid unicode surrogate pair"));
            }
            let low = self.parse_hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(JsonError::new("invalid unicode surrogate pair"));
            }
            let scalar = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
            return char::from_u32(scalar).ok_or_else(|| JsonError::new("invalid unicode escape"));
        }
        if (0xDC00..=0xDFFF).contains(&code) {
            return Err(JsonError::new("invalid unicode surrogate pair"));
        }
        char::from_u32(code).ok_or_else(|| JsonError::new("invalid unicode escape"))
    }

    fn parse_hex4(&mut self) -> Result<u32, JsonError> {
        let mut value = 0_u32;
        for _ in 0..4 {
            let byte = self
                .next()
                .ok_or_else(|| JsonError::new("truncated unicode escape"))?;
            value = value * 16 + hex_value(byte)?;
        }
        Ok(value)
    }

    fn parse_array(&mut self) -> Result<JsonValue, JsonError> {
        self.expect(b'[')?;
        let mut values = Vec::new();
        self.skip_ws();
        if self.consume(b']') {
            return Ok(JsonValue::Array(values));
        }
        loop {
            values.push(self.parse_value()?);
            self.skip_ws();
            if self.consume(b']') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(JsonValue::Array(values))
    }

    fn parse_object(&mut self) -> Result<JsonValue, JsonError> {
        self.expect(b'{')?;
        let mut values = HashMap::new();
        self.skip_ws();
        if self.consume(b'}') {
            return Ok(JsonValue::Object(values));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.parse_value()?;
            if values.insert(key, value).is_some() {
                return Err(JsonError::new("duplicate JSON object key"));
            }
            self.skip_ws();
            if self.consume(b'}') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(JsonValue::Object(values))
    }

    fn parse_number(&mut self) -> Result<String, JsonError> {
        let start = self.index;
        self.consume(b'-');
        match self.peek() {
            Some(b'0') => {
                self.index += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(JsonError::new("invalid JSON number"));
                }
            }
            Some(b'1'..=b'9') => {
                self.index += 1;
                self.take_digits();
            }
            _ => return Err(JsonError::new("invalid JSON number")),
        }
        if self.consume(b'.') {
            self.take_required_digits()?;
        }
        if self.consume(b'e') || self.consume(b'E') {
            let _ = self.consume(b'+') || self.consume(b'-');
            self.take_required_digits()?;
        }
        let text = std::str::from_utf8(&self.input[start..self.index])
            .map_err(|_| JsonError::new("invalid JSON number"))?;
        Ok(text.to_string())
    }

    fn take_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.index += 1;
        }
    }

    fn take_required_digits(&mut self) -> Result<(), JsonError> {
        let start = self.index;
        self.take_digits();
        if self.index == start {
            Err(JsonError::new("invalid JSON number"))
        } else {
            Ok(())
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.index += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), JsonError> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(JsonError::new("unexpected JSON character"))
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.index).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.index += 1;
        Some(byte)
    }
}

fn utf8_width(first: u8) -> Result<usize, JsonError> {
    match first {
        0x00..=0x7F => Ok(1),
        0xC2..=0xDF => Ok(2),
        0xE0..=0xEF => Ok(3),
        0xF0..=0xF4 => Ok(4),
        _ => Err(JsonError::new("invalid UTF-8 leading byte")),
    }
}

fn hex_value(byte: u8) -> Result<u32, JsonError> {
    match byte {
        b'0'..=b'9' => Ok(u32::from(byte - b'0')),
        b'a'..=b'f' => Ok(u32::from(byte - b'a' + 10)),
        b'A'..=b'F' => Ok(u32::from(byte - b'A' + 10)),
        _ => Err(JsonError::new("invalid hex digit")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_resource_exhaustion_and_ambiguous_json() {
        let allowed = format!(
            "{}0{}",
            "[".repeat(MAX_JSON_DEPTH),
            "]".repeat(MAX_JSON_DEPTH)
        );
        assert!(parse_json(&allowed).is_ok());
        let deep = format!("{}0{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(parse_json(&deep)
            .unwrap_err()
            .to_string()
            .contains("nesting"));
        for input in [
            r#"{"id":"1","id":"2"}"#,
            r#"{"id":1,"\u0069d":2}"#,
            "[1,]",
            "{\"id\":1,}",
        ] {
            assert!(parse_json(input).is_err(), "{input}");
        }
        let wide = format!("[{}0]", "0,".repeat(MAX_JSON_VALUES));
        assert!(parse_json(&wide)
            .unwrap_err()
            .to_string()
            .contains("too many"));
        assert!(parse_json(&" ".repeat(MAX_JSON_BYTES + 1))
            .unwrap_err()
            .to_string()
            .contains("too large"));
    }

    #[test]
    fn adversarial_json_corpus_does_not_panic() {
        let mut seed = 0x7b31u64;
        let alphabet = b"{}[],:\"\\0123456789nultruefas \r\n\t";
        for length in 0..4096 {
            let mut bytes = Vec::new();
            for _ in 0..length % 1024 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push(alphabet[(seed >> 32) as usize % alphabet.len()]);
            }
            let _ = parse_json(std::str::from_utf8(&bytes).unwrap());
        }
    }

    #[test]
    fn parses_nested_json_values() {
        let value =
            parse_json(r#"{"op":0,"t":"READY","d":{"roles":["1","2"],"ok":true}}"#).unwrap();
        assert_eq!(value.get("op").and_then(JsonValue::as_i64), Some(0));
        assert_eq!(value.get("t").and_then(JsonValue::as_str), Some("READY"));
        assert_eq!(
            value
                .get("d")
                .and_then(|d| d.get("roles"))
                .and_then(JsonValue::as_array)
                .map(|roles| roles.len()),
            Some(2)
        );
    }

    #[test]
    fn parses_string_escapes() {
        let value = parse_json(r#"{"content":"hello\n\u0041"}"#).unwrap();
        assert_eq!(
            value.get("content").and_then(JsonValue::as_str),
            Some("hello\nA")
        );
    }

    #[test]
    fn parses_unicode_surrogate_pair_escapes() {
        let value = parse_json(r#"{"content":"emoji \uD83D\uDE00"}"#).unwrap();
        let content = value.get("content").and_then(JsonValue::as_str).unwrap();
        assert_eq!(content.chars().last(), char::from_u32(0x1F600));
        assert_eq!(content.len(), 10);
    }

    #[test]
    fn rejects_invalid_unicode_surrogate_pairs() {
        let error = parse_json(r#"{"content":"\uD83D!"}"#).unwrap_err();
        assert!(error.to_string().contains("invalid unicode surrogate pair"));
        let error = parse_json(r#"{"content":"\uDE00"}"#).unwrap_err();
        assert!(error.to_string().contains("invalid unicode surrogate pair"));
    }

    #[test]
    fn rejects_invalid_json_numbers() {
        for input in [
            r#"{"value":-}"#,
            r#"{"value":01}"#,
            r#"{"value":1.}"#,
            r#"{"value":1e}"#,
            r#"{"value":1e+}"#,
        ] {
            let error = parse_json(input).unwrap_err();
            assert!(error.to_string().contains("invalid JSON number"));
        }
    }
}
