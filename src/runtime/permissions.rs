use super::*;

impl DforRuntime {
    pub(super) fn admin(&mut self, request: Request) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, self.state_status());
        }
        if !self.validate_request_ids(&request, &["guild_id", "user_id"]) {
            return Response::empty(request.id, D7ARG);
        }
        let guild_id = request.field("guild_id").unwrap_or_default().to_string();
        let user_id = request.field("user_id").unwrap_or_default().to_string();
        let decision = self.admin_decision(&guild_id, &user_id, now_ms_u64());
        self.admin_response(request.id, decision)
    }

    pub(super) fn admin_response(
        &mut self,
        id: u64,
        decision: Result<bool, PreflightDecision>,
    ) -> Response {
        let is_admin = match decision {
            Ok(is_admin) => is_admin,
            Err(decision) => {
                self.errors.act.set(
                    decision.status,
                    decision.http_status,
                    decision.discord_code,
                    decision.why,
                    decision.why,
                );
                return Response::empty(id, decision.status);
            }
        };
        self.errors.act.clear();
        Response {
            id,
            status: D7OK,
            fields: vec![Field::raw("is_admin", if is_admin { "1" } else { "0" })],
        }
    }

    pub(super) fn current_admin(&mut self, id: u64) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(id, self.state_status());
        }
        let Some(event) = self.current_event.as_ref().filter(|event| {
            event.event_type == D7MSG
                && validate_snowflake(&event.guild_id)
                && validate_snowflake(&event.user_id)
        }) else {
            self.errors
                .act
                .set(D7STA, 0, 0, "current event has no member context", WHY_OK);
            return Response::empty(id, D7STA);
        };
        let guild_id = event.guild_id.clone();
        let user_id = event.user_id.clone();
        let now_ms = now_ms_u64();
        let permissions = self.event_member_permissions(event, now_ms).cloned();
        if let Some(decision) = self.guild_state_decision(&guild_id, now_ms) {
            return self.admin_response(id, Err(decision));
        }
        if let Some(decision) = self.known_owner_admin_decision(&guild_id, &user_id, now_ms) {
            return self.admin_response(id, decision);
        }
        let decision = match permissions {
            Some(permissions) => {
                if MemberPermissions::new(permissions).administrator() {
                    Ok(true)
                } else {
                    self.guild_owner_id_decision(&guild_id, now_ms)
                        .map(|owner_id| {
                            owner_id
                                .as_deref()
                                .is_some_and(|owner_id| owner_id == user_id)
                        })
                }
            }
            None => self.admin_decision(&guild_id, &user_id, now_ms),
        };
        self.admin_response(id, decision)
    }

    pub(super) fn send_preflight(&mut self, request: Request) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, self.state_status());
        }
        if !self.validate_request_ids(&request, &["guild_id", "channel_id"]) {
            return self.preflight_decision(request.id, D7ARG, false, WHY_INVALID_FORM_BODY);
        }
        let guild_id = request.field("guild_id").unwrap_or_default().to_string();
        let channel_id = request.field("channel_id").unwrap_or_default().to_string();
        let decision = self.send_preflight_decision(&guild_id, &channel_id, now_ms_u64());
        self.preflight_response(request.id, decision)
    }

    pub(super) fn timeout_preflight(&mut self, request: Request) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, self.state_status());
        }
        if !self.validate_request_ids(&request, &["guild_id", "user_id"]) {
            return self.preflight_decision(request.id, D7ARG, false, WHY_INVALID_FORM_BODY);
        }
        let guild_id = request.field("guild_id").unwrap_or_default().to_string();
        let user_id = request.field("user_id").unwrap_or_default().to_string();
        let decision = self.timeout_preflight_decision(&guild_id, &user_id, now_ms_u64());
        self.preflight_response(request.id, decision)
    }

    pub(super) fn delete_preflight(&mut self, request: Request) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, self.state_status());
        }
        if !self.validate_request_ids(&request, &["guild_id", "channel_id", "message_id"]) {
            return self.preflight_decision(request.id, D7ARG, false, WHY_INVALID_FORM_BODY);
        }
        let channel_id = request.field("channel_id").unwrap_or_default().to_string();
        let message_id = request.field("message_id").unwrap_or_default().to_string();
        let guild_id = request.field("guild_id").unwrap_or_default().to_string();
        let decision =
            self.delete_preflight_decision(&guild_id, &channel_id, &message_id, now_ms_u64());
        self.preflight_response(request.id, decision)
    }

    pub(super) fn current_delete_preflight(&mut self, id: u64) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(id, self.state_status());
        }
        let Some(event) = &self.current_event else {
            self.errors
                .act
                .set(D7STA, 0, 0, "current event has no action context", WHY_OK);
            return Response::empty(id, D7STA);
        };
        if event.event_type != D7MSG {
            self.errors
                .act
                .set(D7STA, 0, 0, "current event has no message context", WHY_OK);
            return Response::empty(id, D7STA);
        }
        let guild_id = event.guild_id.clone();
        let channel_id = event.channel_id.clone();
        let message_id = event.message_id.clone();
        let decision =
            self.delete_preflight_decision(&guild_id, &channel_id, &message_id, now_ms_u64());
        self.preflight_response(id, decision)
    }

    pub(super) fn current_send_preflight(&mut self, id: u64) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(id, self.state_status());
        }
        if !matches!(
            self.current_event.as_ref().map(|event| event.event_type),
            Some(D7MSG)
        ) {
            self.errors
                .act
                .set(D7STA, 0, 0, "current event has no action context", WHY_OK);
            return Response::empty(id, D7STA);
        }
        let Some(event) = &self.current_event else {
            return Response::empty(id, D7STA);
        };
        let guild_id = event.guild_id.clone();
        let channel_id = event.channel_id.clone();
        let decision = self.send_preflight_decision(&guild_id, &channel_id, now_ms_u64());
        self.preflight_response(id, decision)
    }

    pub(super) fn current_timeout_preflight(&mut self, id: u64) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(id, self.state_status());
        }
        let Some(event) = &self.current_event else {
            self.errors
                .act
                .set(D7STA, 0, 0, "current event has no action context", WHY_OK);
            return Response::empty(id, D7STA);
        };
        if event.event_type != D7MSG {
            self.errors
                .act
                .set(D7STA, 0, 0, "current event has no member context", WHY_OK);
            return Response::empty(id, D7STA);
        }
        let guild_id = event.guild_id.clone();
        let user_id = event.user_id.clone();
        let decision = self.timeout_preflight_decision(&guild_id, &user_id, now_ms_u64());
        self.preflight_response(id, decision)
    }

    pub(super) fn validate_request_ids(&mut self, request: &Request, keys: &[&str]) -> bool {
        for key in keys {
            if self.require_id(request, key) != D7OK {
                self.errors
                    .act
                    .set(D7ARG, 0, 0, "invalid Discord id", WHY_INVALID_FORM_BODY);
                return false;
            }
        }
        true
    }

    pub(super) fn delete_preflight_decision(
        &mut self,
        guild_id: &str,
        channel_id: &str,
        message_id: &str,
        now_ms: u64,
    ) -> PreflightDecision {
        if let Some(decision) = self.guild_state_decision(guild_id, now_ms) {
            return decision;
        }
        if matches!(
            self.gateway.snapshot_mut().get_channel(channel_id, now_ms),
            SnapshotLookup::Tombstone
        ) {
            return PreflightDecision::new(D7NFD, false, WHY_CHANNEL_NOT_FOUND);
        }
        match self
            .gateway
            .snapshot_mut()
            .get_message(channel_id, message_id, now_ms)
        {
            SnapshotLookup::Found(snapshot) => {
                self.delete_with_message_snapshot(&snapshot, guild_id, channel_id, now_ms, false)
            }
            SnapshotLookup::Tombstone => {
                PreflightDecision::new(D7NFD, false, WHY_MESSAGE_NOT_FOUND)
            }
            SnapshotLookup::Missing | SnapshotLookup::Stale(_) => {
                self.fetch_message_snapshot_decision(guild_id, channel_id, message_id, now_ms)
            }
        }
    }

    pub(super) fn admin_decision(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> Result<bool, PreflightDecision> {
        if let Some(decision) = self.guild_state_decision(guild_id, now_ms) {
            return Err(decision);
        }
        if let Some(decision) = self.known_owner_admin_decision(guild_id, user_id, now_ms) {
            return decision;
        }
        let member = self.member_snapshot_decision(guild_id, user_id, now_ms, false)?;
        let roles = self.guild_roles_decision(guild_id, now_ms)?;
        let Some(permissions) = member_permissions_from_roles(guild_id, &member, &roles, None)
        else {
            return Err(PreflightDecision::new(
                D7STA,
                false,
                WHY_ROLE_SNAPSHOT_INCOMPLETE,
            ));
        };
        if permissions.administrator() {
            return Ok(true);
        }
        Ok(self
            .guild_owner_id_decision(guild_id, now_ms)?
            .as_deref()
            .is_some_and(|owner_id| owner_id == user_id))
    }

    pub(super) fn send_preflight_decision(
        &mut self,
        guild_id: &str,
        channel_id: &str,
        now_ms: u64,
    ) -> PreflightDecision {
        self.send_decision(guild_id, channel_id, now_ms, SendDecisionMode::Preflight)
    }

    pub(super) fn send_decision(
        &mut self,
        guild_id: &str,
        channel_id: &str,
        now_ms: u64,
        mode: SendDecisionMode,
    ) -> PreflightDecision {
        if let Some(decision) = self.guild_state_decision(guild_id, now_ms) {
            return decision;
        }
        match self.gateway.snapshot_mut().get_channel(channel_id, now_ms) {
            SnapshotLookup::Found(snapshot) if snapshot.complete => {
                self.send_with_channel_snapshot(&snapshot, guild_id, now_ms, mode)
            }
            SnapshotLookup::Tombstone => {
                PreflightDecision::new(D7NFD, false, WHY_CHANNEL_NOT_FOUND)
            }
            SnapshotLookup::Found(_) | SnapshotLookup::Missing | SnapshotLookup::Stale(_) => {
                self.fetch_channel_snapshot_decision(guild_id, channel_id, now_ms, mode)
            }
        }
    }

    pub(super) fn send_action_precheck(
        &mut self,
        channel_id: &str,
        now_ms: u64,
    ) -> Option<PreflightDecision> {
        let decision = action_decision_from_preflight(self.send_decision(
            "",
            channel_id,
            now_ms,
            SendDecisionMode::Action,
        ));
        if decision.status == D7OK && decision.can {
            None
        } else {
            Some(decision)
        }
    }

    pub(super) fn fetch_channel_snapshot_decision(
        &mut self,
        guild_id: &str,
        channel_id: &str,
        now_ms: u64,
        mode: SendDecisionMode,
    ) -> PreflightDecision {
        if !self.rest.can_execute() {
            return PreflightDecision::new(D7STA, false, WHY_CHANNEL_SNAPSHOT_INCOMPLETE);
        }
        let outcome = self.rest.execute_get_channel(channel_id, now_ms);
        self.observe_rest_outcome(&outcome);
        match outcome.status {
            D7OK => {
                let Some(body) = outcome.response_body.as_deref() else {
                    return PreflightDecision::new(D7STA, false, WHY_CHANNEL_SNAPSHOT_INCOMPLETE)
                        .with_rest_outcome(&outcome);
                };
                let Some(snapshot) =
                    self.validated_rest_channel(body, guild_id, channel_id, now_ms)
                else {
                    return PreflightDecision::new(D7STA, false, WHY_CHANNEL_SNAPSHOT_INCOMPLETE)
                        .with_rest_outcome(&outcome);
                };
                self.cache_rest_channel(snapshot.clone());
                self.send_with_channel_snapshot(&snapshot, guild_id, now_ms, mode)
            }
            D7NFD => {
                self.gateway
                    .snapshot_mut()
                    .tombstone_channel(channel_id, now_ms);
                PreflightDecision::new(D7NFD, false, WHY_CHANNEL_NOT_FOUND)
                    .with_rest_outcome(&outcome)
            }
            D7FOR => {
                PreflightDecision::new(D7FOR, false, WHY_MISSING_ACCESS).with_rest_outcome(&outcome)
            }
            D7ARG => PreflightDecision::new(D7ARG, false, WHY_INVALID_FORM_BODY)
                .with_rest_outcome(&outcome),
            D7INV => PreflightDecision::new(D7INV, false, "INVALID_REQUEST_BUDGET")
                .with_rest_outcome(&outcome),
            D7RTL => {
                PreflightDecision::new(D7RTL, false, "RATE_LIMITED").with_rest_outcome(&outcome)
            }
            _ => PreflightDecision::new(D7STA, false, WHY_CHANNEL_SNAPSHOT_INCOMPLETE)
                .with_rest_outcome(&outcome),
        }
    }

    pub(super) fn send_with_channel_snapshot(
        &mut self,
        snapshot: &ChannelSnapshot,
        guild_id: &str,
        now_ms: u64,
        mode: SendDecisionMode,
    ) -> PreflightDecision {
        if let Some(decision) = send_channel_non_access_block_decision(snapshot, guild_id, mode) {
            return decision;
        }
        if snapshot.channel_type == 1 {
            return if snapshot.guild_id.is_empty() {
                PreflightDecision::new(D7OK, true, WHY_OK)
            } else {
                PreflightDecision::new(D7STA, false, WHY_CHANNEL_SNAPSHOT_INCOMPLETE)
            };
        }
        let snapshot = match self.resolve_private_thread_access(snapshot.clone(), now_ms) {
            Ok(snapshot) => snapshot,
            Err(decision) => return decision,
        };
        if let Some(decision) = send_channel_non_access_block_decision(&snapshot, guild_id, mode) {
            return decision;
        }
        let effective_guild_id = if guild_id.is_empty() {
            snapshot.guild_id.as_str()
        } else {
            guild_id
        };
        if effective_guild_id.is_empty() {
            return PreflightDecision::new(D7STA, false, WHY_CHANNEL_SNAPSHOT_INCOMPLETE);
        }
        if let Some(decision) = self.guild_state_decision(effective_guild_id, now_ms) {
            return decision;
        }
        let (_, bot_permissions) = match self.bot_permissions_for_channel(&snapshot, now_ms) {
            Ok(resolved) => resolved,
            Err(decision) => return decision,
        };
        if let Some(decision) =
            private_thread_access_block_decision(&snapshot, &bot_permissions, false)
        {
            return decision;
        }
        let resolver = PermissionResolver::new(bot_permissions);
        let decision = resolver.can_send(
            channel_type_is_thread(snapshot.channel_type),
            true,
            snapshot.archived,
            snapshot.locked,
        );
        PreflightDecision::new(decision.status, decision.can, decision.why)
    }

    pub(super) fn member_snapshot_decision(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
        bot_member: bool,
    ) -> Result<MemberSnapshot, PreflightDecision> {
        if !bot_member
            && self
                .gateway
                .snapshot_mut()
                .is_user_tombstoned(user_id, now_ms)
        {
            return Err(PreflightDecision::new(D7NFD, false, WHY_USER_NOT_FOUND));
        }
        let lookup = if bot_member {
            self.gateway
                .snapshot_mut()
                .get_bot_member(guild_id, user_id, now_ms)
        } else {
            self.gateway
                .snapshot_mut()
                .get_member(guild_id, user_id, now_ms)
        };
        match lookup {
            SnapshotLookup::Found(snapshot) => {
                if snapshot.complete {
                    Ok(snapshot)
                } else {
                    self.fetch_member_snapshot_decision(guild_id, user_id, now_ms, bot_member)
                }
            }
            SnapshotLookup::Tombstone => Err(if bot_member {
                PreflightDecision::new(D7STA, false, WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE)
            } else {
                PreflightDecision::new(D7NFD, false, WHY_MEMBER_NOT_FOUND)
            }),
            SnapshotLookup::Missing | SnapshotLookup::Stale(_) => {
                self.fetch_member_snapshot_decision(guild_id, user_id, now_ms, bot_member)
            }
        }
    }

    pub(super) fn fetch_member_snapshot_decision(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
        bot_member: bool,
    ) -> Result<MemberSnapshot, PreflightDecision> {
        if !self.rest.can_execute() {
            return Err(PreflightDecision::new(
                D7STA,
                false,
                if bot_member {
                    WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE
                } else {
                    WHY_MEMBER_SNAPSHOT_INCOMPLETE
                },
            ));
        }
        let outcome = self
            .rest
            .execute_get_guild_member(guild_id, user_id, now_ms);
        self.observe_rest_outcome(&outcome);
        match outcome.status {
            D7OK => {
                let Some(body) = outcome.response_body.as_deref() else {
                    return Err(PreflightDecision::new(
                        D7STA,
                        false,
                        if bot_member {
                            WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE
                        } else {
                            WHY_MEMBER_SNAPSHOT_INCOMPLETE
                        },
                    )
                    .with_rest_outcome(&outcome));
                };
                let Some(snapshot) =
                    member_snapshot_from_rest_body(body, guild_id, user_id, now_ms)
                else {
                    return Err(PreflightDecision::new(
                        D7STA,
                        false,
                        if bot_member {
                            WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE
                        } else {
                            WHY_MEMBER_SNAPSHOT_INCOMPLETE
                        },
                    )
                    .with_rest_outcome(&outcome));
                };
                self.cache_rest_member(snapshot.clone());
                Ok(snapshot)
            }
            D7NFD => {
                if outcome.why == WHY_USER_NOT_FOUND {
                    self.invalidate_event_permissions(None, Some(user_id));
                    self.gateway.snapshot_mut().tombstone_user(user_id, now_ms);
                } else if !bot_member {
                    self.invalidate_event_permissions(Some(guild_id), Some(user_id));
                    self.gateway
                        .snapshot_mut()
                        .tombstone_member(guild_id, user_id, now_ms);
                }
                Err(if bot_member {
                    PreflightDecision::new(D7STA, false, WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE)
                        .with_rest_outcome(&outcome)
                } else if outcome.why == WHY_USER_NOT_FOUND {
                    PreflightDecision::new(D7NFD, false, WHY_USER_NOT_FOUND)
                        .with_rest_outcome(&outcome)
                } else {
                    PreflightDecision::new(D7NFD, false, WHY_MEMBER_NOT_FOUND)
                        .with_rest_outcome(&outcome)
                })
            }
            D7FOR => Err(PreflightDecision::new(D7FOR, false, WHY_MISSING_ACCESS)
                .with_rest_outcome(&outcome)),
            D7ARG => Err(PreflightDecision::new(D7ARG, false, WHY_INVALID_FORM_BODY)
                .with_rest_outcome(&outcome)),
            D7INV => Err(
                PreflightDecision::new(D7INV, false, "INVALID_REQUEST_BUDGET")
                    .with_rest_outcome(&outcome),
            ),
            D7RTL => {
                Err(PreflightDecision::new(D7RTL, false, "RATE_LIMITED")
                    .with_rest_outcome(&outcome))
            }
            _ => Err(PreflightDecision::new(
                D7STA,
                false,
                if bot_member {
                    WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE
                } else {
                    WHY_MEMBER_SNAPSHOT_INCOMPLETE
                },
            )
            .with_rest_outcome(&outcome)),
        }
    }

    pub(super) fn guild_roles_decision(
        &mut self,
        guild_id: &str,
        now_ms: u64,
    ) -> Result<GuildRolesSnapshot, PreflightDecision> {
        match self
            .gateway
            .snapshot_mut()
            .get_guild_roles(guild_id, now_ms)
        {
            SnapshotLookup::Found(snapshot) => {
                if snapshot.complete {
                    Ok(snapshot)
                } else {
                    self.fetch_guild_roles_decision(guild_id, now_ms)
                }
            }
            SnapshotLookup::Tombstone => Err(PreflightDecision::new(
                D7STA,
                false,
                WHY_ROLE_SNAPSHOT_INCOMPLETE,
            )),
            SnapshotLookup::Missing | SnapshotLookup::Stale(_) => {
                self.fetch_guild_roles_decision(guild_id, now_ms)
            }
        }
    }

    pub(super) fn fetch_guild_roles_decision(
        &mut self,
        guild_id: &str,
        now_ms: u64,
    ) -> Result<GuildRolesSnapshot, PreflightDecision> {
        if !self.rest.can_execute() {
            return Err(PreflightDecision::new(
                D7STA,
                false,
                WHY_ROLE_SNAPSHOT_INCOMPLETE,
            ));
        }
        let outcome = self.rest.execute_get_guild_roles(guild_id, now_ms);
        self.observe_rest_outcome(&outcome);
        match outcome.status {
            D7OK => {
                let Some(body) = outcome.response_body.as_deref() else {
                    return Err(
                        PreflightDecision::new(D7STA, false, WHY_ROLE_SNAPSHOT_INCOMPLETE)
                            .with_rest_outcome(&outcome),
                    );
                };
                let Some(snapshot) = guild_roles_snapshot_from_rest_body(body, guild_id, now_ms)
                else {
                    return Err(
                        PreflightDecision::new(D7STA, false, WHY_ROLE_SNAPSHOT_INCOMPLETE)
                            .with_rest_outcome(&outcome),
                    );
                };
                self.cache_rest_guild_roles(snapshot.clone());
                Ok(snapshot)
            }
            D7NFD => {
                self.gateway
                    .snapshot_mut()
                    .tombstone_guild(guild_id, now_ms);
                self.invalidate_event_permissions(Some(guild_id), None);
                Err(PreflightDecision::new(D7NFD, false, WHY_GUILD_NOT_FOUND)
                    .with_rest_outcome(&outcome))
            }
            D7FOR => Err(PreflightDecision::new(D7FOR, false, WHY_MISSING_ACCESS)
                .with_rest_outcome(&outcome)),
            D7ARG => Err(PreflightDecision::new(D7ARG, false, WHY_INVALID_FORM_BODY)
                .with_rest_outcome(&outcome)),
            D7INV => Err(
                PreflightDecision::new(D7INV, false, "INVALID_REQUEST_BUDGET")
                    .with_rest_outcome(&outcome),
            ),
            D7RTL => {
                Err(PreflightDecision::new(D7RTL, false, "RATE_LIMITED")
                    .with_rest_outcome(&outcome))
            }
            _ => Err(
                PreflightDecision::new(D7STA, false, WHY_ROLE_SNAPSHOT_INCOMPLETE)
                    .with_rest_outcome(&outcome),
            ),
        }
    }

    pub(super) fn guild_state_decision(
        &mut self,
        guild_id: &str,
        now_ms: u64,
    ) -> Option<PreflightDecision> {
        if guild_id.is_empty() {
            return None;
        }
        let state = self.gateway.snapshot_mut().get_guild(guild_id, now_ms);
        if self.gateway.snapshot().guild_rest_protected(guild_id)
            && match &state {
                SnapshotLookup::Found(guild) => !guild.complete,
                SnapshotLookup::Missing | SnapshotLookup::Stale(_) => true,
                SnapshotLookup::Tombstone => false,
            }
        {
            return self.guild_owner_id_decision(guild_id, now_ms).err();
        }
        match state {
            SnapshotLookup::Found(snapshot) | SnapshotLookup::Stale(snapshot) => {
                if snapshot.unavailable {
                    Some(PreflightDecision::new(D7STA, false, WHY_GUILD_UNAVAILABLE))
                } else {
                    None
                }
            }
            SnapshotLookup::Tombstone => {
                Some(PreflightDecision::new(D7NFD, false, WHY_GUILD_NOT_FOUND))
            }
            SnapshotLookup::Missing => None,
        }
    }

    pub(super) fn guild_owner_id_decision(
        &mut self,
        guild_id: &str,
        now_ms: u64,
    ) -> Result<Option<String>, PreflightDecision> {
        if guild_id.is_empty() {
            return Ok(None);
        }
        let protected = self.gateway.snapshot().guild_rest_protected(guild_id);
        match self.gateway.snapshot_mut().get_guild(guild_id, now_ms) {
            SnapshotLookup::Found(snapshot)
                if snapshot.unavailable && (snapshot.complete || !protected) =>
            {
                return Err(PreflightDecision::new(D7STA, false, WHY_GUILD_UNAVAILABLE))
            }
            SnapshotLookup::Stale(snapshot) if snapshot.unavailable && !protected => {
                return Err(PreflightDecision::new(D7STA, false, WHY_GUILD_UNAVAILABLE))
            }
            SnapshotLookup::Found(snapshot) if snapshot.complete && snapshot.owner_id.is_some() => {
                return Ok(snapshot.owner_id)
            }
            SnapshotLookup::Found(snapshot) | SnapshotLookup::Stale(snapshot)
                if snapshot.complete && snapshot.owner_id.is_none() && !self.rest.can_execute() =>
            {
                return Ok(None)
            }
            SnapshotLookup::Missing if !self.rest.can_execute() && !protected => return Ok(None),
            SnapshotLookup::Tombstone => {
                return Err(PreflightDecision::new(D7NFD, false, WHY_GUILD_NOT_FOUND))
            }
            _ => {}
        }
        if !self.rest.can_execute() {
            return Err(PreflightDecision::new(
                D7STA,
                false,
                WHY_GUILD_SNAPSHOT_INCOMPLETE,
            ));
        }
        let outcome = self.rest.execute_get_guild(guild_id, now_ms);
        self.observe_rest_outcome(&outcome);
        match outcome.status {
            D7OK => {
                let snapshot = outcome
                    .response_body
                    .as_deref()
                    .and_then(|body| guild_snapshot_from_rest_body(body, guild_id, now_ms))
                    .ok_or_else(|| {
                        PreflightDecision::new(D7STA, false, WHY_GUILD_SNAPSHOT_INCOMPLETE)
                            .with_rest_outcome(&outcome)
                    })?;
                let unavailable = snapshot.unavailable;
                let owner_id = snapshot.owner_id.clone();
                self.cache_rest_guild(snapshot);
                if unavailable {
                    Err(PreflightDecision::new(D7STA, false, WHY_GUILD_UNAVAILABLE)
                        .with_rest_outcome(&outcome))
                } else {
                    Ok(owner_id)
                }
            }
            D7NFD => {
                self.gateway
                    .snapshot_mut()
                    .tombstone_guild(guild_id, now_ms);
                self.invalidate_event_permissions(Some(guild_id), None);
                Err(PreflightDecision::new(D7NFD, false, WHY_GUILD_NOT_FOUND)
                    .with_rest_outcome(&outcome))
            }
            D7FOR => Err(PreflightDecision::new(D7FOR, false, WHY_MISSING_ACCESS)
                .with_rest_outcome(&outcome)),
            D7ARG => Err(PreflightDecision::new(D7ARG, false, WHY_INVALID_FORM_BODY)
                .with_rest_outcome(&outcome)),
            D7INV => Err(
                PreflightDecision::new(D7INV, false, "INVALID_REQUEST_BUDGET")
                    .with_rest_outcome(&outcome),
            ),
            D7RTL => {
                Err(PreflightDecision::new(D7RTL, false, "RATE_LIMITED")
                    .with_rest_outcome(&outcome))
            }
            _ => Err(
                PreflightDecision::new(D7STA, false, WHY_GUILD_SNAPSHOT_INCOMPLETE)
                    .with_rest_outcome(&outcome),
            ),
        }
    }

    fn known_owner_admin_decision(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> Option<Result<bool, PreflightDecision>> {
        let store = self.gateway.snapshot_mut();
        if store.is_user_tombstoned(user_id, now_ms) {
            return Some(Err(PreflightDecision::new(
                D7NFD,
                false,
                WHY_USER_NOT_FOUND,
            )));
        }
        if matches!(
            store.get_member(guild_id, user_id, now_ms),
            SnapshotLookup::Tombstone
        ) {
            return Some(Err(PreflightDecision::new(
                D7NFD,
                false,
                WHY_MEMBER_NOT_FOUND,
            )));
        }
        matches!(
            store.get_guild(guild_id, now_ms),
            SnapshotLookup::Found(guild)
                if guild.complete && !guild.unavailable && guild.owner_id.as_deref() == Some(user_id)
        ).then_some(Ok(true))
    }

    pub(super) fn bot_user_id_decision(
        &mut self,
        now_ms: u64,
    ) -> Result<String, PreflightDecision> {
        if let Some(user_id) = self.gateway.machine().bot_user_id() {
            return Ok(user_id.to_string());
        }
        if let Some(user_id) = &self.bot_user_id_cache {
            return Ok(user_id.clone());
        }
        if !self.rest.can_execute() {
            return Err(PreflightDecision::new(
                D7STA,
                false,
                WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE,
            ));
        }
        let outcome = self.rest.execute_get_current_user(now_ms);
        self.observe_rest_outcome(&outcome);
        match outcome.status {
            D7OK => {
                let Some(body) = outcome.response_body.as_deref() else {
                    return Err(PreflightDecision::new(
                        D7STA,
                        false,
                        WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE,
                    )
                    .with_rest_outcome(&outcome));
                };
                let Some(user_id) = current_user_id_from_rest_body(body) else {
                    return Err(PreflightDecision::new(
                        D7STA,
                        false,
                        WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE,
                    )
                    .with_rest_outcome(&outcome));
                };
                self.bot_user_id_cache = Some(user_id.clone());
                Ok(user_id)
            }
            D7FOR => Err(PreflightDecision::new(D7FOR, false, WHY_MISSING_ACCESS)
                .with_rest_outcome(&outcome)),
            D7ARG => Err(PreflightDecision::new(D7ARG, false, WHY_INVALID_FORM_BODY)
                .with_rest_outcome(&outcome)),
            D7INV => Err(
                PreflightDecision::new(D7INV, false, "INVALID_REQUEST_BUDGET")
                    .with_rest_outcome(&outcome),
            ),
            D7RTL => {
                Err(PreflightDecision::new(D7RTL, false, "RATE_LIMITED")
                    .with_rest_outcome(&outcome))
            }
            _ => Err(
                PreflightDecision::new(D7STA, false, WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE)
                    .with_rest_outcome(&outcome),
            ),
        }
    }

    pub(super) fn timeout_preflight_decision(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> PreflightDecision {
        if let Some(decision) = self.guild_state_decision(guild_id, now_ms) {
            return decision;
        }
        match self.current_event_target_is_owner(guild_id, user_id, now_ms) {
            Ok(true) => return PreflightDecision::new(D7OK, false, WHY_TARGET_IS_OWNER),
            Err(decision) => return decision,
            Ok(false) => {}
        }
        if self.current_event_target_is_admin(guild_id, user_id, now_ms) {
            return PreflightDecision::new(D7OK, false, WHY_TARGET_IS_ADMIN);
        }
        let bot_user_id = match self.bot_user_id_decision(now_ms) {
            Ok(user_id) => user_id,
            Err(decision) => return decision,
        };
        let bot_member = match self.member_snapshot_decision(guild_id, &bot_user_id, now_ms, true) {
            Ok(member) => member,
            Err(decision) => return decision,
        };
        let roles = match self.guild_roles_decision(guild_id, now_ms) {
            Ok(roles) => roles,
            Err(decision) => return decision,
        };
        let owner_id = match self.guild_owner_id_decision(guild_id, now_ms) {
            Ok(owner_id) => owner_id,
            Err(decision) => return decision,
        };
        let Some(bot_permissions) =
            member_permissions_from_roles(guild_id, &bot_member, &roles, owner_id.as_deref())
        else {
            return PreflightDecision::new(D7STA, false, WHY_ROLE_SNAPSHOT_INCOMPLETE);
        };
        let target_member = match self.member_snapshot_decision(guild_id, user_id, now_ms, false) {
            Ok(member) => member,
            Err(decision) => return decision,
        };
        let Some(target_permissions) =
            member_permissions_from_roles(guild_id, &target_member, &roles, owner_id.as_deref())
        else {
            return PreflightDecision::new(D7STA, false, WHY_MEMBER_SNAPSHOT_INCOMPLETE);
        };
        let resolver = PermissionResolver::new(bot_permissions);
        let decision = resolver.can_timeout(&target_permissions);
        PreflightDecision::new(decision.status, decision.can, decision.why)
    }

    pub(super) fn untimeout_action_precheck(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> Option<PreflightDecision> {
        if let Some(decision) =
            moderation_action_precheck(self.timeout_preflight_decision(guild_id, user_id, now_ms))
        {
            return Some(decision);
        }
        let target_member = match self.member_snapshot_decision(guild_id, user_id, now_ms, false) {
            Ok(member) => member,
            Err(decision) => return Some(decision),
        };
        if target_member.timed_out {
            None
        } else {
            Some(PreflightDecision::new(D7SKP, false, WHY_OK))
        }
    }

    pub(super) fn event_member_permissions<'a>(
        &self,
        event: &'a Event,
        now_ms: u64,
    ) -> Option<&'a PermissionSet> {
        if now_ms.saturating_sub(event.received_at_ms) > self.settings.member_ttl_ms {
            return None;
        }
        if self
            .gateway
            .snapshot()
            .member_rest_protected(&event.guild_id, &event.user_id)
            || self
                .gateway
                .snapshot()
                .roles_rest_protected(&event.guild_id)
            || self
                .gateway
                .snapshot()
                .guild_rest_protected(&event.guild_id)
        {
            return None;
        }
        event.member_permissions.as_ref()
    }

    pub(super) fn current_event_target_is_admin(
        &self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> bool {
        let Some(event) = &self.current_event else {
            return false;
        };
        if event.event_type != D7MSG || event.guild_id != guild_id || event.user_id != user_id {
            return false;
        }
        let Some(permissions) = self.event_member_permissions(event, now_ms) else {
            return false;
        };
        MemberPermissions::new(permissions.clone()).administrator()
    }

    pub(super) fn current_event_target_is_owner(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> Result<bool, PreflightDecision> {
        let Some(event) = &self.current_event else {
            return Ok(false);
        };
        if event.event_type != D7MSG || event.guild_id != guild_id || event.user_id != user_id {
            return Ok(false);
        }
        Ok(self
            .guild_owner_id_decision(guild_id, now_ms)?
            .as_deref()
            .is_some_and(|owner_id| owner_id == user_id))
    }

    pub(super) fn delete_with_message_snapshot(
        &mut self,
        snapshot: &MessageSnapshot,
        fallback_guild_id: &str,
        fallback_channel_id: &str,
        now_ms: u64,
        requires_read_message_history: bool,
    ) -> PreflightDecision {
        let type_decision = delete_snapshot_decision(snapshot);
        if !(type_decision.status == D7OK && type_decision.can) {
            return type_decision;
        }
        let channel_id = if snapshot.channel_id.is_empty() {
            fallback_channel_id
        } else {
            &snapshot.channel_id
        };
        if channel_id.is_empty() {
            return PreflightDecision::new(D7STA, false, WHY_MESSAGE_SNAPSHOT_INCOMPLETE);
        }
        let mut guild_id = if snapshot.guild_id.is_empty() {
            fallback_guild_id.to_string()
        } else {
            snapshot.guild_id.clone()
        };
        let channel = match self.channel_snapshot_for_permissions(&guild_id, channel_id, now_ms) {
            Ok(snapshot) => snapshot,
            Err(decision) => return decision,
        };
        if channel.channel_type == 1 {
            if !guild_id.is_empty() || !channel.guild_id.is_empty() {
                return PreflightDecision::new(D7STA, false, WHY_CHANNEL_SNAPSHOT_INCOMPLETE);
            }
            let bot_user_id = match self.bot_user_id_decision(now_ms) {
                Ok(user_id) => user_id,
                Err(decision) => return decision,
            };
            return if snapshot.user_id == bot_user_id {
                PreflightDecision::new(D7OK, true, WHY_OK)
            } else {
                PreflightDecision::new(D7OK, false, "DM_MESSAGE_NOT_OWNED")
            };
        }
        if guild_id.is_empty() {
            guild_id = channel.guild_id.clone();
        }
        if guild_id.is_empty() {
            return PreflightDecision::new(D7STA, false, WHY_CHANNEL_SNAPSHOT_INCOMPLETE);
        }
        if let Some(decision) = self.guild_state_decision(&guild_id, now_ms) {
            return decision;
        }
        let (bot_user_id, bot_permissions) =
            match self.bot_permissions_for_channel(&channel, now_ms) {
                Ok(resolved) => resolved,
                Err(decision) => return decision,
            };
        if let Some(decision) =
            private_thread_access_block_decision(&channel, &bot_permissions, true)
        {
            return decision;
        }
        if channel_type_is_thread(channel.channel_type)
            && channel.archived
            && channel.locked
            && !bot_permissions.timed_out
            && !bot_permissions.administrator()
            && !bot_permissions.permissions.has(BIT_MANAGE_THREADS)
        {
            return PreflightDecision::new(D7OK, false, WHY_THREAD_LOCKED);
        }
        let resolver = PermissionResolver::new(bot_permissions);
        let decision = resolver.can_delete_message(
            snapshot.user_id == bot_user_id,
            true,
            message_type_requires_manage_messages(snapshot.message_type),
            requires_read_message_history,
        );
        PreflightDecision::new(decision.status, decision.can, decision.why)
    }

    pub(super) fn bot_permissions_for_channel(
        &mut self,
        channel: &ChannelSnapshot,
        now_ms: u64,
    ) -> Result<(String, MemberPermissions), PreflightDecision> {
        let guild_id = &channel.guild_id;
        let bot_user_id = self.bot_user_id_decision(now_ms)?;
        let bot_member = self.member_snapshot_decision(guild_id, &bot_user_id, now_ms, true)?;
        let roles = self.guild_roles_decision(guild_id, now_ms)?;
        let mut permissions = member_permissions_from_roles(guild_id, &bot_member, &roles, None)
            .ok_or_else(|| PreflightDecision::new(D7STA, false, WHY_ROLE_SNAPSHOT_INCOMPLETE))?;
        if !permissions.administrator() {
            permissions.is_owner = self
                .guild_owner_id_decision(guild_id, now_ms)?
                .as_deref()
                .is_some_and(|owner_id| owner_id == bot_user_id);
        }
        let parent = if channel_type_is_thread(channel.channel_type) {
            let parent_id = channel
                .parent_channel_id
                .as_deref()
                .filter(|parent| validate_snowflake(parent) && *parent != channel.channel_id)
                .ok_or_else(|| {
                    PreflightDecision::new(D7STA, false, WHY_CHANNEL_SNAPSHOT_INCOMPLETE)
                })?;
            Some(self.thread_parent_for_permissions(
                guild_id,
                parent_id,
                channel.channel_type,
                now_ms,
            )?)
        } else {
            None
        };
        let permissions = member_permissions_for_channel(
            permissions,
            guild_id,
            &bot_user_id,
            &bot_member.role_ids,
            parent.as_ref().unwrap_or(channel),
        );
        Ok((bot_user_id, permissions))
    }

    pub(super) fn thread_parent_for_permissions(
        &mut self,
        guild_id: &str,
        parent_channel_id: &str,
        thread_type: i32,
        now_ms: u64,
    ) -> Result<ChannelSnapshot, PreflightDecision> {
        let parent =
            self.load_channel_snapshot_for_permissions(guild_id, parent_channel_id, now_ms)?;
        if !matches!(
            (thread_type, parent.channel_type),
            (10, 5) | (11, 0 | 15 | 16) | (12, 0)
        ) {
            return Err(PreflightDecision::new(
                D7STA,
                false,
                WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
            ));
        }
        Ok(parent)
    }

    pub(super) fn channel_snapshot_for_permissions(
        &mut self,
        expected_guild_id: &str,
        channel_id: &str,
        now_ms: u64,
    ) -> Result<ChannelSnapshot, PreflightDecision> {
        let snapshot =
            self.load_channel_snapshot_for_permissions(expected_guild_id, channel_id, now_ms)?;
        let snapshot = self.resolve_private_thread_access(snapshot, now_ms)?;
        if let Some(decision) =
            validate_channel_snapshot_common_decision(&snapshot, expected_guild_id)
        {
            return Err(decision);
        }
        Ok(snapshot)
    }

    pub(super) fn load_channel_snapshot_for_permissions(
        &mut self,
        expected_guild_id: &str,
        channel_id: &str,
        now_ms: u64,
    ) -> Result<ChannelSnapshot, PreflightDecision> {
        let snapshot = match self.gateway.snapshot_mut().get_channel(channel_id, now_ms) {
            SnapshotLookup::Found(snapshot) if snapshot.complete => snapshot,
            SnapshotLookup::Tombstone => {
                return Err(PreflightDecision::new(D7NFD, false, WHY_CHANNEL_NOT_FOUND));
            }
            SnapshotLookup::Found(_) | SnapshotLookup::Missing | SnapshotLookup::Stale(_) => {
                self.fetch_channel_snapshot_for_permissions(expected_guild_id, channel_id, now_ms)?
            }
        };
        if let Some(decision) =
            validate_channel_snapshot_common_decision(&snapshot, expected_guild_id)
        {
            return Err(decision);
        }
        Ok(snapshot)
    }

    pub(super) fn fetch_channel_snapshot_for_permissions(
        &mut self,
        expected_guild_id: &str,
        channel_id: &str,
        now_ms: u64,
    ) -> Result<ChannelSnapshot, PreflightDecision> {
        if !self.rest.can_execute() {
            return Err(PreflightDecision::new(
                D7STA,
                false,
                WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
            ));
        }
        let outcome = self.rest.execute_get_channel(channel_id, now_ms);
        self.observe_rest_outcome(&outcome);
        match outcome.status {
            D7OK => {
                let Some(body) = outcome.response_body.as_deref() else {
                    return Err(PreflightDecision::new(
                        D7STA,
                        false,
                        WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
                    )
                    .with_rest_outcome(&outcome));
                };
                let Some(snapshot) =
                    self.validated_rest_channel(body, expected_guild_id, channel_id, now_ms)
                else {
                    return Err(PreflightDecision::new(
                        D7STA,
                        false,
                        WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
                    )
                    .with_rest_outcome(&outcome));
                };
                self.cache_rest_channel(snapshot.clone());
                Ok(snapshot)
            }
            D7NFD => {
                self.gateway
                    .snapshot_mut()
                    .tombstone_channel(channel_id, now_ms);
                Err(PreflightDecision::new(D7NFD, false, WHY_CHANNEL_NOT_FOUND)
                    .with_rest_outcome(&outcome))
            }
            D7FOR => Err(PreflightDecision::new(D7FOR, false, WHY_MISSING_ACCESS)
                .with_rest_outcome(&outcome)),
            D7ARG => Err(PreflightDecision::new(D7ARG, false, WHY_INVALID_FORM_BODY)
                .with_rest_outcome(&outcome)),
            D7INV => Err(
                PreflightDecision::new(D7INV, false, "INVALID_REQUEST_BUDGET")
                    .with_rest_outcome(&outcome),
            ),
            D7RTL => {
                Err(PreflightDecision::new(D7RTL, false, "RATE_LIMITED")
                    .with_rest_outcome(&outcome))
            }
            _ => Err(
                PreflightDecision::new(D7STA, false, WHY_CHANNEL_SNAPSHOT_INCOMPLETE)
                    .with_rest_outcome(&outcome),
            ),
        }
    }

    pub(super) fn resolve_private_thread_access(
        &mut self,
        mut snapshot: ChannelSnapshot,
        now_ms: u64,
    ) -> Result<ChannelSnapshot, PreflightDecision> {
        if !channel_type_is_private_thread(snapshot.channel_type)
            || snapshot.current_user_thread_joined.is_some()
        {
            return Ok(snapshot);
        }
        if !self.rest.can_execute() {
            return Ok(snapshot);
        }
        if snapshot.archived && snapshot.parent_channel_id.is_some() {
            return self.resolve_archived_private_thread_access(snapshot, now_ms);
        }
        let bot_user_id = self.bot_user_id_decision(now_ms)?;
        let outcome =
            self.rest
                .execute_get_thread_member(&snapshot.channel_id, &bot_user_id, now_ms);
        self.observe_rest_outcome(&outcome);
        match outcome.status {
            D7OK => {
                if !outcome.response_body.as_deref().is_some_and(|body| {
                    parse_json(body).is_ok_and(|member| {
                        thread_member_matches(&member, &snapshot.channel_id, &bot_user_id, true)
                    })
                }) {
                    return Err(private_thread_rest_failure(&outcome));
                }
                snapshot.current_user_thread_joined = Some(true);
                snapshot.current_user_thread_joined_at_ms = Some(now_ms);
                self.gateway.snapshot_mut().set_current_user_thread_joined(
                    &snapshot.channel_id,
                    true,
                    now_ms,
                );
                Ok(snapshot)
            }
            D7NFD => self.refresh_private_thread_membership(&snapshot, now_ms),
            D7FOR => Err(
                PreflightDecision::new(D7FOR, false, WHY_PRIVATE_THREAD_FORBIDDEN)
                    .with_rest_outcome(&outcome),
            ),
            D7ARG => Err(PreflightDecision::new(D7ARG, false, WHY_INVALID_FORM_BODY)
                .with_rest_outcome(&outcome)),
            D7INV => Err(
                PreflightDecision::new(D7INV, false, "INVALID_REQUEST_BUDGET")
                    .with_rest_outcome(&outcome),
            ),
            D7RTL => {
                Err(PreflightDecision::new(D7RTL, false, "RATE_LIMITED")
                    .with_rest_outcome(&outcome))
            }
            _ => Err(
                PreflightDecision::new(D7STA, false, WHY_THREAD_ACCESS_UNKNOWN)
                    .with_rest_outcome(&outcome),
            ),
        }
    }

    pub(super) fn resolve_archived_private_thread_access(
        &mut self,
        snapshot: ChannelSnapshot,
        now_ms: u64,
    ) -> Result<ChannelSnapshot, PreflightDecision> {
        let joined =
            self.private_archive_page(&snapshot, ArchivedThreadKind::JoinedPrivate, None, now_ms)?;
        if let Some(refreshed) = joined.find(&snapshot.channel_id) {
            return Ok(self.store_archived_thread_access(refreshed, true, now_ms));
        }
        let target_id = snapshot
            .channel_id
            .parse::<u64>()
            .map_err(|_| PreflightDecision::new(D7STA, false, WHY_THREAD_ACCESS_UNKNOWN))?;
        if joined.has_more && joined.last_id().is_some_and(|last| last > target_id) {
            let before = target_id
                .checked_add(1)
                .ok_or_else(|| PreflightDecision::new(D7STA, false, WHY_THREAD_ACCESS_UNKNOWN))?;
            let page = self.private_archive_page(
                &snapshot,
                ArchivedThreadKind::JoinedPrivate,
                Some(&before.to_string()),
                now_ms,
            )?;
            if page.threads.len() > 1
                || page
                    .threads
                    .iter()
                    .any(|thread| !thread.channel_id.parse::<u64>().is_ok_and(|id| id < before))
            {
                return Err(PreflightDecision::new(
                    D7STA,
                    false,
                    WHY_THREAD_ACCESS_UNKNOWN,
                ));
            }
            if let Some(refreshed) = page.find(&snapshot.channel_id) {
                return Ok(self.store_archived_thread_access(refreshed, true, now_ms));
            }
        }
        match self.bot_can_manage_threads_for_parent(
            &snapshot.guild_id,
            snapshot
                .parent_channel_id
                .as_deref()
                .ok_or_else(|| PreflightDecision::new(D7STA, false, WHY_THREAD_ACCESS_UNKNOWN))?,
            now_ms,
        )? {
            ManageThreadsAccess::Allowed => {}
            ManageThreadsAccess::Denied => {
                return Ok(self.store_archived_thread_access(snapshot, false, now_ms));
            }
            ManageThreadsAccess::Unknown(decision) => {
                return Err(
                    PreflightDecision::new(D7STA, false, WHY_THREAD_ACCESS_UNKNOWN)
                        .with_preflight_details(&decision),
                );
            }
        }
        let page =
            self.private_archive_page(&snapshot, ArchivedThreadKind::Private, None, now_ms)?;
        if let Some(refreshed) = page.find(&snapshot.channel_id) {
            let joined = refreshed.current_user_thread_joined == Some(true);
            return Ok(self.store_archived_thread_access(refreshed, joined, now_ms));
        }
        self.refresh_private_thread_membership(&snapshot, now_ms)
    }

    pub(super) fn private_archive_page(
        &mut self,
        snapshot: &ChannelSnapshot,
        kind: ArchivedThreadKind,
        before: Option<&str>,
        now_ms: u64,
    ) -> Result<ArchivedThreadPage, PreflightDecision> {
        let parent = snapshot
            .parent_channel_id
            .as_deref()
            .ok_or_else(|| PreflightDecision::new(D7STA, false, WHY_THREAD_ACCESS_UNKNOWN))?;
        let bot_user_id = self.bot_user_id_decision(now_ms)?;
        let outcome = if before.is_some() {
            self.rest
                .execute_get_archived_threads_page(parent, kind, before, 1, now_ms)
        } else {
            match kind {
                ArchivedThreadKind::JoinedPrivate => self
                    .rest
                    .execute_get_joined_private_archived_threads(parent, now_ms),
                ArchivedThreadKind::Private => self
                    .rest
                    .execute_get_archived_private_threads(parent, now_ms),
                ArchivedThreadKind::Public => {
                    return Err(PreflightDecision::new(D7ARG, false, WHY_INVALID_FORM_BODY))
                }
            }
        };
        self.observe_rest_outcome(&outcome);
        if outcome.status == D7OK {
            let page = outcome.response_body.as_deref().and_then(|body| {
                ArchivedThreadPage::parse(
                    body,
                    &snapshot.guild_id,
                    parent,
                    &bot_user_id,
                    kind == ArchivedThreadKind::JoinedPrivate,
                    now_ms,
                )
            });
            if let Some(page) = page {
                for thread in &page.threads {
                    self.cache_rest_channel(thread.clone());
                }
                return Ok(page);
            }
        }
        Err(private_thread_rest_failure(&outcome))
    }

    pub(super) fn refresh_private_thread_membership(
        &mut self,
        snapshot: &ChannelSnapshot,
        now_ms: u64,
    ) -> Result<ChannelSnapshot, PreflightDecision> {
        self.bot_user_id_decision(now_ms)?;
        let outcome = self.rest.execute_get_channel(&snapshot.channel_id, now_ms);
        self.observe_rest_outcome(&outcome);
        if outcome.status == D7OK {
            let refreshed = outcome
                .response_body
                .as_deref()
                .and_then(|body| {
                    self.validated_rest_channel(
                        body,
                        &snapshot.guild_id,
                        &snapshot.channel_id,
                        now_ms,
                    )
                })
                .filter(|channel| {
                    channel.complete
                        && channel.channel_type == 12
                        && channel.parent_channel_id == snapshot.parent_channel_id
                });
            if let Some(refreshed) = refreshed {
                let joined = refreshed.current_user_thread_joined == Some(true);
                return Ok(self.store_archived_thread_access(refreshed, joined, now_ms));
            }
        }
        if outcome.status == D7NFD {
            self.gateway
                .snapshot_mut()
                .tombstone_channel(&snapshot.channel_id, now_ms);
        }
        Err(private_thread_rest_failure(&outcome))
    }

    pub(super) fn store_archived_thread_access(
        &mut self,
        mut snapshot: ChannelSnapshot,
        joined: bool,
        now_ms: u64,
    ) -> ChannelSnapshot {
        snapshot.current_user_thread_joined = Some(joined);
        snapshot.current_user_thread_joined_at_ms = Some(now_ms);
        self.gateway.snapshot_mut().upsert_channel(snapshot.clone());
        snapshot
    }

    pub(super) fn bot_can_manage_threads_for_parent(
        &mut self,
        guild_id: &str,
        parent_channel_id: &str,
        now_ms: u64,
    ) -> Result<ManageThreadsAccess, PreflightDecision> {
        let parent =
            match self.thread_parent_for_permissions(guild_id, parent_channel_id, 12, now_ms) {
                Ok(parent) => parent,
                Err(decision) if decision.status == D7STA => {
                    return Ok(ManageThreadsAccess::Unknown(decision));
                }
                Err(decision) => return Err(decision),
            };
        let (_, permissions) = match self.bot_permissions_for_channel(&parent, now_ms) {
            Ok(resolved) => resolved,
            Err(decision) if decision.status == D7STA => {
                return Ok(ManageThreadsAccess::Unknown(decision));
            }
            Err(decision) => return Err(decision),
        };
        if permissions.administrator() || permissions.permissions.has(BIT_MANAGE_THREADS) {
            Ok(ManageThreadsAccess::Allowed)
        } else {
            Ok(ManageThreadsAccess::Denied)
        }
    }

    pub(super) fn delete_action_precheck(
        &mut self,
        channel_id: &str,
        message_id: &str,
        now_ms: u64,
    ) -> Option<PreflightDecision> {
        match self
            .gateway
            .snapshot_mut()
            .get_message(channel_id, message_id, now_ms)
        {
            SnapshotLookup::Found(snapshot) => {
                let decision =
                    self.delete_with_message_snapshot(&snapshot, "", channel_id, now_ms, false);
                if decision.can {
                    None
                } else if decision.status == D7OK {
                    Some(PreflightDecision::new(D7FOR, false, decision.why))
                } else {
                    Some(decision)
                }
            }
            SnapshotLookup::Tombstone => {
                Some(PreflightDecision::new(D7NFD, false, WHY_MESSAGE_NOT_FOUND))
            }
            SnapshotLookup::Missing | SnapshotLookup::Stale(_) => {
                let decision =
                    self.fetch_message_snapshot_decision("", channel_id, message_id, now_ms);
                if decision.can {
                    None
                } else {
                    Some(action_decision_from_preflight(decision))
                }
            }
        }
    }

    pub(super) fn fetch_message_snapshot_decision(
        &mut self,
        guild_id: &str,
        channel_id: &str,
        message_id: &str,
        now_ms: u64,
    ) -> PreflightDecision {
        if !self.rest.can_execute() {
            return PreflightDecision::new(D7STA, false, WHY_MESSAGE_SNAPSHOT_INCOMPLETE);
        }
        let outcome = self
            .rest
            .execute_get_message(channel_id, message_id, now_ms);
        self.observe_rest_outcome(&outcome);
        match outcome.status {
            D7OK => {
                let Some(body) = outcome.response_body.as_deref() else {
                    return PreflightDecision::new(D7STA, false, WHY_MESSAGE_SNAPSHOT_INCOMPLETE)
                        .with_rest_outcome(&outcome);
                };
                let Some((snapshot, origin, revision)) =
                    message_snapshot_from_rest_body(body, guild_id, channel_id, now_ms)
                else {
                    return PreflightDecision::new(D7STA, false, WHY_MESSAGE_SNAPSHOT_INCOMPLETE)
                        .with_rest_outcome(&outcome);
                };
                if snapshot.message_id != message_id {
                    return PreflightDecision::new(D7STA, false, WHY_MESSAGE_SNAPSHOT_INCOMPLETE)
                        .with_rest_outcome(&outcome);
                }
                let Some(snapshot) = self
                    .gateway
                    .snapshot_mut()
                    .cache_rest_message(snapshot, origin, revision, now_ms)
                else {
                    return PreflightDecision::new(D7STA, false, WHY_MESSAGE_SNAPSHOT_INCOMPLETE)
                        .with_rest_outcome(&outcome);
                };
                self.delete_with_message_snapshot(&snapshot, guild_id, channel_id, now_ms, true)
            }
            D7NFD => {
                self.gateway
                    .snapshot_mut()
                    .tombstone_message(channel_id, message_id, now_ms);
                PreflightDecision::new(D7NFD, false, WHY_MESSAGE_NOT_FOUND)
                    .with_rest_outcome(&outcome)
            }
            D7FOR => PreflightDecision::new(D7FOR, false, WHY_MESSAGE_FETCH_FORBIDDEN)
                .with_rest_outcome(&outcome),
            D7ARG => PreflightDecision::new(D7ARG, false, WHY_INVALID_FORM_BODY)
                .with_rest_outcome(&outcome),
            D7INV => PreflightDecision::new(D7INV, false, "INVALID_REQUEST_BUDGET")
                .with_rest_outcome(&outcome),
            D7RTL => {
                PreflightDecision::new(D7RTL, false, "RATE_LIMITED").with_rest_outcome(&outcome)
            }
            _ => PreflightDecision::new(D7STA, false, WHY_MESSAGE_SNAPSHOT_INCOMPLETE)
                .with_rest_outcome(&outcome),
        }
    }

    pub(super) fn preflight_decision(
        &mut self,
        id: u64,
        status: i32,
        can: bool,
        why: &'static str,
    ) -> Response {
        self.preflight_response(id, PreflightDecision::new(status, can, why))
    }

    pub(super) fn preflight_response(&mut self, id: u64, decision: PreflightDecision) -> Response {
        let status = decision.status;
        let why = decision.why;
        if status == D7OK {
            self.errors.act.clear();
        } else {
            self.errors.act.set(
                status,
                decision.http_status,
                decision.discord_code,
                why,
                why,
            );
        }
        Response {
            id,
            status,
            fields: vec![
                Field::raw("can", if decision.can { "1" } else { "0" }),
                Field::raw("why", why),
            ],
        }
    }

    pub(super) fn require_id(&self, request: &Request, key: &str) -> i32 {
        match request.field(key) {
            Some(value) if validate_snowflake(value) => D7OK,
            _ => D7ARG,
        }
    }

    pub(super) fn invalidate_event_permissions(
        &mut self,
        guild_id: Option<&str>,
        user_id: Option<&str>,
    ) {
        self.invalidate_event_permissions_matching(|event| {
            guild_id.is_none_or(|guild_id| event.guild_id == guild_id)
                && user_id.is_none_or(|user_id| event.user_id == user_id)
        });
    }

    pub(super) fn invalidate_event_permissions_for_members(
        &mut self,
        guild_id: &str,
        user_ids: &[String],
    ) {
        if user_ids.is_empty() {
            return;
        }
        let users = user_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        self.invalidate_event_permissions_matching(|event| {
            event.guild_id == guild_id && users.contains(event.user_id.as_str())
        });
    }

    fn invalidate_event_permissions_matching(&mut self, matches: impl Fn(&Event) -> bool) {
        let invalidate = |event: &mut Event| {
            if matches(event) {
                event.member_permissions = None;
            }
        };
        if let Some(event) = &mut self.current_event {
            invalidate(event);
        }
        if self.events.shrink_normal_events(invalidate) {
            self.gateway.snapshot_mut().mark_gap();
            self.last_message_search.clear();
            if let Some(event) = &mut self.current_event {
                event.member_permissions = None;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PreflightDecision {
    pub(super) status: i32,
    pub(super) can: bool,
    pub(super) why: &'static str,
    pub(super) http_status: i32,
    pub(super) discord_code: i32,
}

impl PreflightDecision {
    pub(super) fn new(status: i32, can: bool, why: &'static str) -> Self {
        Self {
            status,
            can,
            why,
            http_status: 0,
            discord_code: 0,
        }
    }

    pub(super) fn with_rest_outcome(mut self, outcome: &RestOutcome) -> Self {
        self.http_status = outcome.http_status;
        self.discord_code = outcome.discord_error_code;
        self
    }

    pub(super) fn with_preflight_details(mut self, decision: &PreflightDecision) -> Self {
        self.http_status = decision.http_status;
        self.discord_code = decision.discord_code;
        self
    }
}

pub(super) fn rest_outcome_from_preflight(decision: PreflightDecision) -> RestOutcome {
    let mut outcome = RestOutcome::not_sent(decision.status, decision.why);
    outcome.http_status = decision.http_status;
    outcome.discord_error_code = decision.discord_code;
    outcome
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SendDecisionMode {
    Preflight,
    Action,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MessageDeletePolicy {
    Deletable,
    NotDeletable,
    Unknown,
}

pub(super) enum ManageThreadsAccess {
    Allowed,
    Denied,
    Unknown(PreflightDecision),
}

pub(super) fn delete_snapshot_decision(snapshot: &MessageSnapshot) -> PreflightDecision {
    if !snapshot.complete {
        return PreflightDecision::new(D7STA, false, WHY_MESSAGE_SNAPSHOT_INCOMPLETE);
    }
    match message_delete_policy(snapshot.message_type) {
        MessageDeletePolicy::Deletable => PreflightDecision::new(D7OK, true, WHY_OK),
        MessageDeletePolicy::NotDeletable => {
            PreflightDecision::new(D7OK, false, WHY_MESSAGE_TYPE_NOT_DELETABLE)
        }
        MessageDeletePolicy::Unknown => {
            PreflightDecision::new(D7STA, false, WHY_UNKNOWN_MESSAGE_TYPE)
        }
    }
}

pub(super) fn send_channel_non_access_block_decision(
    snapshot: &ChannelSnapshot,
    expected_guild_id: &str,
    mode: SendDecisionMode,
) -> Option<PreflightDecision> {
    if snapshot.obfuscated {
        return Some(PreflightDecision::new(D7OK, false, "MISSING_VIEW_CHANNEL"));
    }
    if !snapshot.complete || !expected_guild_id.is_empty() && snapshot.guild_id != expected_guild_id
    {
        return Some(PreflightDecision::new(
            D7STA,
            false,
            WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
        ));
    }
    if mode == SendDecisionMode::Action {
        if channel_type_is_forum_parent(snapshot.channel_type) {
            return Some(PreflightDecision::new(
                D7FOR,
                false,
                WHY_CANNOT_SEND_TO_FORUM_PARENT,
            ));
        }
        if channel_type_is_media_parent(snapshot.channel_type) {
            return Some(PreflightDecision::new(
                D7FOR,
                false,
                WHY_CANNOT_SEND_TO_MEDIA_PARENT,
            ));
        }
    }
    if !channel_type_supports_message_send(snapshot.channel_type) {
        return Some(PreflightDecision::new(
            D7OK,
            false,
            WHY_CHANNEL_TYPE_UNSUPPORTED,
        ));
    }
    if snapshot.archived {
        return Some(PreflightDecision::new(D7OK, false, WHY_THREAD_ARCHIVED));
    }
    if snapshot.locked {
        return Some(PreflightDecision::new(D7OK, false, WHY_THREAD_LOCKED));
    }
    None
}

pub(super) fn private_thread_rest_failure(outcome: &RestOutcome) -> PreflightDecision {
    let (status, why) = match outcome.status {
        D7NFD => (D7NFD, WHY_CHANNEL_NOT_FOUND),
        D7FOR => (D7FOR, WHY_PRIVATE_THREAD_FORBIDDEN),
        D7ARG => (D7ARG, WHY_INVALID_FORM_BODY),
        D7INV => (D7INV, "INVALID_REQUEST_BUDGET"),
        D7RTL => (D7RTL, "RATE_LIMITED"),
        _ => (D7STA, WHY_THREAD_ACCESS_UNKNOWN),
    };
    PreflightDecision::new(status, false, why).with_rest_outcome(outcome)
}

pub(super) fn private_thread_access_block_decision(
    snapshot: &ChannelSnapshot,
    permissions: &MemberPermissions,
    deleting: bool,
) -> Option<PreflightDecision> {
    if !channel_type_is_private_thread(snapshot.channel_type)
        || permissions.timed_out
        || permissions.administrator()
        || permissions.permissions.has(BIT_MANAGE_THREADS)
    {
        return None;
    }
    match snapshot.current_user_thread_joined {
        Some(true) => None,
        Some(false) if deleting => Some(PreflightDecision::new(
            D7FOR,
            false,
            WHY_PRIVATE_THREAD_FORBIDDEN,
        )),
        Some(false) => Some(PreflightDecision::new(D7OK, false, WHY_THREAD_NOT_JOINED)),
        None => Some(PreflightDecision::new(
            D7STA,
            false,
            WHY_THREAD_ACCESS_UNKNOWN,
        )),
    }
}

pub(super) fn validate_channel_snapshot_common_decision(
    snapshot: &ChannelSnapshot,
    expected_guild_id: &str,
) -> Option<PreflightDecision> {
    if snapshot.obfuscated {
        return Some(PreflightDecision::new(D7FOR, false, "MISSING_VIEW_CHANNEL"));
    }
    if !snapshot.complete {
        return Some(PreflightDecision::new(
            D7STA,
            false,
            WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
        ));
    }
    if !expected_guild_id.is_empty() && snapshot.guild_id != expected_guild_id {
        return Some(PreflightDecision::new(
            D7STA,
            false,
            WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
        ));
    }
    None
}

pub(super) fn member_permissions_for_channel(
    mut permissions: MemberPermissions,
    guild_id: &str,
    user_id: &str,
    role_ids: &[String],
    channel: &ChannelSnapshot,
) -> MemberPermissions {
    if !permissions.administrator() {
        apply_channel_permission_overwrites(
            &mut permissions.permissions,
            guild_id,
            user_id,
            role_ids,
            &channel.permission_overwrites,
        );
    }
    permissions
}

pub(super) fn member_permissions_from_roles(
    guild_id: &str,
    member: &MemberSnapshot,
    roles: &GuildRolesSnapshot,
    owner_id: Option<&str>,
) -> Option<MemberPermissions> {
    if !member.complete
        || !roles.complete
        || member.guild_id != guild_id
        || roles.guild_id != guild_id
    {
        return None;
    }
    let mut indexed_roles = std::collections::HashMap::with_capacity(roles.roles.len());
    for role in &roles.roles {
        if role.guild_id != guild_id || indexed_roles.insert(role.role_id.as_str(), role).is_some()
        {
            return None;
        }
    }
    let mut permissions = PermissionSet::zero();
    let everyone = indexed_roles.get(guild_id)?;
    permissions.union_with(&everyone.permissions);
    let mut top_role_position = 0;
    for role_id in &member.role_ids {
        let role = indexed_roles.get(role_id.as_str())?;
        permissions.union_with(&role.permissions);
        top_role_position = top_role_position.max(role.position);
    }
    let mut member_permissions = MemberPermissions::new(permissions);
    member_permissions.is_owner = owner_id.is_some_and(|owner_id| owner_id == member.user_id);
    member_permissions.timed_out = member.timed_out;
    member_permissions.top_role_position = top_role_position;
    Some(member_permissions)
}

pub(super) fn apply_channel_permission_overwrites(
    permissions: &mut PermissionSet,
    guild_id: &str,
    user_id: &str,
    role_ids: &[String],
    overwrites: &[ChannelPermissionOverwrite],
) {
    if overwrites.is_empty() {
        return;
    }
    let guild_permissions = permissions.clone();
    if let Some(overwrite) = overwrites
        .iter()
        .find(|overwrite| overwrite.kind == 0 && overwrite.overwrite_id == guild_id)
    {
        permissions.remove_all(&overwrite.deny);
        permissions.union_with(&overwrite.allow);
    }
    let mut role_allow = PermissionSet::zero();
    let mut role_deny = PermissionSet::zero();
    let role_ids = role_ids
        .iter()
        .map(String::as_str)
        .filter(|role_id| *role_id != guild_id)
        .collect::<std::collections::HashSet<_>>();
    for overwrite in overwrites.iter().filter(|overwrite| {
        overwrite.kind == 0 && role_ids.contains(overwrite.overwrite_id.as_str())
    }) {
        role_allow.union_with(&overwrite.allow);
        role_deny.union_with(&overwrite.deny);
    }
    permissions.remove_all(&role_deny);
    permissions.union_with(&role_allow);
    if let Some(overwrite) = overwrites
        .iter()
        .find(|overwrite| overwrite.kind == 1 && overwrite.overwrite_id == user_id)
    {
        permissions.remove_all(&overwrite.deny);
        permissions.union_with(&overwrite.allow);
    }
    for &bit in GUILD_ONLY_PERMISSION_BITS {
        if guild_permissions.has(bit) {
            permissions.set(bit);
        } else {
            permissions.unset(bit);
        }
    }
}

pub(super) fn action_decision_from_preflight(decision: PreflightDecision) -> PreflightDecision {
    if decision.status == D7OK && !decision.can {
        PreflightDecision::new(D7FOR, false, decision.why)
    } else {
        decision
    }
}

pub(super) fn moderation_action_precheck(decision: PreflightDecision) -> Option<PreflightDecision> {
    let decision = action_decision_from_preflight(decision);
    if decision.status == D7OK && decision.can {
        None
    } else {
        Some(decision)
    }
}

pub(super) fn channel_type_supports_message_send(channel_type: i32) -> bool {
    matches!(channel_type, 0 | 1 | 5 | 10 | 11 | 12)
}

pub(super) fn channel_type_is_forum_parent(channel_type: i32) -> bool {
    channel_type == 15
}

pub(super) fn channel_type_is_media_parent(channel_type: i32) -> bool {
    channel_type == 16
}

pub(super) fn channel_type_is_thread(channel_type: i32) -> bool {
    matches!(channel_type, 10..=12)
}

pub(super) fn channel_type_is_private_thread(channel_type: i32) -> bool {
    channel_type == 12
}

pub(super) fn message_delete_policy(message_type: i32) -> MessageDeletePolicy {
    match message_type {
        0 | 6..=12 | 14..=20 | 22..=29 | 31 | 32 | 36..=39 | 44 | 46 => {
            MessageDeletePolicy::Deletable
        }
        1..=5 | 21 => MessageDeletePolicy::NotDeletable,
        _ => MessageDeletePolicy::Unknown,
    }
}

pub(super) fn message_type_requires_manage_messages(message_type: i32) -> bool {
    message_type == 24
}

#[cfg(test)]
pub(super) fn channel_snapshot_from_rest_body(
    body: &str,
    fallback_guild_id: &str,
    fallback_channel_id: &str,
    now_ms: u64,
) -> Option<ChannelSnapshot> {
    let root = parse_json(body).ok()?;
    channel_snapshot_from_json(&root, fallback_guild_id, fallback_channel_id, now_ms)
}

pub(super) fn channel_snapshot_from_json(
    root: &JsonValue,
    fallback_guild_id: &str,
    fallback_channel_id: &str,
    now_ms: u64,
) -> Option<ChannelSnapshot> {
    let snapshot =
        crate::snapshot::channel::full(root, fallback_guild_id, fallback_channel_id, now_ms, true)?;
    if !fallback_channel_id.is_empty() && snapshot.channel_id != fallback_channel_id
        || !fallback_guild_id.is_empty() && snapshot.guild_id != fallback_guild_id
    {
        return None;
    }
    Some(snapshot)
}

pub(super) fn guild_snapshot_from_rest_body(
    body: &str,
    guild_id: &str,
    now_ms: u64,
) -> Option<GuildSnapshot> {
    let value = parse_json(body).ok()?;
    let snapshot = crate::snapshot::parse::guild(&value, now_ms)?;
    (snapshot.guild_id == guild_id && snapshot.owner_id.is_some()).then_some(snapshot)
}

pub(super) fn member_snapshot_from_rest_body(
    body: &str,
    fallback_guild_id: &str,
    fallback_user_id: &str,
    now_ms: u64,
) -> Option<MemberSnapshot> {
    let root = parse_json(body).ok()?;
    let user_id = match root.get("user") {
        None => fallback_user_id,
        Some(user) => json_str(user, "id")?,
    };
    if !validate_snowflake(user_id)
        || user_id != fallback_user_id
        || !validate_snowflake(fallback_guild_id)
        || root
            .get("guild_id")
            .is_some_and(|guild| guild.as_str() != Some(fallback_guild_id))
    {
        return None;
    }
    let role_ids = crate::snapshot::parse::snowflakes(root.get("roles")?)?;
    let mut snapshot = MemberSnapshot::new(fallback_guild_id, user_id, role_ids, now_ms);
    crate::snapshot::member::timeout(&root)?.apply(&mut snapshot, now_ms);
    snapshot.complete = true;
    Some(snapshot)
}

pub(super) fn guild_roles_snapshot_from_rest_body(
    body: &str,
    guild_id: &str,
    now_ms: u64,
) -> Option<GuildRolesSnapshot> {
    let root = parse_json(body).ok()?;
    let mut seen = std::collections::HashSet::new();
    let roles = root
        .as_array()?
        .iter()
        .map(|value| {
            let role = crate::snapshot::parse::role(value, guild_id)?;
            seen.insert(role.role_id.clone()).then_some(role)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(GuildRolesSnapshot::new(guild_id, roles, now_ms))
}

pub(super) fn current_user_id_from_rest_body(body: &str) -> Option<String> {
    let root = parse_json(body).ok()?;
    let user_id = json_str(&root, "id")?;
    if validate_snowflake(user_id) {
        Some(user_id.to_string())
    } else {
        None
    }
}

pub(super) fn message_snapshot_from_rest_body(
    body: &str,
    fallback_guild_id: &str,
    fallback_channel_id: &str,
    now_ms: u64,
) -> Option<(
    MessageSnapshot,
    crate::snapshot::webhook::MessageOrigin,
    Option<crate::snapshot::revision::MessageRevision>,
)> {
    let data = parse_json(body).ok()?;
    let snapshot =
        crate::snapshot::message::full(&data, fallback_guild_id, fallback_channel_id, now_ms)?;
    Some((
        snapshot,
        crate::snapshot::webhook::MessageOrigin::from_data(&data, true)?,
        crate::snapshot::revision::MessageRevision::parse(&data)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::CommitState;

    fn call(runtime: &mut DforRuntime, verb: &str, fields: Vec<Field>) -> Response {
        runtime.handle(Request {
            id: 100,
            verb: verb.to_string(),
            fields,
        })
    }

    fn permission_runtime(backend: MockRestBackend, author_admin: bool) -> DforRuntime {
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        let now = now_ms_u64();
        runtime.bot_user_id_cache = Some("99".into());
        let guild = r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}],"members":[{"user":{"id":"99"},"roles":["50"],"communication_disabled_until":null},{"user":{"id":"40"},"roles":AUTHOR_ROLES,"communication_disabled_until":null},{"user":{"id":"41"},"roles":["50"],"communication_disabled_until":null}]}}"#.replace("AUTHOR_ROLES", if author_admin { r#"["50"]"# } else { "[]" });
        runtime.ingest_gateway_json(&guild, now).unwrap();
        let message = r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":AUTHOR_PERMISSIONS}}}"#.replace("AUTHOR_PERMISSIONS", if author_admin { r#""8""# } else { r#""0""# });
        runtime.ingest_gateway_json(&message, now).unwrap();
        call(&mut runtime, "INI", vec![]);
        call(&mut runtime, "NXT", vec![]);
        call(&mut runtime, "NXT", vec![]);
        runtime
            .gateway
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new("11", "40", vec![], now));
        for (guild, channel) in [("10", "21"), ("11", "22")] {
            let mut snapshot = ChannelSnapshot::new(guild, channel, 12, now);
            snapshot.current_user_thread_joined = Some(true);
            snapshot.current_user_thread_joined_at_ms = Some(now);
            runtime.gateway.snapshot_mut().upsert_channel(snapshot);
        }
        runtime
    }

    fn channel_permission_runtime(backend: MockRestBackend, thread: bool) -> DforRuntime {
        let mut runtime = permission_runtime(backend, false);
        let now = now_ms_u64();
        runtime
            .gateway
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new("10", "99", vec![], now));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_guild_roles(GuildRolesSnapshot::new(
                "10",
                vec![crate::snapshot::RoleSnapshot {
                    guild_id: "10".into(),
                    role_id: "10".into(),
                    permissions: PermissionSet::parse_decimal(if thread {
                        "274877910016"
                    } else {
                        "3072"
                    })
                    .unwrap(),
                    position: 0,
                }],
                now,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("10", "20", 0, now));
        runtime
    }

    fn mutation(method: HttpMethod, path: &str, state: CommitState) -> RestOutcome {
        let mut outcome = RestPlanner::new().raw_json_request(method, path, Some("{}"), "");
        assert!(outcome.request.is_some());
        outcome.commit_state = state;
        outcome
    }

    fn resumed_runtime_without_guild(backend: MockRestBackend) -> DforRuntime {
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        let now = now_ms_u64();
        runtime
            .restore_gateway_resume_session(
                GatewayResumeSession::new("saved", "wss://resume", 10, Some("99".into())).unwrap(),
            )
            .unwrap();
        runtime
            .ingest_gateway_json(r#"{"op":0,"s":11,"t":"RESUMED","d":{}}"#, now)
            .unwrap();
        let store = runtime.gateway.snapshot_mut();
        store.upsert_member(MemberSnapshot::new("10", "99", vec!["50".into()], now));
        store.upsert_member(MemberSnapshot::new("10", "40", vec![], now));
        store.upsert_guild_roles(GuildRolesSnapshot::new(
            "10",
            vec![
                crate::snapshot::RoleSnapshot {
                    guild_id: "10".into(),
                    role_id: "10".into(),
                    permissions: PermissionSet::zero(),
                    position: 0,
                },
                crate::snapshot::RoleSnapshot {
                    guild_id: "10".into(),
                    role_id: "50".into(),
                    permissions: PermissionSet::from_bits(&[40]),
                    position: 10,
                },
            ],
            now,
        ));
        call(&mut runtime, "INI", vec![]);
        runtime
    }

    #[test]
    fn ownership_refresh_drops_only_affected_current_and_queued_administrator_hints() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"id":"10","owner_id":"70"}"#.into();
        backend.push_response(response);
        let mut runtime = permission_runtime(backend, false);
        let now = now_ms_u64();
        let mut guild = GuildSnapshot::new("10", false, now);
        guild.owner_id = Some("40".into());
        runtime.gateway.snapshot_mut().upsert_guild(guild.clone());
        runtime.current_event.as_mut().unwrap().member_permissions =
            Some(PermissionSet::from_bits(&[3]));
        runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"id":"31","guild_id":"10","channel_id":"20","type":0,"author":{"id":"40"},"member":{"permissions":"8"}}}"#, now).unwrap();
        runtime.ingest_gateway_json(r#"{"op":0,"s":4,"t":"MESSAGE_CREATE","d":{"id":"32","guild_id":"11","channel_id":"22","type":0,"author":{"id":"41"},"member":{"permissions":"8"}}}"#, now).unwrap();
        assert_eq!(
            call(&mut runtime, "CAD", vec![]).field("is_admin"),
            Some("1")
        );
        guild.complete = false;
        runtime.gateway.snapshot_mut().upsert_guild(guild);
        assert_eq!(
            runtime.guild_owner_id_decision("10", now),
            Ok(Some("70".into()))
        );
        assert!(runtime
            .current_event
            .as_ref()
            .unwrap()
            .member_permissions
            .is_none());
        assert_eq!(
            call(&mut runtime, "CAD", vec![]).field("is_admin"),
            Some("0")
        );
        assert_eq!(
            call(&mut runtime, "NXT", vec![]).field("event_type"),
            Some("101")
        );
        assert_eq!(
            call(&mut runtime, "CAD", vec![]).field("is_admin"),
            Some("0")
        );
        assert_eq!(
            call(&mut runtime, "NXT", vec![]).field("event_type"),
            Some("101")
        );
        assert_eq!(
            call(&mut runtime, "CAD", vec![]).field("is_admin"),
            Some("1")
        );
        let RuntimeRest::Mock(executor) = &runtime.rest else {
            panic!("expected mock REST")
        };
        assert_eq!(executor.backend.requests().len(), 1);
    }

    #[test]
    fn a_missing_guild_cannot_reuse_administrator_hints_when_negative_caching_is_disabled() {
        for roles in [false, true] {
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::discord_error(404, 10004));
            backend.push_response(crate::rest::RestResponse::discord_error(403, 50001));
            let mut runtime = permission_runtime(backend, false);
            *runtime.gateway.snapshot_mut() = crate::snapshot::SnapshotStore::new(1000, 0, 10, 10);
            runtime.current_event.as_mut().unwrap().member_permissions =
                Some(PermissionSet::from_bits(&[3]));
            let missing = if roles {
                runtime
                    .guild_roles_decision("10", now_ms_u64())
                    .unwrap_err()
            } else {
                runtime
                    .guild_owner_id_decision("10", now_ms_u64())
                    .unwrap_err()
            };
            assert_eq!(missing.status, D7NFD);
            assert_eq!(
                runtime.gateway.snapshot_mut().get_guild("10", now_ms_u64()),
                SnapshotLookup::Missing
            );
            let response = call(&mut runtime, "CAD", vec![]);
            assert_eq!(response.status, D7FOR);
            assert_ne!(response.field("is_admin"), Some("1"));
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(executor.backend.requests().len(), 2);
            assert_eq!(
                executor.backend.requests()[0].path,
                if roles {
                    "/api/v10/guilds/10/roles"
                } else {
                    "/api/v10/guilds/10"
                }
            );
            assert_eq!(executor.backend.requests()[1].path, "/api/v10/guilds/10");
        }
    }

    #[test]
    fn removed_owners_cannot_reuse_cached_ownership_when_negative_caching_is_disabled() {
        for user_missing in [false, true] {
            let mut backend = MockRestBackend::default();
            if user_missing {
                backend.push_response(crate::rest::RestResponse::discord_error(404, 10013));
            }
            for _ in 0..2 {
                backend.push_response(crate::rest::RestResponse::discord_error(403, 50001));
            }
            let mut runtime = permission_runtime(backend, false);
            *runtime.gateway.snapshot_mut() = crate::snapshot::SnapshotStore::new(1000, 0, 10, 10);
            let now = now_ms_u64();
            let mut guild = GuildSnapshot::new("10", false, now);
            guild.owner_id = Some("40".into());
            runtime.gateway.snapshot_mut().upsert_guild(guild);
            runtime.current_event.as_mut().unwrap().member_permissions =
                Some(PermissionSet::from_bits(&[3]));
            assert_eq!(
                call(&mut runtime, "CAD", vec![]).field("is_admin"),
                Some("1")
            );
            if user_missing {
                assert_eq!(
                    runtime
                        .fetch_member_snapshot_decision("10", "40", now, false)
                        .unwrap_err()
                        .status,
                    D7NFD
                );
            } else {
                runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"GUILD_MEMBER_REMOVE","d":{"guild_id":"10","user":{"id":"40"}}}"#, now).unwrap();
            }
            for verb in ["ADM", "CAD"] {
                let response = call(
                    &mut runtime,
                    verb,
                    if verb == "ADM" {
                        vec![Field::raw("guild_id", "10"), Field::raw("user_id", "40")]
                    } else {
                        vec![]
                    },
                );
                assert_eq!(
                    response.status, D7FOR,
                    "user_missing={user_missing} verb={verb}"
                );
                assert_ne!(response.field("is_admin"), Some("1"));
            }
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(
                executor.backend.requests().len(),
                2 + usize::from(user_missing)
            );
            assert!(executor
                .backend
                .requests()
                .iter()
                .all(|request| request.method == HttpMethod::Get
                    && request.path == "/api/v10/guilds/10/members/40"));
        }
    }

    #[test]
    fn fresh_owner_identity_needs_no_member_or_role_fetch_and_respects_tombstones() {
        for removed in 0..3 {
            let mut runtime =
                DforRuntime::with_mock_rest_backend(false, false, MockRestBackend::default());
            let now = now_ms_u64();
            runtime
                .ingest_gateway_json(
                    r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"40"}}"#,
                    now,
                )
                .unwrap();
            runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"id":"30","guild_id":"10","channel_id":"20","type":0,"author":{"id":"40"},"member":{"permissions":"0"}}}"#, now).unwrap();
            call(&mut runtime, "INI", vec![]);
            for _ in 0..4 {
                if call(&mut runtime, "NXT", vec![]).field("event_type") == Some("101") {
                    break;
                }
            }
            if removed == 1 {
                runtime
                    .gateway
                    .snapshot_mut()
                    .tombstone_member("10", "40", now);
            } else if removed == 2 {
                runtime.gateway.snapshot_mut().tombstone_user("40", now);
            }
            for verb in ["ADM", "CAD"] {
                let response = call(
                    &mut runtime,
                    verb,
                    if verb == "ADM" {
                        vec![Field::raw("guild_id", "10"), Field::raw("user_id", "40")]
                    } else {
                        vec![]
                    },
                );
                assert_eq!(
                    response.status,
                    if removed == 0 { D7OK } else { D7NFD },
                    "removed={removed} verb={verb}"
                );
                assert_eq!(response.field("is_admin"), (removed == 0).then_some("1"));
            }
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert!(executor.backend.requests().is_empty());
        }
    }

    #[test]
    fn missing_or_ownerless_guilds_fetch_and_cache_authoritative_ownership() {
        for context in 0..4 {
            let mut backend = MockRestBackend::default();
            let mut response = crate::rest::RestResponse::ok();
            response.body = r#"{"id":"10","owner_id":"40"}"#.into();
            backend.push_response(response);
            let mut runtime = resumed_runtime_without_guild(backend);
            let now = now_ms_u64();
            if context != 0 {
                let mut guild = GuildSnapshot::new("10", false, if context == 2 { 0 } else { now });
                guild.complete = context != 3;
                runtime.gateway.snapshot_mut().upsert_guild(guild);
            }
            assert_eq!(
                runtime.guild_owner_id_decision("10", now),
                Ok(Some("40".into())),
                "context={context}"
            );
            assert_eq!(
                runtime.guild_owner_id_decision("10", now),
                Ok(Some("40".into()))
            );
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(executor.backend.requests().len(), 1);
            assert_eq!(executor.backend.requests()[0].path, "/api/v10/guilds/10");
        }
    }

    #[test]
    fn resumed_owner_messages_remain_administrative_without_guild_create() {
        for owner in ["40", "70"] {
            let mut backend = MockRestBackend::default();
            let mut response = crate::rest::RestResponse::ok();
            response.body = format!(r#"{{"id":"10","owner_id":"{owner}"}}"#);
            backend.push_response(response);
            let mut runtime = resumed_runtime_without_guild(backend);
            runtime.ingest_gateway_json(r#"{"op":0,"s":12,"t":"MESSAGE_CREATE","d":{"id":"30","guild_id":"10","channel_id":"20","type":0,"author":{"id":"40"},"member":{"permissions":"0"}}}"#, now_ms_u64()).unwrap();
            for _ in 0..4 {
                if call(&mut runtime, "NXT", vec![]).field("event_type") == Some("101") {
                    break;
                }
            }
            for _ in 0..2 {
                let response = call(&mut runtime, "CAD", vec![]);
                assert_eq!(response.status, D7OK);
                assert_eq!(
                    response.field("is_admin"),
                    Some(if owner == "40" { "1" } else { "0" })
                );
            }
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(executor.backend.requests().len(), 1);
            assert_eq!(executor.backend.requests()[0].path, "/api/v10/guilds/10");
        }
    }

    #[test]
    fn resumed_guild_owners_are_protected_from_timeout_without_guild_create() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"id":"10","owner_id":"40"}"#.into();
        backend.push_response(response);
        let mut runtime = resumed_runtime_without_guild(backend);
        let decision = runtime.timeout_preflight_decision("10", "40", now_ms_u64());
        assert_eq!(decision.status, D7OK);
        assert!(!decision.can);
        assert_eq!(decision.why, WHY_TARGET_IS_OWNER);
        let response = call(
            &mut runtime,
            "TMO",
            vec![
                Field::raw("guild_id", "10"),
                Field::raw("user_id", "40"),
                Field::raw("seconds", "60"),
            ],
        );
        assert_eq!(response.status, D7FOR);
        let RuntimeRest::Mock(executor) = &runtime.rest else {
            panic!("expected mock REST")
        };
        assert_eq!(executor.backend.requests().len(), 1);
        assert_eq!(executor.backend.requests()[0].method, HttpMethod::Get);
        assert_eq!(executor.backend.requests()[0].path, "/api/v10/guilds/10");
    }

    #[test]
    fn missing_ownership_errors_cannot_authorise_timeout_or_hide_rest_details() {
        for (http, code, body, status, why) in [
            (
                200,
                0,
                r#"{"id":"11","owner_id":"40"}"#,
                D7STA,
                WHY_GUILD_SNAPSHOT_INCOMPLETE,
            ),
            (
                200,
                0,
                r#"{"id":"10"}"#,
                D7STA,
                WHY_GUILD_SNAPSHOT_INCOMPLETE,
            ),
            (
                200,
                0,
                r#"{"id":"10","owner_id":true}"#,
                D7STA,
                WHY_GUILD_SNAPSHOT_INCOMPLETE,
            ),
            (
                200,
                0,
                r#"{"id":"10","owner_id":"40","unavailable":true}"#,
                D7STA,
                WHY_GUILD_UNAVAILABLE,
            ),
            (403, 50001, "", D7FOR, WHY_MISSING_ACCESS),
            (404, 10004, "", D7NFD, WHY_GUILD_NOT_FOUND),
            (500, 0, "", D7STA, WHY_GUILD_SNAPSHOT_INCOMPLETE),
        ] {
            for verb in ["CTM", "TMO"] {
                let mut backend = MockRestBackend::default();
                let mut response = if http == 200 {
                    crate::rest::RestResponse::ok()
                } else {
                    crate::rest::RestResponse::discord_error(http, code)
                };
                if http == 200 {
                    response.body = body.into();
                }
                backend.push_response(response);
                let mut runtime = resumed_runtime_without_guild(backend);
                let mut fields = vec![Field::raw("guild_id", "10"), Field::raw("user_id", "40")];
                if verb == "TMO" {
                    fields.push(Field::raw("seconds", "60"));
                }
                let response = call(&mut runtime, verb, fields);
                assert_eq!(
                    response.status, status,
                    "verb={verb} http={http} body={body}"
                );
                assert_ne!(response.field("can"), Some("1"));
                let error = call(&mut runtime, "ERR", vec![Field::raw("scope", "1")]);
                assert_eq!(error.field("http"), Some(http.to_string().as_str()));
                assert_eq!(error.field("discord_code"), Some(code.to_string().as_str()));
                let reason = call(&mut runtime, "WHY", vec![Field::raw("scope", "1")]);
                assert_eq!(reason.field("message"), Some(why));
                let RuntimeRest::Mock(executor) = &runtime.rest else {
                    panic!("expected mock REST")
                };
                assert_eq!(executor.backend.requests().len(), 1);
                assert_eq!(executor.backend.requests()[0].method, HttpMethod::Get);
                assert_eq!(executor.backend.requests()[0].path, "/api/v10/guilds/10");
            }
        }
    }

    #[test]
    fn a_non_resumable_close_invalidates_message_hints_before_another_frame_arrives() {
        let mut runtime = permission_runtime(MockRestBackend::default(), true);
        let now = now_ms_u64();
        runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"old","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, now).unwrap();
        call(&mut runtime, "NXT", vec![]);
        runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"id":"31","guild_id":"10","channel_id":"20","author":{"id":"40"},"type":0,"member":{"permissions":"8"}}}"#, now).unwrap();
        call(&mut runtime, "NXT", vec![]);
        assert_eq!(
            call(&mut runtime, "CAD", vec![]).field("is_admin"),
            Some("1")
        );
        runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"id":"32","guild_id":"10","channel_id":"20","author":{"id":"40"},"type":0,"member":{"permissions":"8"}}}"#, now).unwrap();
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_close_code(4007);
        runtime.pump_gateway_once(&mut transport, now);
        assert_eq!(call(&mut runtime, "CAD", vec![]).status, D7STA);
        assert_eq!(
            call(&mut runtime, "NXT", vec![]).field("event_type"),
            Some("103")
        );
        assert_eq!(call(&mut runtime, "NXT", vec![]).status, D7TIM);
    }

    #[test]
    fn leaving_a_guild_cannot_keep_current_message_administrator_hints() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::ok());
        let mut runtime = permission_runtime(backend, true);
        assert_eq!(
            call(&mut runtime, "CAD", vec![]).field("is_admin"),
            Some("1")
        );
        let response = call(
            &mut runtime,
            "RST",
            vec![
                Field::raw("method", "5"),
                Field::raw("path", "/api/v10/users/@me/guilds/10"),
                Field::percent("reason", ""),
            ],
        );
        assert_eq!(response.status, D7OK);
        assert_eq!(call(&mut runtime, "CAD", vec![]).status, D7NFD);
    }

    #[test]
    fn transferring_guild_ownership_rechecks_both_the_old_and_new_owner() {
        for was_owner in [false, true] {
            let new_owner = if was_owner { "70" } else { "40" };
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::ok());
            let mut guild = crate::rest::RestResponse::ok();
            guild.body = format!(r#"{{"id":"10","owner_id":"{new_owner}"}}"#);
            backend.push_response(guild);
            let mut runtime = permission_runtime(backend, false);
            if was_owner {
                let mut guild = GuildSnapshot::new("10", false, now_ms_u64());
                guild.owner_id = Some("40".into());
                runtime.gateway.snapshot_mut().upsert_guild(guild);
                runtime.current_event.as_mut().unwrap().member_permissions =
                    Some(PermissionSet::from_bits(&[3]));
            }
            assert_eq!(
                call(&mut runtime, "CAD", vec![]).field("is_admin"),
                Some(if was_owner { "1" } else { "0" })
            );
            let response = call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "4"),
                    Field::raw("path", "/api/v10/guilds/10"),
                    Field::percent("payload", format!(r#"{{"owner_id":"{new_owner}"}}"#)),
                    Field::percent("reason", ""),
                ],
            );
            assert_eq!(response.status, D7OK);
            let admin = call(&mut runtime, "CAD", vec![]);
            assert_eq!(admin.status, D7OK);
            assert_eq!(
                admin.field("is_admin"),
                Some(if was_owner { "0" } else { "1" })
            );
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST");
            };
            assert_eq!(executor.backend.requests().len(), 2);
        }
    }

    #[test]
    fn channel_acl_writes_and_position_sync_refresh_permissions_before_sending() {
        for (method, path, payload) in [
            (
                "4",
                "/api/v10/channels/20",
                r#"{"permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"2048"}]}"#,
            ),
            (
                "3",
                "/api/v10/channels/20/permissions/10",
                r#"{"type":0,"allow":"0","deny":"2048"}"#,
            ),
            ("5", "/api/v10/channels/020/permissions/010", ""),
            (
                "4",
                "/api/v10/guilds/10/channels",
                r#"[{"id":"20","parent_id":"30","lock_permissions":true}]"#,
            ),
        ] {
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::ok());
            let mut channel = crate::rest::RestResponse::ok();
            channel.body = r#"{"id":"20","guild_id":"10","type":0,"permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"2048"}]}"#.into();
            backend.push_response(channel);
            let mut runtime = permission_runtime(backend, true);
            let now = now_ms_u64();
            runtime
                .gateway
                .snapshot_mut()
                .upsert_guild_roles(GuildRolesSnapshot::new(
                    "10",
                    vec![
                        crate::snapshot::RoleSnapshot {
                            guild_id: "10".into(),
                            role_id: "10".into(),
                            permissions: PermissionSet::zero(),
                            position: 0,
                        },
                        crate::snapshot::RoleSnapshot {
                            guild_id: "10".into(),
                            role_id: "50".into(),
                            permissions: PermissionSet::from_bits(&[3]),
                            position: 1,
                        },
                        crate::snapshot::RoleSnapshot {
                            guild_id: "10".into(),
                            role_id: "60".into(),
                            permissions: PermissionSet::from_bits(&[10, 11]),
                            position: 10,
                        },
                    ],
                    now,
                ));
            runtime
                .gateway
                .snapshot_mut()
                .upsert_member(MemberSnapshot::new("10", "99", vec!["60".into()], now));
            runtime
                .gateway
                .snapshot_mut()
                .upsert_channel(ChannelSnapshot::new("10", "20", 0, now));
            assert!(runtime.send_preflight_decision("10", "20", now).can);
            let response = call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", method),
                    Field::raw("path", path),
                    Field::percent("payload", payload),
                    Field::percent("reason", ""),
                ],
            );
            assert_eq!(response.status, D7OK, "{path}");
            let decision = runtime.send_preflight_decision("10", "20", now);
            assert_eq!(decision.status, D7OK, "{path}");
            assert!(!decision.can, "{path}");
            assert_eq!(decision.why, "MISSING_SEND_MESSAGES");
            assert_eq!(
                call(&mut runtime, "CAD", vec![]).field("is_admin"),
                Some("1")
            );
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST");
            };
            assert_eq!(executor.backend.requests().len(), 2);
        }
    }

    #[test]
    fn thread_membership_writes_recheck_the_bot_and_preserve_other_members() {
        for (user, changed) in [("@me", true), ("99", true), ("40", false)] {
            let mut runtime = permission_runtime(MockRestBackend::default(), true);
            let path = format!("/api/v10/channels/21/thread-members/{user}");
            runtime.observe_rest_outcome(&mutation(
                HttpMethod::Delete,
                &path,
                CommitState::Unknown,
            ));
            assert_eq!(
                matches!(
                    runtime
                        .gateway
                        .snapshot_mut()
                        .get_channel("21", now_ms_u64()),
                    SnapshotLookup::Missing
                ),
                changed
            );
            assert!(matches!(
                runtime
                    .gateway
                    .snapshot_mut()
                    .get_channel("22", now_ms_u64()),
                SnapshotLookup::Found(_)
            ));
            assert!(runtime
                .current_event
                .as_ref()
                .unwrap()
                .member_permissions
                .is_some());
        }
    }

    #[test]
    fn committed_deletions_tombstone_resources_and_uncertain_deletions_allow_revalidation() {
        for state in [CommitState::Committed, CommitState::Unknown] {
            for path in ["/api/v10/guilds/10", "/api/v10/channels/21"] {
                let mut runtime = permission_runtime(MockRestBackend::default(), true);
                runtime.observe_rest_outcome(&mutation(HttpMethod::Delete, path, state));
                let now = now_ms_u64();
                let tombstone = if path.ends_with("/10") {
                    if state == CommitState::Unknown {
                        assert!(matches!(
                            runtime.gateway.snapshot_mut().get_guild_roles("10", now),
                            SnapshotLookup::Missing
                        ));
                        assert!(matches!(
                            runtime.gateway.snapshot_mut().get_member("10", "99", now),
                            SnapshotLookup::Missing
                        ));
                        assert_eq!(runtime.gateway.snapshot().message_count(), 1);
                    }
                    matches!(
                        runtime.gateway.snapshot_mut().get_guild("10", now),
                        SnapshotLookup::Tombstone
                    )
                } else {
                    matches!(
                        runtime.gateway.snapshot_mut().get_channel("21", now),
                        SnapshotLookup::Tombstone
                    )
                };
                assert_eq!(tombstone, state == CommitState::Committed, "{path}");
            }
        }
    }

    #[test]
    fn only_committed_or_uncertain_writes_invalidate_permission_snapshots() {
        for state in [
            CommitState::NotSent,
            CommitState::Rejected,
            CommitState::Committed,
            CommitState::SentNoResponse,
            CommitState::Unknown,
        ] {
            let mut runtime = permission_runtime(MockRestBackend::default(), true);
            let outcome = mutation(HttpMethod::Patch, "/api/v10/guilds/10/roles/50", state);
            runtime.observe_rest_outcome(&outcome);
            let now = now_ms_u64();
            let changed = !matches!(state, CommitState::NotSent | CommitState::Rejected);
            assert_eq!(
                runtime
                    .current_event
                    .as_ref()
                    .unwrap()
                    .member_permissions
                    .is_none(),
                changed
            );
            assert_eq!(
                matches!(
                    runtime.gateway.snapshot_mut().get_guild_roles("10", now),
                    SnapshotLookup::Missing
                ),
                changed
            );
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_member("10", "40", now),
                SnapshotLookup::Found(_)
            ));
            assert_eq!(runtime.gateway.snapshot().message_count(), 1);
            let SnapshotLookup::Found(thread) =
                runtime.gateway.snapshot_mut().get_channel("21", now)
            else {
                panic!("missing private thread");
            };
            assert_eq!(
                thread.current_user_thread_joined,
                if changed { None } else { Some(true) }
            );
            let SnapshotLookup::Found(other) =
                runtime.gateway.snapshot_mut().get_channel("22", now)
            else {
                panic!("missing unrelated private thread");
            };
            assert_eq!(other.current_user_thread_joined, Some(true));
        }
    }

    #[test]
    fn role_invalidation_normalises_numeric_and_encoded_route_identifiers() {
        for path in [
            "/api/v10/guilds/010/roles/050",
            "/api/v10/guilds/%31%30/roles/%35%30?reason=test",
            "/api/v10/%67uilds/10/roles/./50",
            "/api/v10/guilds/10/members/40/../../roles/50",
        ] {
            let mut runtime = permission_runtime(MockRestBackend::default(), true);
            runtime.observe_rest_outcome(&mutation(
                HttpMethod::Patch,
                path,
                CommitState::Committed,
            ));
            assert!(
                runtime
                    .current_event
                    .as_ref()
                    .unwrap()
                    .member_permissions
                    .is_none(),
                "{path}"
            );
            assert!(matches!(
                runtime
                    .gateway
                    .snapshot_mut()
                    .get_guild_roles("10", now_ms_u64()),
                SnapshotLookup::Missing
            ));
        }
    }

    #[test]
    fn member_role_writes_clear_only_the_target_member_and_its_negative_cache() {
        for path in [
            "/api/v10/guilds/10/members/40/roles/50",
            "/api/v10/guilds/010/members/00040",
            "/api/v10/guilds/%31%30/members/%34%30/roles/50",
            "/api/v10/guilds/10/bans/40",
        ] {
            let mut runtime = permission_runtime(MockRestBackend::default(), true);
            let now = now_ms_u64();
            runtime
                .gateway
                .snapshot_mut()
                .tombstone_member("10", "40", now);
            runtime.observe_rest_outcome(&mutation(
                HttpMethod::Patch,
                path,
                CommitState::Committed,
            ));
            assert!(
                runtime
                    .current_event
                    .as_ref()
                    .unwrap()
                    .member_permissions
                    .is_none(),
                "{path}"
            );
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_member("10", "40", now),
                SnapshotLookup::Missing
            ));
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_member("10", "41", now),
                SnapshotLookup::Found(_)
            ));
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_member("11", "40", now),
                SnapshotLookup::Found(_)
            ));
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_guild_roles("10", now),
                SnapshotLookup::Found(_)
            ));
            let SnapshotLookup::Found(thread) =
                runtime.gateway.snapshot_mut().get_channel("21", now)
            else {
                panic!("missing private thread");
            };
            assert_eq!(thread.current_user_thread_joined, Some(true));
        }
    }

    #[test]
    fn deleting_roles_revalidates_member_assignments_without_discarding_messages() {
        let mut runtime = permission_runtime(MockRestBackend::default(), true);
        runtime.observe_rest_outcome(&mutation(
            HttpMethod::Delete,
            "/api/v10/guilds/10/roles/50",
            CommitState::Unknown,
        ));
        let now = now_ms_u64();
        for user in ["99", "40", "41"] {
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_member("10", user, now),
                SnapshotLookup::Missing
            ));
        }
        assert!(matches!(
            runtime.gateway.snapshot_mut().get_member("11", "40", now),
            SnapshotLookup::Found(_)
        ));
        assert_eq!(runtime.gateway.snapshot().message_count(), 1);
        assert_eq!(runtime.gateway.snapshot().tombstone_count(), 0);
    }

    #[test]
    fn current_member_alias_revalidates_bot_access_even_when_identity_is_unknown() {
        for known in [false, true] {
            let mut runtime = permission_runtime(MockRestBackend::default(), true);
            if !known {
                runtime.bot_user_id_cache = None;
            }
            runtime.observe_rest_outcome(&mutation(
                HttpMethod::Patch,
                "/api/v10/guilds/10/members/@me",
                CommitState::Committed,
            ));
            let now = now_ms_u64();
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_member("10", "99", now),
                SnapshotLookup::Missing
            ));
            assert_eq!(
                matches!(
                    runtime.gateway.snapshot_mut().get_member("10", "40", now),
                    SnapshotLookup::Found(_)
                ),
                known
            );
            assert_eq!(
                runtime
                    .current_event
                    .as_ref()
                    .unwrap()
                    .member_permissions
                    .is_some(),
                known
            );
            let SnapshotLookup::Found(thread) =
                runtime.gateway.snapshot_mut().get_channel("21", now)
            else {
                panic!("missing private thread");
            };
            assert_eq!(thread.current_user_thread_joined, None);
        }
    }

    #[test]
    fn unrelated_resources_and_guilds_do_not_expire_current_message_hints() {
        for (method, path) in [
            (HttpMethod::Get, "/api/v10/guilds/10/emojis"),
            (HttpMethod::Patch, "/api/v10/guilds/11/roles/50"),
            (HttpMethod::Patch, "/api/v10/guilds/10/members/41/roles/50"),
            (HttpMethod::Post, "/api/v10/channels/20/messages"),
            (HttpMethod::Post, "/api/v10/guilds/10/scheduled-events"),
        ] {
            let mut runtime = permission_runtime(MockRestBackend::default(), true);
            let mut outcome = mutation(HttpMethod::Post, path, CommitState::Committed);
            outcome.request.as_mut().unwrap().method = method;
            runtime.observe_rest_outcome(&outcome);
            assert!(
                runtime
                    .current_event
                    .as_ref()
                    .unwrap()
                    .member_permissions
                    .is_some(),
                "{path}"
            );
            assert!(matches!(
                runtime
                    .gateway
                    .snapshot_mut()
                    .get_guild_roles("10", now_ms_u64()),
                SnapshotLookup::Found(_)
            ));
            assert_eq!(runtime.gateway.snapshot().message_count(), 1);
        }
    }

    #[test]
    fn direct_thread_member_reads_refresh_only_the_bots_membership() {
        for (user, status, body, expected) in [
            ("99", 200, r#"{"id":"21","user_id":"99"}"#, Some(true)),
            ("99", 404, r#"{"code":10007}"#, Some(false)),
            ("99", 403, r#"{"code":50001}"#, None),
            ("99", 200, r#"{"id":"21","user_id":"40"}"#, None),
            (
                "99",
                200,
                r#"{"id":"21","user_id":"99","member":{"user":{"id":"40"}}}"#,
                None,
            ),
            ("99", 200, "{}", None),
            ("40", 404, r#"{"code":10007}"#, Some(true)),
            ("40", 200, r#"{"id":"21","user_id":"40"}"#, Some(true)),
            ("@me", 200, r#"{"id":"21","user_id":"99"}"#, Some(true)),
        ] {
            let mut backend = MockRestBackend::default();
            let mut response = crate::rest::RestResponse::ok();
            response.http_status = status;
            response.discord_error_code = (status == 404).then_some(10007);
            response.body = body.into();
            backend.push_response(response);
            let mut runtime = permission_runtime(backend, false);
            let now = now_ms_u64();
            runtime
                .gateway
                .snapshot_mut()
                .set_current_user_thread_joined("21", status != 200 || user == "40", now);
            let path = format!("/api/v10/channels/21/thread-members/{user}?with_member=true");
            call(
                &mut runtime,
                "RST",
                vec![Field::raw("method", "1"), Field::percent("path", &path)],
            );
            let SnapshotLookup::Found(thread) =
                runtime.gateway.snapshot_mut().get_channel("21", now)
            else {
                panic!("expected retained thread")
            };
            assert_eq!(thread.current_user_thread_joined, expected, "{path} {body}");
            let SnapshotLookup::Found(other) =
                runtime.gateway.snapshot_mut().get_channel("22", now)
            else {
                panic!("missing unrelated thread")
            };
            assert_eq!(other.current_user_thread_joined, Some(true));
        }
    }

    #[test]
    fn thread_member_pages_never_treat_an_absent_bot_as_having_left() {
        for (body, initial, expected) in [
            (r#"[{"id":"21","user_id":"99"}]"#, false, Some(true)),
            (r#"[{"id":"21","user_id":"40"}]"#, true, Some(true)),
            ("[]", true, Some(true)),
            (r#"[{"id":"22","user_id":"99"}]"#, true, None),
            (
                r#"[{"id":"21","user_id":"99","member":{"user":{"id":"40"}}}]"#,
                true,
                None,
            ),
            (
                r#"[{"id":"21","user_id":"99"},{"id":"21","user_id":"99"}]"#,
                true,
                None,
            ),
        ] {
            let mut backend = MockRestBackend::default();
            let mut response = crate::rest::RestResponse::ok();
            response.body = body.into();
            backend.push_response(response);
            let mut runtime = permission_runtime(backend, false);
            let now = now_ms_u64();
            runtime
                .gateway
                .snapshot_mut()
                .set_current_user_thread_joined("21", initial, now);
            call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "1"),
                    Field::raw(
                        "path",
                        "/api/v10/channels/21/thread-members?limit=1&after=40",
                    ),
                ],
            );
            let SnapshotLookup::Found(thread) =
                runtime.gateway.snapshot_mut().get_channel("21", now)
            else {
                panic!("expected retained thread")
            };
            assert_eq!(thread.current_user_thread_joined, expected, "{body}");
        }
    }

    #[test]
    fn direct_thread_reads_validate_participants_and_parent_before_caching_access() {
        for (parent, member, identity_known, accepted) in [
            ("20", r#"{"id":"21","user_id":"99"}"#, true, true),
            ("20", r#"{"id":"21","user_id":"40"}"#, true, false),
            ("20", r#"{"id":"22","user_id":"99"}"#, true, false),
            ("22", r#"{"id":"21","user_id":"99"}"#, true, false),
            ("20", r#"{"id":"21","user_id":"99"}"#, false, true),
        ] {
            let mut backend = MockRestBackend::default();
            let mut response = crate::rest::RestResponse::ok();
            response.body = format!(
                r#"{{"id":"21","guild_id":"10","type":12,"parent_id":"{parent}","thread_metadata":{{"archived":false,"locked":false}},"member":{member}}}"#
            );
            backend.push_response(response);
            let mut runtime = permission_runtime(backend, false);
            if !identity_known {
                runtime.bot_user_id_cache = None;
            }
            let now = now_ms_u64();
            let mut thread = ChannelSnapshot::new("10", "21", 12, now);
            thread.parent_channel_id = Some("20".into());
            thread.current_user_thread_joined = Some(true);
            thread.current_user_thread_joined_at_ms = Some(now);
            runtime.gateway.snapshot_mut().upsert_channel(thread);
            assert_eq!(
                call(
                    &mut runtime,
                    "RST",
                    vec![
                        Field::raw("method", "1"),
                        Field::raw("path", "/api/v10/channels/21")
                    ]
                )
                .status,
                D7OK
            );
            let SnapshotLookup::Found(thread) =
                runtime.gateway.snapshot_mut().get_channel("21", now)
            else {
                panic!("expected preserved metadata")
            };
            assert_eq!(thread.complete, accepted);
            assert_eq!(
                thread.current_user_thread_joined,
                (accepted && identity_known).then_some(true)
            );
            assert_eq!(thread.parent_channel_id.as_deref(), Some("20"));
        }
    }

    #[test]
    fn channel_absence_from_related_reads_cannot_be_undone_by_late_gateway_creates() {
        for path in [
            "/api/v10/channels/20/messages/30",
            "/api/v10/channels/20/thread-members/@me",
        ] {
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::discord_error(404, 10003));
            let mut current = crate::rest::RestResponse::ok();
            current.body = r#"{"id":"20","guild_id":"10","type":0,"permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"1024"}]}"#.into();
            backend.push_response(current);
            let mut runtime = channel_permission_runtime(backend, false);
            assert_eq!(
                call(
                    &mut runtime,
                    "RST",
                    vec![Field::raw("method", "GET"), Field::raw("path", path)]
                )
                .status,
                D7NFD
            );
            runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"CHANNEL_CREATE","d":{"id":"20","guild_id":"10","type":0,"permission_overwrites":[]}}"#, now_ms_u64()).unwrap();
            let decision = runtime.send_preflight_decision("10", "20", now_ms_u64());
            assert_eq!(decision.status, D7OK);
            assert!(!decision.can);
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(executor.backend.requests().len(), 2);
        }
    }

    #[test]
    fn channel_permission_mutations_require_rest_revalidation_after_late_notifications() {
        for state in [
            CommitState::Committed,
            CommitState::SentNoResponse,
            CommitState::Unknown,
        ] {
            let mut backend = MockRestBackend::default();
            let mut current = crate::rest::RestResponse::ok();
            current.body = r#"{"id":"20","guild_id":"10","type":0,"permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"1024"}]}"#.into();
            backend.push_response(current);
            let mut runtime = channel_permission_runtime(backend, false);
            runtime.observe_rest_outcome(&mutation(
                HttpMethod::Put,
                "/api/v10/channels/20/permissions/10",
                state,
            ));
            runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"CHANNEL_CREATE","d":{"id":"20","guild_id":"10","type":0,"permission_overwrites":[]}}"#, now_ms_u64()).unwrap();
            let decision = runtime.send_preflight_decision("10", "20", now_ms_u64());
            assert_eq!(decision.status, D7OK);
            assert!(!decision.can);
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(executor.backend.requests().len(), 1);
        }
    }

    #[test]
    fn unsuccessful_channel_reads_cannot_be_replaced_by_late_create_notifications() {
        for initial in 0..3 {
            for current in 0..3 {
                let mut backend = MockRestBackend::default();
                backend.push_response(match initial {
                    0 => crate::rest::RestResponse::discord_error(404, 10003),
                    1 => crate::rest::RestResponse::discord_error(403, 50001),
                    _ => {
                        let mut response = crate::rest::RestResponse::ok();
                        response.body = r#"{"id":"20"}"#.into();
                        response
                    }
                });
                backend.push_response(if current == 2 {
                    crate::rest::RestResponse::discord_error(404, 10003)
                } else {
                    let mut response = crate::rest::RestResponse::ok();
                    response.body = format!(
                        r#"{{"id":"20","guild_id":"10","type":0,"permission_overwrites":{}}}"#,
                        if current == 1 {
                            "[]"
                        } else {
                            r#"[{"id":"10","type":0,"allow":"0","deny":"1024"}]"#
                        }
                    );
                    response
                });
                let mut runtime = channel_permission_runtime(backend, false);
                let read = call(
                    &mut runtime,
                    "RST",
                    vec![
                        Field::raw("method", "GET"),
                        Field::raw("path", "/api/v10/channels/20"),
                    ],
                );
                assert_eq!(read.status, [D7NFD, D7FOR, D7OK][initial]);
                runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"CHANNEL_CREATE","d":{"id":"20","guild_id":"10","type":0,"permission_overwrites":[]}}"#, now_ms_u64()).unwrap();
                let decision = runtime.send_preflight_decision("10", "20", now_ms_u64());
                assert_eq!(decision.status, if current == 2 { D7NFD } else { D7OK });
                assert_eq!(decision.can, current == 1);
                let RuntimeRest::Mock(executor) = &runtime.rest else {
                    panic!("expected mock REST")
                };
                assert_eq!(executor.backend.requests().len(), 2);
            }
        }
    }

    #[test]
    fn delayed_thread_updates_revalidate_archive_and_lock_state_before_send() {
        for locked in [false, true] {
            for blocked in [false, true] {
                let mut backend = MockRestBackend::default();
                for restricted in [true, blocked] {
                    let mut response = crate::rest::RestResponse::ok();
                    response.body = format!(
                        r#"{{"id":"21","guild_id":"10","parent_id":"20","type":11,"permission_overwrites":[],"thread_metadata":{{"archived":{},"locked":{}}}}}"#,
                        restricted && !locked,
                        restricted && locked
                    );
                    backend.push_response(response);
                }
                let mut runtime = channel_permission_runtime(backend, true);
                assert_eq!(
                    call(
                        &mut runtime,
                        "RST",
                        vec![
                            Field::raw("method", "GET"),
                            Field::raw("path", "/api/v10/channels/21")
                        ]
                    )
                    .status,
                    D7OK
                );
                runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"THREAD_UPDATE","d":{"id":"21","guild_id":"10","parent_id":"20","type":11,"permission_overwrites":[],"thread_metadata":{"archived":false,"locked":false}}}"#, now_ms_u64()).unwrap();
                let decision = runtime.send_preflight_decision("10", "21", now_ms_u64());
                assert_eq!(decision.status, D7OK);
                assert_eq!(decision.can, !blocked);
                if blocked {
                    assert_eq!(
                        decision.why,
                        if locked {
                            WHY_THREAD_LOCKED
                        } else {
                            WHY_THREAD_ARCHIVED
                        }
                    );
                }
                let RuntimeRest::Mock(executor) = &runtime.rest else {
                    panic!("expected mock REST")
                };
                assert_eq!(executor.backend.requests().len(), 2);
            }
        }
    }

    #[test]
    fn delayed_channel_changes_after_rest_revalidate_before_authorising_send() {
        for remove in [false, true] {
            for actual_can in [false, true] {
                let mut backend = MockRestBackend::default();
                for allowed in [false, actual_can] {
                    let mut response = crate::rest::RestResponse::ok();
                    response.body = format!(
                        r#"{{"id":"20","guild_id":"10","type":0,"permission_overwrites":{}}}"#,
                        if allowed {
                            "[]"
                        } else {
                            r#"[{"id":"10","type":0,"allow":"0","deny":"1024"}]"#
                        }
                    );
                    backend.push_response(response);
                }
                let mut runtime = channel_permission_runtime(backend, false);
                assert_eq!(
                    call(
                        &mut runtime,
                        "RST",
                        vec![
                            Field::raw("method", "GET"),
                            Field::raw("path", "/api/v10/channels/20")
                        ]
                    )
                    .status,
                    D7OK
                );
                let event = if remove {
                    r#"{"op":0,"s":3,"t":"CHANNEL_DELETE","d":{"id":"20","guild_id":"10","type":0}}"#
                } else {
                    r#"{"op":0,"s":3,"t":"CHANNEL_UPDATE","d":{"id":"20","guild_id":"10","type":0,"permission_overwrites":[]}}"#
                };
                runtime.ingest_gateway_json(event, now_ms_u64()).unwrap();
                let decision = runtime.send_preflight_decision("10", "20", now_ms_u64());
                assert_eq!(decision.status, D7OK, "{event}");
                assert_eq!(decision.can, actual_can, "{event}");
                let RuntimeRest::Mock(executor) = &runtime.rest else {
                    panic!("expected mock REST")
                };
                assert_eq!(executor.backend.requests().len(), 2);
            }
        }
    }

    #[test]
    fn expired_rest_unavailability_can_recover_without_an_availability_notification() {
        let mut backend = MockRestBackend::default();
        for unavailable in [true, false] {
            let mut response = crate::rest::RestResponse::ok();
            response.body = format!(r#"{{"id":"10","owner_id":"70","unavailable":{unavailable}}}"#);
            backend.push_response(response);
        }
        let mut runtime = permission_runtime(backend, false);
        call(
            &mut runtime,
            "RST",
            vec![
                Field::raw("method", "GET"),
                Field::raw("path", "/api/v10/guilds/10"),
            ],
        );
        let now = now_ms_u64();
        assert_eq!(
            runtime.guild_state_decision("10", now).unwrap().why,
            WHY_GUILD_UNAVAILABLE
        );
        assert!(runtime
            .guild_state_decision(
                "10",
                now.saturating_add(runtime.settings.message_ttl_ms)
                    .saturating_add(1)
            )
            .is_none());
        let RuntimeRest::Mock(executor) = &runtime.rest else {
            panic!("expected mock REST")
        };
        assert_eq!(executor.backend.requests().len(), 2);
    }

    #[test]
    fn missing_or_invalid_guild_reads_require_rest_confirmation_before_gateway_recovery() {
        for initial in 0..3 {
            for recovered in [false, true] {
                let mut backend = MockRestBackend::default();
                let first = match initial {
                    0 => crate::rest::RestResponse::discord_error(404, 10004),
                    1 => crate::rest::RestResponse::discord_error(403, 50001),
                    _ => {
                        let mut response = crate::rest::RestResponse::ok();
                        response.body = r#"{"id":"10"}"#.into();
                        response
                    }
                };
                backend.push_response(first);
                let second = if recovered {
                    let mut response = crate::rest::RestResponse::ok();
                    response.body = r#"{"id":"10","owner_id":"40"}"#.into();
                    response
                } else {
                    crate::rest::RestResponse::discord_error(404, 10004)
                };
                backend.push_response(second);
                let mut runtime = permission_runtime(backend, false);
                let read = call(
                    &mut runtime,
                    "RST",
                    vec![
                        Field::raw("method", "GET"),
                        Field::raw("path", "/api/v10/guilds/10"),
                    ],
                );
                assert_eq!(read.status, [D7NFD, D7FOR, D7OK][initial]);
                runtime
                    .ingest_gateway_json(
                        r#"{"op":0,"s":3,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"40"}}"#,
                        now_ms_u64(),
                    )
                    .unwrap();
                let decision = call(&mut runtime, "CAD", vec![]);
                assert_eq!(decision.status, if recovered { D7OK } else { D7NFD });
                assert_eq!(
                    decision.field("is_admin"),
                    if recovered { Some("1") } else { None }
                );
                let RuntimeRest::Mock(executor) = &runtime.rest else {
                    panic!("expected mock REST")
                };
                assert_eq!(executor.backend.requests().len(), 2);
            }
        }
    }

    #[test]
    fn unavailable_or_removed_guilds_cannot_reuse_cached_role_administrators() {
        for event in [
            r#"{"op":0,"s":3,"t":"GUILD_UPDATE","d":{"id":"10","owner_id":"40"}}"#,
            r#"{"op":0,"s":3,"t":"GUILD_DELETE","d":{"id":"10"}}"#,
        ] {
            for state in 0..3 {
                let mut backend = MockRestBackend::default();
                let mut first = crate::rest::RestResponse::ok();
                first.body = r#"{"id":"10","owner_id":"70"}"#.into();
                backend.push_response(first);
                backend.push_response(match state {
                    0 => crate::rest::RestResponse::discord_error(404, 10004),
                    1 => crate::rest::RestResponse::discord_error(403, 50001),
                    _ => {
                        let mut response = crate::rest::RestResponse::ok();
                        response.body = r#"{"id":"10","owner_id":"70","unavailable":true}"#.into();
                        response
                    }
                });
                let mut runtime = permission_runtime(backend, true);
                call(
                    &mut runtime,
                    "RST",
                    vec![
                        Field::raw("method", "GET"),
                        Field::raw("path", "/api/v10/guilds/10"),
                    ],
                );
                runtime.ingest_gateway_json(event, now_ms_u64()).unwrap();
                let decision = call(&mut runtime, "CAD", vec![]);
                assert_eq!(decision.status, [D7NFD, D7FOR, D7STA][state]);
                assert_ne!(decision.field("is_admin"), Some("1"));
                let RuntimeRest::Mock(executor) = &runtime.rest else {
                    panic!("expected mock REST")
                };
                assert_eq!(executor.backend.requests().len(), 2);
            }
        }
    }

    #[test]
    fn late_administrator_hints_after_guild_reads_require_current_facts_and_accept_grants() {
        for grant in 0..3 {
            let mut backend = MockRestBackend::default();
            let owner = if grant == 2 { "40" } else { "70" };
            let roles = if grant == 1 { r#"["50"]"# } else { "[]" };
            for body in [
                r#"{"id":"10","owner_id":"70"}"#.to_string(),
                format!(r#"{{"id":"10","owner_id":"{owner}"}}"#),
                format!(r#"{{"user":{{"id":"40"}},"roles":{roles},"communication_disabled_until":null}}"#),
                r#"[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}]"#.to_string(),
            ] {
                let mut response = crate::rest::RestResponse::ok(); response.body = body; backend.push_response(response);
            }
            let mut runtime = permission_runtime(backend, false);
            call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "GET"),
                    Field::raw("path", "/api/v10/guilds/10"),
                ],
            );
            runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"31","type":0,"author":{"id":"40"},"member":{"permissions":"8"}}}"#, now_ms_u64()).unwrap();
            call(&mut runtime, "NXT", vec![]);
            let decision = call(&mut runtime, "CAD", vec![]);
            assert_eq!(decision.status, D7OK);
            assert_eq!(
                decision.field("is_admin"),
                Some(if grant == 0 { "0" } else { "1" })
            );
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(
                executor.backend.requests().len(),
                if grant == 2 { 2 } else { 4 }
            );
        }
    }

    #[test]
    fn delayed_guild_events_after_rest_require_current_ownership_and_availability() {
        let events = [
            r#"{"op":0,"s":3,"t":"GUILD_UPDATE","d":{"id":"10","owner_id":"40"}}"#,
            r#"{"op":0,"s":3,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"40"}}"#,
            r#"{"op":0,"s":3,"t":"GUILD_DELETE","d":{"id":"10","unavailable":true}}"#,
            r#"{"op":0,"s":3,"t":"GUILD_DELETE","d":{"id":"10"}}"#,
        ];
        for event in events {
            for actual_owner in [false, true] {
                let mut backend = MockRestBackend::default();
                for owner in ["70", if actual_owner { "40" } else { "70" }] {
                    let mut response = crate::rest::RestResponse::ok();
                    response.body = format!(r#"{{"id":"10","owner_id":"{owner}"}}"#);
                    backend.push_response(response);
                }
                let mut runtime = permission_runtime(backend, false);
                assert_eq!(
                    call(
                        &mut runtime,
                        "RST",
                        vec![
                            Field::raw("method", "GET"),
                            Field::raw("path", "/api/v10/guilds/10")
                        ]
                    )
                    .status,
                    D7OK
                );
                runtime.ingest_gateway_json(event, now_ms_u64()).unwrap();
                let decision = call(&mut runtime, "CAD", vec![]);
                assert_eq!(decision.status, D7OK, "{event}");
                assert_eq!(
                    decision.field("is_admin"),
                    Some(if actual_owner { "1" } else { "0" }),
                    "{event}"
                );
                let RuntimeRest::Mock(executor) = &runtime.rest else {
                    panic!("expected mock REST")
                };
                assert_eq!(executor.backend.requests().len(), 2);
            }
        }
    }

    #[test]
    fn unsuccessful_role_reads_cannot_be_replaced_by_late_full_gateway_lists() {
        for malformed in [false, true] {
            let mut backend = MockRestBackend::default();
            let mut first = if malformed {
                crate::rest::RestResponse::ok()
            } else {
                crate::rest::RestResponse::discord_error(403, 50001)
            };
            if malformed {
                first.body = "[]".into();
            }
            backend.push_response(first);
            let mut current = crate::rest::RestResponse::ok();
            current.body = r#"[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"0","position":1}]"#.into();
            backend.push_response(current);
            let mut runtime = permission_runtime(backend, true);
            let read = call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "GET"),
                    Field::raw("path", "/api/v10/guilds/10/roles"),
                ],
            );
            assert_eq!(read.status, if malformed { D7OK } else { D7FOR });
            runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}]}}"#, now_ms_u64()).unwrap();
            let decision = call(&mut runtime, "CAD", vec![]);
            assert_eq!(decision.status, D7OK);
            assert_eq!(decision.field("is_admin"), Some("0"));
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(executor.backend.requests().len(), 2);
        }
    }

    #[test]
    fn delayed_role_events_after_rest_revalidate_without_losing_member_assignments() {
        for (remove, initially_absent) in [(false, false), (true, false), (true, true)] {
            for actual_admin in [false, true] {
                let mut backend = MockRestBackend::default();
                for (index, admin) in [remove, actual_admin].into_iter().enumerate() {
                    if index == 1 && remove {
                        let mut response = crate::rest::RestResponse::ok();
                        response.body = format!(
                            r#"{{"user":{{"id":"40"}},"roles":{},"communication_disabled_until":null}}"#,
                            if actual_admin { r#"["50"]"# } else { "[]" }
                        );
                        backend.push_response(response);
                    }
                    let mut response = crate::rest::RestResponse::ok();
                    response.body = format!(
                        r#"[{{"id":"10","permissions":"0","position":0}},{{"id":"50","permissions":"{}","position":1}}]"#,
                        if admin { "8" } else { "0" }
                    );
                    if index == 0 && initially_absent {
                        response.body = r#"[{"id":"10","permissions":"0","position":0}]"#.into();
                    }
                    if index == 1 && remove && !actual_admin {
                        response.body = r#"[{"id":"10","permissions":"0","position":0}]"#.into();
                    }
                    backend.push_response(response);
                }
                let mut runtime = permission_runtime(backend, true);
                assert_eq!(
                    call(
                        &mut runtime,
                        "RST",
                        vec![
                            Field::raw("method", "GET"),
                            Field::raw("path", "/api/v10/guilds/10/roles"),
                        ]
                    )
                    .status,
                    D7OK
                );
                let event = if remove {
                    r#"{"op":0,"s":3,"t":"GUILD_ROLE_DELETE","d":{"guild_id":"10","role_id":"50"}}"#
                } else {
                    r#"{"op":0,"s":3,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":"8","position":1}}}"#
                };
                runtime.ingest_gateway_json(event, now_ms_u64()).unwrap();
                let decision = call(&mut runtime, "CAD", vec![]);
                assert_eq!(decision.status, D7OK, "{remove} {actual_admin}");
                assert_eq!(
                    decision.field("is_admin"),
                    Some(if actual_admin { "1" } else { "0" }),
                    "{remove} {actual_admin}"
                );
                let RuntimeRest::Mock(executor) = &runtime.rest else {
                    panic!("expected mock REST")
                };
                assert_eq!(
                    executor.backend.requests().len(),
                    if remove { 3 } else { 2 }
                );
            }
        }
    }

    #[test]
    fn delayed_message_admin_hints_require_current_rest_facts_and_accept_real_grants() {
        for roles_proof in [false, true] {
            for actual_admin in [false, true] {
                let mut backend = MockRestBackend::default();
                let first =
                    r#"{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}"#;
                let member = if actual_admin {
                    r#"{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}"#
                } else {
                    first
                };
                for body in [
                    if roles_proof {
                        r#"[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"0","position":1}]"#
                    } else {
                        first
                    },
                    member,
                    r#"[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}]"#,
                    r#"{"id":"10","owner_id":"70"}"#,
                ] {
                    let mut response = crate::rest::RestResponse::ok();
                    response.body = body.into();
                    backend.push_response(response);
                }
                let mut runtime = permission_runtime(backend, false);
                assert_eq!(
                    call(
                        &mut runtime,
                        "RST",
                        vec![
                            Field::raw("method", "1"),
                            Field::raw(
                                "path",
                                if roles_proof {
                                    "/api/v10/guilds/10/roles"
                                } else {
                                    "/api/v10/guilds/10/members/40"
                                }
                            )
                        ]
                    )
                    .status,
                    D7OK
                );
                runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"31","type":0,"author":{"id":"40"},"member":{"permissions":"8"}}}"#, now_ms_u64()).unwrap();
                call(&mut runtime, "NXT", vec![]);
                let result = call(&mut runtime, "CAD", vec![]);
                assert_eq!(result.status, D7OK);
                assert_eq!(
                    result.field("is_admin"),
                    Some(if actual_admin { "1" } else { "0" })
                );
                let RuntimeRest::Mock(executor) = &runtime.rest else {
                    panic!("expected mock REST")
                };
                assert_eq!(
                    executor.backend.requests().len(),
                    if actual_admin { 3 } else { 4 }
                );
            }
        }
    }

    #[test]
    fn confirmed_rest_removals_survive_late_member_adds_without_granting_administrator() {
        let mut backend = MockRestBackend::default();
        for _ in 0..2 {
            backend.push_response(crate::rest::RestResponse::discord_error(404, 10007));
        }
        let mut runtime = permission_runtime(backend, false);
        assert_eq!(
            call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "1"),
                    Field::raw("path", "/api/v10/guilds/10/members/40")
                ]
            )
            .status,
            D7NFD
        );
        runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"GUILD_MEMBER_ADD","d":{"guild_id":"10","user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}}"#, now_ms_u64()).unwrap();
        assert_eq!(call(&mut runtime, "CAD", vec![]).status, D7NFD);
    }

    #[test]
    fn delayed_member_updates_after_rest_require_fresh_membership_before_admin_decisions() {
        for actual_admin in [false, true] {
            let mut backend = MockRestBackend::default();
            for roles in ["[]", if actual_admin { r#"["50"]"# } else { "[]" }] {
                let mut response = crate::rest::RestResponse::ok();
                response.body = format!(
                    r#"{{"user":{{"id":"40"}},"roles":{roles},"communication_disabled_until":null}}"#
                );
                backend.push_response(response);
            }
            let mut runtime = permission_runtime(backend, false);
            let read = call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "1"),
                    Field::raw("path", "/api/v10/guilds/10/members/40"),
                ],
            );
            assert_eq!(read.status, D7OK);
            runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"40"},"roles":["50"]}}"#, now_ms_u64()).unwrap();
            let result = call(&mut runtime, "CAD", vec![]);
            assert_eq!(result.status, D7OK);
            assert_eq!(
                result.field("is_admin"),
                Some(if actual_admin { "1" } else { "0" })
            );
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(executor.backend.requests().len(), 2);
        }
    }

    #[test]
    fn delayed_member_removals_cannot_replace_a_rest_confirmed_member_with_a_tombstone() {
        let mut backend = MockRestBackend::default();
        for _ in 0..2 {
            let mut response = crate::rest::RestResponse::ok();
            response.body =
                r#"{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}"#.into();
            backend.push_response(response);
        }
        let mut runtime = permission_runtime(backend, true);
        assert_eq!(
            call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "1"),
                    Field::raw("path", "/api/v10/guilds/10/members/40")
                ]
            )
            .status,
            D7OK
        );
        runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"GUILD_MEMBER_REMOVE","d":{"guild_id":"10","user":{"id":"40"}}}"#, now_ms_u64()).unwrap();
        let result = call(&mut runtime, "CAD", vec![]);
        assert_eq!(result.status, D7OK);
        assert_eq!(result.field("is_admin"), Some("1"));
    }

    #[test]
    fn direct_channel_reads_refresh_send_permissions_without_an_additional_fetch() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"id":"20","guild_id":"10","type":0,"permission_overwrites":[{"id":"99","type":1,"allow":"0","deny":"2048"}]}"#.into();
        backend.push_response(response);
        let mut runtime = permission_runtime(backend, false);
        let now = now_ms_u64();
        let store = runtime.gateway.snapshot_mut();
        store.upsert_member(MemberSnapshot::new("10", "99", vec![], now));
        store.upsert_guild_roles(GuildRolesSnapshot::new(
            "10",
            vec![crate::snapshot::RoleSnapshot {
                guild_id: "10".into(),
                role_id: "10".into(),
                permissions: PermissionSet::from_bits(&[10, 11]),
                position: 0,
            }],
            now,
        ));
        store.upsert_channel(ChannelSnapshot::new("10", "20", 0, now));
        assert!(runtime.send_preflight_decision("10", "20", now).can);
        assert_eq!(
            call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "1"),
                    Field::raw("path", "/api/v10/channels/020")
                ]
            )
            .status,
            D7OK
        );
        let decision = runtime.send_preflight_decision("10", "20", now);
        assert!(!decision.can);
        assert_eq!(decision.why, "MISSING_SEND_MESSAGES");
        let RuntimeRest::Mock(executor) = &runtime.rest else {
            panic!("expected mock REST")
        };
        assert_eq!(executor.backend.requests().len(), 1);
    }

    #[test]
    fn direct_permission_reads_replace_cached_and_queued_administrator_hints() {
        for (path, body, author_admin, other_admin) in [
            (
                "/api/v10/guilds/010/members/%34%30?with_counts=true",
                r#"{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}"#,
                true,
                "1",
            ),
            (
                "/api/v10/guilds/%31%30/roles",
                r#"[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"0","position":1}]"#,
                true,
                "0",
            ),
            (
                "/api/v10/guilds/10",
                r#"{"id":"10","owner_id":"70"}"#,
                false,
                "1",
            ),
        ] {
            let mut backend = MockRestBackend::default();
            let mut response = crate::rest::RestResponse::ok();
            response.body = body.into();
            backend.push_response(response);
            let mut runtime = permission_runtime(backend, author_admin);
            let now = now_ms_u64();
            if !author_admin {
                let mut guild = GuildSnapshot::new("10", false, now);
                guild.owner_id = Some("40".into());
                runtime.gateway.snapshot_mut().upsert_guild(guild);
            }
            runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"31","type":0,"author":{"id":"40"},"member":{"permissions":"8"}}}"#, now).unwrap();
            runtime.ingest_gateway_json(r#"{"op":0,"s":4,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"32","type":0,"author":{"id":"41"},"member":{"permissions":"8"}}}"#, now).unwrap();
            assert_eq!(
                call(&mut runtime, "CAD", vec![]).field("is_admin"),
                Some("1")
            );
            let read = call(
                &mut runtime,
                "RST",
                vec![Field::raw("method", "1"), Field::percent("path", path)],
            );
            assert_eq!(read.status, D7OK);
            for expected in ["0", "0", other_admin] {
                assert_eq!(
                    call(&mut runtime, "CAD", vec![]).field("is_admin"),
                    Some(expected),
                    "{path}"
                );
                call(&mut runtime, "NXT", vec![]);
            }
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(executor.backend.requests().len(), 1, "{path}");
        }
    }

    #[test]
    fn direct_member_not_found_reads_remove_administrator_and_owner_evidence() {
        for code in [10007, 10013, 10004] {
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::discord_error(404, code));
            let mut runtime = permission_runtime(backend, true);
            let now = now_ms_u64();
            let mut guild = GuildSnapshot::new("10", false, now);
            guild.owner_id = Some("40".into());
            runtime.gateway.snapshot_mut().upsert_guild(guild);
            let read = call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "1"),
                    Field::raw("path", "/api/v10/guilds/10/members/40"),
                ],
            );
            assert_eq!(read.status, D7NFD);
            assert!(runtime
                .current_event
                .as_ref()
                .unwrap()
                .member_permissions
                .is_none());
            assert!(runtime.admin_decision("10", "40", now).is_err(), "{code}");
        }
    }

    #[test]
    fn unusable_direct_permission_reads_discard_only_the_affected_positive_evidence() {
        for (path, code, body) in [
            (
                "/api/v10/guilds/10/members/40",
                200,
                r#"{"user":{"id":"41"},"roles":[]}"#,
            ),
            (
                "/api/v10/guilds/10/members/40",
                200,
                r#"{"guild_id":"11","user":{"id":"40"},"roles":[]}"#,
            ),
            (
                "/api/v10/guilds/10/members/40",
                200,
                r#"{"user":{"id":"40"},"roles":[],"communication_disabled_until":false}"#,
            ),
            ("/api/v10/guilds/10/members/40", 200, "{}"),
            ("/api/v10/guilds/10/members/40", 403, r#"{"code":50001}"#),
            (
                "/api/v10/guilds/10/roles",
                200,
                r#"[{"id":"50","permissions":"8","position":1},{"id":"50","permissions":"0","position":1}]"#,
            ),
            ("/api/v10/guilds/10/roles", 403, r#"{"code":50013}"#),
            ("/api/v10/guilds/10", 200, r#"{"id":"11","owner_id":"40"}"#),
        ] {
            let mut backend = MockRestBackend::default();
            let mut response = crate::rest::RestResponse::ok();
            response.http_status = code;
            response.discord_error_code = (code == 403).then_some(50001);
            response.body = body.into();
            backend.push_response(response);
            let mut runtime = permission_runtime(backend, true);
            let read = call(
                &mut runtime,
                "RST",
                vec![Field::raw("method", "1"), Field::raw("path", path)],
            );
            assert_eq!(read.status, if code == 200 { D7OK } else { D7FOR });
            assert!(
                runtime
                    .current_event
                    .as_ref()
                    .unwrap()
                    .member_permissions
                    .is_none(),
                "{path}"
            );
            let now = now_ms_u64();
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_member("11", "40", now),
                SnapshotLookup::Found(_)
            ));
            if path.contains("members") {
                assert!(matches!(
                    runtime.gateway.snapshot_mut().get_member("10", "40", now),
                    SnapshotLookup::Missing
                ));
                assert!(matches!(
                    runtime.gateway.snapshot_mut().get_member("10", "41", now),
                    SnapshotLookup::Found(_)
                ));
            } else if path.ends_with("roles") {
                assert!(matches!(
                    runtime.gateway.snapshot_mut().get_guild_roles("10", now),
                    SnapshotLookup::Missing
                ));
            } else {
                let SnapshotLookup::Found(guild) =
                    runtime.gateway.snapshot_mut().get_guild("10", now)
                else {
                    panic!("missing invalidated guild")
                };
                assert!(!guild.complete);
            }
        }
    }

    #[test]
    fn transient_direct_read_failures_keep_fresh_permission_evidence() {
        for status in [429, 500, 502] {
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::discord_error(status, 0));
            let mut runtime = permission_runtime(backend, true);
            let read = call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "1"),
                    Field::raw("path", "/api/v10/guilds/10/members/40"),
                ],
            );
            assert_ne!(read.status, D7OK);
            assert!(runtime
                .current_event
                .as_ref()
                .unwrap()
                .member_permissions
                .is_some());
            assert_eq!(
                call(&mut runtime, "CAD", vec![]).field("is_admin"),
                Some("1")
            );
        }
    }

    #[test]
    fn direct_bot_member_and_role_reads_revalidate_private_thread_access() {
        for (path, body) in [
            (
                "/api/v10/guilds/10/members/@me",
                r#"{"user":{"id":"99"},"roles":[],"communication_disabled_until":null}"#,
            ),
            (
                "/api/v10/guilds/10/roles",
                r#"[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}]"#,
            ),
        ] {
            let mut backend = MockRestBackend::default();
            let mut response = crate::rest::RestResponse::ok();
            response.body = body.into();
            backend.push_response(response);
            let mut runtime = permission_runtime(backend, true);
            assert_eq!(
                call(
                    &mut runtime,
                    "RST",
                    vec![Field::raw("method", "1"), Field::raw("path", path)]
                )
                .status,
                D7OK
            );
            let now = now_ms_u64();
            for (channel, expected) in [("21", None), ("22", Some(true))] {
                let SnapshotLookup::Found(thread) =
                    runtime.gateway.snapshot_mut().get_channel(channel, now)
                else {
                    panic!("missing private thread")
                };
                assert_eq!(
                    thread.current_user_thread_joined, expected,
                    "{path} {channel}"
                );
            }
            assert_eq!(
                runtime
                    .current_event
                    .as_ref()
                    .unwrap()
                    .member_permissions
                    .is_some(),
                path.contains("members")
            );
        }
    }

    #[test]
    fn role_only_gateway_updates_revalidate_expired_bot_timeout_before_sending() {
        for (timeout, status, can, why) in [
            (
                Some(r#""2100-01-01T00:00:00Z""#),
                D7FOR,
                false,
                WHY_BOT_TIMED_OUT,
            ),
            (Some("null"), D7OK, true, WHY_OK),
            (None, D7FOR, false, WHY_MISSING_ACCESS),
        ] {
            let mut backend = MockRestBackend::default();
            let response = match timeout {
                Some(timeout) => {
                    let mut response = crate::rest::RestResponse::ok();
                    response.body = format!(
                        r#"{{"user":{{"id":"99"}},"roles":[],"communication_disabled_until":{timeout}}}"#
                    );
                    response
                }
                None => crate::rest::RestResponse::discord_error(403, 50001),
            };
            backend.push_response(response);
            let mut runtime = permission_runtime(backend, false);
            let now = now_ms_u64();
            let old = now.saturating_sub(runtime.settings.bot_member_ttl_ms + 1);
            let store = runtime.gateway.snapshot_mut();
            store.upsert_member(MemberSnapshot::new("10", "99", vec![], old));
            store.upsert_guild_roles(GuildRolesSnapshot::new(
                "10",
                vec![crate::snapshot::RoleSnapshot {
                    guild_id: "10".into(),
                    role_id: "10".into(),
                    permissions: PermissionSet::from_bits(&[10, 11]),
                    position: 0,
                }],
                now,
            ));
            store.upsert_channel(ChannelSnapshot::new("10", "20", 0, now));
            runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"99"},"roles":[]}}"#, now).unwrap();
            let decision = runtime.send_preflight_decision("10", "20", now);
            assert_eq!(decision.can, can);
            assert_eq!(decision.status, status);
            assert_eq!(decision.why, why);
            if timeout.is_some() {
                let cached = runtime.send_preflight_decision("10", "20", now);
                assert_eq!(cached.status, status);
                assert_eq!(cached.can, can);
                assert_eq!(cached.why, why);
            }
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST");
            };
            assert_eq!(executor.backend.requests().len(), 1);
        }
    }

    #[test]
    fn adding_a_member_role_can_grant_admin_after_a_fresh_member_read() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::ok());
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}"#.into();
        backend.push_response(member);
        let mut runtime = permission_runtime(backend, false);
        assert_eq!(
            call(&mut runtime, "CAD", vec![]).field("is_admin"),
            Some("0")
        );
        let write = call(
            &mut runtime,
            "RST",
            vec![
                Field::raw("method", "3"),
                Field::raw("path", "/api/v10/guilds/10/members/40/roles/50"),
                Field::percent("payload", ""),
                Field::percent("reason", ""),
            ],
        );
        assert_eq!(write.status, D7OK);
        let admin = call(&mut runtime, "CAD", vec![]);
        assert_eq!(admin.status, D7OK);
        assert_eq!(admin.field("is_admin"), Some("1"));
        let RuntimeRest::Mock(executor) = &runtime.rest else {
            panic!("expected mock REST");
        };
        assert_eq!(executor.backend.requests().len(), 2);
    }
}
