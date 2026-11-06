use std::collections::{HashMap, HashSet};

use super::ConfigError;

pub(super) enum Value {
    String(String),
    StringArray(Vec<String>),
    Scalar(String),
}

impl Value {
    pub(super) fn scalar(&self) -> Option<&str> {
        match self {
            Self::Scalar(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Default)]
struct StringState {
    quote: Option<u8>,
    escaped: bool,
}

impl StringState {
    fn consume(&mut self, byte: u8) -> bool {
        match self.quote {
            Some(b'"') => {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.quote = None;
                }
                true
            }
            Some(_) => {
                if byte == b'\'' {
                    self.quote = None;
                }
                true
            }
            None if matches!(byte, b'\'' | b'"') => {
                self.quote = Some(byte);
                true
            }
            None => false,
        }
    }
}

pub(super) fn document(content: &str) -> Result<HashMap<String, Value>, ConfigError> {
    let mut section = String::new();
    let mut out = HashMap::new();
    let mut seen_sections = HashSet::new();
    let mut lines = content.lines().enumerate();
    while let Some((line_index, raw_line)) = lines.next() {
        let line = trim_space(strip_comment(raw_line, line_index + 1)?);
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = trim_space(&line[1..line.len() - 1]).to_string();
            if section.is_empty() {
                return Err(ConfigError::new(format!(
                    "empty section at line {}",
                    line_index + 1
                )));
            }
            if !matches!(
                section.as_str(),
                "discord" | "gateway" | "rest" | "snapshot" | "audit" | "runtime" | "mode"
            ) {
                return Err(ConfigError::new(format!(
                    "unknown config section {} at line {}",
                    section,
                    line_index + 1
                )));
            }
            if !seen_sections.insert(section.clone()) {
                return Err(ConfigError::new(format!(
                    "duplicate config section {} at line {}",
                    section,
                    line_index + 1
                )));
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ConfigError::new(format!(
                "invalid config line {}",
                line_index + 1
            )));
        };
        let key = trim_space(key);
        if key.is_empty() || section.is_empty() {
            return Err(ConfigError::new(format!(
                "invalid config key at line {}",
                line_index + 1
            )));
        }
        let full_key = format!("{}.{}", section, key);
        let mut value = trim_space(value).to_string();
        let value = if value.starts_with('[') {
            while !value.ends_with(']') {
                let Some((next_index, next_line)) = lines.next() else {
                    return Err(ConfigError::new(format!(
                        "{} contains an unterminated array at line {}",
                        full_key,
                        line_index + 1
                    )));
                };
                value.push('\n');
                value.push_str(trim_space(strip_comment(next_line, next_index + 1)?));
            }
            Value::StringArray(string_array(&value, &full_key)?)
        } else if value.starts_with(['\'', '"']) {
            Value::String(string(&value, &full_key)?)
        } else {
            Value::Scalar(value)
        };
        if out.insert(full_key.clone(), value).is_some() {
            return Err(ConfigError::new(format!(
                "duplicate config field {} at line {}",
                full_key,
                line_index + 1
            )));
        }
    }
    Ok(out)
}

fn trim_space(value: &str) -> &str {
    value.trim_matches([' ', '\t'])
}

fn trim_array_space(value: &str) -> &str {
    value.trim_matches([' ', '\t', '\r', '\n'])
}

fn strip_comment(line: &str, line_number: usize) -> Result<&str, ConfigError> {
    if line
        .bytes()
        .any(|byte| byte < b' ' && byte != b'\t' || byte == 0x7f)
    {
        return Err(ConfigError::new(format!(
            "invalid control character at config line {}",
            line_number
        )));
    }
    let mut state = StringState::default();
    for (index, byte) in line.bytes().enumerate() {
        if !state.consume(byte) && byte == b'#' {
            return Ok(&line[..index]);
        }
    }
    if state.quote.is_some() {
        return Err(ConfigError::new(format!(
            "unterminated string at config line {}",
            line_number
        )));
    }
    Ok(line)
}

pub(super) fn string(value: &str, key: &str) -> Result<String, ConfigError> {
    let value = trim_space(value);
    let quote = value
        .chars()
        .next()
        .filter(|quote| matches!(quote, '\'' | '"'))
        .ok_or_else(|| ConfigError::new(format!("{} must be a string", key)))?;
    let inner = value
        .strip_prefix(quote)
        .and_then(|value| value.strip_suffix(quote))
        .ok_or_else(|| ConfigError::new(format!("{} must be a string", key)))?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == quote {
            return Err(ConfigError::new(format!(
                "{} contains an unescaped string delimiter",
                key
            )));
        }
        if ch < ' ' && ch != '\t' || ch == '\u{7f}' {
            return Err(ConfigError::new(format!(
                "{} contains an unescaped control character",
                key
            )));
        }
        if ch != '\\' || quote == '\'' {
            out.push(ch);
            continue;
        }
        let escaped = chars
            .next()
            .ok_or_else(|| ConfigError::new(format!("{} has an incomplete escape", key)))?;
        match escaped {
            'b' => out.push('\u{0008}'),
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'f' => out.push('\u{000C}'),
            'r' => out.push('\r'),
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'u' => out.push(unicode_escape(&mut chars, 4, key)?),
            'U' => out.push(unicode_escape(&mut chars, 8, key)?),
            _ => {
                return Err(ConfigError::new(format!(
                    "{} contains an invalid escape",
                    key
                )));
            }
        }
    }
    Ok(out)
}

fn unicode_escape<I>(chars: &mut I, len: usize, key: &str) -> Result<char, ConfigError>
where
    I: Iterator<Item = char>,
{
    let mut value = 0_u32;
    for _ in 0..len {
        let Some(ch) = chars.next() else {
            return Err(ConfigError::new(format!(
                "{} has an incomplete unicode escape",
                key
            )));
        };
        let digit = ch
            .is_ascii_hexdigit()
            .then(|| ch.to_digit(16))
            .flatten()
            .ok_or_else(|| {
                ConfigError::new(format!("{} contains an invalid unicode escape", key))
            })?;
        value = value * 16 + digit;
    }
    char::from_u32(value)
        .ok_or_else(|| ConfigError::new(format!("{} contains an invalid unicode escape", key)))
}

pub(super) fn string_array(value: &str, key: &str) -> Result<Vec<String>, ConfigError> {
    let value = trim_array_space(value);
    let inner = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| ConfigError::new(format!("{} must be an array", key)))?;
    let inner = trim_array_space(inner);
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut state = StringState::default();
    for (index, byte) in inner.bytes().enumerate() {
        if !state.consume(byte) && byte == b',' {
            let part = trim_array_space(&inner[start..index]);
            if part.is_empty() {
                return Err(ConfigError::new(format!("{} contains an empty value", key)));
            }
            out.push(string(part, key)?);
            start = index + 1;
        }
    }
    if state.quote.is_some() {
        return Err(ConfigError::new(format!(
            "{} contains an unterminated string",
            key
        )));
    }
    let tail = trim_array_space(&inner[start..]);
    if !tail.is_empty() {
        out.push(string(tail, key)?);
    }
    Ok(out)
}
