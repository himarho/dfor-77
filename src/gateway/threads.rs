use super::*;

struct SyncScope {
    guild_id: String,
    parents: Option<HashSet<String>>,
    returned: HashSet<String>,
}

pub(super) struct ChannelContext {
    pub(super) sequence: i64,
    invalid: HashSet<String>,
    unverified: HashSet<String>,
    sync: Option<SyncScope>,
}

impl ChannelContext {
    pub(super) fn capture(
        frame: &GatewayFrame,
        data: Option<&JsonValue>,
        bot_user_id: Option<&str>,
    ) -> Result<Option<Self>, GatewayNormalizeError> {
        let (sequence, channels) = match frame {
            GatewayFrame::Dispatch(
                GatewayDispatch::ChannelUpsert { sequence, .. }
                | GatewayDispatch::ChannelUpdate { sequence, .. },
            ) => (*sequence, None),
            GatewayFrame::Dispatch(
                GatewayDispatch::GuildCreate {
                    sequence, channels, ..
                }
                | GatewayDispatch::ChannelSync {
                    sequence, channels, ..
                },
            ) => (*sequence, Some(channels)),
            _ => return Ok(None),
        };
        let Some(data) = data else { return Ok(None) };
        let mut context = Self {
            sequence,
            invalid: HashSet::new(),
            unverified: HashSet::new(),
            sync: None,
        };
        if let Some(channels) = channels {
            let ids = channels
                .iter()
                .map(|channel| channel.channel_id.as_str())
                .collect::<HashSet<_>>();
            for key in ["channels", "threads"] {
                if let Some(entries) = data.get(key).and_then(JsonValue::as_array) {
                    for entry in entries {
                        if let Some(id) = entry
                            .get("id")
                            .and_then(JsonValue::as_str)
                            .filter(|id| ids.contains(id))
                        {
                            context.inspect(entry, id, bot_user_id);
                        }
                    }
                }
            }
            if let GatewayFrame::Dispatch(GatewayDispatch::ChannelSync {
                guild_id: Some(guild_id),
                ..
            }) = frame
            {
                if data.get("threads").is_some() {
                    context.sync = Some(SyncScope {
                        guild_id: guild_id.clone(),
                        parents: sync_parents(data)?,
                        returned: channels
                            .iter()
                            .map(|channel| channel.channel_id.clone())
                            .collect(),
                    });
                }
            }
        } else {
            let id = match frame {
                GatewayFrame::Dispatch(GatewayDispatch::ChannelUpsert { channel, .. }) => {
                    &channel.channel_id
                }
                GatewayFrame::Dispatch(GatewayDispatch::ChannelUpdate { patch, .. }) => {
                    &patch.channel_id
                }
                _ => return Ok(None),
            };
            context.inspect(data, id, bot_user_id);
        }
        Ok((!context.invalid.is_empty()
            || !context.unverified.is_empty()
            || context.sync.is_some())
        .then_some(context))
    }

    fn inspect(&mut self, data: &JsonValue, channel_id: &str, bot_user_id: Option<&str>) {
        let mut observed = None;
        for key in ["member", "thread_member"] {
            let Some(member) = data.get(key) else {
                continue;
            };
            let actor = member
                .get("user_id")
                .and_then(JsonValue::as_str)
                .or_else(|| member.get("member")?.get("user")?.get("id")?.as_str());
            let expected = bot_user_id.or(actor).unwrap_or_default();
            if !crate::snapshot::archive::thread_member_matches(member, channel_id, expected, false)
                || actor.is_some_and(|actor| {
                    !validate_snowflake(actor) || observed.is_some_and(|other| other != actor)
                })
            {
                self.invalid.insert(channel_id.to_string());
                return;
            }
            observed = observed.or(actor);
        }
        if bot_user_id.is_none() && observed.is_some() {
            self.unverified.insert(channel_id.to_string());
        }
    }

    pub(super) fn apply(self, snapshot: &mut SnapshotStore, now_ms: u64) {
        if let Some(sync) = self.sync {
            snapshot.reconcile_active_threads(
                &sync.guild_id,
                sync.parents.as_ref(),
                &sync.returned,
                now_ms,
            );
        }
        snapshot.invalidate_channel_evidence(&self.invalid, &self.unverified, now_ms);
    }
}

