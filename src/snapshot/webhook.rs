use super::{message_key, MessageSnapshot, SnapshotStore};
use crate::json::JsonValue;
use crate::memory::HeapSize;
use crate::validation::validate_snowflake;
use std::ops::{Deref, DerefMut};

const MAX_ORIGINAL_TARGET_TTL_MS: u64 = 15 * 60 * 1000;

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum MessageOrigin {
    Unknown,
    Known {
        webhook_id: Option<String>,
        application_id: Option<String>,
    },
}

impl MessageOrigin {
    pub(crate) fn from_data(data: &JsonValue, full: bool) -> Option<Self> {
        if !full && data.get("webhook_id").is_none() && data.get("application_id").is_none() {
            return None;
        }
        let parse = |key| match data.get(key) {
            Some(JsonValue::String(id)) if validate_snowflake(id) => {
                Some(Some(id.trim_start_matches('0').to_string()))
            }
            None => Some(None),
            _ => None,
        };
        Some(Self::Known {
            webhook_id: parse("webhook_id")?,
            application_id: parse("application_id")?,
        })
    }
}

impl HeapSize for MessageOrigin {
    fn heap_size(&self) -> usize {
        match self {
            Self::Known {
                webhook_id,
                application_id,
            } => webhook_id
                .heap_size()
                .saturating_add(application_id.heap_size()),
            _ => 0,
        }
    }
}

#[derive(Clone)]
pub(super) struct StoredMessage {
    snapshot: MessageSnapshot,
    origin: MessageOrigin,
    pub(super) revision: Option<super::revision::MessageRevision>,
}

impl From<MessageSnapshot> for StoredMessage {
    fn from(snapshot: MessageSnapshot) -> Self {
        Self {
            snapshot,
            origin: MessageOrigin::Unknown,
            revision: None,
        }
    }
}

impl Deref for StoredMessage {
    type Target = MessageSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl DerefMut for StoredMessage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.snapshot
    }
}

impl HeapSize for StoredMessage {
    fn heap_size(&self) -> usize {
        self.snapshot
            .heap_size()
            .saturating_add(self.origin.heap_size())
            .saturating_add(self.revision.heap_size())
    }
}

#[derive(Clone)]
pub(crate) struct WebhookMessageTarget {
    pub(crate) channel_id: String,
    pub(crate) message_id: String,
}

impl HeapSize for WebhookMessageTarget {
    fn heap_size(&self) -> usize {
        self.channel_id
            .heap_size()
            .saturating_add(self.message_id.heap_size())
    }
}

impl SnapshotStore {
    pub(crate) fn message_origin_matches(
        &self,
        channel_id: &str,
        message_id: &str,
        origin: &MessageOrigin,
        full: bool,
    ) -> bool {
        self.messages
            .get(&message_key(channel_id, message_id))
            .is_none_or(|message| match (&message.origin, origin) {
                (MessageOrigin::Unknown, _) => true,
                (
                    MessageOrigin::Known {
                        webhook_id: old_hook,
                        application_id: old_app,
                    },
                    MessageOrigin::Known {
                        webhook_id,
                        application_id,
                    },
                ) => {
                    (!full && webhook_id.is_none() || old_hook == webhook_id)
                        && (old_app.is_none()
                            || !full && application_id.is_none()
                            || old_app == application_id)
                }
                _ => false,
            })
    }

    pub(crate) fn set_message_origin(
        &mut self,
        channel_id: &str,
        message_id: &str,
        origin: MessageOrigin,
        full: bool,
    ) -> bool {
        let key = message_key(channel_id, message_id);
        if !self.message_origin_matches(channel_id, message_id, &origin, full) {
            self.messages.remove(&key);
            return false;
        }
        if full {
            self.messages
                .update(&key, |message| message.origin = origin);
        } else if let MessageOrigin::Known {
            application_id: Some(application),
            ..
        } = origin
        {
            self.messages.update(&key, |message| {
                if let MessageOrigin::Known { application_id, .. } = &mut message.origin {
                    if application_id.is_none() {
                        *application_id = Some(application);
                    }
                }
            });
        }
        self.enforce_limits();
        true
    }

