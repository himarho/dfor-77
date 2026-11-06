use super::*;

impl DforRuntime {
    pub(super) fn observe_resource_read(
        &mut self,
        collection: &str,
        id: &str,
        path: &[&str],
        outcome: &RestOutcome,
        now_ms: u64,
    ) {
        let Some(id) = crate::rest::path::canonical_id(id) else {
            return;
        };
        match (collection, path) {
            ("guilds", _) => self.observe_guild_read(&id, path, outcome, now_ms),
            ("channels", []) => self.observe_channel_read(&id, outcome, now_ms),
            ("channels", ["thread-members"]) => {
                self.observe_thread_members_read(&id, None, outcome, now_ms)
            }
            ("channels", ["thread-members", user]) => {
                self.observe_thread_members_read(&id, Some(user), outcome, now_ms)
            }
            ("channels", ["messages", message_id]) => {
                let Some(message_id) = crate::rest::path::canonical_id(message_id) else {
                    return;
                };
                self.observe_message_read(&id, &message_id, outcome, now_ms);
            }
            _ => {}
        }
    }

    fn observe_channel_read(&mut self, channel_id: &str, outcome: &RestOutcome, now_ms: u64) {
        if outcome.status == D7OK {
            let guild_id = match self.gateway.snapshot_mut().get_channel(channel_id, now_ms) {
                SnapshotLookup::Found(channel) | SnapshotLookup::Stale(channel) => channel.guild_id,
                _ => String::new(),
            };
            if let Some(snapshot) = outcome
                .response_body
                .as_deref()
                .and_then(|body| self.validated_rest_channel(body, &guild_id, channel_id, now_ms))
            {
                self.cache_rest_channel(snapshot);
                return;
            }
            self.gateway
                .snapshot_mut()
                .invalidate_channel_state(channel_id, now_ms);
            self.gateway
                .snapshot_mut()
                .mark_channel_rest_result(channel_id, now_ms);
            return;
        }
        if outcome.status == D7NFD
            && matches!(
                outcome.why.as_str(),
                WHY_CHANNEL_NOT_FOUND | "RESOURCE_NOT_FOUND"
            )
        {
            self.gateway
                .snapshot_mut()
                .tombstone_channel(channel_id, now_ms);
        } else {
            self.gateway
                .snapshot_mut()
                .invalidate_channel(channel_id, now_ms);
            self.gateway
                .snapshot_mut()
                .invalidate_channel_messages(channel_id, now_ms);
        }
        self.gateway
            .snapshot_mut()
            .mark_channel_rest_result(channel_id, now_ms);
    }

    fn observe_thread_members_read(
        &mut self,
        channel_id: &str,
        user: Option<&str>,
        outcome: &RestOutcome,
        now_ms: u64,
    ) {
        if outcome.status == D7NFD && outcome.why == WHY_CHANNEL_NOT_FOUND {
            self.gateway
                .snapshot_mut()
                .tombstone_channel(channel_id, now_ms);
            self.gateway
                .snapshot_mut()
                .mark_channel_rest_result(channel_id, now_ms);
            return;
        }
        let bot = self.known_bot_user_id();
        let own_request = user.is_some_and(|user| {
            user == "@me"
                || crate::rest::path::canonical_id(user)
                    .as_ref()
                    .zip(bot.as_ref())
                    .is_some_and(|(user, bot)| user == bot)
        });
        if user.is_some() && !own_request {
            return;
        }
        if outcome.status != D7OK {
            if own_request
                && outcome.status == D7NFD
                && matches!(
                    outcome.why.as_str(),
                    WHY_MEMBER_NOT_FOUND | WHY_USER_NOT_FOUND | "RESOURCE_NOT_FOUND"
                )
            {
                self.gateway
                    .snapshot_mut()
                    .set_current_user_thread_joined(channel_id, false, now_ms);
            } else {
                self.gateway
                    .snapshot_mut()
                    .clear_current_user_thread_joined(channel_id, now_ms);
            }
            return;
        }
        let root = outcome
            .response_body
            .as_deref()
            .and_then(|body| parse_json(body).ok());
        let confirmed = root.as_ref().and_then(|root| {
            if own_request {
                let user_id = bot
                    .as_deref()
                    .or_else(|| {
                        root.get("user_id")?
                            .as_str()
                            .filter(|id| validate_snowflake(id))
                    })
                    .unwrap_or_default();
                return thread_member_matches(root, channel_id, user_id, true).then_some(true);
            }
            let mut users = std::collections::HashSet::new();
            for member in root.as_array()? {
                let user_id = member.get("user_id")?.as_str()?;
                if !validate_snowflake(user_id)
                    || !users.insert(user_id)
                    || !thread_member_matches(member, channel_id, user_id, true)
                {
                    return None;
                }
            }
            Some(bot.as_deref().is_some_and(|bot| users.contains(bot)))
        });
        match confirmed {
            Some(true) => {
                self.gateway
                    .snapshot_mut()
                    .set_current_user_thread_joined(channel_id, true, now_ms);
            }
            Some(false) => {}
            None => self
                .gateway
                .snapshot_mut()
                .clear_current_user_thread_joined(channel_id, now_ms),
        }
    }

