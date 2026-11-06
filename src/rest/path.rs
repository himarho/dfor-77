use crate::validation::validate_snowflake;

pub(crate) struct RestPath {
    segments: Vec<String>,
}

impl RestPath {
    pub(crate) fn parse(input: &str) -> Option<Self> {
        if input.len() > 2048
            || !input
                .bytes()
                .all(|byte| (0x21..=0x7e).contains(&byte) && !matches!(byte, b'#' | b'\\'))
        {
            return None;
        }
        let path = input.split_once('?').map_or(input, |(path, _)| path);
        if path != "/api/v10" && !path.starts_with("/api/v10/") {
            return None;
        }
        let mut segments = Vec::new();
        for segment in path.strip_prefix('/')?.split('/') {
            let segment = decode_segment(segment)?;
            if segment
                .bytes()
                .any(|byte| byte.is_ascii_control() || matches!(byte, b'/' | b'\\'))
                || segment.as_bytes().windows(3).any(|bytes| {
                    bytes[0] == b'%' && bytes[1].is_ascii_hexdigit() && bytes[2].is_ascii_hexdigit()
                })
            {
                return None;
            }
            match segment.as_str() {
                "." => {}
                ".." => {
                    segments.pop()?;
                }
                _ => segments.push(segment),
            }
        }
        if segments.first().is_none_or(|segment| segment != "api")
            || segments.get(1).is_none_or(|segment| segment != "v10")
        {
            return None;
        }
        segments.retain(|segment| !segment.is_empty());
        Some(Self { segments })
    }

    pub(crate) fn parts(&self) -> Vec<&str> {
        self.segments.iter().map(String::as_str).collect()
    }

    pub(crate) fn resource_parts(&self) -> Vec<&str> {
        self.segments.iter().skip(2).map(String::as_str).collect()
    }
}

fn decode_segment(segment: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(segment.len());
    let mut bytes = segment.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            decoded.push(((high << 4) | low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).ok()
}

pub(crate) fn canonical_id(value: &str) -> Option<String> {
    validate_snowflake(value).then(|| value.trim_start_matches('0').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_unicode_and_reserved_data_keep_segment_boundaries_and_opaque_queries() {
        for (path, expected) in [
            (
                "/api/v10/channels/20/messages/30/reactions/%F0%9F%94%A5?type=0&after=31",
                vec!["channels", "20", "messages", "30", "reactions", "🔥"],
            ),
            (
                "/api/v10/channels/20/messages/30/reactions/%23%EF%B8%8F%E2%83%A3",
                vec!["channels", "20", "messages", "30", "reactions", "#️⃣"],
            ),
            (
                "/api/v10/webhooks/50/token%3F%23%3A%25zz/messages/%40original",
                vec!["webhooks", "50", "token?#:%zz", "messages", "@original"],
            ),
            (
                "/api/v10/guilds/10/roles?filter=%2f%252e%2500",
                vec!["guilds", "10", "roles"],
            ),
            ("/api/v10/custom/100%25", vec!["custom", "100%"]),
        ] {
            assert_eq!(
                RestPath::parse(path).unwrap().resource_parts(),
                expected,
                "path={path}"
            );
        }
    }

    #[test]
    fn relative_segments_resolve_before_empty_segments_are_discarded() {
        for (path, expected) in [
            (
                "/api/v10/channels/99/../20/messages/30",
                vec!["channels", "20", "messages", "30"],
            ),
            (
                "/api/v10/a/b/.%2e/%2e/c/..%2e/d",
                vec!["a", "c", "...", "d"],
            ),
            ("/api/v10//../guilds/10", vec!["guilds", "10"]),
            ("/api/v10/guilds/10/roles/.", vec!["guilds", "10", "roles"]),
        ] {
            assert_eq!(
                RestPath::parse(path)
                    .unwrap_or_else(|| panic!("unexpected invalid path: {path}"))
                    .resource_parts(),
                expected,
                "path={path}"
            );
        }
    }

    #[test]
    fn path_limits_and_invalid_octets_are_checked_before_allocating_route_metadata() {
        let path = format!("/api/v10/{}", "x".repeat(2048 - "/api/v10/".len()));
        assert!(RestPath::parse(&path).is_some());
        assert!(RestPath::parse(&(path + "x")).is_none());
        for path in [
            "/api/v10/%ff",
            "/api/v10/%c0%af",
            "/api/v10/%7f",
            "/api/v10/%2500",
            "/api/v10/%252f",
            "/api/v10/%2525252e",
            "/api/v10/%2e%2e/gateway",
        ] {
            assert!(RestPath::parse(path).is_none(), "path={path}");
        }
    }
}