fn sync_parents(data: &JsonValue) -> Result<Option<HashSet<String>>, GatewayNormalizeError> {
    data.get("channel_ids")
        .map(|_| required_snowflake_array(data, "channel_ids").map(|ids| ids.into_iter().collect()))
        .transpose()
}

pub(super) fn validate_sync_scope(
    data: &JsonValue,
    channels: &[ChannelSnapshot],
) -> Result<(), GatewayNormalizeError> {
    if sync_parents(data)?.as_ref().is_some_and(|parents| {
        channels.iter().any(|channel| {
            channel
                .parent_channel_id
                .as_ref()
                .is_none_or(|parent| !parents.contains(parent))
        })
    }) {
        return Err(GatewayNormalizeError::new("thread sync parent mismatch"));
    }
    Ok(())
}

pub(super) fn optional_id<'a>(
    data: &'a JsonValue,
    key: &str,
) -> Result<Option<&'a str>, GatewayNormalizeError> {
    match data.get(key) {
        None => Ok(None),
        Some(JsonValue::String(id)) if validate_snowflake(id) => Ok(Some(id)),
        Some(_) => Err(GatewayNormalizeError::new(format!(
            "thread member {key} invalid"
        ))),
    }
}

pub(super) fn parse_thread_member_array(
    data: &JsonValue,
    key: &str,
    fallback_thread_id: Option<&str>,
) -> Result<Vec<ThreadMembershipChange>, GatewayNormalizeError> {
    let Some(values) = data.get(key) else {
        return Ok(Vec::new());
    };
    values
        .as_array()
        .ok_or_else(|| GatewayNormalizeError::new(format!("{key} must be array")))?
        .iter()
        .map(|value| parse_thread_member(value, fallback_thread_id, true))
        .collect()
}

pub(super) fn parse_thread_member(
    data: &JsonValue,
    fallback_thread_id: Option<&str>,
    joined: bool,
) -> Result<ThreadMembershipChange, GatewayNormalizeError> {
    if !matches!(data, JsonValue::Object(_)) {
        return Err(GatewayNormalizeError::new("thread member must be object"));
    }
    let thread_id = optional_id(data, "id")?
        .or(fallback_thread_id)
        .ok_or_else(|| GatewayNormalizeError::new("thread member id missing"))?;
    if fallback_thread_id.is_some_and(|id| id != thread_id) {
        return Err(GatewayNormalizeError::new("thread member id mismatch"));
    }
    let user_id = optional_id(data, "user_id")?;
    let nested_user_id = match data.get("member") {
        None => None,
        Some(member @ JsonValue::Object(_)) => match member.get("user") {
            None => None,
            Some(user @ JsonValue::Object(_)) => Some(required_str(user, "id")?),
            Some(_) => return Err(GatewayNormalizeError::new("thread member user invalid")),
        },
        Some(_) => return Err(GatewayNormalizeError::new("thread guild member invalid")),
    };
    if user_id
        .zip(nested_user_id)
        .is_some_and(|(user, nested)| user != nested)
    {
        return Err(GatewayNormalizeError::new("thread member user mismatch"));
    }
    let user_id = user_id.or(nested_user_id);
    if fallback_thread_id.is_some() && user_id.is_none() {
        return Err(GatewayNormalizeError::new(
            "added thread member user missing",
        ));
    }
    Ok(ThreadMembershipChange {
        thread_id: thread_id.to_string(),
        user_id: user_id.map(ToOwned::to_owned),
        joined,
    })
}

pub(super) fn validate_sync(
    guild_id: Option<&str>,
    channels: &[ChannelSnapshot],
    memberships: &[ThreadMembershipChange],
) -> Result<(), GatewayNormalizeError> {
    let mut ids = HashSet::new();
    if channels.iter().any(|channel| {
        !matches!(channel.channel_type, 10..=12)
            || guild_id.is_some_and(|guild| guild != channel.guild_id)
            || !ids.insert(channel.channel_id.as_str())
    }) || memberships
        .iter()
        .any(|member| !ids.contains(member.thread_id.as_str()))
    {
        return Err(GatewayNormalizeError::new("thread sync identity mismatch"));
    }
    Ok(())
}