    fn observe_message_read(
        &mut self,
        channel_id: &str,
        message_id: &str,
        outcome: &RestOutcome,
        now_ms: u64,
    ) {
        if outcome.status == D7OK {
            let Some(data) = outcome
                .response_body
                .as_deref()
                .and_then(|body| parse_json(body).ok())
            else {
                return;
            };
            let complete = crate::snapshot::message::patch(&data).is_some_and(|patch| {
                patch.user_id.is_some()
                    && patch.content.is_some()
                    && patch.image_attachment_count.is_some()
                    && patch.message_type.is_some()
            });
            if complete {
                self.gateway
                    .snapshot_mut()
                    .apply_rest_message_response(&data, channel_id, message_id, now_ms);
            }
            return;
        }
        if outcome.status == D7NFD && outcome.why == WHY_CHANNEL_NOT_FOUND {
            self.gateway
                .snapshot_mut()
                .tombstone_channel(channel_id, now_ms);
            self.gateway
                .snapshot_mut()
                .mark_channel_rest_result(channel_id, now_ms);
        } else if outcome.status == D7NFD
            && matches!(
                outcome.why.as_str(),
                WHY_MESSAGE_NOT_FOUND | "RESOURCE_NOT_FOUND"
            )
        {
            self.gateway
                .snapshot_mut()
                .tombstone_message(channel_id, message_id, now_ms);
        } else {
            self.gateway
                .snapshot_mut()
                .invalidate_message(channel_id, message_id, now_ms);
        }
    }

    pub(super) fn cache_rest_channel(&mut self, snapshot: ChannelSnapshot) {
        let (channel_id, at_ms) = (snapshot.channel_id.clone(), snapshot.captured_at_ms);
        self.gateway
            .snapshot_mut()
            .clear_private_thread_access_for_channel(&snapshot.channel_id, snapshot.captured_at_ms);
        self.gateway.snapshot_mut().upsert_channel(snapshot);
        self.gateway
            .snapshot_mut()
            .mark_channel_rest_result(&channel_id, at_ms);
    }

    pub(super) fn validated_rest_channel(
        &mut self,
        body: &str,
        guild_id: &str,
        channel_id: &str,
        now_ms: u64,
    ) -> Option<ChannelSnapshot> {
        let root = parse_json(body).ok()?;
        let bot_user_id = self.known_bot_user_id();
        for key in ["member", "thread_member"] {
            if root.get(key).is_some_and(|member| {
                let expected_user = bot_user_id
                    .as_deref()
                    .or_else(|| {
                        member
                            .get("user_id")?
                            .as_str()
                            .filter(|id| validate_snowflake(id))
                    })
                    .unwrap_or_default();
                !thread_member_matches(member, channel_id, expected_user, false)
            }) {
                return None;
            }
        }
        let mut snapshot = channel_snapshot_from_json(&root, guild_id, channel_id, now_ms)?;
        if bot_user_id.is_none() && snapshot.current_user_thread_joined == Some(true) {
            snapshot.current_user_thread_joined = None;
            snapshot.current_user_thread_joined_at_ms = None;
        }
        if let SnapshotLookup::Found(known) | SnapshotLookup::Stale(known) =
            self.gateway.snapshot_mut().get_channel(channel_id, now_ms)
        {
            if channel_type_is_thread(known.channel_type)
                && channel_type_is_thread(snapshot.channel_type)
                && known.parent_channel_id.is_some()
                && known.parent_channel_id != snapshot.parent_channel_id
            {
                return None;
            }
        }
        Some(snapshot)
    }

