use super::revision::{MessageCachePolicy, MessageRevision};
use super::webhook::MessageOrigin;
use super::{message_key, MessageSnapshot, MessageSnapshotPatch, SnapshotStore};
use crate::json::JsonValue;
use crate::permission::PermissionSet;
use crate::validation::validate_snowflake;

impl SnapshotStore {
    pub(crate) fn apply_rest_message_response(
        &mut self,
        data: &JsonValue,
        channel_id: &str,
        message_id: &str,
        now_ms: u64,
    ) -> bool {
        self.apply_rest_message_response_with_origin(
            data,
            channel_id,
            message_id,
            MessageOrigin::from_data(data, true).unwrap_or(MessageOrigin::Unknown),
            now_ms,
        )
    }

    pub(super) fn apply_rest_message_response_with_origin(
        &mut self,
        data: &JsonValue,
        channel_id: &str,
        message_id: &str,
        origin: MessageOrigin,
        now_ms: u64,
    ) -> bool {
        self.prune(now_ms);
        let Some(mut patch) = patch(data).filter(|patch| {
            patch.channel_id == channel_id
                && patch.message_id == message_id
                && patch.user_id.is_some()
                && patch.content.is_some()
                && patch.image_attachment_count.is_some()
                && patch.message_type.is_some()
        }) else {
            self.invalidate_message(channel_id, message_id, now_ms);
            return false;
        };
        if !self.message_origin_matches(channel_id, message_id, &origin, true) {
            self.invalidate_message(channel_id, message_id, now_ms);
            return false;
        }
        if !self.message_identity_matches(
            channel_id,
            message_id,
            patch.guild_id.as_deref(),
            patch.user_id.as_deref(),
        ) {
            self.invalidate_message(channel_id, message_id, now_ms);
            return false;
        }
        let Some(revision) = MessageRevision::parse(data) else {
            self.invalidate_message(channel_id, message_id, now_ms);
            return false;
        };
        let key = message_key(channel_id, message_id);
        let channel_guild = self
            .channels
            .get(channel_id)
            .filter(|channel| {
                channel.complete && (!channel.guild_id.is_empty() || channel.channel_type == 1)
            })
            .map(|channel| channel.guild_id.clone());
        if patch
            .guild_id
            .as_ref()
            .zip(channel_guild.as_ref())
            .is_some_and(|(guild, known)| guild != known)
        {
            self.invalidate_message(channel_id, message_id, now_ms);
            return false;
        }
        patch.guild_id = patch
            .guild_id
            .or_else(|| {
                self.messages
                    .get(&key)
                    .map(|message| message.guild_id.clone())
            })
            .filter(|guild| !guild.is_empty())
            .or_else(|| channel_guild.filter(|guild| !guild.is_empty()));
        patch.author_is_bot = Some(patch.author_is_bot.unwrap_or(false));
        patch.role_ids = Some(patch.role_ids.unwrap_or_default());
        match self.message_cache_policy(channel_id, message_id, revision.as_ref(), true) {
            MessageCachePolicy::Ignore => return true,
            MessageCachePolicy::Invalidate => {
                self.discard_message_content(channel_id, message_id);
                return false;
            }
            MessageCachePolicy::Apply => {}
        }
        let applied = self.patch_message_checked(patch, now_ms)
            && self.set_message_origin(channel_id, message_id, origin, true);
        if applied {
            self.set_message_revision(channel_id, message_id, revision);
        }
        applied
    }
}

pub(crate) fn patch(data: &JsonValue) -> Option<MessageSnapshotPatch> {
    fields(data, "")
}

fn fields(data: &JsonValue, fallback_channel_id: &str) -> Option<MessageSnapshotPatch> {
    MessageRevision::parse(data)?;
    optional(data, "webhook_id", |value| {
        validate_snowflake(value.as_str()?).then_some(())
    })?;
    optional(data, "application_id", |value| {
        validate_snowflake(value.as_str()?).then_some(())
    })?;
    let channel_id = super::parse::snowflake(data, "channel_id", fallback_channel_id, false)?;
    let message_id = data.get("id")?.as_str()?;
    if !validate_snowflake(channel_id) || !validate_snowflake(message_id) {
        return None;
    }
    let guild_id = optional(data, "guild_id", |value| {
        let id = value.as_str()?;
        validate_snowflake(id).then(|| id.to_string())
    })?;
    let author = optional(data, "author", |author| {
        let id = author.get("id")?.as_str()?;
        validate_snowflake(id).then_some(author)
    })?;
    let author_is_bot = match author {
        Some(author) => optional(author, "bot", JsonValue::as_bool)?,
        None => None,
    };
    let member = optional(data, "member", |value| {
        matches!(value, JsonValue::Object(_)).then_some(value)
    })?;
    let role_ids = match member {
        Some(member) => optional(member, "roles", super::parse::snowflakes)?,
        None => None,
    };
    let member_permissions = match member {
        Some(member) => optional(member, "permissions", |value| {
            PermissionSet::parse_decimal(value.as_str()?).ok()
        })?,
        None => None,
    };
    let image_attachment_count = optional(data, "attachments", |value| {
        let attachments = value.as_array()?;
        for attachment in attachments {
            if !matches!(attachment, JsonValue::Object(_)) {
                return None;
            }
            for key in ["filename", "content_type"] {
                optional(attachment, key, JsonValue::as_str)?;
            }
        }
        Some(
            attachments
                .iter()
                .filter(|attachment| attachment_is_image(attachment))
                .count(),
        )
    })?;
    Some(MessageSnapshotPatch {
        guild_id,
        channel_id: channel_id.to_string(),
        message_id: message_id.to_string(),
        user_id: author
            .and_then(|author| author.get("id")?.as_str())
            .map(ToOwned::to_owned),
        content: optional(data, "content", |value| {
            value.as_str().map(ToOwned::to_owned)
        })?,
        author_is_bot,
        role_ids,
        image_attachment_count,
        member_permissions,
        message_type: optional(data, "type", |value| i32::try_from(value.as_u64()?).ok())?,
    })
}

