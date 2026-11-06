use crate::constants::{D7ARG, D7OK};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HttpOriginScheme {
    Http,
    Https,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HttpOrigin {
    pub(crate) scheme: HttpOriginScheme,
    pub(crate) host: String,
    pub(crate) port: u16,
}

pub fn validate_snowflake(value: &str) -> bool {
    if value.is_empty() || value.len() > 20 || value.trim() != value {
        return false;
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match value.parse::<u64>() {
        Ok(parsed) => parsed > 0,
        Err(_) => false,
    }
}

pub fn validate_reason(reason: &str) -> i32 {
    if reason.is_empty() {
        return D7OK;
    }
    let encoded_len = percent_encoded_len(reason);
    if (1..=512).contains(&encoded_len) {
        D7OK
    } else {
        D7ARG
    }
}

pub fn validate_message_content(content: &str) -> i32 {
    if content.is_empty() || content.chars().count() > 2000 {
        return D7ARG;
    }
    D7OK
}

pub fn percent_encoded_len(value: &str) -> usize {
    value
        .bytes()
        .map(|byte| if is_unreserved(byte) { 1 } else { 3 })
        .sum()
}

pub fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

pub fn validate_env_var_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return false;
    }
    bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

pub fn validate_bot_token_value(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("bot token must not be empty");
    }
    if value.bytes().any(|byte| byte <= b' ' || byte == 0x7F) {
        return Err("bot token must not contain whitespace or control characters");
    }
    Ok(())
}

pub(crate) fn parse_http_origin(value: &str) -> Result<HttpOrigin, &'static str> {
    if value.trim() != value {
        return Err("base_url must not contain surrounding whitespace");
    }
    let (scheme, without_scheme, default_port) = if let Some(rest) = value.strip_prefix("http://") {
        (HttpOriginScheme::Http, rest, 80)
    } else if let Some(rest) = value.strip_prefix("https://") {
        (HttpOriginScheme::Https, rest, 443)
    } else {
        return Err("base_url must start with http:// or https://");
    };
    let origin = without_scheme.strip_suffix('/').unwrap_or(without_scheme);
    if origin.is_empty() {
        return Err("base_url host must not be empty");
    }
    if origin.contains('/') {
        return Err("base_url must not include a path");
    }
    if origin.contains('?') || origin.contains('#') {
        return Err("base_url must not include query or fragment");
    }
    if origin.contains('@') {
        return Err("base_url must not include credentials");
    }
    if origin.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err("base_url must not contain whitespace");
    }
    let (host, port) = parse_http_host_port(origin, default_port)?;
    Ok(HttpOrigin { scheme, host, port })
}

pub(crate) fn validate_rest_origin_for_token(value: &str) -> Result<HttpOrigin, &'static str> {
    let origin = parse_http_origin(value)?;
    if origin.scheme == HttpOriginScheme::Http && !is_loopback_http_host(&origin.host) {
        return Err("base_url with http:// is only allowed for local mock REST");
    }
    Ok(origin)
}

fn is_loopback_http_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host == "::1" {
        return true;
    }
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|address| address.is_loopback())
}

fn parse_http_host_port(value: &str, default_port: u16) -> Result<(String, u16), &'static str> {
    if let Some(rest) = value.strip_prefix('[') {
        let Some((host, after_host)) = rest.split_once(']') else {
            return Err("base_url host is invalid");
        };
        if host.is_empty() || host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("base_url host is invalid");
        }
        let port = if after_host.is_empty() {
            default_port
        } else if let Some(port_text) = after_host.strip_prefix(':') {
            parse_http_port(port_text)?
        } else {
            return Err("base_url host is invalid");
        };
        return Ok((host.to_string(), port));
    }
    let (host, port) = if let Some((host, port_text)) = value.rsplit_once(':') {
        let port = parse_http_port(port_text)?;
        (host, port)
    } else {
        (value, default_port)
    };
    if host.is_empty() {
        return Err("base_url host must not be empty");
    }
    if host.starts_with('.') || host.ends_with('.') || host.contains("..") {
        return Err("base_url host is invalid");
    }
    if host.starts_with('-') || host.ends_with('-') || host.contains(".-") || host.contains("-.") {
        return Err("base_url host is invalid");
    }
    if !host
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err("base_url host is invalid");
    }
    Ok((host.to_string(), port))
}

