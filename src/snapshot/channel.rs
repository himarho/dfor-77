use super::{ChannelPermissionOverwrite, ChannelSnapshot};
use crate::json::JsonValue;
use crate::validation::validate_snowflake;

#[derive(Clone, Default, Eq, PartialEq)]
pub struct ChannelSnapshotPatch {
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub channel_type: Option<i32>,
    pub parent_channel_id: Option<Option<String>>,
    pub archived: Option<bool>,
    pub locked: Option<bool>,
    pub current_user_thread_joined: Option<bool>,
    pub permission_overwrites: Option<Vec<ChannelPermissionOverwrite>>,
    pub obfuscated: Option<bool>,
}

impl ChannelSnapshotPatch {
    pub(crate) fn has_complete_state(&self) -> bool {
        let Some(kind) = self.channel_type else {
            return false;
        };
        self.guild_id.is_some()
            && self.parent_channel_id.is_some()
            && self.permission_overwrites.is_some()
            && self.obfuscated.is_some()
            && (!matches!(kind, 10..=12)
                || self.archived.is_some()
                    && self.locked.is_some()
                    && self.parent_channel_id.as_ref().is_some_and(Option::is_some))
    }

    pub(crate) fn affects_thread_access(&self) -> bool {
        self.permission_overwrites.is_some()
            || self.parent_channel_id.is_some()
            || self.obfuscated == Some(true)
    }

    pub(crate) fn apply(self, snapshot: &mut ChannelSnapshot, now_ms: u64) {
        if self
            .guild_id
            .as_ref()
            .is_some_and(|id| *id != snapshot.guild_id)
            || self
                .channel_type
                .is_some_and(|kind| kind != snapshot.channel_type)
        {
            snapshot.complete = false;
            snapshot.current_user_thread_joined = None;
            snapshot.current_user_thread_joined_at_ms = None;
        }
        if let Some(guild_id) = self.guild_id {
            snapshot.guild_id = guild_id;
        }
        if let Some(channel_type) = self.channel_type {
            snapshot.channel_type = channel_type;
        }
        if let Some(parent) = self.parent_channel_id {
            snapshot.parent_channel_id = parent;
        }
        if let Some(archived) = self.archived {
            snapshot.archived = archived;
        }
        if let Some(locked) = self.locked {
            snapshot.locked = locked;
        }
        if let Some(joined) = self.current_user_thread_joined {
            snapshot.current_user_thread_joined = Some(joined);
            snapshot.current_user_thread_joined_at_ms = Some(now_ms);
        }
        if let Some(overwrites) = self.permission_overwrites {
            snapshot.has_permission_overwrites = !overwrites.is_empty();
            snapshot.permission_overwrites = overwrites;
        }
        if let Some(obfuscated) = self.obfuscated {
            if !obfuscated && snapshot.obfuscated {
                snapshot.complete = false;
            }
            snapshot.obfuscated = obfuscated;
        }
    }
}

pub(crate) fn patch(data: &JsonValue) -> Option<ChannelSnapshotPatch> {
    fields(data, "", false).map(|(patch, _)| patch)
}

pub(crate) fn full(
    data: &JsonValue,
    fallback_guild_id: &str,
    fallback_channel_id: &str,
    now_ms: u64,
    lossy: bool,
) -> Option<ChannelSnapshot> {
    let (patch, complete) = fields(data, fallback_channel_id, lossy)?;
    let kind = patch.channel_type.unwrap_or(0);
    let guild_id = patch.guild_id.as_deref().unwrap_or(fallback_guild_id);
    if !guild_id.is_empty() && !validate_snowflake(guild_id)
        || guild_id.is_empty() && !matches!(kind, 1 | 3)
    {
        return None;
    }
    let mut snapshot = ChannelSnapshot::new(guild_id, &patch.channel_id, kind, now_ms);
    let thread_fields_known = !matches!(kind, 10..=12)
        || patch
            .parent_channel_id
            .as_ref()
            .is_some_and(Option::is_some)
            && patch.archived.is_some()
            && patch.locked.is_some();
    snapshot.complete = complete && patch.channel_type.is_some() && thread_fields_known;
    patch.apply(&mut snapshot, now_ms);
    Some(snapshot)
}

