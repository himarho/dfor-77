use super::*;
use crate::snapshot::{ChannelPermissionOverwrite, SnapshotLookup};

struct PendingMember {
    guild_id: String,
    user_id: String,
    guard_at_ms: u64,
}

#[derive(Default)]
pub(super) struct AuthorityContext {
    pending: Vec<PendingMember>,
    pending_roles: Vec<(String, u64)>,
    pending_guilds: Vec<(String, u64)>,
    pending_channels: Vec<(String, u64)>,
    hints: Vec<(String, String)>,
}

impl AuthorityContext {
    pub(super) fn capture(frame: &GatewayFrame, snapshot: &mut SnapshotStore, now_ms: u64) -> Self {
        match frame {
            GatewayFrame::Dispatch(dispatch) => Self::capture_dispatch(dispatch, snapshot, now_ms),
            _ => Self::default(),
        }
    }

    pub(super) fn capture_dispatch(
        dispatch: &GatewayDispatch,
        snapshot: &mut SnapshotStore,
        now_ms: u64,
    ) -> Self {
        let mut context = Self::default();
        match dispatch {
            GatewayDispatch::MemberUpsert { member, .. } => {
                context.full_member(snapshot, member, now_ms)
            }
            GatewayDispatch::GuildCreate {
                guild,
                channels,
                members,
                roles,
                ..
            } => {
                context.full_guild(snapshot, guild, now_ms);
                for channel in channels {
                    context.full_channel(snapshot, channel, now_ms);
                }
                for member in members {
                    context.full_member(snapshot, member, now_ms);
                }
                if let Some(roles) = roles {
                    context.full_roles(snapshot, roles, now_ms);
                }
            }
            GatewayDispatch::GuildUpdate { guild, .. } => {
                context.full_guild(snapshot, guild, now_ms);
            }
            GatewayDispatch::ChannelUpsert { channel, .. } => {
                context.full_channel(snapshot, channel, now_ms);
            }
            GatewayDispatch::ChannelSync { channels, .. } => {
                for channel in channels {
                    context.full_channel(snapshot, channel, now_ms);
                }
            }
            GatewayDispatch::ChannelUpdate { patch, .. } => {
                if snapshot.channel_rest_protected(&patch.channel_id) {
                    let unchanged = match snapshot.get_channel(&patch.channel_id, now_ms) {
                        SnapshotLookup::Found(old) | SnapshotLookup::Stale(old) => {
                            let mut next = old.clone();
                            patch.clone().apply(&mut next, now_ms);
                            old.complete && next.complete && same_channel_config(&old, &next)
                        }
                        _ => false,
                    };
                    if !unchanged {
                        context.pending_channel(snapshot, &patch.channel_id, now_ms);
                    }
                }
            }
            GatewayDispatch::RoleUpsert { role, .. } => {
                if snapshot.roles_rest_protected(&role.guild_id) {
                    let unchanged = match snapshot.get_guild_roles(&role.guild_id, now_ms) {
                        SnapshotLookup::Found(roles) | SnapshotLookup::Stale(roles) => {
                            roles.complete && roles.roles.iter().any(|old| old == role)
                        }
                        _ => false,
                    };
                    if !unchanged {
                        context.pending_roles(snapshot, &role.guild_id, now_ms);
                    }
                }
            }
            GatewayDispatch::MembersChunk { chunk, .. } => {
                for member in &chunk.members {
                    context.full_member(snapshot, member, now_ms);
                }
            }
            GatewayDispatch::MemberUpdate { patch, .. } => {
                if snapshot.member_rest_protected(&patch.guild_id, &patch.user_id)
                    && (patch.role_ids.is_some()
                        || patch.timeout != crate::snapshot::MemberTimeoutUpdate::Missing)
                {
                    let unchanged =
                        match snapshot.get_member(&patch.guild_id, &patch.user_id, now_ms) {
                            SnapshotLookup::Found(member) | SnapshotLookup::Stale(member)
                                if member.complete =>
                            {
                                patch
                                    .role_ids
                                    .as_ref()
                                    .is_none_or(|roles| *roles == member.role_ids)
                                    && match patch.timeout {
                                        crate::snapshot::MemberTimeoutUpdate::Missing => true,
                                        crate::snapshot::MemberTimeoutUpdate::Clear => {
                                            member.timeout_until_ms.is_none() && !member.timed_out
                                        }
                                        crate::snapshot::MemberTimeoutUpdate::Until(until) => {
                                            member.timeout_until_ms == Some(until)
                                        }
                                    }
                            }
                            _ => false,
                        };
                    if !unchanged {
                        context.pending(snapshot, &patch.guild_id, &patch.user_id, now_ms);
                    }
                }
            }
            GatewayDispatch::MessageCreate { message, .. }
                if validate_snowflake(&message.guild_id)
                    && (snapshot.member_rest_protected(&message.guild_id, &message.user_id)
                        || snapshot.roles_rest_protected(&message.guild_id)
                        || snapshot.guild_rest_protected(&message.guild_id))
                    && message
                        .member_permissions
                        .as_ref()
                        .is_some_and(|permissions| {
                            permissions.has(crate::permission::BIT_ADMINISTRATOR)
                        })
                    && !matches!(
                        snapshot.get_guild(&message.guild_id, now_ms),
                        SnapshotLookup::Tombstone
                    )
                    && !cached_admin(snapshot, &message.guild_id, &message.user_id, now_ms) =>
            {
                context
                    .hints
                    .push((message.guild_id.clone(), message.user_id.clone()));
            }
            _ => {}
        }
        context
    }