fn parse_http_port(value: &str) -> Result<u16, &'static str> {
    if value.is_empty() {
        return Err("base_url port is invalid");
    }
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or("base_url port is invalid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_rejects_bad_values() {
        assert!(validate_snowflake("1"));
        assert!(validate_snowflake("18446744073709551615"));
        assert!(!validate_snowflake(""));
        assert!(!validate_snowflake("0"));
        assert!(!validate_snowflake(" 1"));
        assert!(!validate_snowflake("-1"));
        assert!(!validate_snowflake("18446744073709551616"));
        assert!(!validate_snowflake("123456789012345678901"));
    }

    #[test]
    fn message_content_validator_counts_unicode_scalar_values() {
        assert_eq!(validate_message_content("a"), D7OK);
        assert_eq!(validate_message_content(&"a".repeat(2000)), D7OK);
        assert_eq!(validate_message_content(&"a".repeat(2001)), D7ARG);
        assert_eq!(validate_message_content(&"\u{1F600}".repeat(2000)), D7OK);
        assert_eq!(validate_message_content(""), D7ARG);
    }

    #[test]
    fn utf8_prefix_stops_on_boundary() {
        assert_eq!(utf8_prefix("abc", 2), "ab");
        assert_eq!(utf8_prefix("aあ", 2), "a");
        assert_eq!(utf8_prefix("aあ", 4), "aあ");
    }

    #[test]
    fn env_var_name_validator_accepts_shell_safe_names() {
        for value in ["DISCORD_BOT_TOKEN", "_DFOR77_TOKEN", "token_1"] {
            assert!(validate_env_var_name(value), "{value}");
        }
        for value in ["", "1TOKEN", "TOKEN-NAME", "TOKEN NAME", "TOKEN=VALUE"] {
            assert!(!validate_env_var_name(value), "{value}");
        }
    }

    #[test]
    fn bot_token_validator_rejects_empty_or_header_breaking_values() {
        assert!(validate_bot_token_value("abc.def.ghi").is_ok());
        for value in ["", "abc def", "abc\nX-Bad: yes", "abc\rbad", "abc\tbad"] {
            assert!(validate_bot_token_value(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn http_origin_parser_accepts_origins_and_rejects_non_origins() {
        assert_eq!(
            parse_http_origin("https://discord.com").unwrap(),
            HttpOrigin {
                scheme: HttpOriginScheme::Https,
                host: "discord.com".to_string(),
                port: 443
            }
        );
        assert_eq!(
            parse_http_origin("http://127.0.0.1:18080/").unwrap(),
            HttpOrigin {
                scheme: HttpOriginScheme::Http,
                host: "127.0.0.1".to_string(),
                port: 18080
            }
        );
        assert_eq!(
            parse_http_origin("http://[::1]:18080/").unwrap(),
            HttpOrigin {
                scheme: HttpOriginScheme::Http,
                host: "::1".to_string(),
                port: 18080
            }
        );
        assert_eq!(
            parse_http_origin("https://[2001:db8::1]").unwrap(),
            HttpOrigin {
                scheme: HttpOriginScheme::Https,
                host: "2001:db8::1".to_string(),
                port: 443
            }
        );
        for value in [
            "ftp://discord.com",
            "https://discord.com/api",
            "https://discord.com?x=1",
            "https://discord.com#frag",
            "https://user@discord.com",
            "https://discord.com:abc",
            "https://discord.com:0",
            "https://dis cord.com",
            " https://discord.com",
            "http://::1:18080",
            "http://[::1",
            "http://[::1]bad",
            "http://[]:18080",
            "http://[::1]:0",
        ] {
            assert!(parse_http_origin(value).is_err(), "{value}");
        }
    }

    #[test]
    fn token_rest_origin_rejects_plaintext_non_loopback() {
        for value in [
            "https://discord.com",
            "http://localhost:18080",
            "http://127.0.0.1:18080",
            "http://[::1]:18080",
        ] {
            assert!(validate_rest_origin_for_token(value).is_ok(), "{value}");
        }
        for value in [
            "http://discord.com",
            "http://192.168.0.10:18080",
            "http://10.0.0.2:18080",
            "http://[2001:db8::1]:18080",
        ] {
            assert!(validate_rest_origin_for_token(value).is_err(), "{value}");
        }
    }
}