pub(super) fn apply_thread_membership_change(
    snapshot: &mut SnapshotStore,
    bot_user_id: Option<&str>,
    guild_id: Option<&str>,
    change: &ThreadMembershipChange,
    now_ms: u64,
) {
    let applies = match (&change.user_id, bot_user_id) {
        (None, _) => true,
        (Some(user_id), Some(bot_user_id)) => user_id == bot_user_id,
        (Some(_), None) => false,
    };
    if !applies {
        return;
    }
    if let Some(guild_id) = guild_id {
        if let crate::snapshot::SnapshotLookup::Found(channel)
        | crate::snapshot::SnapshotLookup::Stale(channel) =
            snapshot.get_channel(&change.thread_id, now_ms)
        {
            if channel.guild_id != guild_id {
                snapshot.invalidate_channel_state(&change.thread_id, now_ms);
                return;
            }
        }
    }
    snapshot.set_current_user_thread_joined(&change.thread_id, change.joined, now_ms);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::SnapshotLookup;

    fn ingestor() -> GatewayIngestor {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 10, 10);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"fixture","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
        for id in ["30", "31"] {
            let mut channel = ChannelSnapshot::new("10", id, 12, 0);
            channel.current_user_thread_joined = Some(false);
            channel.current_user_thread_joined_at_ms = Some(0);
            ingestor.snapshot_mut().upsert_channel(channel);
        }
        ingestor
    }

    #[test]
    fn duplicate_channel_records_cannot_hide_foreign_membership_proofs() {
        let mut state = ingestor();
        state.ingest_json(r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","threads":[{"id":"30","type":12,"parent_id":"20","thread_metadata":{"archived":false,"locked":false},"member":{"user_id":"40"}},{"id":"30","type":"bad","member":{"user_id":"99"}}]}}"#, 1).unwrap();
        let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel("30", 1) else {
            panic!("expected thread metadata")
        };
        assert!(!channel.complete);
        assert_eq!(channel.current_user_thread_joined, None);
    }

    #[test]
    fn embedded_foreign_members_cannot_grant_access_through_channel_payloads() {
        for name in [
            "THREAD_CREATE",
            "THREAD_UPDATE",
            "GUILD_CREATE",
            "THREAD_LIST_SYNC",
        ] {
            for (member, accepted) in [
                (r#"{"id":"30","user_id":"40"}"#, false),
                (r#"{"id":"31","user_id":"99"}"#, false),
                (
                    r#"{"id":"30","user_id":"99","member":{"user":{"id":"40"}}}"#,
                    false,
                ),
                (r#"{"id":"30","user_id":"99"}"#, true),
                ("{}", true),
            ] {
                let mut state = ingestor();
                let channel = format!(
                    r#"{{"id":"30","guild_id":"10","type":12,"parent_id":"20","thread_metadata":{{"archived":false,"locked":false}},"member":{member}}}"#
                );
                let data = match name {
                    "GUILD_CREATE" => format!(r#"{{"id":"10","threads":[{channel}]}}"#),
                    "THREAD_LIST_SYNC" => format!(r#"{{"guild_id":"10","threads":[{channel}]}}"#),
                    _ => channel,
                };
                state
                    .ingest_json(&format!(r#"{{"op":0,"s":2,"t":"{name}","d":{data}}}"#), 1)
                    .unwrap();
                let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel("30", 1)
                else {
                    panic!("expected preserved thread metadata")
                };
                assert_eq!(
                    channel.current_user_thread_joined,
                    accepted.then_some(true),
                    "{name} {member}"
                );
                assert_eq!(channel.complete, accepted, "{name} {member}");
            }
        }
    }

    #[test]
    fn missing_scope_and_empty_parent_scope_preserve_state_while_replays_cannot_expire_fresh_reads()
    {
        let mut state = ingestor();
        let mut channel = ChannelSnapshot::new("10", "30", 12, 0);
        channel.parent_channel_id = Some("20".into());
        channel.current_user_thread_joined = Some(true);
        channel.current_user_thread_joined_at_ms = Some(0);
        state.snapshot_mut().upsert_channel(channel.clone());
        for (seq, data, complete) in [
            (2, r#"{"guild_id":"10"}"#, true),
            (
                3,
                r#"{"guild_id":"10","channel_ids":[],"threads":[]}"#,
                true,
            ),
            (4, r#"{"guild_id":"10","threads":[]}"#, false),
        ] {
            state
                .ingest_json(
                    &format!(r#"{{"op":0,"s":{seq},"t":"THREAD_LIST_SYNC","d":{data}}}"#),
                    seq as u64,
                )
                .unwrap();
            let SnapshotLookup::Found(current) = state.snapshot_mut().get_channel("30", 4) else {
                panic!("missing thread metadata")
            };
            assert_eq!(current.complete, complete, "{data}");
            assert_eq!(
                current.current_user_thread_joined,
                complete.then_some(true),
                "{data}"
            );
        }
        state.snapshot_mut().upsert_channel(channel);
        for (name, data) in [
            ("THREAD_LIST_SYNC", r#"{"guild_id":"10","threads":[]}"#),
            (
                "THREAD_CREATE",
                r#"{"id":"30","guild_id":"10","type":12,"parent_id":"20","thread_metadata":{"archived":false,"locked":false},"member":{"user_id":"40"}}"#,
            ),
        ] {
            state
                .ingest_json(&format!(r#"{{"op":0,"s":4,"t":"{name}","d":{data}}}"#), 5)
                .unwrap();
            let SnapshotLookup::Found(current) = state.snapshot_mut().get_channel("30", 5) else {
                panic!("missing fresh thread")
            };
            assert!(current.complete);
            assert_eq!(current.current_user_thread_joined, Some(true));
        }
    }

    #[test]
    fn malformed_sync_scopes_cannot_apply_as_whole_guild_updates() {
        for data in [
            r#"{"guild_id":"10","channel_ids":null,"threads":[]}"#,
            r#"{"guild_id":"10","channel_ids":[false],"threads":[]}"#,
            r#"{"guild_id":"10","channel_ids":["bad"],"threads":[]}"#,
            r#"{"guild_id":"10","channel_ids":["20"],"threads":[{"id":"30","type":12,"parent_id":"21"}]}"#,
        ] {
            let frame = format!(r#"{{"op":0,"s":2,"t":"THREAD_LIST_SYNC","d":{data}}}"#);
            let mut state = ingestor();
            assert!(state.ingest_json(&frame, 1).is_err(), "{data}");
            let mut tolerant = ingestor();
            tolerant.ingest_json_tolerant(&frame, 1).unwrap();
            assert!(tolerant.machine().gap_epoch() > 0, "{data}");
        }
    }

    #[test]
    fn unknown_bot_identity_keeps_metadata_without_trusting_a_named_participant() {
        let mut state = GatewayIngestor::new(16, 1000, 1000, 10, 10);
        state.ingest_json(r#"{"op":0,"s":1,"t":"THREAD_CREATE","d":{"id":"30","guild_id":"10","type":12,"parent_id":"20","thread_metadata":{"archived":false,"locked":false},"member":{"user_id":"99"}}}"#, 0).unwrap();
        let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel("30", 0) else {
            panic!("missing thread metadata")
        };
        assert!(channel.complete);
        assert_eq!(channel.current_user_thread_joined, None);
    }

    #[test]
    fn scoped_sync_revalidates_omitted_active_threads_without_expiring_other_scopes() {
        let mut state = ingestor();
        for (id, guild, parent, archived) in [
            ("30", "10", "20", false),
            ("31", "10", "20", false),
            ("32", "10", "21", false),
            ("33", "11", "20", false),
            ("34", "10", "20", true),
        ] {
            let mut channel = ChannelSnapshot::new(guild, id, 12, 0);
            channel.parent_channel_id = Some(parent.into());
            channel.archived = archived;
            channel.current_user_thread_joined = Some(true);
            channel.current_user_thread_joined_at_ms = Some(0);
            state.snapshot_mut().upsert_channel(channel);
        }
        state.ingest_json(r#"{"op":0,"s":2,"t":"THREAD_LIST_SYNC","d":{"guild_id":"10","channel_ids":["20"],"threads":[{"id":"30","parent_id":"20","type":12,"thread_metadata":{"archived":false,"locked":false}}],"members":[{"id":"30","user_id":"99"}]}}"#, 1).unwrap();
        for (id, complete, joined) in [
            ("30", true, Some(true)),
            ("31", false, None),
            ("32", true, Some(true)),
            ("33", true, Some(true)),
            ("34", true, Some(true)),
        ] {
            let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel(id, 1) else {
                panic!("missing thread {id}")
            };
            assert_eq!(channel.complete, complete, "{id}");
            assert_eq!(channel.current_user_thread_joined, joined, "{id}");
        }
    }

    #[test]
    fn singular_notifications_reject_invalid_present_ids_and_preserve_current_user_omission() {
        for field in [
            r#""user_id":null"#,
            r#""user_id":99"#,
            r#""guild_id":false"#,
            r#""id":null"#,
        ] {
            let mut state = ingestor();
            let data = if field.starts_with(r#""id""#) {
                format!("{{{field}}}")
            } else {
                format!(r#"{{"id":"30",{field}}}"#)
            };
            assert!(
                state
                    .ingest_json(
                        &format!(r#"{{"op":0,"s":2,"t":"THREAD_MEMBER_UPDATE","d":{data}}}"#),
                        1
                    )
                    .is_err(),
                "{field}"
            );
        }
        let mut state = ingestor();
        state
            .ingest_json(
                r#"{"op":0,"s":2,"t":"THREAD_MEMBER_UPDATE","d":{"id":"30","guild_id":"10"}}"#,
                1,
            )
            .unwrap();
        let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel("30", 1) else {
            panic!("missing thread")
        };
        assert_eq!(channel.current_user_thread_joined, Some(true));
    }

    #[test]
    fn sync_members_must_belong_to_the_returned_guild_and_threads() {
        for data in [
            r#"{"guild_id":"10","threads":[{"id":"30","type":12}],"members":[{"id":"31"}]}"#,
            r#"{"guild_id":"10","threads":[{"id":"30","guild_id":"11","type":12}],"members":[{"id":"30"}]}"#,
            r#"{"guild_id":"10","threads":[{"id":"30","type":0}],"members":[{"id":"30"}]}"#,
        ] {
            let mut state = ingestor();
            assert!(
                state
                    .ingest_json(
                        &format!(r#"{{"op":0,"s":2,"t":"THREAD_LIST_SYNC","d":{data}}}"#),
                        1
                    )
                    .is_err(),
                "{data}"
            );
        }
        let mut state = ingestor();
        state.ingest_json(r#"{"op":0,"s":2,"t":"THREAD_LIST_SYNC","d":{"guild_id":"10","threads":[{"id":"30","type":12}],"members":[{"id":"30"}]}}"#, 1).unwrap();
        let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel("30", 1) else {
            panic!("missing synced thread")
        };
        assert_eq!(channel.current_user_thread_joined, Some(true));
    }

    #[test]
    fn nested_member_identity_is_usable_but_a_foreign_guild_cannot_grant_membership() {
        for user in ["40", "99"] {
            let mut state = ingestor();
            state.ingest_json(&format!(r#"{{"op":0,"s":2,"t":"THREAD_MEMBERS_UPDATE","d":{{"guild_id":"10","id":"30","added_members":[{{"member":{{"user":{{"id":"{user}"}}}}}}]}}}}"#), 1).unwrap();
            let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel("30", 1) else {
                panic!("missing updated thread")
            };
            assert_eq!(channel.current_user_thread_joined, Some(user == "99"));
        }
        let mut state = ingestor();
        state.ingest_json(r#"{"op":0,"s":2,"t":"THREAD_MEMBER_UPDATE","d":{"id":"30","user_id":"99","guild_id":"11"}}"#, 1).unwrap();
        let SnapshotLookup::Found(channel) = state.snapshot_mut().get_channel("30", 1) else {
            panic!("missing preserved metadata")
        };
        assert!(!channel.complete);
        assert_eq!(channel.current_user_thread_joined, None);
        let SnapshotLookup::Found(other) = state.snapshot_mut().get_channel("31", 1) else {
            panic!("missing unrelated thread")
        };
        assert_eq!(other.current_user_thread_joined, Some(false));
    }
}