    fn observe_guild_read(
        &mut self,
        guild_id: &str,
        path: &[&str],
        outcome: &RestOutcome,
        now_ms: u64,
    ) {
        let user_id = match path {
            [] | ["roles"] => None,
            ["members", user] => {
                let user = if *user == "@me" {
                    self.known_bot_user_id()
                } else {
                    crate::rest::path::canonical_id(user)
                };
                let Some(user) = user else { return };
                Some(user)
            }
            _ => return,
        };
        if outcome.status == D7OK {
            if let Some(body) = outcome.response_body.as_deref() {
                match path {
                    [] => {
                        if let Some(snapshot) =
                            guild_snapshot_from_rest_body(body, guild_id, now_ms)
                        {
                            self.cache_rest_guild(snapshot);
                            return;
                        }
                    }
                    ["roles"] => {
                        if let Some(snapshot) =
                            guild_roles_snapshot_from_rest_body(body, guild_id, now_ms)
                        {
                            self.cache_rest_guild_roles(snapshot);
                            return;
                        }
                    }
                    _ => {
                        if let Some(snapshot) = user_id.as_deref().and_then(|user| {
                            member_snapshot_from_rest_body(body, guild_id, user, now_ms)
                        }) {
                            self.cache_rest_member(snapshot);
                            return;
                        }
                    }
                }
            }
        }
        if outcome.status == D7NFD && (outcome.why == WHY_GUILD_NOT_FOUND || path.is_empty()) {
            self.gateway
                .snapshot_mut()
                .tombstone_guild(guild_id, now_ms);
            self.invalidate_event_permissions(Some(guild_id), None);
            self.gateway
                .snapshot_mut()
                .mark_guild_rest_result(guild_id, now_ms);
            if let Some(user_id) = user_id.as_deref() {
                self.gateway
                    .snapshot_mut()
                    .mark_member_rest_result(guild_id, user_id, now_ms);
            }
            if path == ["roles"] {
                self.gateway
                    .snapshot_mut()
                    .mark_roles_rest_result(guild_id, now_ms);
            }
            return;
        }
        if let Some(user_id) = user_id {
            self.invalidate_event_permissions(Some(guild_id), Some(&user_id));
            self.clear_bot_membership_access(guild_id, &user_id, now_ms);
            if outcome.status == D7NFD && outcome.why == WHY_USER_NOT_FOUND {
                self.invalidate_event_permissions(None, Some(&user_id));
                self.gateway.snapshot_mut().tombstone_user(&user_id, now_ms);
            } else if outcome.status == D7NFD && outcome.why == WHY_MEMBER_NOT_FOUND {
                self.gateway
                    .snapshot_mut()
                    .tombstone_member(guild_id, &user_id, now_ms);
            } else {
                self.gateway
                    .snapshot_mut()
                    .invalidate_owner_membership(guild_id, &user_id, now_ms);
                self.gateway
                    .snapshot_mut()
                    .invalidate_member(guild_id, &user_id, now_ms);
            }
            self.gateway
                .snapshot_mut()
                .mark_member_rest_result(guild_id, &user_id, now_ms);
        } else {
            self.invalidate_event_permissions(Some(guild_id), None);
            if path.is_empty() {
                self.gateway
                    .snapshot_mut()
                    .invalidate_guild(guild_id, now_ms);
                self.gateway
                    .snapshot_mut()
                    .mark_guild_rest_result(guild_id, now_ms);
            } else {
                self.gateway
                    .snapshot_mut()
                    .invalidate_guild_roles(guild_id, now_ms);
                self.gateway
                    .snapshot_mut()
                    .mark_roles_rest_result(guild_id, now_ms);
            }
        }
    }

    pub(super) fn cache_rest_guild(&mut self, snapshot: GuildSnapshot) {
        let (guild_id, at_ms) = (snapshot.guild_id.clone(), snapshot.captured_at_ms);
        self.invalidate_event_permissions(Some(&snapshot.guild_id), None);
        self.gateway.snapshot_mut().upsert_guild(snapshot);
        self.gateway
            .snapshot_mut()
            .mark_guild_rest_result(&guild_id, at_ms);
    }

    pub(super) fn cache_rest_guild_roles(&mut self, snapshot: GuildRolesSnapshot) {
        let (guild_id, at_ms) = (snapshot.guild_id.clone(), snapshot.captured_at_ms);
        self.invalidate_event_permissions(Some(&snapshot.guild_id), None);
        self.gateway
            .snapshot_mut()
            .clear_private_thread_access_for_guild(&snapshot.guild_id, snapshot.captured_at_ms);
        self.gateway.snapshot_mut().upsert_guild_roles(snapshot);
        self.gateway
            .snapshot_mut()
            .mark_roles_rest_result(&guild_id, at_ms);
    }

    pub(super) fn cache_rest_member(&mut self, snapshot: MemberSnapshot) {
        let (guild_id, user_id, at_ms) = (
            snapshot.guild_id.clone(),
            snapshot.user_id.clone(),
            snapshot.captured_at_ms,
        );
        self.invalidate_event_permissions(Some(&snapshot.guild_id), Some(&snapshot.user_id));
        self.clear_bot_membership_access(
            &snapshot.guild_id,
            &snapshot.user_id,
            snapshot.captured_at_ms,
        );
        self.gateway.snapshot_mut().upsert_member(snapshot);
        self.gateway
            .snapshot_mut()
            .mark_member_rest_result(&guild_id, &user_id, at_ms);
    }

    fn clear_bot_membership_access(&mut self, guild_id: &str, user_id: &str, now_ms: u64) {
        if self.known_bot_user_id().is_none_or(|bot| bot == user_id) {
            self.gateway
                .snapshot_mut()
                .clear_private_thread_access_for_guild(guild_id, now_ms);
        }
    }
}
