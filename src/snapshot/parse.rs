use std::collections::HashSet;

use super::{ChannelPermissionOverwrite, GuildSnapshot, RoleSnapshot};
use crate::json::JsonValue;
use crate::permission::PermissionSet;
use crate::validation::validate_snowflake;

pub(crate) fn snowflake<'a>(
    value: &'a JsonValue,
    key: &str,
    fallback: &'a str,
    allow_empty: bool,
) -> Option<&'a str> {
    let id = match value.get(key) {
        None => fallback,
        Some(JsonValue::String(id)) => id,
        Some(JsonValue::Null) if allow_empty && fallback.is_empty() => "",
        Some(_) => return None,
    };
    (validate_snowflake(id) || allow_empty && id.is_empty()).then_some(id)
}

pub(crate) fn snowflakes(value: &JsonValue) -> Option<Vec<String>> {
    let (ids, complete) = snowflakes_lossy(value);
    complete.then_some(ids)
}

pub(crate) fn snowflakes_lossy(value: &JsonValue) -> (Vec<String>, bool) {
    let Some(values) = value.as_array() else {
        return (Vec::new(), false);
    };
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let mut complete = true;
    for value in values {
        match value.as_str() {
            Some(id) if validate_snowflake(id) && seen.insert(id) => ids.push(id.to_string()),
            _ => complete = false,
        }
    }
    (ids, complete)
}

pub(crate) fn guild(value: &JsonValue, now_ms: u64) -> Option<GuildSnapshot> {
    let id = snowflake(value, "id", "", false)?;
    let unavailable = match value.get("unavailable") {
        Some(JsonValue::Bool(value)) => *value,
        None => false,
        _ => return None,
    };
    let owner_id = match value.get("owner_id") {
        Some(JsonValue::String(id)) if validate_snowflake(id) => Some(id.clone()),
        None => None,
        _ => return None,
    };
    let mut snapshot = GuildSnapshot::new(id, unavailable, now_ms);
    snapshot.owner_id = owner_id;
    Some(snapshot)
}

pub(crate) fn role(value: &JsonValue, guild_id: &str) -> Option<RoleSnapshot> {
    let id = value.get("id")?.as_str()?;
    if !validate_snowflake(id) || !validate_snowflake(guild_id) {
        return None;
    }
    Some(RoleSnapshot {
        guild_id: guild_id.to_string(),
        role_id: id.to_string(),
        permissions: PermissionSet::parse_decimal(value.get("permissions")?.as_str()?).ok()?,
        position: value
            .get("position")
            .and_then(JsonValue::as_i64)
            .filter(|value| *value >= 0)?,
    })
}

pub(crate) fn overwrite(value: &JsonValue) -> Option<ChannelPermissionOverwrite> {
    let id = value.get("id")?.as_str()?;
    let kind = i32::try_from(value.get("type")?.as_i64()?).ok()?;
    if !validate_snowflake(id) || !matches!(kind, 0 | 1) {
        return None;
    }
    Some(ChannelPermissionOverwrite {
        overwrite_id: id.to_string(),
        kind,
        allow: PermissionSet::parse_decimal(value.get("allow")?.as_str()?).ok()?,
        deny: PermissionSet::parse_decimal(value.get("deny")?.as_str()?).ok()?,
    })
}

pub(crate) fn overwrites(value: &JsonValue) -> Option<Vec<ChannelPermissionOverwrite>> {
    let (values, complete) = overwrites_lossy(value);
    complete.then_some(values)
}

pub(crate) fn overwrites_lossy(value: &JsonValue) -> (Vec<ChannelPermissionOverwrite>, bool) {
    let Some(values) = value.as_array() else {
        return (Vec::new(), false);
    };
    let mut seen = HashSet::new();
    let mut output = Vec::new();
    let mut complete = true;
    for value in values {
        match overwrite(value) {
            Some(overwrite) if seen.insert((overwrite.kind, overwrite.overwrite_id.clone())) => {
                output.push(overwrite)
            }
            _ => complete = false,
        }
    }
    (output, complete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse_json;

    #[test]
    fn optional_identifiers_reject_malformed_present_values_instead_of_using_the_fallback() {
        for input in [
            r#"{"id":12}"#,
            r#"{"id":null}"#,
            r#"{"id":"0"}"#,
            r#"{"id":"not-an-id"}"#,
        ] {
            assert_eq!(
                snowflake(&parse_json(input).unwrap(), "id", "10", false),
                None
            );
        }
        assert_eq!(
            snowflake(&parse_json("{}").unwrap(), "id", "10", false),
            Some("10")
        );
        assert_eq!(
            snowflake(
                &parse_json(r#"{"guild_id":null}"#).unwrap(),
                "guild_id",
                "",
                true
            ),
            Some("")
        );
        assert_eq!(
            snowflake(
                &parse_json(r#"{"guild_id":null}"#).unwrap(),
                "guild_id",
                "10",
                true
            ),
            None
        );
    }

    #[test]
    fn invalid_role_arrays_are_not_silently_reduced_to_a_different_member() {
        for input in [
            r#"["10",12]"#,
            r#"["10","0"]"#,
            r#"["10",null]"#,
            r#"["10","bad"]"#,
            "{}",
            "null",
        ] {
            assert_eq!(snowflakes(&parse_json(input).unwrap()), None, "{input}");
        }
        assert_eq!(
            snowflakes(&parse_json(r#"["10","20"]"#).unwrap()),
            Some(vec!["10".into(), "20".into()])
        );
        assert_eq!(snowflakes(&parse_json("[]").unwrap()), Some(vec![]));
    }

    #[test]
    fn permission_overwrites_require_known_kinds_and_unambiguous_keys() {
        for kind in ["-1", "2", r#""0""#, "null"] {
            let input = format!(r#"{{"id":"10","type":{kind},"allow":"0","deny":"1024"}}"#);
            assert!(overwrite(&parse_json(&input).unwrap()).is_none());
        }
        let values = parse_json(r#"[{"id":"10","type":0,"allow":"0","deny":"1024"},{"id":"10","type":0,"allow":"1024","deny":"0"}]"#).unwrap();
        assert!(overwrites(&values).is_none());
        let (kept, complete) = overwrites_lossy(&values);
        assert!(!complete);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].deny.has(crate::permission::BIT_VIEW_CHANNEL));
    }

    #[test]
    fn role_positions_must_be_nonnegative_integers_when_present() {
        assert!(role(
            &parse_json(r#"{"id":"10","permissions":"8"}"#).unwrap(),
            "1"
        )
        .is_none());
        for position in ["-1", "1.5", r#""10""#, "null"] {
            let input = format!(r#"{{"id":"10","permissions":"8","position":{position}}}"#);
            assert!(role(&parse_json(&input).unwrap(), "1").is_none());
        }
        let value = parse_json(r#"{"id":"10","permissions":"8","position":5}"#).unwrap();
        assert_eq!(role(&value, "1").unwrap().position, 5);
        assert!(role(&value, "bad").is_none());
    }
}