    fn full_channel(
        &mut self,
        snapshot: &mut SnapshotStore,
        channel: &ChannelSnapshot,
        now_ms: u64,
    ) {
        if !snapshot.channel_rest_protected(&channel.channel_id) {
            return;
        }
        let unchanged = match snapshot.get_channel(&channel.channel_id, now_ms) {
            SnapshotLookup::Found(old) | SnapshotLookup::Stale(old) => {
                old.complete && channel.complete && same_channel_config(&old, channel)
            }
            _ => false,
        };
        if !unchanged {
            self.pending_channel(snapshot, &channel.channel_id, now_ms);
        }
    }

    fn pending_channel(&mut self, snapshot: &SnapshotStore, channel_id: &str, now_ms: u64) {
        self.pending_channels.push((
            channel_id.into(),
            snapshot.channel_rest_guard_at(channel_id).unwrap_or(now_ms),
        ));
    }

    fn full_guild(&mut self, snapshot: &mut SnapshotStore, guild: &GuildSnapshot, now_ms: u64) {
        if !snapshot.guild_rest_protected(&guild.guild_id) {
            return;
        }
        let unchanged = match snapshot.get_guild(&guild.guild_id, now_ms) {
            SnapshotLookup::Found(old) | SnapshotLookup::Stale(old) => {
                old.complete
                    && guild.complete
                    && old.unavailable == guild.unavailable
                    && guild
                        .owner_id
                        .as_ref()
                        .is_none_or(|owner| old.owner_id.as_ref() == Some(owner))
            }
            _ => false,
        };
        if !unchanged {
            self.pending_guilds.push((
                guild.guild_id.clone(),
                snapshot
                    .guild_rest_guard_at(&guild.guild_id)
                    .unwrap_or(now_ms),
            ));
        }
    }

    fn full_roles(
        &mut self,
        snapshot: &mut SnapshotStore,
        roles: &GuildRolesSnapshot,
        now_ms: u64,
    ) {
        if !snapshot.roles_rest_protected(&roles.guild_id) {
            return;
        }
        let unchanged = match snapshot.get_guild_roles(&roles.guild_id, now_ms) {
            SnapshotLookup::Found(old) | SnapshotLookup::Stale(old) => {
                old.complete && roles.complete && same_roles(&old.roles, &roles.roles)
            }
            _ => false,
        };
        if !unchanged {
            self.pending_roles(snapshot, &roles.guild_id, now_ms);
        }
    }

    fn pending_roles(&mut self, snapshot: &SnapshotStore, guild_id: &str, now_ms: u64) {
        self.pending_roles.push((
            guild_id.into(),
            snapshot.roles_rest_guard_at(guild_id).unwrap_or(now_ms),
        ));
    }

