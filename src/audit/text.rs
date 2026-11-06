pub fn json_escape(value: &str) -> String {
    escape_json(value, false)
}

pub fn json_log_escape(value: &str) -> String {
    escape_json(value, true)
}

fn escape_json(value: &str, log: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            ch if ch <= '\u{1F}' || log && (ch.is_control() || is_log_unicode_control(ch)) => {
                out.push_str(&format!("\\u{:04X}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out
}

pub fn diagnostic_text(value: &str) -> String {
    redact_sensitive_text(value)
}

pub fn diagnostic_field(value: &str) -> String {
    let text = diagnostic_text(value);
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_whitespace() || matches!(ch, '=' | '%' | '"' | '\\') {
            let mut bytes = [0_u8; 4];
            for byte in ch.encode_utf8(&mut bytes).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn write_diagnostic(arguments: std::fmt::Arguments<'_>) {
    eprintln!("{}", diagnostic_text(&arguments.to_string()));
}

fn is_log_unicode_control(ch: char) -> bool {
    matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{206f}')
}

pub fn redact_sensitive_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(512));
    let mut index = 0;
    while index < value.len() {
        if let Some(end) = authorization_header_end(value, index)
            .or_else(|| prefixed_secret_end(value, index))
            .or_else(|| discord_token_end(value, index))
            .or_else(|| long_tokenish_end(value, index))
        {
            out.push_str("[REDACTED]");
            index = end;
            continue;
        }
        let ch = value[index..].chars().next().unwrap_or('\0');
        if ch.is_control() {
            out.push_str("[CTRL]");
            index += ch.len_utf8();
            if ch == '\u{1b}' {
                index = ansi_escape_tail_end(value, index);
            }
            continue;
        }
        if is_log_unicode_control(ch) {
            out.push_str(&format!("[U+{:04X}]", ch as u32));
            index += ch.len_utf8();
            continue;
        }
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

pub(super) fn contains_definite_sensitive_text(value: &str) -> bool {
    let mut index = 0;
    while index < value.len() {
        if authorization_header_end(value, index)
            .or_else(|| prefixed_secret_end(value, index))
            .or_else(|| discord_token_end(value, index))
            .is_some()
        {
            return true;
        }
        let Some(ch) = value[index..].chars().next() else {
            break;
        };
        index += ch.len_utf8();
    }
    false
}

fn authorization_header_end(value: &str, index: usize) -> Option<usize> {
    let needle = "authorization";
    if !starts_with_ascii_ci(&value[index..], needle) || !token_boundary_before(value, index) {
        return None;
    }
    let mut cursor = index + needle.len();
    while matches!(value.as_bytes().get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }
    if value.as_bytes().get(cursor) != Some(&b':') {
        return None;
    }
    cursor += 1;
    while cursor < value.len() {
        let ch = value[cursor..].chars().next()?;
        if matches!(ch, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}') {
            break;
        }
        cursor += ch.len_utf8();
    }
    Some(cursor)
}

fn prefixed_secret_end(value: &str, index: usize) -> Option<usize> {
    for prefix in ["Bot ", "Bearer "] {
        if starts_with_ascii_ci(&value[index..], prefix) && token_boundary_before(value, index) {
            let start = index + prefix.len();
            let end = tokenish_run_end(value, start, true);
            if end.saturating_sub(start) >= 20 {
                return Some(end);
            }
        }
    }
    None
}

fn discord_token_end(value: &str, index: usize) -> Option<usize> {
    if !token_boundary_before(value, index) {
        return None;
    }
    if value[index..].starts_with("mfa.") {
        let start = index + 4;
        let end = tokenish_run_end(value, start, false);
        if end.saturating_sub(start) >= 20 && token_boundary_after(value, end) {
            return Some(end);
        }
    }
    let first = tokenish_run_end(value, index, false);
    if first.saturating_sub(index) < 16 || value.as_bytes().get(first) != Some(&b'.') {
        return None;
    }
    let second_start = first + 1;
    let second = tokenish_run_end(value, second_start, false);
    if second.saturating_sub(second_start) < 6 || value.as_bytes().get(second) != Some(&b'.') {
        return None;
    }
    let third_start = second + 1;
    let third = tokenish_run_end(value, third_start, false);
    if third.saturating_sub(third_start) >= 16 && token_boundary_after(value, third) {
        Some(third)
    } else {
        None
    }
}

fn long_tokenish_end(value: &str, index: usize) -> Option<usize> {
    if !token_boundary_before(value, index) {
        return None;
    }
    let end = tokenish_run_end(value, index, false);
    if end.saturating_sub(index) >= 48 && token_boundary_after(value, end) {
        Some(end)
    } else {
        None
    }
}

fn tokenish_run_end(value: &str, start: usize, allow_dot: bool) -> usize {
    let mut cursor = start;
    while let Some(byte) = value.as_bytes().get(cursor) {
        if is_tokenish_byte(*byte) || (allow_dot && *byte == b'.') {
            cursor += 1;
        } else {
            break;
        }
    }
    cursor
}

fn is_tokenish_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn token_boundary_before(value: &str, index: usize) -> bool {
    index == 0
        || value
            .as_bytes()
            .get(index - 1)
            .is_none_or(|byte| !is_tokenish_byte(*byte) && *byte != b'.')
}

fn token_boundary_after(value: &str, index: usize) -> bool {
    index >= value.len()
        || value
            .as_bytes()
            .get(index)
            .is_none_or(|byte| !is_tokenish_byte(*byte) && *byte != b'.')
}

fn ansi_escape_tail_end(value: &str, mut index: usize) -> usize {
    if value.as_bytes().get(index) == Some(&b'[') {
        index += 1;
        while let Some(byte) = value.as_bytes().get(index) {
            index += 1;
            if (0x40..=0x7e).contains(byte) {
                break;
            }
        }
    }
    index
}

fn starts_with_ascii_ci(value: &str, needle: &str) -> bool {
    let Some(prefix) = value.as_bytes().get(..needle.len()) else {
        return false;
    };
    prefix.eq_ignore_ascii_case(needle.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_controls_are_visible_without_damaging_normal_unicode() {
        let codes = (0..=31)
            .chain(127..=159)
            .chain([0x061c, 0x200e, 0x200f])
            .chain(0x2028..=0x202e)
            .chain(0x2066..=0x206f);
        for code in codes {
            let control = char::from_u32(code).unwrap();
            let output = diagnostic_text(&format!("before{control}after"));
            assert!(!output.contains(control), "U+{code:04X}");
            assert!(output.contains("before"));
            assert!(output.contains("after"));
            assert!(!output.chars().any(char::is_control));
        }
        let normal = "診断 العربية עברית 👩‍💻";
        assert_eq!(diagnostic_text(normal), normal);
    }

    #[test]
    fn diagnostic_fields_protect_boundaries_and_preserve_unicode_paths() {
        let value = "message content=other%20\"\\path 日本語\u{00a0}";
        let output = diagnostic_field(value);
        assert!(!output.chars().any(char::is_whitespace));
        assert!(!output.chars().any(|ch| matches!(ch, '=' | '"' | '\\')));
        assert_eq!(
            crate::ipc::percent_decode(&output).unwrap(),
            diagnostic_text(value)
        );
        assert_eq!(diagnostic_field("/tmp/日本語-test"), "/tmp/日本語-test");
        assert_eq!(diagnostic_field("123456789012345678"), "123456789012345678");
    }

    #[test]
    fn log_json_round_trips_controls_without_changing_network_json_encoding() {
        let value = "診断\n\u{0085}\u{2028}\u{2029}\u{202e}RTL\u{202c}\u{2066}text\u{2069}👩‍💻";
        let escaped = json_log_escape(value);
        assert!(!escaped
            .chars()
            .any(|ch| ch.is_control() || is_log_unicode_control(ch)));
        let parsed = crate::json::parse_json(&format!("\"{escaped}\"")).unwrap();
        assert_eq!(parsed.as_str(), Some(value));
        assert!(json_escape(value).contains('\u{202e}'));
    }

    #[test]
    fn diagnostic_text_keeps_long_ordinary_messages_complete() {
        let message = "日本語 ordinary text 👩‍💻 ".repeat(4000);
        assert_eq!(diagnostic_text(&message), message);
    }
}
