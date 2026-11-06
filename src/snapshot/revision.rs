use super::webhook::MessageOrigin;
use super::{message_key, MessageSnapshot, SnapshotLookup, SnapshotStore};
use crate::json::JsonValue;
use crate::memory::HeapSize;

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum MessageRevision {
    Unedited,
    Edited {
        time: chrono::DateTime<chrono::Utc>,
        subnanosecond: String,
    },
}

impl MessageRevision {
    pub(crate) fn parse(data: &JsonValue) -> Option<Option<Self>> {
        match data.get("edited_timestamp") {
            None => Some(None),
            Some(JsonValue::Null) => Some(Some(Self::Unedited)),
            Some(JsonValue::String(value)) if value.trim() == value => {
                let time = chrono::DateTime::parse_from_rfc3339(value).ok()?.to_utc();
                let subnanosecond = if value.as_bytes().get(19) == Some(&b'.') {
                    value[20..]
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .skip(9)
                        .collect::<String>()
                        .trim_end_matches('0')
                        .to_string()
                } else {
                    String::new()
                };
                Some(Some(Self::Edited {
                    time,
                    subnanosecond,
                }))
            }
            _ => None,
        }
    }
}

impl HeapSize for MessageRevision {
    fn heap_size(&self) -> usize {
        match self {
            Self::Unedited => 0,
            Self::Edited { subnanosecond, .. } => subnanosecond.heap_size(),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum MessageCachePolicy {
    Apply,
    Ignore,
    Invalidate,
}

impl SnapshotStore {
    pub(crate) fn discard_message_content(&mut self, channel: &str, message: &str) {
        self.messages
            .update(&message_key(channel, message), |stored| {
                stored.content.clear();
                stored.content_available = false;
                stored.image_attachment_count = 0;
                stored.role_ids.clear();
                stored.member_permissions = None;
                stored.complete = false;
            });
    }

    pub(crate) fn message_cache_policy(
        &self,
        channel: &str,
        message: &str,
        revision: Option<&MessageRevision>,
        changes_content: bool,
    ) -> MessageCachePolicy {
        let stored = self
            .messages
            .get(&message_key(channel, message))
            .and_then(|message| message.revision.as_ref());
        match (stored, revision) {
            (Some(stored), Some(incoming)) if incoming < stored => MessageCachePolicy::Ignore,
            (Some(_), None) if changes_content => MessageCachePolicy::Invalidate,
            _ => MessageCachePolicy::Apply,
        }
    }

    pub(crate) fn set_message_revision(
        &mut self,
        channel: &str,
        message: &str,
        revision: Option<MessageRevision>,
    ) {
        if let Some(revision) = revision {
            self.messages
                .update(&message_key(channel, message), |message| {
                    message.revision = Some(revision)
                });
            self.enforce_limits();
        }
    }

    pub(crate) fn message_identity_matches(
        &self,
        channel: &str,
        message: &str,
        guild: Option<&str>,
        user: Option<&str>,
    ) -> bool {
        self.messages
            .get(&message_key(channel, message))
            .is_none_or(|snapshot| {
                !guild.is_some_and(|id| {
                    (snapshot.complete || !snapshot.guild_id.is_empty()) && id != snapshot.guild_id
                }) && !user.is_some_and(|id| !snapshot.user_id.is_empty() && id != snapshot.user_id)
            })
    }

    pub(crate) fn cache_rest_message(
        &mut self,
        snapshot: MessageSnapshot,
        origin: MessageOrigin,
        revision: Option<MessageRevision>,
        now_ms: u64,
    ) -> Option<MessageSnapshot> {
        self.prune(now_ms);
        let channel = snapshot.channel_id.clone();
        let message = snapshot.message_id.clone();
        if self
            .tombstones
            .contains_key(&message_key(&channel, &message))
        {
            return None;
        }
        if !self.message_identity_matches(
            &channel,
            &message,
            Some(&snapshot.guild_id),
            Some(&snapshot.user_id),
        ) || !self.message_origin_matches(&channel, &message, &origin, true)
        {
            self.invalidate_message(&channel, &message, now_ms);
            return None;
        }
        match self.message_cache_policy(&channel, &message, revision.as_ref(), true) {
            MessageCachePolicy::Ignore => match self.get_message(&channel, &message, now_ms) {
                SnapshotLookup::Found(snapshot) | SnapshotLookup::Stale(snapshot) => Some(snapshot),
                _ => None,
            },
            MessageCachePolicy::Invalidate => {
                self.discard_message_content(&channel, &message);
                None
            }
            MessageCachePolicy::Apply => {
                self.upsert_message(snapshot.clone());
                self.set_message_origin(&channel, &message, origin, true);
                self.set_message_revision(&channel, &message, revision);
                Some(snapshot)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayIngestor;
    use crate::json::parse_json;

    fn body(content: &str, timestamp: &str) -> JsonValue {
        parse_json(&format!(r#"{{"guild_id":"10","channel_id":"20","id":"30","author":{{"id":"40"}},"type":0,"content":"{content}","attachments":[],"edited_timestamp":{timestamp}}}"#)).unwrap()
    }

    fn cache(store: &mut SnapshotStore, data: &JsonValue, now: u64) -> Option<MessageSnapshot> {
        store.cache_rest_message(
            super::super::message::full(data, "10", "20", now)?,
            MessageOrigin::from_data(data, true)?,
            MessageRevision::parse(data)?,
            now,
        )
    }

    #[test]
    fn revisions_compare_offsets_and_fractional_digits_without_rounding_away_order() {
        let parse = |time: &str| {
            MessageRevision::parse(
                &parse_json(&format!(r#"{{"edited_timestamp":"{time}"}}"#)).unwrap(),
            )
            .unwrap()
            .unwrap()
        };
        assert!(parse("1970-01-01T00:00:01.1Z") == parse("1970-01-01T01:00:01.100000000000+01:00"));
        assert!(
            parse("1970-01-01T00:00:01.000000000001Z") < parse("1970-01-01T00:00:01.000000000002Z")
        );
        assert!(
            parse("1970-01-01T00:00:01.000000001Z") > parse("1970-01-01T00:00:01.0000000009999Z")
        );
        assert!(parse("1970-01-01T00:00:01Z") > MessageRevision::Unedited);
        for timestamp in [
            "false",
            "0",
            r#""invalid""#,
            r#""2023-02-29T00:00:00Z""#,
            r#"" 1970-01-01T00:00:01Z""#,
        ] {
            assert!(MessageRevision::parse(&body("bad", timestamp)).is_none());
            assert!(super::super::message::patch(&body("bad", timestamp)).is_none());
        }
    }

    #[test]
    fn rest_reads_keep_newer_content_and_cannot_revive_deleted_messages() {
        let mut store = SnapshotStore::new(1000, 1000, 16, 16);
        let newer = body("newer", r#""1970-01-01T00:00:02Z""#);
        let older = body("older", r#""1970-01-01T00:00:01Z""#);
        assert_eq!(cache(&mut store, &newer, 1).unwrap().content, "newer");
        assert_eq!(cache(&mut store, &older, 2).unwrap().content, "newer");
        store.tombstone_message("20", "30", 3);
        assert!(cache(&mut store, &newer, 4).is_none());
        assert!(matches!(
            store.get_message("20", "30", 4),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn unknown_update_order_keeps_the_revision_floor_until_a_full_read_recovers_content() {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 16, 16);
        let current = body("current", r#""1970-01-01T00:00:02Z""#);
        cache(ingestor.snapshot_mut(), &current, 0).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"MESSAGE_UPDATE","d":{"channel_id":"20","id":"30","embeds":[]}}"#, 1).unwrap();
        assert!(
            matches!(ingestor.snapshot_mut().get_message("20", "30", 1), SnapshotLookup::Found(message) if message.content == "current")
        );
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"MESSAGE_UPDATE","d":{"channel_id":"20","id":"30","content":"uncertain"}}"#, 2).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":3,"t":"MESSAGE_UPDATE","d":{"guild_id":"10","channel_id":"20","id":"30","author":{"id":"40"},"type":0,"content":"old","attachments":[{"filename":"old.png"}],"edited_timestamp":"1970-01-01T00:00:01Z"}}"#, 3).unwrap();
        assert!(
            matches!(ingestor.snapshot_mut().get_message("20", "30", 3), SnapshotLookup::Found(message) if !message.complete && !message.content_available && message.image_attachment_count == 0)
        );
        assert_eq!(
            cache(ingestor.snapshot_mut(), &current, 4).unwrap().content,
            "current"
        );
        assert!(
            matches!(ingestor.snapshot_mut().get_message("20", "30", 4), SnapshotLookup::Found(message) if message.complete)
        );
    }

    #[test]
    fn delayed_creation_does_not_overwrite_a_rest_edit_and_bad_identity_still_invalidates() {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 16, 16);
        cache(
            ingestor.snapshot_mut(),
            &body("edited", r#""1970-01-01T00:00:02Z""#),
            0,
        )
        .unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","author":{"id":"40"},"type":0,"content":"original","attachments":[],"edited_timestamp":null}}"#, 1).unwrap();
        assert!(
            matches!(ingestor.snapshot_mut().get_message("20", "30", 1), SnapshotLookup::Found(message) if message.content == "edited")
        );
        let mut bad = body("bad identity", r#""1970-01-01T00:00:01Z""#);
        if let JsonValue::Object(fields) = &mut bad {
            fields.insert("author".into(), parse_json(r#"{"id":"41"}"#).unwrap());
        }
        assert!(!ingestor
            .snapshot_mut()
            .apply_rest_message_response(&bad, "20", "30", 2));
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "30", 2),
            SnapshotLookup::Missing
        ));
    }

    #[test]
    fn revision_precision_is_included_in_the_shared_snapshot_memory_limit() {
        let mut store = SnapshotStore::new(1000, 1000, 16, 16);
        store.set_max_bytes(1024);
        let time = format!("\"1970-01-01T00:00:01.{}1Z\"", "0".repeat(2048));
        let data = body("bounded", &time);
        assert!(store.apply_rest_message_response(&data, "20", "30", 1));
        assert!(store.retained_bytes() <= 1024);
        assert!(matches!(
            store.get_message("20", "30", 1),
            SnapshotLookup::Missing
        ));
    }
}