pub(crate) fn full(
    data: &JsonValue,
    fallback_guild_id: &str,
    fallback_channel_id: &str,
    now_ms: u64,
) -> Option<MessageSnapshot> {
    let guild_id = super::parse::snowflake(data, "guild_id", fallback_guild_id, true)?;
    let channel_id = super::parse::snowflake(data, "channel_id", fallback_channel_id, false)?;
    let patch = fields(data, fallback_channel_id)?;
    let user_id = patch.user_id?;
    if !fallback_guild_id.is_empty() && guild_id != fallback_guild_id
        || !fallback_channel_id.is_empty() && channel_id != fallback_channel_id
    {
        return None;
    }
    let mut snapshot =
        MessageSnapshot::new(guild_id, channel_id, patch.message_id, user_id, now_ms);
    snapshot.content_available = patch.content.is_some();
    snapshot.content = patch.content.unwrap_or_default();
    snapshot.author_is_bot = patch.author_is_bot.unwrap_or(false);
    snapshot.role_ids = patch.role_ids.unwrap_or_default();
    snapshot.image_attachment_count = patch.image_attachment_count.unwrap_or(0);
    snapshot.member_permissions = patch.member_permissions;
    snapshot.message_type = patch.message_type.unwrap_or(0);
    snapshot.complete = patch.message_type.is_some();
    Some(snapshot)
}

fn optional<'a, T>(
    data: &'a JsonValue,
    key: &str,
    parse: impl FnOnce(&'a JsonValue) -> Option<T>,
) -> Option<Option<T>> {
    match data.get(key) {
        None => Some(None),
        Some(value) => parse(value).map(Some),
    }
}

