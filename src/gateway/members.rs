use std::collections::HashSet;
use std::fmt;

use super::{
    json_escape, parse_member_snapshot, required_str, required_u32, GatewayEvent, GatewayEventData,
    GatewayNormalizeError, JsonValue, MemberSnapshot, SnapshotStore, D7GEV,
};
use crate::validation::validate_snowflake;

#[derive(Clone, Eq, PartialEq)]
pub struct GuildMembersChunk {
    pub guild_id: String,
    pub members: Vec<MemberSnapshot>,
    pub chunk_index: u32,
    pub chunk_count: u32,
    pub nonce: Option<String>,
    pub not_found: Vec<String>,
}

impl GuildMembersChunk {
    pub(super) fn parse(data: &JsonValue, now_ms: u64) -> Result<Self, GatewayNormalizeError> {
        let guild_id = required_str(data, "guild_id")?;
        let chunk_index = required_u32(data, "chunk_index")?;
        let chunk_count = required_u32(data, "chunk_count")?;
        if chunk_count == 0 || chunk_index >= chunk_count {
            return Err(GatewayNormalizeError::new("member chunk range invalid"));
        }
        let values = data
            .get("members")
            .and_then(JsonValue::as_array)
            .filter(|values| values.len() <= 1000)
            .ok_or_else(|| GatewayNormalizeError::new("member chunk members invalid"))?;
        let mut members = Vec::with_capacity(values.len());
        let mut user_ids = HashSet::with_capacity(values.len());
        for value in values {
            if value
                .get("guild_id")
                .is_some_and(|value| value.as_str() != Some(guild_id))
            {
                return Err(GatewayNormalizeError::new("member chunk guild mismatch"));
            }
            let member = parse_member_snapshot(value, guild_id, now_ms)?;
            if !user_ids.insert(member.user_id.clone()) {
                return Err(GatewayNormalizeError::new("member chunk duplicate user"));
            }
            members.push(member);
        }
        let nonce = data
            .get("nonce")
            .map(|value| {
                value
                    .as_str()
                    .filter(|value| value.len() <= 32)
                    .map(str::to_string)
                    .ok_or_else(|| GatewayNormalizeError::new("member chunk nonce invalid"))
            })
            .transpose()?;
        let not_found = match data.get("not_found") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .filter(|values| values.len() <= 1000)
                .ok_or_else(|| GatewayNormalizeError::new("member chunk not_found invalid"))?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| GatewayNormalizeError::new("member chunk not_found invalid"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        if not_found.iter().any(|user| user_ids.contains(user)) {
            return Err(GatewayNormalizeError::new("member chunk conflicting user"));
        }
        Ok(Self {
            guild_id: guild_id.into(),
            members,
            chunk_index,
            chunk_count,
            nonce,
            not_found,
        })
    }

    pub(super) fn event(&self, sequence: i64) -> GatewayEvent {
        let mut user_ids = self
            .members
            .iter()
            .map(|member| member.user_id.clone())
            .collect::<Vec<_>>();
        let nonce = self
            .nonce
            .as_ref()
            .map(|nonce| format!(",\"nonce\":\"{}\"", json_escape(nonce)))
            .unwrap_or_default();
        let metadata = format!(
            "{{\"chunk_index\":{},\"chunk_count\":{},\"member_ids\":{},\"not_found\":{}{}}}",
            self.chunk_index,
            self.chunk_count,
            string_array(&user_ids),
            string_array(&self.not_found),
            nonce,
        );
        user_ids.extend(
            self.not_found
                .iter()
                .filter(|id| validate_snowflake(id))
                .cloned(),
        );
        GatewayEvent {
            event_type: D7GEV,
            sequence: Some(sequence),
            description: "GUILD_MEMBERS_CHUNK".into(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::MembersChunk {
                guild_id: self.guild_id.clone(),
                user_ids,
                metadata,
            },
        }
    }

    pub(super) fn apply(
        self,
        snapshot: &mut SnapshotStore,
        bot_user_id: Option<&str>,
        now_ms: u64,
    ) {
        if bot_user_id.is_some_and(|bot_id| {
            self.members.iter().any(|member| member.user_id == bot_id)
                || self.not_found.iter().any(|user_id| user_id == bot_id)
        }) {
            snapshot.clear_private_thread_access_for_guild(&self.guild_id, now_ms);
        }
        for user_id in self.not_found.iter().filter(|id| validate_snowflake(id)) {
            snapshot.gateway_member_unconfirmed(&self.guild_id, user_id, now_ms);
        }
        for member in self.members {
            snapshot.upsert_member(member);
        }
    }
}

fn string_array(values: &[String]) -> String {
    let mut output = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push('"');
        output.push_str(&json_escape(value));
        output.push('"');
    }
    output.push(']');
    output
}

impl fmt::Debug for GuildMembersChunk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuildMembersChunk")
            .field("member_count", &self.members.len())
            .field("chunk_index", &self.chunk_index)
            .field("chunk_count", &self.chunk_count)
            .field("has_nonce", &self.nonce.is_some())
            .field("not_found_count", &self.not_found.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{
        apply_dispatch_to_snapshot, parse_gateway_frame, GatewayFrame, GatewayIngestor,
    };
    use crate::json::parse_json;
    use crate::memory::HeapSize;
    use crate::snapshot::{GuildSnapshot, SnapshotLookup};

    fn frame(data: &str) -> String {
        format!(r#"{{"op":0,"s":2,"t":"GUILD_MEMBERS_CHUNK","d":{data}}}"#)
    }

    #[test]
    fn metadata_round_trips_nonce_and_invalid_identifiers_without_leaking_into_debug() {
        let data = parse_json(r#"{"guild_id":"10","chunk_index":0,"chunk_count":2,"nonce":"private-nonce\"\\\n","not_found":["bad\"id","41"],"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}]}"#).unwrap();
        let chunk = GuildMembersChunk::parse(&data, 1).unwrap();
        let event = chunk.event(2);
        let GatewayEventData::MembersChunk {
            guild_id,
            user_ids,
            metadata,
        } = &event.data
        else {
            panic!("expected a member chunk event");
        };
        assert_eq!(guild_id, "10");
        assert_eq!(user_ids, &["40", "41"]);
        let metadata = parse_json(metadata).unwrap();
        assert_eq!(metadata.get("nonce"), data.get("nonce"));
        assert_eq!(metadata.get("not_found"), data.get("not_found"));
        assert_eq!(
            metadata.get("chunk_index").and_then(JsonValue::as_u64),
            Some(0)
        );
        assert_eq!(
            metadata.get("chunk_count").and_then(JsonValue::as_u64),
            Some(2)
        );
        assert_eq!(
            metadata.get("member_ids").unwrap().as_array().unwrap(),
            &[JsonValue::String("40".into())]
        );
        let debug = format!("{chunk:?} {event:?}");
        assert!(!debug.contains("private-nonce"));
        assert!(!debug.contains("bad"));
        assert!(event.heap_size() >= user_ids.heap_size() + guild_id.heap_size());
        let dispatch = parse_gateway_frame(&frame(r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}]}"#), 1).unwrap();
        let GatewayFrame::Dispatch(dispatch) = dispatch else {
            panic!("expected dispatch");
        };
        let mut store = SnapshotStore::new(1000, 1000, 16, 16);
        apply_dispatch_to_snapshot(&dispatch, &mut store, 1);
        assert!(
            matches!(store.get_member("10", "40", 1), SnapshotLookup::Found(member) if member.role_ids == ["50"])
        );
    }

    #[test]
    fn malformed_chunks_clear_old_authority_without_applying_partial_members() {
        for data in [
            r#"{"guild_id":"10","chunk_index":0,"chunk_count":0,"members":[]}"#,
            r#"{"guild_id":"10","chunk_index":1,"chunk_count":1,"members":[]}"#,
            r#"{"guild_id":"10","chunk_index":-1,"chunk_count":1,"members":[]}"#,
            r#"{"guild_id":"10","chunk_index":0.0,"chunk_count":1,"members":[]}"#,
            r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":{}}"#,
            r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"40"},"roles":[]},{"roles":[]}]}"#,
            r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"40"},"roles":[]},{"user":{"id":"40"},"roles":["50"]}]}"#,
            r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[{"guild_id":"11","user":{"id":"40"},"roles":[]}]}"#,
            r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"nonce":null,"members":[]}"#,
            r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"not_found":{},"members":[]}"#,
            r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"not_found":["40"],"members":[{"user":{"id":"40"},"roles":[]}]}"#,
        ] {
            assert!(parse_gateway_frame(&frame(data), 2).is_err(), "{data}");
            let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 16, 16);
            ingestor.snapshot_mut().upsert_member(MemberSnapshot::new(
                "10",
                "40",
                vec!["50".into()],
                1,
            ));
            ingestor.ingest_json_tolerant(&frame(data), 2).unwrap();
            assert!(matches!(
                ingestor.snapshot_mut().get_member("10", "40", 2),
                SnapshotLookup::Missing
            ));
            let events = std::iter::from_fn(|| ingestor.pop_event()).collect::<Vec<_>>();
            assert!(events
                .iter()
                .any(|event| event.description == "INVALID_PERMISSION_DISPATCH"));
            assert!(events
                .iter()
                .any(|event| event.description == "INVALID_GUILD_MEMBERS_CHUNK"));
        }
    }

    #[test]
    fn full_chunks_and_nonce_byte_limits_are_bounded_without_truncating_members() {
        let members = (1..=1000)
            .map(|id| {
                format!(
                    r#"{{"user":{{"id":"{id}"}},"roles":[],"communication_disabled_until":null}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let data =
            format!(r#"{{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[{members}]}}"#);
        let chunk = GuildMembersChunk::parse(&parse_json(&data).unwrap(), 1).unwrap();
        assert_eq!(chunk.members.len(), 1000);
        let too_many = data.replace("}]}", r#"},{"user":{"id":"1001"},"roles":[]}]}"#);
        assert!(GuildMembersChunk::parse(&parse_json(&too_many).unwrap(), 1).is_err());
        for (nonce, valid) in [
            ("a".repeat(32), true),
            ("a".repeat(33), false),
            ("あ".repeat(10), true),
            ("あ".repeat(11), false),
        ] {
            let data = format!(
                r#"{{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[],"nonce":"{nonce}"}}"#
            );
            assert_eq!(
                GuildMembersChunk::parse(&parse_json(&data).unwrap(), 1).is_ok(),
                valid
            );
        }
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 1000, 16);
        assert!(!ingestor.machine_mut().set_event_queue_max_bytes(256));
        ingestor.ingest_json(&frame(&data), 1).unwrap();
        assert!(ingestor.machine().gap_epoch() > 0);
        assert!(matches!(
            ingestor.snapshot_mut().get_member("10", "1", 1),
            SnapshotLookup::Missing
        ));
    }

    #[test]
    fn bot_membership_results_invalidate_derived_thread_access_only_in_the_affected_guild() {
        use crate::snapshot::ChannelSnapshot;
        for not_found in [false, true] {
            let mut store = SnapshotStore::new(1000, 1000, 16, 16);
            for (guild, channel) in [("10", "20"), ("11", "21")] {
                store.upsert_channel(ChannelSnapshot::new(guild, channel, 12, 0));
                assert!(store.set_current_user_thread_joined(channel, true, 0));
            }
            let data = parse_json(if not_found {
                r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[],"not_found":["99"]}"#
            } else {
                r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"99"},"roles":[],"communication_disabled_until":null}]}"#
            }).unwrap();
            GuildMembersChunk::parse(&data, 1)
                .unwrap()
                .apply(&mut store, Some("99"), 1);
            assert!(
                matches!(store.get_channel("20", 1), SnapshotLookup::Found(channel) if channel.current_user_thread_joined.is_none())
            );
            assert!(
                matches!(store.get_channel("21", 1), SnapshotLookup::Found(channel) if channel.current_user_thread_joined == Some(true))
            );
        }
    }

    #[test]
    fn not_found_requires_a_new_lookup_without_inventing_deletions_or_removing_other_members() {
        let mut store = SnapshotStore::new(1000, 1000, 16, 16);
        let mut guild = GuildSnapshot::new("10", false, 0);
        guild.owner_id = Some("40".into());
        store.upsert_guild(guild);
        store.upsert_member(MemberSnapshot::new("10", "40", vec!["50".into()], 0));
        store.upsert_member(MemberSnapshot::new("10", "41", vec![], 0));
        store.tombstone_member("10", "42", 0);
        let data = parse_json(r#"{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[],"not_found":["40","42","bad-id"]}"#).unwrap();
        GuildMembersChunk::parse(&data, 1)
            .unwrap()
            .apply(&mut store, None, 1);
        assert!(matches!(
            store.get_member("10", "40", 1),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            store.get_member("10", "41", 1),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            store.get_member("10", "42", 1),
            SnapshotLookup::Tombstone
        ));
        assert!(
            matches!(store.get_guild("10", 1), SnapshotLookup::Found(guild) if !guild.complete)
        );
    }
}
