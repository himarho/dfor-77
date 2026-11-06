use super::*;

pub(super) struct GuildContext {
    pub(super) sequence: i64,
    guild_id: String,
    channel_scope: bool,
    thread_scope: bool,
    channels: HashSet<String>,
    invalid_channels: HashSet<String>,
    members: HashSet<String>,
    invalid_members: HashSet<String>,
    unknown_members: bool,
}

impl GuildContext {
    pub(super) fn capture(
        frame: &GatewayFrame,
        data: Option<&JsonValue>,
        now_ms: u64,
    ) -> Option<Self> {
        let GatewayFrame::Dispatch(GatewayDispatch::GuildCreate {
            sequence,
            guild,
            channels,
            members,
            ..
        }) = frame
        else {
            return None;
        };
        let data = data?;
        if !["channels", "threads", "members"]
            .iter()
            .any(|key| data.get(key).is_some())
        {
            return None;
        }
        let mut context = Self {
            sequence: *sequence,
            guild_id: guild.guild_id.clone(),
            channel_scope: data.get("channels").is_some(),
            thread_scope: data.get("threads").is_some(),
            channels: channels
                .iter()
                .filter(|channel| channel.guild_id == guild.guild_id)
                .map(|channel| channel.channel_id.clone())
                .collect(),
            invalid_channels: HashSet::new(),
            members: members
                .iter()
                .filter(|member| member.guild_id == guild.guild_id)
                .map(|member| member.user_id.clone())
                .collect(),
            invalid_members: HashSet::new(),
            unknown_members: false,
        };
        let mut channel_ids = HashSet::new();
        for key in ["channels", "threads"] {
            if let Some(entries) = data.get(key).and_then(JsonValue::as_array) {
                for entry in entries {
                    let Some(id) = entry
                        .get("id")
                        .and_then(JsonValue::as_str)
                        .filter(|id| validate_snowflake(id))
                    else {
                        continue;
                    };
                    let valid = parse_channel_snapshot_lossy(entry, Some(&guild.guild_id), now_ms)
                        .is_ok_and(|channel| {
                            channel.guild_id == guild.guild_id
                                && (key != "threads" || matches!(channel.channel_type, 10..=12))
                        });
                    if !valid || !channel_ids.insert(id) {
                        context.invalid_channels.insert(id.into());
                    }
                }
            }
        }
        match data.get("members") {
            None => {}
            Some(JsonValue::Array(entries)) => {
                let mut ids = HashSet::new();
                for entry in entries {
                    let Some(id) = entry
                        .get("user")
                        .and_then(|user| user.get("id"))
                        .and_then(JsonValue::as_str)
                        .filter(|id| validate_snowflake(id))
                    else {
                        context.unknown_members = true;
                        continue;
                    };
                    if parse_member_snapshot(entry, &guild.guild_id, now_ms).is_err()
                        || entry
                            .get("guild_id")
                            .is_some_and(|id| id.as_str() != Some(guild.guild_id.as_str()))
                        || !ids.insert(id)
                    {
                        context.invalid_members.insert(id.into());
                    }
                }
            }
            Some(_) => context.unknown_members = true,
        }
        Some(context)
    }

    pub(super) fn apply(self, snapshot: &mut SnapshotStore, now_ms: u64) {
        snapshot.revalidate_guild_channels(
            &self.guild_id,
            &self.channels,
            &self.invalid_channels,
            self.channel_scope,
            self.thread_scope,
            now_ms,
        );
        snapshot.revalidate_guild_members(
            &self.guild_id,
            &self.members,
            &self.invalid_members,
            self.unknown_members,
            now_ms,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::SnapshotLookup;

    fn state() -> GatewayIngestor {
        let mut state = GatewayIngestor::new(16, 1000, 1000, 32, 32);
        for (guild, channel, kind, archived) in [
            ("10", "20", 0, false),
            ("10", "21", 12, false),
            ("10", "22", 12, true),
            ("11", "23", 0, false),
        ] {
            let mut channel = ChannelSnapshot::new(guild, channel, kind, 0);
            channel.archived = archived;
            channel.parent_channel_id = Some("20".into());
            channel.current_user_thread_joined = Some(true);
            channel.current_user_thread_joined_at_ms = Some(0);
            state.snapshot_mut().upsert_channel(channel);
        }
        for (guild, user) in [("10", "40"), ("10", "99"), ("11", "40")] {
            state
                .snapshot_mut()
                .upsert_member(MemberSnapshot::new(guild, user, vec![], 0));
        }
        state
    }

    #[test]
    fn collection_scopes_preserve_archived_threads_other_guilds_and_partial_member_lists() {
        for (field, channel_complete, thread_complete) in [
            (r#""channels":false"#, false, true),
            (r#""threads":false"#, true, false),
            (r#""channels":[],"threads":[]"#, false, false),
            (r#""members":[]"#, true, true),
        ] {
            let mut state = state();
            state
                .ingest_json(
                    &format!(r#"{{"op":0,"s":1,"t":"GUILD_CREATE","d":{{"id":"10",{field}}}}}"#),
                    1,
                )
                .unwrap();
            for (id, complete) in [
                ("20", channel_complete),
                ("21", thread_complete),
                ("22", true),
                ("23", true),
            ] {
                let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel(id, 1) else {
                    panic!("missing channel {id}")
                };
                assert_eq!(channel.complete, complete, "{field} {id}");
            }
            let SnapshotLookup::Found(member) = state.snapshot_mut().get_member("10", "40", 1)
            else {
                panic!("missing member")
            };
            assert!(
                member.complete,
                "a partial member list does not prove absence"
            );
        }
    }

    #[test]
    fn duplicate_children_and_channel_identity_conflicts_never_leave_positive_proofs() {
        for body in [
            r#"{"id":"10","channels":[{"id":"20","type":0},{"id":"20","type":0}]}"#,
            r#"{"id":"10","channels":[{"id":"20","type":0}],"threads":[{"id":"20","type":12,"parent_id":"20"}]}"#,
            r#"{"id":"10","channels":[{"id":"23","type":0}]}"#,
            r#"{"id":"10","threads":[{"id":"21","type":0}]}"#,
        ] {
            let mut state = state();
            state
                .ingest_json(
                    &format!(r#"{{"op":0,"s":1,"t":"GUILD_CREATE","d":{body}}}"#),
                    1,
                )
                .unwrap();
            let id = if body.contains(r#""23""#) {
                "23"
            } else if body.contains(r#""21""#) {
                "21"
            } else {
                "20"
            };
            let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel(id, 1) else {
                panic!("missing conflicting channel")
            };
            assert!(!channel.complete, "{body}");
            if id == "23" {
                assert_eq!(channel.guild_id, "11");
            }
        }
    }

    #[test]
    fn replays_cannot_invalidate_fresh_member_and_channel_reads() {
        let mut state = state();
        let frame =
            r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","channels":false,"members":false}}"#;
        state.ingest_json(frame, 1).unwrap();
        state
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("10", "20", 0, 2));
        state
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new("10", "40", vec![], 2));
        state.ingest_json(frame, 3).unwrap();
        let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel("20", 3) else {
            panic!("missing fresh channel")
        };
        assert!(channel.complete);
        let SnapshotLookup::Found(member) = state.snapshot_mut().get_member("10", "40", 3) else {
            panic!("missing fresh member")
        };
        assert!(member.complete);
    }
}
