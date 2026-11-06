use super::*;

impl SnapshotStore {
    pub(crate) fn channel_rest_protected(&self, channel_id: &str) -> bool {
        self.channels.rest_protected(channel_id)
            || self.channel_tombstones.rest_protected(channel_id)
    }

    pub(crate) fn channel_rest_guard_at(&self, channel_id: &str) -> Option<u64> {
        [
            self.channels.rest_guard_at(channel_id),
            self.channel_tombstones.rest_guard_at(channel_id),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    pub(crate) fn mark_channel_rest_result(&mut self, channel_id: &str, now_ms: u64) {
        if self.channels.contains_key(channel_id) {
            self.channels.mark_rest_guard(channel_id, now_ms);
        } else if self.channel_tombstones.contains_key(channel_id) {
            self.channel_tombstones.mark_rest_guard(channel_id, now_ms);
        } else {
            self.channels.require_rest_revalidation();
        }
    }

    pub(crate) fn defer_channel_authority(&mut self, channel_id: &str, at_ms: u64) {
        self.channel_tombstones.remove(channel_id);
        self.channels
            .update(channel_id, |channel| channel.complete = false);
        self.channels.mark_rest_guard(channel_id, at_ms);
        self.clear_private_thread_access_for_channel(channel_id, at_ms);
    }

    pub(crate) fn gateway_channel_removed(&mut self, channel_id: &str, now_ms: u64) {
        if !self.channel_rest_protected(channel_id) {
            self.tombstone_channel(channel_id, now_ms);
            return;
        }
        if self.channel_tombstones.rest_guard_at(channel_id).is_some() {
            return;
        }
        let at_ms = self.channel_rest_guard_at(channel_id).unwrap_or(now_ms);
        self.defer_channel_authority(channel_id, at_ms);
    }

    pub(crate) fn guild_rest_protected(&self, guild_id: &str) -> bool {
        self.guilds.rest_protected(guild_id) || self.guild_tombstones.rest_protected(guild_id)
    }

    pub(crate) fn guild_rest_guard_at(&self, guild_id: &str) -> Option<u64> {
        [
            self.guilds.rest_guard_at(guild_id),
            self.guild_tombstones.rest_guard_at(guild_id),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    pub(crate) fn mark_guild_rest_result(&mut self, guild_id: &str, now_ms: u64) {
        if self.guilds.contains_key(guild_id) {
            self.guilds.mark_rest_guard(guild_id, now_ms);
        } else if self.guild_tombstones.contains_key(guild_id) {
            self.guild_tombstones.mark_rest_guard(guild_id, now_ms);
        } else {
            self.guilds.require_rest_revalidation();
        }
    }

    pub(crate) fn defer_guild_authority(&mut self, guild_id: &str, at_ms: u64) {
        self.guild_tombstones.remove(guild_id);
        self.guilds.update(guild_id, |guild| {
            guild.complete = false;
            guild.unavailable = false;
        });
        self.guilds.mark_rest_guard(guild_id, at_ms);
        self.clear_private_thread_access_for_guild(guild_id, at_ms);
    }

    pub(crate) fn gateway_guild_removed(&mut self, guild_id: &str, unavailable: bool, now_ms: u64) {
        if !self.guild_rest_protected(guild_id) {
            if unavailable {
                self.mark_guild_unavailable(guild_id, now_ms);
            } else {
                self.tombstone_guild(guild_id, now_ms);
            }
            return;
        }
        if self.guild_tombstones.rest_guard_at(guild_id).is_some() {
            return;
        }
        let at_ms = self.guild_rest_guard_at(guild_id).unwrap_or(now_ms);
        self.defer_guild_authority(guild_id, at_ms);
    }

    pub(crate) fn roles_rest_protected(&self, guild_id: &str) -> bool {
        self.guild_roles.rest_protected(guild_id)
    }

    pub(crate) fn roles_rest_guard_at(&self, guild_id: &str) -> Option<u64> {
        self.guild_roles.rest_guard_at(guild_id)
    }

    pub(crate) fn mark_roles_rest_result(&mut self, guild_id: &str, now_ms: u64) {
        if self.guild_roles.contains_key(guild_id) {
            self.guild_roles.mark_rest_guard(guild_id, now_ms);
        } else {
            self.guild_roles.require_rest_revalidation();
        }
    }

    pub(crate) fn defer_roles_authority(&mut self, guild_id: &str, at_ms: u64) {
        self.guild_roles
            .update(guild_id, |roles| roles.complete = false);
        self.guild_roles.mark_rest_guard(guild_id, at_ms);
    }

    pub(crate) fn gateway_role_removed(&mut self, guild_id: &str, role_id: &str, now_ms: u64) {
        if !self.roles_rest_protected(guild_id) {
            self.remove_guild_role(guild_id, role_id, now_ms);
            return;
        }
        let affected = self
            .members
            .values()
            .filter(|member| {
                member.snapshot.guild_id == guild_id
                    && member.snapshot.role_ids.iter().any(|id| id == role_id)
            })
            .map(|member| member.snapshot.user_id.clone())
            .collect::<Vec<_>>();
        if affected.is_empty()
            && self.guild_roles.get(guild_id).is_some_and(|roles| {
                roles.complete && roles.roles.iter().all(|role| role.role_id != role_id)
            })
        {
            return;
        }
        let at_ms = self.roles_rest_guard_at(guild_id).unwrap_or(now_ms);
        self.defer_roles_authority(guild_id, at_ms);
        for user_id in affected {
            self.defer_member_authority(guild_id, &user_id, at_ms);
        }
    }

    pub(crate) fn member_rest_protected(&self, guild_id: &str, user_id: &str) -> bool {
        let key = member_key(guild_id, user_id);
        self.members.rest_protected(&key)
            || self.member_tombstones.rest_protected(&key)
            || self.user_tombstones.rest_protected(user_id)
    }

    pub(crate) fn member_rest_guard_at(&self, guild_id: &str, user_id: &str) -> Option<u64> {
        let key = member_key(guild_id, user_id);
        [
            self.members.rest_guard_at(&key),
            self.member_tombstones.rest_guard_at(&key),
            self.user_tombstones.rest_guard_at(user_id),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    pub(crate) fn mark_member_rest_result(&mut self, guild_id: &str, user_id: &str, now_ms: u64) {
        let key = member_key(guild_id, user_id);
        if self.members.contains_key(&key) {
            self.members.mark_rest_guard(&key, now_ms);
        } else if self.member_tombstones.contains_key(&key) {
            self.member_tombstones.mark_rest_guard(&key, now_ms);
        } else if self.user_tombstones.contains_key(user_id) {
            self.user_tombstones.mark_rest_guard(user_id, now_ms);
        } else {
            self.members.require_rest_revalidation();
        }
    }

    pub(crate) fn defer_member_authority(&mut self, guild_id: &str, user_id: &str, at_ms: u64) {
        let key = member_key(guild_id, user_id);
        self.members.update(&key, |stored| {
            let member = &mut stored.snapshot;
            member.complete = false;
            member.roles_complete = false;
            member.timeout_known = false;
            member.timeout_until_ms = None;
            member.timed_out = true;
        });
        self.members.mark_rest_guard(&key, at_ms);
    }

    pub(crate) fn gateway_member_removed(&mut self, guild_id: &str, user_id: &str, now_ms: u64) {
        if !self.member_rest_protected(guild_id, user_id) {
            self.tombstone_member(guild_id, user_id, now_ms);
            return;
        }
        let key = member_key(guild_id, user_id);
        if self.member_tombstones.rest_guard_at(&key).is_some()
            || self.user_tombstones.rest_guard_at(user_id).is_some()
        {
            return;
        }
        self.invalidate_owner_membership(guild_id, user_id, now_ms);
        let at_ms = self
            .member_rest_guard_at(guild_id, user_id)
            .unwrap_or(now_ms);
        self.defer_member_authority(guild_id, user_id, at_ms);
    }

    pub(crate) fn gateway_member_unconfirmed(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) {
        if self.member_rest_protected(guild_id, user_id) {
            self.gateway_member_removed(guild_id, user_id, now_ms);
        } else {
            self.discard_member_evidence(guild_id, user_id, now_ms);
        }
    }

    pub(crate) fn refresh_member_hint(&mut self, guild_id: &str, user_id: &str, now_ms: u64) {
        self.member_tombstones
            .remove(&member_key(guild_id, user_id));
        self.user_tombstones.remove(user_id);
        self.defer_member_authority(guild_id, user_id, now_ms);
        self.invalidate_guild(guild_id, now_ms);
        self.invalidate_guild_roles(guild_id, now_ms);
    }
}