fn fields(
    data: &JsonValue,
    fallback_channel_id: &str,
    lossy: bool,
) -> Option<(ChannelSnapshotPatch, bool)> {
    let channel_id = super::parse::snowflake(data, "id", fallback_channel_id, false)?;
    if !validate_snowflake(channel_id) {
        return None;
    }
    let guild_id = match data.get("guild_id") {
        None => None,
        Some(value) => {
            let id = value.as_str()?;
            if !validate_snowflake(id) {
                return None;
            }
            Some(id.to_string())
        }
    };
    let channel_type = optional(data, "type", |value| i32::try_from(value.as_u64()?).ok())?;
    let parent_channel_id = match data.get("parent_id") {
        None => None,
        Some(JsonValue::Null) => Some(None),
        Some(value) => {
            let id = value.as_str()?;
            if !validate_snowflake(id) {
                return None;
            }
            Some(Some(id.to_string()))
        }
    };
    let (archived, locked) = match data.get("thread_metadata") {
        None => (None, None),
        Some(metadata @ JsonValue::Object(_)) => (
            optional_bool(metadata, "archived")?,
            optional_bool(metadata, "locked")?,
        ),
        Some(_) => return None,
    };
    let joined = optional_bool(data, "current_user_joined")?;
    let legacy_joined = optional_bool(data, "joined")?;
    if joined.zip(legacy_joined).is_some_and(|(a, b)| a != b) {
        return None;
    }
    let mut has_member = false;
    for key in ["member", "thread_member"] {
        match data.get(key) {
            None => {}
            Some(JsonValue::Object(_)) => has_member = true,
            Some(_) => return None,
        }
    }
    let current_user_thread_joined = joined
        .or(legacy_joined)
        .or_else(|| has_member.then_some(true));
    let (permission_overwrites, complete) = match data.get("permission_overwrites") {
        None => (None, true),
        Some(value) if lossy => {
            let (parsed, complete) = super::parse::overwrites_lossy(value);
            (Some(parsed), complete)
        }
        Some(value) => (Some(super::parse::overwrites(value)?), true),
    };
    let obfuscated = optional(data, "flags", |value| {
        value.as_u64().map(|flags| flags & (1 << 17) != 0)
    })?;
    Some((
        ChannelSnapshotPatch {
            channel_id: channel_id.to_string(),
            guild_id,
            channel_type,
            parent_channel_id,
            archived,
            locked,
            current_user_thread_joined,
            permission_overwrites,
            obfuscated,
        },
        complete,
    ))
}

fn optional_bool(data: &JsonValue, key: &str) -> Option<Option<bool>> {
    optional(data, key, JsonValue::as_bool)
}

fn optional<T>(
    data: &JsonValue,
    key: &str,
    parse: impl FnOnce(&JsonValue) -> Option<T>,
) -> Option<Option<T>> {
    match data.get(key) {
        None => Some(None),
        Some(value) => parse(value).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse_json;

    #[test]
    fn malformed_present_channel_fields_are_never_treated_as_omitted() {
        for field in [
            r#""guild_id":null"#,
            r#""guild_id":"invalid""#,
            r#""type":-1"#,
            r#""type":2147483648"#,
            r#""type":"0""#,
            r#""parent_id":false"#,
            r#""parent_id":"invalid""#,
            r#""thread_metadata":null"#,
            r#""thread_metadata":{"locked":0}"#,
            r#""thread_metadata":{"archived":"false"}"#,
            r#""current_user_joined":null"#,
            r#""current_user_joined":false,"joined":true"#,
            r#""member":false"#,
            r#""thread_member":null"#,
            r#""permission_overwrites":null"#,
            r#""permission_overwrites":[{"id":"10","type":3,"allow":"8","deny":"0"}]"#,
            r#""flags":-1"#,
            r#""flags":"131072""#,
            r#""flags":18446744073709551616"#,
        ] {
            let data = parse_json(&format!(r#"{{"id":"20",{field}}}"#)).unwrap();
            assert!(patch(&data).is_none(), "{field}");
        }
    }

    #[test]
    fn full_channel_parsing_preserves_partial_overwrites_without_trusting_them() {
        let data = parse_json(r#"{"id":"20","type":0,"permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"1024"},{"id":"99","type":3,"allow":"8","deny":"0"}]}"#).unwrap();
        assert!(full(&data, "10", "", 0, false).is_none());
        let channel = full(&data, "10", "", 0, true).unwrap();
        assert!(!channel.complete);
        assert_eq!(channel.permission_overwrites.len(), 1);
        assert!(channel.permission_overwrites[0].deny.has(10));
    }

    #[test]
    fn obfuscation_is_detected_from_its_flag_and_respects_other_flags() {
        for (flags, expected) in [
            (0, false),
            (1 << 17, true),
            (1 << 21, false),
            (2228224, true),
        ] {
            let data = parse_json(&format!(r#"{{"id":"20","type":0,"flags":{flags}}}"#)).unwrap();
            let channel = full(&data, "10", "", 0, false).unwrap();
            assert_eq!(channel.obfuscated, expected);
        }
        assert_eq!(
            patch(&parse_json(r#"{"id":"20"}"#).unwrap())
                .unwrap()
                .obfuscated,
            None
        );
    }

    #[test]
    fn full_threads_need_a_parent_and_known_archive_and_lock_state() {
        for fields in [
            r#""parent_id":"30""#,
            r#""parent_id":"30","thread_metadata":{"archived":false}"#,
            r#""parent_id":"30","thread_metadata":{"locked":false}"#,
            r#""thread_metadata":{"archived":false,"locked":false}"#,
            r#""parent_id":null,"thread_metadata":{"archived":false,"locked":false}"#,
        ] {
            let data = parse_json(&format!(r#"{{"id":"20","type":12,{fields}}}"#)).unwrap();
            for lossy in [false, true] {
                let thread = full(&data, "10", "", 0, lossy).unwrap();
                assert!(!thread.complete, "{fields}");
            }
        }
    }
}
