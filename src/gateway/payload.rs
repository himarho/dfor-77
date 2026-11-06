use crate::json::JsonValue;
use crate::validation::validate_snowflake;

pub(crate) fn valid_send_data(opcode: u64, data: &JsonValue) -> bool {
    match opcode {
        3 => valid_presence(data),
        4 => {
            valid_id(data.get("guild_id"))
                && data
                    .get("channel_id")
                    .is_some_and(|value| matches!(value, JsonValue::Null) || valid_id(Some(value)))
                && data.get("self_mute").and_then(JsonValue::as_bool).is_some()
                && data.get("self_deaf").and_then(JsonValue::as_bool).is_some()
        }
        8 => valid_member_request(data),
        31 => data
            .get("guild_ids")
            .and_then(JsonValue::as_array)
            .is_some_and(|ids| ids.iter().all(|id| valid_id(Some(id)))),
        43 => {
            valid_id(data.get("guild_id"))
                && data
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .is_some_and(|fields| fields.iter().all(|field| field.as_str().is_some()))
        }
        _ => true,
    }
}

fn valid_id(value: Option<&JsonValue>) -> bool {
    value
        .and_then(JsonValue::as_str)
        .is_some_and(validate_snowflake)
}

fn valid_presence(data: &JsonValue) -> bool {
    data.get("since").is_some_and(|value| {
        matches!(value, JsonValue::Null) || value.as_i64().is_some_and(|since| since >= 0)
    }) && data
        .get("activities")
        .and_then(JsonValue::as_array)
        .is_some_and(|activities| {
            activities.iter().all(|activity| {
                activity.get("name").and_then(JsonValue::as_str).is_some()
                    && activity.get("type").and_then(JsonValue::as_u64).is_some()
            })
        })
        && matches!(
            data.get("status").and_then(JsonValue::as_str),
            Some("online" | "dnd" | "idle" | "invisible" | "offline")
        )
        && data.get("afk").and_then(JsonValue::as_bool).is_some()
}

fn valid_member_request(data: &JsonValue) -> bool {
    if !valid_id(data.get("guild_id"))
        || !data
            .get("presences")
            .is_none_or(|value| value.as_bool().is_some())
        || !data
            .get("nonce")
            .is_none_or(|value| value.as_str().is_some_and(|nonce| nonce.len() <= 32))
        || !data
            .get("limit")
            .is_none_or(|value| value.as_u64().is_some())
    {
        return false;
    }
    match (data.get("query"), data.get("user_ids")) {
        (Some(query), None) => query.as_str().is_some() && data.get("limit").is_some(),
        (None, Some(ids)) => {
            valid_id(Some(ids))
                || ids.as_array().is_some_and(|ids| {
                    !ids.is_empty() && ids.len() <= 100 && ids.iter().all(|id| valid_id(Some(id)))
                })
        }
        _ => false,
    }
}
