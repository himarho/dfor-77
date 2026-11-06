use super::MemberSnapshot;
use crate::json::JsonValue;
use crate::memory::HeapSize;

#[derive(Clone)]
pub(super) struct StoredMember {
    pub(super) snapshot: MemberSnapshot,
    timeout_captured_at_ms: u64,
}

impl StoredMember {
    pub(super) fn new(snapshot: MemberSnapshot) -> Self {
        Self {
            timeout_captured_at_ms: snapshot.captured_at_ms,
            snapshot,
        }
    }

    pub(super) fn patch(&mut self, patch: MemberSnapshotPatch, now_ms: u64) {
        let member = &mut self.snapshot;
        let changed = patch.role_ids.is_some() || patch.timeout != MemberTimeoutUpdate::Missing;
        if let Some(roles) = patch.role_ids {
            member.role_ids = roles;
            member.roles_complete = true;
            member.captured_at_ms = now_ms;
        }
        if patch.timeout != MemberTimeoutUpdate::Missing {
            self.timeout_captured_at_ms = now_ms;
        }
        patch.timeout.apply(member, now_ms);
        member.refresh_timeout(now_ms);
        if changed {
            member.complete = member.roles_complete && member.timeout_known;
        }
    }

    pub(super) fn stale(&self, now_ms: u64, ttl_ms: u64) -> bool {
        now_ms.saturating_sub(self.snapshot.captured_at_ms) > ttl_ms
            || self.snapshot.timeout_known
                && now_ms.saturating_sub(self.timeout_captured_at_ms) > ttl_ms
    }
}

impl HeapSize for StoredMember {
    fn heap_size(&self) -> usize {
        self.snapshot.heap_size()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberTimeoutUpdate {
    Missing,
    Clear,
    Until(u64),
}

impl MemberTimeoutUpdate {
    pub(crate) fn apply(self, member: &mut MemberSnapshot, now_ms: u64) {
        match self {
            Self::Missing => {}
            Self::Clear => member.set_timeout_until(None, now_ms),
            Self::Until(until) => member.set_timeout_until(Some(until), now_ms),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct MemberSnapshotPatch {
    pub guild_id: String,
    pub user_id: String,
    pub role_ids: Option<Vec<String>>,
    pub timeout: MemberTimeoutUpdate,
}

pub(crate) fn timeout(data: &JsonValue) -> Option<MemberTimeoutUpdate> {
    match data.get("communication_disabled_until") {
        None => Some(MemberTimeoutUpdate::Missing),
        Some(JsonValue::Null) => Some(MemberTimeoutUpdate::Clear),
        Some(JsonValue::String(value)) => timestamp_ceil_ms(value).map(MemberTimeoutUpdate::Until),
        Some(_) => None,
    }
}

fn timestamp_ceil_ms(value: &str) -> Option<u64> {
    if value.trim() != value {
        return None;
    }
    let time = chrono::DateTime::parse_from_rfc3339(value).ok()?;
    let mut milliseconds = time.timestamp_millis();
    if value.as_bytes().get(19) == Some(&b'.') {
        let mut fraction = value.as_bytes()[20..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .skip(3);
        if fraction.any(|byte| *byte != b'0') {
            milliseconds = milliseconds.checked_add(1)?;
        }
    }
    Some(u64::try_from(milliseconds).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse_json;

    #[test]
    fn timestamps_respect_calendar_dates_offsets_and_the_unix_epoch() {
        for (value, expected) in [
            ("1970-01-01T00:00:00Z", 0),
            ("1970-01-01T01:00:01+01:00", 1000),
            ("1969-12-31T23:00:01-01:00", 1000),
            ("1969-12-31T23:59:59.999Z", 0),
            ("2000-02-29T00:00:00Z", 951_782_400_000),
            ("2024-02-29T09:00:00+09:00", 1_709_164_800_000),
            ("2016-12-31T23:59:60Z", 1_483_228_800_000),
        ] {
            assert_eq!(timestamp_ceil_ms(value), Some(expected), "{value}");
        }
        for value in [
            "2023-02-29T00:00:00Z",
            "2000-02-30T00:00:00Z",
            "2024-13-01T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T00:00:00+24:00",
            "2024-01-01T00:00:00",
            "2024-01-01",
            " 2024-01-01T00:00:00Z",
            "2024-01-01T00:00:00Z\n",
            "2024-01-01T00:00:00Zjunk",
            "",
        ] {
            assert_eq!(timestamp_ceil_ms(value), None, "{value:?}");
        }
    }

    #[test]
    fn fractional_expiry_never_ends_a_timeout_early() {
        for (fraction, expected) in [
            ("1", 100),
            ("12", 120),
            ("123", 123),
            ("123000", 123),
            ("123001", 124),
            ("000000001", 1),
            ("000000000000000001", 1),
            ("999999999999999999", 1000),
        ] {
            let value = format!("1970-01-01T00:00:00.{fraction}Z");
            assert_eq!(timestamp_ceil_ms(&value), Some(expected), "{value}");
        }
    }

    #[test]
    fn timeout_fields_distinguish_omission_clearing_expiry_and_invalid_values() {
        for (body, expected) in [
            ("{}", Some(MemberTimeoutUpdate::Missing)),
            (
                r#"{"communication_disabled_until":null}"#,
                Some(MemberTimeoutUpdate::Clear),
            ),
            (
                r#"{"communication_disabled_until":"1970-01-01T00:00:01Z"}"#,
                Some(MemberTimeoutUpdate::Until(1000)),
            ),
            (r#"{"communication_disabled_until":0}"#, None),
            (r#"{"communication_disabled_until":false}"#, None),
            (r#"{"communication_disabled_until":[]}"#, None),
            (r#"{"communication_disabled_until":{}}"#, None),
            (r#"{"communication_disabled_until":"invalid"}"#, None),
        ] {
            assert_eq!(timeout(&parse_json(body).unwrap()), expected, "{body}");
        }
    }
}