fn attachment_is_image(attachment: &JsonValue) -> bool {
    attachment
        .get("content_type")
        .and_then(JsonValue::as_str)
        .is_some_and(|content_type| {
            content_type
                .trim_start()
                .get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
        })
        || attachment
            .get("filename")
            .and_then(JsonValue::as_str)
            .is_some_and(|file_name| {
                file_name.rsplit_once('.').is_some_and(|(_, extension)| {
                    [
                        "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "avif", "heic",
                        "heif",
                    ]
                    .iter()
                    .any(|known| extension.eq_ignore_ascii_case(known))
                })
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse_json;
    use crate::snapshot::{ChannelSnapshot, SnapshotLookup};

    #[test]
    fn rest_message_responses_keep_known_context_without_renewing_absent_authority() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let mut before = MessageSnapshot::new("10", "20", "30", "40", 0);
        before.author_is_bot = true;
        before.role_ids = vec!["50".into()];
        before.member_permissions = Some(PermissionSet::from_bits(&[3]));
        store.upsert_message(before);
        let data = parse_json(r#"{"id":"30","channel_id":"20","author":{"id":"40"},"content":"updated","type":0,"attachments":[{"filename":"new.PNG"}]}"#).unwrap();
        assert!(store.apply_rest_message_response(&data, "20", "30", 101));
        let SnapshotLookup::Found(message) = store.get_message("20", "30", 101) else {
            panic!("expected refreshed message");
        };
        assert_eq!(message.guild_id, "10");
        assert_eq!(message.captured_at_ms, 101);
        assert_eq!(message.image_attachment_count, 1);
        assert_eq!(message.member_permissions, None);
        assert!(message.role_ids.is_empty());
        assert!(!message.author_is_bot);
        assert!(message.complete);
    }

    #[test]
    fn rest_message_responses_can_fill_uncached_guild_and_direct_messages() {
        for context in 0..4 {
            let mut store = SnapshotStore::new(100, 100, 10, 10);
            if context < 2 {
                store.upsert_channel(ChannelSnapshot::new(
                    if context == 0 { "10" } else { "" },
                    "20",
                    if context == 0 { 0 } else { 1 },
                    0,
                ));
            }
            let guild = if context == 2 {
                r#""guild_id":"10","#
            } else {
                ""
            };
            let data = parse_json(&format!(r#"{{"id":"30","channel_id":"20",{guild}"author":{{"id":"40","bot":true}},"content":"new","type":0,"attachments":[]}}"#)).unwrap();
            assert!(store.apply_rest_message_response(&data, "20", "30", 1));
            let SnapshotLookup::Found(message) = store.get_message("20", "30", 1) else {
                panic!("expected message");
            };
            assert_eq!(message.complete, context != 3);
            assert_eq!(
                message.guild_id,
                if context == 0 || context == 2 {
                    "10"
                } else {
                    ""
                }
            );
            assert!(message.author_is_bot);
        }
    }

    #[test]
    fn rest_message_responses_preserve_tombstones_and_reject_conflicting_channel_context() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let data = parse_json(r#"{"id":"30","channel_id":"20","guild_id":"10","author":{"id":"40"},"content":"old","type":0,"attachments":[]}"#).unwrap();
        store.tombstone_message("20", "30", 0);
        assert!(store.apply_rest_message_response(&data, "20", "30", 1));
        assert_eq!(store.get_message("20", "30", 1), SnapshotLookup::Tombstone);
        for channel in [
            ChannelSnapshot::new("11", "20", 0, 0),
            ChannelSnapshot::new("", "20", 1, 0),
        ] {
            let mut store = SnapshotStore::new(100, 100, 10, 10);
            store.upsert_channel(channel);
            assert!(!store.apply_rest_message_response(&data, "20", "30", 1));
            assert_eq!(store.get_message("20", "30", 1), SnapshotLookup::Missing);
        }
    }

    #[test]
    fn malformed_present_message_fields_are_rejected_in_both_full_and_partial_messages() {
        for field in [
            r#""webhook_id":null"#,
            r#""webhook_id":"invalid""#,
            r#""application_id":false"#,
            r#""application_id":"invalid""#,
            r#""guild_id":null"#,
            r#""guild_id":"invalid""#,
            r#""author":{"id":"40","bot":"false"}"#,
            r#""author":null"#,
            r#""content":null"#,
            r#""member":false"#,
            r#""member":{"roles":["50",false]}"#,
            r#""member":{"roles":["50","50"]}"#,
            r#""member":{"permissions":8}"#,
            r#""member":{"permissions":null}"#,
            r#""attachments":null"#,
            r#""attachments":[false]"#,
            r#""attachments":[{"filename":false}]"#,
            r#""attachments":[{"content_type":null}]"#,
            r#""type":-1"#,
            r#""type":2147483648"#,
        ] {
            let data = parse_json(&format!(r#"{{"id":"30","channel_id":"20",{field}}}"#)).unwrap();
            assert!(patch(&data).is_none(), "{field}");
            assert!(full(&data, "10", "20", 0).is_none(), "{field}");
        }
    }

    #[test]
    fn optional_message_fields_remain_distinct_from_empty_values() {
        let omitted = patch(&parse_json(r#"{"id":"30","channel_id":"20"}"#).unwrap()).unwrap();
        assert_eq!(omitted.content, None);
        assert_eq!(omitted.author_is_bot, None);
        assert_eq!(omitted.image_attachment_count, None);
        let empty = patch(&parse_json(r#"{"id":"30","channel_id":"20","content":"","attachments":[],"member":{"roles":[]},"author":{"id":"40"}}"#).unwrap()).unwrap();
        assert_eq!(empty.content.as_deref(), Some(""));
        assert_eq!(empty.role_ids, Some(Vec::new()));
        assert_eq!(empty.author_is_bot, None);
        assert_eq!(empty.image_attachment_count, Some(0));
    }

    #[test]
    fn image_detection_keeps_mime_and_all_filename_fallbacks_without_case_allocations() {
        for extension in [
            "PNG", "JPG", "JPEG", "GIF", "WEBP", "BMP", "TIF", "TIFF", "AVIF", "HEIC", "HEIF",
        ] {
            let data = parse_json(&format!(r#"{{"id":"30","channel_id":"20","attachments":[{{"filename":"日本語.{extension}"}}]}}"#)).unwrap();
            assert_eq!(patch(&data).unwrap().image_attachment_count, Some(1));
        }
        let data = parse_json(r#"{"id":"30","channel_id":"20","attachments":[{"content_type":" IMAGE/PNG","filename":"no-extension"},{"content_type":"日本語/image","filename":"text.txt"},{"filename":"photo.png.backup"},{"filename":"photo.png "},{"content_type":"application/octet-stream","filename":"photo.png"}]}"#).unwrap();
        assert_eq!(patch(&data).unwrap().image_attachment_count, Some(2));
    }

    #[test]
    fn full_messages_preserve_fallback_context_and_mark_unknown_types_incomplete() {
        let data = parse_json(r#"{"id":"30","author":{"id":"40"},"attachments":[]}"#).unwrap();
        let message = full(&data, "10", "20", 7).unwrap();
        assert_eq!(message.guild_id, "10");
        assert_eq!(message.channel_id, "20");
        assert!(!message.complete);
        assert!(!message.content_available);
        let data =
            parse_json(r#"{"id":"30","channel_id":"21","type":0,"author":{"id":"40"}}"#).unwrap();
        assert!(full(&data, "10", "20", 7).is_none());
    }
}