    fn full_member(&mut self, snapshot: &mut SnapshotStore, member: &MemberSnapshot, now_ms: u64) {
        if !snapshot.member_rest_protected(&member.guild_id, &member.user_id) {
            return;
        }
        let unchanged = match snapshot.get_member(&member.guild_id, &member.user_id, now_ms) {
            SnapshotLookup::Found(old) | SnapshotLookup::Stale(old) => {
                old.complete
                    && member.complete
                    && old.role_ids == member.role_ids
                    && old.timeout_until_ms == member.timeout_until_ms
                    && old.timed_out == member.timed_out
            }
            _ => false,
        };
        if !unchanged {
            self.pending(snapshot, &member.guild_id, &member.user_id, now_ms);
        }
    }

    fn pending(&mut self, snapshot: &SnapshotStore, guild_id: &str, user_id: &str, now_ms: u64) {
        self.pending.push(PendingMember {
            guild_id: guild_id.into(),
            user_id: user_id.into(),
            guard_at_ms: snapshot
                .member_rest_guard_at(guild_id, user_id)
                .unwrap_or(now_ms),
        });
    }

    pub(super) fn apply(self, snapshot: &mut SnapshotStore, now_ms: u64) {
        for member in self.pending {
            snapshot.defer_member_authority(&member.guild_id, &member.user_id, member.guard_at_ms);
        }
        for (guild_id, at_ms) in self.pending_roles {
            snapshot.defer_roles_authority(&guild_id, at_ms);
        }
        for (guild_id, at_ms) in self.pending_guilds {
            snapshot.defer_guild_authority(&guild_id, at_ms);
        }
        for (channel_id, at_ms) in self.pending_channels {
            snapshot.defer_channel_authority(&channel_id, at_ms);
        }
        for (guild_id, user_id) in self.hints {
            snapshot.refresh_member_hint(&guild_id, &user_id, now_ms);
        }
    }
}

fn same_channel_config(left: &ChannelSnapshot, right: &ChannelSnapshot) -> bool {
    fn keyed(
        overwrites: &[ChannelPermissionOverwrite],
    ) -> std::collections::HashMap<(i32, &str), &ChannelPermissionOverwrite> {
        overwrites
            .iter()
            .map(|overwrite| ((overwrite.kind, overwrite.overwrite_id.as_str()), overwrite))
            .collect()
    }
    let left_by_id = keyed(&left.permission_overwrites);
    let right_by_id = keyed(&right.permission_overwrites);
    left.guild_id == right.guild_id
        && left.channel_id == right.channel_id
        && left.channel_type == right.channel_type
        && left.obfuscated == right.obfuscated
        && left.parent_channel_id == right.parent_channel_id
        && left.archived == right.archived
        && left.locked == right.locked
        && left.has_permission_overwrites == right.has_permission_overwrites
        && left_by_id.len() == left.permission_overwrites.len()
        && right_by_id.len() == right.permission_overwrites.len()
        && left_by_id == right_by_id
}

fn same_roles(left: &[RoleSnapshot], right: &[RoleSnapshot]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    fn keyed(roles: &[RoleSnapshot]) -> std::collections::HashMap<&str, &RoleSnapshot> {
        roles
            .iter()
            .map(|role| (role.role_id.as_str(), role))
            .collect()
    }
    let left_by_id = keyed(left);
    let right_by_id = keyed(right);
    left_by_id.len() == left.len() && right_by_id.len() == right.len() && left_by_id == right_by_id
}