    pub(crate) fn apply_webhook_message_response(
        &mut self,
        data: &JsonValue,
        target: &WebhookMessageTarget,
        now_ms: u64,
    ) -> bool {
        self.apply_rest_message_response_with_origin(
            data,
            &target.channel_id,
            &target.message_id,
            MessageOrigin::from_data(data, true).unwrap_or(MessageOrigin::Unknown),
            now_ms,
        )
    }

    pub(crate) fn webhook_message_targets(&self, message_id: &str) -> Vec<WebhookMessageTarget> {
        self.messages
            .values()
            .filter(|message| message.message_id.trim_start_matches('0') == message_id)
            .map(|message| WebhookMessageTarget {
                channel_id: message.channel_id.clone(),
                message_id: message.message_id.clone(),
            })
            .collect()
    }

    pub(crate) fn invalidate_unresolved_webhook_messages(&mut self, webhook_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.messages.retain(|_, message| match &message.origin {
            MessageOrigin::Known {
                webhook_id: id,
                application_id,
            } => {
                id.as_deref() != Some(webhook_id)
                    && application_id.as_deref() != Some(webhook_id)
                    && !(id.is_none() && application_id.is_none() && message.author_is_bot)
            }
            MessageOrigin::Unknown => false,
        });
    }

    pub(crate) fn webhook_original_target(
        &mut self,
        webhook_id: &str,
        token: &str,
        now_ms: u64,
    ) -> Option<WebhookMessageTarget> {
        self.prune(now_ms);
        self.webhook_originals
            .get(&original_key(webhook_id, token))
            .cloned()
    }

    pub(crate) fn remember_webhook_original(
        &mut self,
        webhook_id: &str,
        token: &str,
        target: WebhookMessageTarget,
        now_ms: u64,
    ) {
        self.prune(now_ms);
        self.webhook_originals.insert_expiring(
            original_key(webhook_id, token),
            target,
            Some(now_ms.saturating_add(self.message_ttl_ms.min(MAX_ORIGINAL_TARGET_TTL_MS))),
        );
        self.enforce_limits();
    }
}

fn original_key(webhook_id: &str, token: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let input = format!("{webhook_id}\0{token}");
    crate::audit::sha256(input.as_bytes())
        .iter()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 15) as usize] as char,
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_targets_are_private_bounded_and_expire_with_snapshot_continuity() {
        let mut store = SnapshotStore::new(100, 100, 2, 2);
        for token in ["private-first", "private-second", "private-third"] {
            store.remember_webhook_original(
                "50",
                token,
                WebhookMessageTarget {
                    channel_id: "20".into(),
                    message_id: "30".into(),
                },
                0,
            );
        }
        assert!(store
            .webhook_original_target("50", "private-first", 0)
            .is_none());
        assert!(store
            .webhook_original_target("50", "private-second", 100)
            .is_some());
        assert!(store
            .webhook_original_target("51", "private-second", 100)
            .is_none());
        assert!(!format!("{store:?}").contains("private-second"));
        let mut cloned = store.clone();
        assert!(store
            .webhook_original_target("50", "private-second", 101)
            .is_none());
        assert_eq!(store.retained_bytes(), 0);
        cloned.mark_gap();
        assert!(cloned
            .webhook_original_target("50", "private-second", 0)
            .is_none());
        cloned.remember_webhook_original(
            "50",
            "private-second",
            WebhookMessageTarget {
                channel_id: "20".into(),
                message_id: "30".into(),
            },
            0,
        );
        cloned.set_max_bytes(1);
        assert!(cloned.retained_bytes() <= 1);
        assert!(cloned
            .webhook_original_target("50", "private-second", 0)
            .is_none());
    }

    #[test]
    fn original_scope_fingerprints_preserve_component_boundaries() {
        assert_ne!(original_key("5", "0private"), original_key("50", "private"));
        assert_ne!(original_key("50", "private"), original_key("50", "Private"));
        assert_eq!(original_key("50", "private"), original_key("50", "private"));
    }
}