fn cached_admin(snapshot: &mut SnapshotStore, guild_id: &str, user_id: &str, now_ms: u64) -> bool {
    if matches!(snapshot.get_guild(guild_id, now_ms), SnapshotLookup::Found(guild) if guild.complete && guild.owner_id.as_deref() == Some(user_id))
    {
        return true;
    }
    let SnapshotLookup::Found(member) = snapshot.get_member(guild_id, user_id, now_ms) else {
        return false;
    };
    let SnapshotLookup::Found(roles) = snapshot.get_guild_roles(guild_id, now_ms) else {
        return false;
    };
    member.complete
        && roles.complete
        && roles.roles.iter().any(|role| {
            role.guild_id == guild_id
                && (role.role_id == guild_id || member.role_ids.contains(&role.role_id))
                && role.permissions.has(crate::permission::BIT_ADMINISTRATOR)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_gateway_entrypoints_preserve_rest_authority_proofs() {
        for direct in [false, true] {
            for kind in 0..4 {
                let mut state = GatewayIngestor::new(16, 100, 100, 32, 32);
                state.ingest_json(r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"0","position":1}],"members":[{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}]}}"#, 0).unwrap();
                state.snapshot_mut().mark_member_rest_result("10", "40", 0);
                state.snapshot_mut().mark_roles_rest_result("10", 0);
                state.snapshot_mut().mark_guild_rest_result("10", 0);
                state
                    .snapshot_mut()
                    .upsert_channel(ChannelSnapshot::new("10", "20", 0, 0));
                state.snapshot_mut().mark_channel_rest_result("20", 0);
                state
                    .snapshot_mut()
                    .upsert_guild_roles(GuildRolesSnapshot::new("11", vec![], 0));
                let raw = match kind {
                    0 => {
                        r#"{"op":0,"s":2,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":"8","position":1}}}"#
                    }
                    1 => {
                        r#"{"op":0,"s":2,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"40"},"roles":["50"]}}"#
                    }
                    2 => r#"{"op":0,"s":2,"t":"GUILD_UPDATE","d":{"id":"10","owner_id":"40"}}"#,
                    _ => {
                        r#"{"op":0,"s":2,"t":"CHANNEL_UPDATE","d":{"id":"20","guild_id":"10","type":0,"permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"1024"}]}}"#
                    }
                };
                let frame = parse_gateway_frame(raw, 1).unwrap();
                if direct {
                    let GatewayFrame::Dispatch(dispatch) = frame else {
                        panic!("expected dispatch")
                    };
                    apply_dispatch_to_snapshot(&dispatch, state.snapshot_mut(), 1);
                } else {
                    state.ingest_frame(frame, 1);
                }
                if kind == 1 {
                    assert!(
                        matches!(state.snapshot_mut().get_member("10", "40", 1), SnapshotLookup::Found(member) if !member.complete)
                    );
                } else if kind == 0 {
                    assert!(
                        matches!(state.snapshot_mut().get_guild_roles("10", 1), SnapshotLookup::Found(roles) if !roles.complete)
                    );
                } else if kind == 2 {
                    assert!(
                        matches!(state.snapshot_mut().get_guild("10", 1), SnapshotLookup::Found(guild) if !guild.complete)
                    );
                } else {
                    assert!(
                        matches!(state.snapshot_mut().get_channel("20", 1), SnapshotLookup::Found(channel) if !channel.complete)
                    );
                }
                assert!(
                    matches!(state.snapshot_mut().get_guild_roles("11", 1), SnapshotLookup::Found(roles) if roles.complete)
                );
                assert!(!state.snapshot().roles_rest_protected("11"));
            }
        }
    }

    #[test]
    fn channel_rest_age_survives_reordered_overwrites_replays_and_cache_gaps() {
        let mut state = GatewayIngestor::new(16, 10, 100, 32, 32);
        let first = r#"{"op":0,"s":1,"t":"CHANNEL_CREATE","d":{"id":"20","guild_id":"10","type":0,"permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"1024"},{"id":"40","type":1,"allow":"0","deny":"0"}]}}"#;
        state.ingest_json(first, 0).unwrap();
        state.snapshot_mut().mark_channel_rest_result("20", 0);
        state.ingest_json(r#"{"op":0,"s":2,"t":"CHANNEL_UPDATE","d":{"id":"20","guild_id":"10","type":0,"parent_id":null,"flags":0,"permission_overwrites":[{"id":"40","type":1,"allow":"0","deny":"0"},{"id":"10","type":0,"allow":"0","deny":"1024"}]}}"#, 9).unwrap();
        assert!(
            matches!(state.snapshot_mut().get_channel("20", 10), SnapshotLookup::Found(channel) if channel.complete)
        );
        assert!(matches!(
            state.snapshot_mut().get_channel("20", 11),
            SnapshotLookup::Stale(_)
        ));
        let changed = r#"{"op":0,"s":3,"t":"CHANNEL_UPDATE","d":{"id":"20","guild_id":"10","type":0,"parent_id":null,"flags":0,"permission_overwrites":[]}}"#;
        state.ingest_json(changed, 12).unwrap();
        let SnapshotLookup::Stale(mut fresh) = state.snapshot_mut().get_channel("20", 12) else {
            panic!("expected expired channel")
        };
        assert!(!fresh.complete);
        fresh.complete = true;
        fresh.captured_at_ms = 12;
        state.snapshot_mut().upsert_channel(fresh);
        state.snapshot_mut().mark_channel_rest_result("20", 12);
        let replay = changed.replace(
            r#""permission_overwrites":[]"#,
            r#""permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"1024"}]"#,
        );
        state.ingest_json(&replay, 13).unwrap();
        assert!(
            matches!(state.snapshot_mut().get_channel("20", 13), SnapshotLookup::Found(channel) if channel.complete && channel.permission_overwrites.is_empty())
        );
        let mut resumed = GatewayIngestor::new(16, 10, 100, 32, 32);
        *resumed.snapshot_mut() = state.snapshot().clone();
        resumed.snapshot_mut().mark_gap();
        resumed.ingest_json(first, 14).unwrap();
        assert!(
            matches!(resumed.snapshot_mut().get_channel("20", 14), SnapshotLookup::Found(channel) if !channel.complete)
        );
    }

    #[test]
    fn guild_rest_age_survives_identical_updates_replays_and_cache_gaps() {
        let mut state = GatewayIngestor::new(16, 10, 100, 32, 32);
        let first = r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70"}}"#;
        state.ingest_json(first, 0).unwrap();
        state.snapshot_mut().mark_guild_rest_result("10", 0);
        state
            .ingest_json(
                r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70"}}"#,
                9,
            )
            .unwrap();
        assert!(
            matches!(state.snapshot_mut().get_guild("10", 10), SnapshotLookup::Found(guild) if guild.complete)
        );
        assert!(matches!(
            state.snapshot_mut().get_guild("10", 11),
            SnapshotLookup::Stale(_)
        ));
        let mut fresh = GuildSnapshot::new("10", false, 12);
        fresh.owner_id = Some("70".into());
        state.snapshot_mut().upsert_guild(fresh.clone());
        state.snapshot_mut().mark_guild_rest_result("10", 12);
        let changed = r#"{"op":0,"s":3,"t":"GUILD_UPDATE","d":{"id":"10","owner_id":"40"}}"#;
        state.ingest_json(changed, 13).unwrap();
        assert!(
            matches!(state.snapshot_mut().get_guild("10", 13), SnapshotLookup::Found(guild) if !guild.complete)
        );
        fresh.captured_at_ms = 14;
        state.snapshot_mut().upsert_guild(fresh);
        state.snapshot_mut().mark_guild_rest_result("10", 14);
        state.ingest_json(changed, 15).unwrap();
        assert!(
            matches!(state.snapshot_mut().get_guild("10", 15), SnapshotLookup::Found(guild) if guild.complete && guild.owner_id.as_deref() == Some("70"))
        );
        let mut resumed = GatewayIngestor::new(16, 10, 100, 32, 32);
        *resumed.snapshot_mut() = state.snapshot().clone();
        resumed.snapshot_mut().mark_gap();
        resumed.ingest_json(first, 16).unwrap();
        assert!(
            matches!(resumed.snapshot_mut().get_guild("10", 16), SnapshotLookup::Found(guild) if !guild.complete)
        );
    }

    #[test]
    fn identical_role_lists_preserve_rest_age_and_replays_preserve_fresh_proofs() {
        let mut state = GatewayIngestor::new(16, 10, 100, 32, 32);
        let first = r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}]}}"#;
        state.ingest_json(first, 0).unwrap();
        state.snapshot_mut().mark_roles_rest_result("10", 0);
        let same = r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","roles":[{"id":"50","permissions":"8","position":1},{"id":"10","permissions":"0","position":0}]}}"#;
        state.ingest_json(same, 9).unwrap();
        assert!(
            matches!(state.snapshot_mut().get_guild_roles("10", 10), SnapshotLookup::Found(roles) if roles.complete)
        );
        assert!(matches!(
            state.snapshot_mut().get_guild_roles("10", 11),
            SnapshotLookup::Stale(_)
        ));
        let changed = r#"{"op":0,"s":3,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":"0","position":2}}}"#;
        state.ingest_json(changed, 11).unwrap();
        assert!(
            matches!(state.snapshot_mut().get_guild_roles("10", 11), SnapshotLookup::Stale(roles) if !roles.complete)
        );
        let mut fresh = match state.snapshot_mut().get_guild_roles("10", 12) {
            SnapshotLookup::Stale(roles) => roles,
            _ => panic!("expected expired roles"),
        };
        fresh.complete = true;
        fresh.captured_at_ms = 12;
        state.snapshot_mut().upsert_guild_roles(fresh);
        state.snapshot_mut().mark_roles_rest_result("10", 12);
        state.ingest_json(changed, 13).unwrap();
        assert!(
            matches!(state.snapshot_mut().get_guild_roles("10", 13), SnapshotLookup::Found(roles) if roles.complete)
        );
        let mut copy = state.snapshot().clone();
        copy.mark_gap();
        assert!(copy.roles_rest_protected("10"));
        let mut resumed = GatewayIngestor::new(16, 10, 100, 32, 32);
        *resumed.snapshot_mut() = copy;
        resumed.ingest_json(first, 14).unwrap();
        assert!(
            matches!(resumed.snapshot_mut().get_guild_roles("10", 14), SnapshotLookup::Found(roles) if !roles.complete)
        );
    }

    #[test]
    fn identical_member_updates_do_not_renew_rest_age_or_affect_other_guilds() {
        let mut state = GatewayIngestor::new(16, 10, 100, 32, 32);
        state
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new("10", "40", vec![], 0));
        state.snapshot_mut().mark_member_rest_result("10", "40", 0);
        state.ingest_json(r#"{"op":0,"s":1,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"40"},"roles":[],"communication_disabled_until":null}}"#, 9).unwrap();
        let SnapshotLookup::Found(member) = state.snapshot_mut().get_member("10", "40", 10) else {
            panic!("expected unchanged valid facts")
        };
        assert!(member.complete);
        assert!(matches!(
            state.snapshot_mut().get_member("10", "40", 11),
            SnapshotLookup::Stale(_)
        ));
        let mut copy = state.snapshot().clone();
        assert!(copy.member_rest_protected("10", "40"));
        assert!(matches!(
            copy.get_member("10", "40", 11),
            SnapshotLookup::Stale(_)
        ));
        state.ingest_json(r#"{"op":0,"s":2,"t":"GUILD_MEMBER_ADD","d":{"guild_id":"11","user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}}"#, 11).unwrap();
        let SnapshotLookup::Found(member) = state.snapshot_mut().get_member("11", "40", 11) else {
            panic!("missing unrelated member")
        };
        assert!(member.complete);
        assert!(!state.snapshot().member_rest_protected("11", "40"));
    }

    #[test]
    fn cache_gaps_cannot_erase_the_need_to_revalidate_rest_guarded_members() {
        let mut state = GatewayIngestor::new(16, 100, 100, 32, 32);
        state
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new("10", "40", vec![], 0));
        state.snapshot_mut().mark_member_rest_result("10", "40", 0);
        state.snapshot_mut().mark_gap();
        state.ingest_json(r#"{"op":0,"s":1,"t":"GUILD_MEMBER_ADD","d":{"guild_id":"10","user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}}"#, 1).unwrap();
        let SnapshotLookup::Found(member) = state.snapshot_mut().get_member("10", "40", 1) else {
            panic!("missing pending member")
        };
        assert!(!member.complete);
    }
}
