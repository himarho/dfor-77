use crate::memory::HeapSize;
use crate::permission::PermissionSet;

pub(crate) mod archive;
mod authority;
mod cache;
pub(crate) mod channel;
mod debug;
pub(crate) mod member;
pub(crate) mod message;
pub(crate) mod parse;
pub(crate) mod revision;
pub(crate) mod webhook;
use cache::SnapshotCache;
pub use channel::ChannelSnapshotPatch;
use member::StoredMember;
pub use member::{MemberSnapshotPatch, MemberTimeoutUpdate};
use webhook::{StoredMessage, WebhookMessageTarget};

pub(crate) const DEFAULT_SNAPSHOT_MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Eq, PartialEq)]
pub struct MessageSnapshot {
    pub guild_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub user_id: String,
    pub content: String,
    pub content_available: bool,
    pub author_is_bot: bool,
    pub role_ids: Vec<String>,
    pub image_attachment_count: usize,
    pub member_permissions: Option<PermissionSet>,
    pub message_type: i32,
    pub captured_at_ms: u64,
    pub complete: bool,
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct MessageSnapshotPatch {
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub message_id: String,
    pub user_id: Option<String>,
    pub content: Option<String>,
    pub author_is_bot: Option<bool>,
    pub role_ids: Option<Vec<String>>,
    pub image_attachment_count: Option<usize>,
    pub member_permissions: Option<PermissionSet>,
    pub message_type: Option<i32>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ChannelSnapshot {
    pub guild_id: String,
    pub channel_id: String,
    pub channel_type: i32,
    pub obfuscated: bool,
    pub parent_channel_id: Option<String>,
    pub archived: bool,
    pub locked: bool,
    pub current_user_thread_joined: Option<bool>,
    pub current_user_thread_joined_at_ms: Option<u64>,
    pub has_permission_overwrites: bool,
    pub permission_overwrites: Vec<ChannelPermissionOverwrite>,
    pub captured_at_ms: u64,
    pub complete: bool,
}

impl ChannelSnapshot {
    pub fn new(
        guild_id: impl Into<String>,
        channel_id: impl Into<String>,
        channel_type: i32,
        captured_at_ms: u64,
    ) -> Self {
        Self {
            guild_id: guild_id.into(),
            channel_id: channel_id.into(),
            channel_type,
            obfuscated: false,
            parent_channel_id: None,
            archived: false,
            locked: false,
            current_user_thread_joined: None,
            current_user_thread_joined_at_ms: None,
            has_permission_overwrites: false,
            permission_overwrites: Vec::new(),
            captured_at_ms,
            complete: true,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ChannelPermissionOverwrite {
    pub overwrite_id: String,
    pub kind: i32,
    pub allow: PermissionSet,
    pub deny: PermissionSet,
}

#[derive(Clone, Eq, PartialEq)]
pub struct MemberSnapshot {
    pub guild_id: String,
    pub user_id: String,
    pub role_ids: Vec<String>,
    pub timed_out: bool,
    pub timeout_until_ms: Option<u64>,
    pub roles_complete: bool,
    pub timeout_known: bool,
    pub captured_at_ms: u64,
    pub complete: bool,
}

impl MemberSnapshot {
    pub fn new(
        guild_id: impl Into<String>,
        user_id: impl Into<String>,
        role_ids: Vec<String>,
        captured_at_ms: u64,
    ) -> Self {
        Self {
            guild_id: guild_id.into(),
            user_id: user_id.into(),
            role_ids,
            timed_out: false,
            timeout_until_ms: None,
            roles_complete: true,
            timeout_known: true,
            captured_at_ms,
            complete: true,
        }
    }

    pub fn set_timeout_until(&mut self, until_ms: Option<u64>, now_ms: u64) {
        self.timeout_until_ms = until_ms;
        self.timeout_known = true;
        self.timed_out = until_ms.is_some_and(|deadline| deadline > now_ms);
    }

    fn refresh_timeout(&mut self, now_ms: u64) {
        if let Some(deadline) = self.timeout_until_ms {
            self.timed_out = deadline > now_ms;
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RoleSnapshot {
    pub guild_id: String,
    pub role_id: String,
    pub permissions: PermissionSet,
    pub position: i64,
}

#[derive(Clone, Eq, PartialEq)]
pub struct GuildRolesSnapshot {
    pub guild_id: String,
    pub roles: Vec<RoleSnapshot>,
    pub captured_at_ms: u64,
    pub complete: bool,
}

#[derive(Clone, Eq, PartialEq)]
pub struct GuildSnapshot {
    pub guild_id: String,
    pub owner_id: Option<String>,
    pub unavailable: bool,
    pub captured_at_ms: u64,
    pub complete: bool,
}

impl GuildRolesSnapshot {
    pub fn new(guild_id: impl Into<String>, roles: Vec<RoleSnapshot>, captured_at_ms: u64) -> Self {
        Self {
            guild_id: guild_id.into(),
            roles,
            captured_at_ms,
            complete: true,
        }
    }
}

impl GuildSnapshot {
    pub fn new(guild_id: impl Into<String>, unavailable: bool, captured_at_ms: u64) -> Self {
        Self {
            guild_id: guild_id.into(),
            owner_id: None,
            unavailable,
            captured_at_ms,
            complete: true,
        }
    }
}

impl MessageSnapshot {
    pub fn new(
        guild_id: impl Into<String>,
        channel_id: impl Into<String>,
        message_id: impl Into<String>,
        user_id: impl Into<String>,
        captured_at_ms: u64,
    ) -> Self {
        Self {
            guild_id: guild_id.into(),
            channel_id: channel_id.into(),
            message_id: message_id.into(),
            user_id: user_id.into(),
            content: String::new(),
            content_available: true,
            author_is_bot: false,
            role_ids: Vec::new(),
            image_attachment_count: 0,
            member_permissions: None,
            message_type: 0,
            captured_at_ms,
            complete: true,
        }
    }
}

impl HeapSize for MessageSnapshot {
    fn heap_size(&self) -> usize {
        self.guild_id.heap_size()
            + self.channel_id.heap_size()
            + self.message_id.heap_size()
            + self.user_id.heap_size()
            + self.content.heap_size()
            + self.role_ids.heap_size()
            + self.member_permissions.heap_size()
    }
}

impl HeapSize for ChannelSnapshot {
    fn heap_size(&self) -> usize {
        self.guild_id.heap_size()
            + self.channel_id.heap_size()
            + self.parent_channel_id.heap_size()
            + self.permission_overwrites.heap_size()
    }
}

impl HeapSize for ChannelPermissionOverwrite {
    fn heap_size(&self) -> usize {
        self.overwrite_id.heap_size() + self.allow.heap_size() + self.deny.heap_size()
    }
}

impl HeapSize for MemberSnapshot {
    fn heap_size(&self) -> usize {
        self.guild_id.heap_size() + self.user_id.heap_size() + self.role_ids.heap_size()
    }
}

impl HeapSize for RoleSnapshot {
    fn heap_size(&self) -> usize {
        self.guild_id.heap_size() + self.role_id.heap_size() + self.permissions.heap_size()
    }
}

impl HeapSize for GuildRolesSnapshot {
    fn heap_size(&self) -> usize {
        self.guild_id.heap_size() + self.roles.heap_size()
    }
}

impl HeapSize for GuildSnapshot {
    fn heap_size(&self) -> usize {
        self.guild_id.heap_size() + self.owner_id.heap_size()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotLookup<T> {
    Found(T),
    Tombstone,
    Missing,
    Stale(T),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Tombstone;

impl HeapSize for Tombstone {
    fn heap_size(&self) -> usize {
        0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotSettings {
    pub message_ttl_ms: u64,
    pub message_tombstone_ttl_ms: u64,
    pub message_max_entries: usize,
    pub message_tombstone_max_entries: usize,
    pub resource_ttl_ms: u64,
    pub resource_tombstone_ttl_ms: u64,
    pub resource_max_entries: usize,
    pub resource_tombstone_max_entries: usize,
    pub member_ttl_ms: u64,
    pub bot_member_ttl_ms: u64,
    pub thread_member_ttl_ms: u64,
    pub member_tombstone_ttl_ms: u64,
    pub member_max_entries: usize,
    pub member_tombstone_max_entries: usize,
}

impl SnapshotSettings {
    pub fn compact(
        snapshot_ttl_ms: u64,
        tombstone_ttl_ms: u64,
        snapshot_max_entries: usize,
        tombstone_max_entries: usize,
    ) -> Self {
        Self {
            message_ttl_ms: snapshot_ttl_ms,
            message_tombstone_ttl_ms: tombstone_ttl_ms,
            message_max_entries: snapshot_max_entries,
            message_tombstone_max_entries: tombstone_max_entries,
            resource_ttl_ms: snapshot_ttl_ms,
            resource_tombstone_ttl_ms: tombstone_ttl_ms,
            resource_max_entries: snapshot_max_entries,
            resource_tombstone_max_entries: tombstone_max_entries,
            member_ttl_ms: snapshot_ttl_ms,
            bot_member_ttl_ms: snapshot_ttl_ms,
            thread_member_ttl_ms: snapshot_ttl_ms,
            member_tombstone_ttl_ms: tombstone_ttl_ms,
            member_max_entries: snapshot_max_entries,
            member_tombstone_max_entries: tombstone_max_entries,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SnapshotStore {
    message_ttl_ms: u64,
    message_tombstone_ttl_ms: u64,
    max_messages: usize,
    max_bytes: usize,
    max_message_tombstones: usize,
    resource_ttl_ms: u64,
    resource_tombstone_ttl_ms: u64,
    max_resources: usize,
    max_resource_tombstones: usize,
    member_ttl_ms: u64,
    bot_member_ttl_ms: u64,
    thread_member_ttl_ms: u64,
    member_tombstone_ttl_ms: u64,
    max_members: usize,
    max_member_tombstones: usize,
    messages: SnapshotCache<StoredMessage>,
    webhook_originals: SnapshotCache<WebhookMessageTarget>,
    tombstones: SnapshotCache<Tombstone>,
    channels: SnapshotCache<ChannelSnapshot>,
    channel_tombstones: SnapshotCache<Tombstone>,
    members: SnapshotCache<StoredMember>,
    member_tombstones: SnapshotCache<Tombstone>,
    user_tombstones: SnapshotCache<Tombstone>,
    guild_roles: SnapshotCache<GuildRolesSnapshot>,
    guilds: SnapshotCache<GuildSnapshot>,
    guild_tombstones: SnapshotCache<Tombstone>,
}

impl SnapshotStore {
    pub fn new(
        message_ttl_ms: u64,
        tombstone_ttl_ms: u64,
        max_messages: usize,
        max_tombstones: usize,
    ) -> Self {
        Self::with_settings(SnapshotSettings::compact(
            message_ttl_ms,
            tombstone_ttl_ms,
            max_messages,
            max_tombstones,
        ))
    }

    pub fn with_settings(settings: SnapshotSettings) -> Self {
        Self {
            message_ttl_ms: settings.message_ttl_ms.max(1),
            message_tombstone_ttl_ms: settings.message_tombstone_ttl_ms,
            max_messages: settings.message_max_entries.max(1),
            max_bytes: DEFAULT_SNAPSHOT_MAX_BYTES,
            max_message_tombstones: settings.message_tombstone_max_entries.max(1),
            resource_ttl_ms: settings.resource_ttl_ms.max(1),
            resource_tombstone_ttl_ms: settings.resource_tombstone_ttl_ms,
            max_resources: settings.resource_max_entries.max(1),
            max_resource_tombstones: settings.resource_tombstone_max_entries.max(1),
            member_ttl_ms: settings.member_ttl_ms.max(1),
            bot_member_ttl_ms: settings.bot_member_ttl_ms.max(1),
            thread_member_ttl_ms: settings.thread_member_ttl_ms.max(1),
            member_tombstone_ttl_ms: settings.member_tombstone_ttl_ms,
            max_members: settings.member_max_entries.max(1),
            max_member_tombstones: settings.member_tombstone_max_entries.max(1),
            messages: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
            webhook_originals: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
            tombstones: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
            channels: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
            channel_tombstones: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
            members: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
            member_tombstones: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
            user_tombstones: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
            guild_roles: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
            guilds: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
            guild_tombstones: SnapshotCache::with_max_entry_bytes(DEFAULT_SNAPSHOT_MAX_BYTES),
        }
    }

    pub fn mark_gap(&mut self) {
        self.messages.clear();
        self.webhook_originals.clear();
        self.tombstones.clear();
        self.channels.clear();
        self.channel_tombstones.clear();
        self.members.clear();
        self.member_tombstones.clear();
        self.user_tombstones.clear();
        self.guild_roles.clear();
        self.guilds.clear();
        self.guild_tombstones.clear();
    }

    pub(crate) fn invalidate_guild_roles(&mut self, guild_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.guild_roles.remove(guild_id);
        self.guild_tombstones.remove(guild_id);
        self.clear_private_thread_access_for_guild(guild_id, now_ms);
    }

    pub(crate) fn invalidate_guild(&mut self, guild_id: &str, now_ms: u64) {
        self.prune(now_ms);
        let mut snapshot = self
            .guilds
            .remove(guild_id)
            .unwrap_or_else(|| GuildSnapshot::new(guild_id, false, now_ms));
        snapshot.complete = false;
        self.guild_tombstones.remove(guild_id);
        self.clear_private_thread_access_for_guild(guild_id, now_ms);
        self.upsert_guild(snapshot);
    }

    pub(crate) fn invalidate_owner_membership(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) {
        if self
            .guilds
            .get(guild_id)
            .is_some_and(|guild| guild.complete && guild.owner_id.as_deref() == Some(user_id))
        {
            self.invalidate_guild(guild_id, now_ms);
        }
    }

    pub(crate) fn invalidate_channel(&mut self, channel_id: &str, now_ms: u64) {
        self.clear_private_thread_access_for_channel(channel_id, now_ms);
        self.channels.remove(channel_id);
        self.channel_tombstones.remove(channel_id);
    }

    pub(crate) fn invalidate_channel_state(&mut self, channel_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.clear_private_thread_access_for_channel(channel_id, now_ms);
        self.channels
            .update(channel_id, |channel| channel.complete = false);
    }

    pub(crate) fn invalidate_channel_evidence(
        &mut self,
        invalid_channels: &std::collections::HashSet<String>,
        unverified_members: &std::collections::HashSet<String>,
        now_ms: u64,
    ) {
        if invalid_channels.is_empty() && unverified_members.is_empty() {
            return;
        }
        self.prune(now_ms);
        self.channels.for_each_mut(|channel| {
            let invalid = invalid_channels.contains(&channel.channel_id);
            if invalid {
                channel.complete = false;
            }
            if invalid
                || unverified_members.contains(&channel.channel_id)
                || channel.channel_type == 12
                    && channel
                        .parent_channel_id
                        .as_ref()
                        .is_some_and(|parent| invalid_channels.contains(parent))
            {
                channel.current_user_thread_joined = None;
                channel.current_user_thread_joined_at_ms = None;
            }
        });
    }

    pub(crate) fn invalidate_guild_channels(&mut self, guild_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.channels
            .retain(|_, channel| channel.guild_id != guild_id);
    }

    pub(crate) fn upsert_guild_channels(
        &mut self,
        guild_id: &str,
        channels: impl IntoIterator<Item = ChannelSnapshot>,
        now_ms: u64,
    ) {
        let mut invalid = std::collections::HashSet::new();
        for channel in channels {
            if channel.guild_id != guild_id
                || self
                    .channels
                    .get(&channel.channel_id)
                    .is_some_and(|known| known.guild_id != guild_id)
            {
                invalid.insert(channel.channel_id);
            } else {
                self.upsert_channel(channel);
            }
        }
        self.invalidate_channel_evidence(&invalid, &std::collections::HashSet::new(), now_ms);
    }

    pub(crate) fn revalidate_guild_channels(
        &mut self,
        guild_id: &str,
        returned: &std::collections::HashSet<String>,
        invalid_channels: &std::collections::HashSet<String>,
        channel_scope: bool,
        thread_scope: bool,
        now_ms: u64,
    ) {
        if !channel_scope && !thread_scope && invalid_channels.is_empty() {
            return;
        }
        self.prune(now_ms);
        let mut invalid = invalid_channels.clone();
        invalid.extend(
            self.channels
                .values()
                .filter(|channel| {
                    channel.guild_id == guild_id
                        && !returned.contains(&channel.channel_id)
                        && if matches!(channel.channel_type, 10..=12) {
                            thread_scope && !channel.archived
                        } else {
                            channel_scope
                        }
                })
                .map(|channel| channel.channel_id.clone()),
        );
        self.channels.for_each_mut(|channel| {
            let affected = invalid.contains(&channel.channel_id);
            if affected {
                channel.complete = false;
            }
            if affected
                || channel.channel_type == 12
                    && !returned.contains(&channel.channel_id)
                    && channel
                        .parent_channel_id
                        .as_ref()
                        .is_some_and(|parent| invalid.contains(parent))
            {
                channel.current_user_thread_joined = None;
                channel.current_user_thread_joined_at_ms = None;
            }
        });
    }

    pub(crate) fn revalidate_guild_members(
        &mut self,
        guild_id: &str,
        returned: &std::collections::HashSet<String>,
        invalid: &std::collections::HashSet<String>,
        unknown_members: bool,
        now_ms: u64,
    ) {
        if !unknown_members && invalid.is_empty() {
            return;
        }
        self.prune(now_ms);
        self.members.for_each_mut(|stored| {
            let member = &mut stored.snapshot;
            if member.guild_id == guild_id
                && (invalid.contains(&member.user_id)
                    || unknown_members && !returned.contains(&member.user_id))
            {
                member.complete = false;
                member.roles_complete = false;
                member.timeout_known = false;
                member.timeout_until_ms = None;
                member.timed_out = true;
            }
        });
    }

    pub(crate) fn invalidate_guild_members(&mut self, guild_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.members
            .retain(|_, member| member.snapshot.guild_id != guild_id);
    }

    pub(crate) fn invalidate_member(&mut self, guild_id: &str, user_id: &str, now_ms: u64) {
        self.prune(now_ms);
        let key = member_key(guild_id, user_id);
        self.members.remove(&key);
        self.member_tombstones.remove(&key);
        self.user_tombstones.remove(user_id);
        self.guild_tombstones.remove(guild_id);
    }

    pub(crate) fn discard_member_evidence(&mut self, guild_id: &str, user_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.members.remove(&member_key(guild_id, user_id));
        self.invalidate_owner_membership(guild_id, user_id, now_ms);
    }

    pub fn upsert_guild(&mut self, snapshot: GuildSnapshot) {
        self.prune(snapshot.captured_at_ms);
        let mut snapshot = snapshot;
        let key = snapshot.guild_id.clone();
        if snapshot.owner_id.is_none() {
            if let Some(stored) = self
                .guilds
                .get(&key)
                .filter(|guild| guild.owner_id.is_some())
            {
                snapshot.owner_id = stored.owner_id.clone();
                snapshot.captured_at_ms = stored.captured_at_ms;
                snapshot.complete &= stored.complete;
            }
        }
        let owner_changed = self.guilds.get(&key).is_some_and(|stored| {
            stored.owner_id.is_some()
                && snapshot.owner_id.is_some()
                && stored.owner_id != snapshot.owner_id
        });
        if owner_changed {
            self.clear_private_thread_access_for_guild(&key, snapshot.captured_at_ms);
        }
        self.guild_tombstones.remove(&key);
        self.guilds.insert(key, snapshot);
        self.enforce_limits();
    }

    pub fn mark_guild_available(&mut self, guild_id: &str, now_ms: u64) {
        self.upsert_guild(GuildSnapshot::new(guild_id, false, now_ms));
    }

    pub fn mark_guild_unavailable(&mut self, guild_id: &str, now_ms: u64) {
        self.upsert_guild(GuildSnapshot::new(guild_id, true, now_ms));
    }

    pub fn tombstone_guild(&mut self, guild_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.guilds.remove(guild_id);
        self.guild_roles.remove(guild_id);
        self.members
            .retain(|_, member| member.snapshot.guild_id.as_str() != guild_id);
        self.member_tombstones
            .retain(|key, _| !key.starts_with(&format!("{}:", guild_id)));
        self.channels
            .retain(|_, channel| channel.guild_id.as_str() != guild_id);
        self.messages
            .retain(|_, message| message.guild_id.as_str() != guild_id);
        if self.resource_tombstone_ttl_ms == 0 {
            self.enforce_limits();
            return;
        }
        self.guild_tombstones.insert_expiring(
            guild_id.to_string(),
            Tombstone,
            Some(now_ms.saturating_add(self.resource_tombstone_ttl_ms)),
        );
        self.enforce_limits();
    }

    pub fn get_guild(&mut self, guild_id: &str, now_ms: u64) -> SnapshotLookup<GuildSnapshot> {
        self.prune(now_ms);
        if self.guild_tombstones.contains_key(guild_id) {
            return SnapshotLookup::Tombstone;
        }
        let Some(snapshot) = self.guilds.get(guild_id).cloned() else {
            return SnapshotLookup::Missing;
        };
        if now_ms.saturating_sub(snapshot.captured_at_ms) > self.resource_ttl_ms
            || self
                .guilds
                .rest_guard_at(guild_id)
                .is_some_and(|at_ms| now_ms.saturating_sub(at_ms) > self.resource_ttl_ms)
        {
            SnapshotLookup::Stale(snapshot)
        } else {
            SnapshotLookup::Found(snapshot)
        }
    }

    pub fn upsert_message(&mut self, snapshot: MessageSnapshot) {
        self.prune(snapshot.captured_at_ms);
        let key = message_key(&snapshot.channel_id, &snapshot.message_id);
        self.tombstones.remove(&key);
        self.messages.insert(key, snapshot.into());
        self.enforce_limits();
    }

    pub fn patch_message(&mut self, patch: MessageSnapshotPatch, now_ms: u64) {
        self.patch_message_checked(patch, now_ms);
    }

    pub(crate) fn patch_message_checked(
        &mut self,
        patch: MessageSnapshotPatch,
        now_ms: u64,
    ) -> bool {
        self.prune(now_ms);
        let key = message_key(&patch.channel_id, &patch.message_id);
        if self.tombstones.contains_key(&key) {
            return true;
        }
        if !self.message_identity_matches(
            &patch.channel_id,
            &patch.message_id,
            patch.guild_id.as_deref(),
            patch.user_id.as_deref(),
        ) {
            self.messages.remove(&key);
            return false;
        }
        let full_update = patch.user_id.is_some()
            && patch.message_type.is_some()
            && patch.image_attachment_count.is_some()
            && patch.content.is_some();
        let dm_channel = self.channels.get(&patch.channel_id).is_some_and(|channel| {
            channel.complete && channel.channel_type == 1 && channel.guild_id.is_empty()
        });
        if !self.messages.contains_key(&key) {
            let mut snapshot = MessageSnapshot::new(
                patch.guild_id.clone().unwrap_or_default(),
                patch.channel_id.clone(),
                patch.message_id.clone(),
                patch.user_id.clone().unwrap_or_default(),
                now_ms,
            );
            snapshot.complete = false;
            snapshot.content_available = false;
            self.messages.insert(key.clone(), snapshot.into());
        }
        self.messages.update(&key, |snapshot| {
            if let Some(guild_id) = patch.guild_id {
                snapshot.guild_id = guild_id;
            }
            if let Some(user_id) = patch.user_id {
                snapshot.user_id = user_id;
            }
            if let Some(content) = patch.content {
                snapshot.content = content;
                snapshot.content_available = true;
            }
            if let Some(author_is_bot) = patch.author_is_bot {
                snapshot.author_is_bot = author_is_bot;
            }
            if let Some(role_ids) = patch.role_ids {
                snapshot.role_ids = role_ids;
                snapshot.member_permissions = None;
            }
            if let Some(count) = patch.image_attachment_count {
                snapshot.image_attachment_count = count;
            }
            if let Some(member_permissions) = patch.member_permissions {
                snapshot.member_permissions = Some(member_permissions);
            }
            if let Some(message_type) = patch.message_type {
                snapshot.message_type = message_type;
            }
            if full_update && (!snapshot.guild_id.is_empty() || dm_channel) {
                snapshot.captured_at_ms = now_ms;
                snapshot.complete = true;
            }
        });
        self.enforce_limits();
        true
    }

    pub fn tombstone_message(&mut self, channel_id: &str, message_id: &str, now_ms: u64) {
        self.prune(now_ms);
        let key = message_key(channel_id, message_id);
        self.messages.remove(&key);
        if self.message_tombstone_ttl_ms == 0 {
            self.enforce_limits();
            return;
        }
        self.tombstones.insert_expiring(
            key,
            Tombstone,
            Some(now_ms.saturating_add(self.message_tombstone_ttl_ms)),
        );
        self.enforce_limits();
    }

    pub(crate) fn invalidate_message(&mut self, channel_id: &str, message_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.messages.remove(&message_key(channel_id, message_id));
    }

    pub(crate) fn invalidate_channel_messages(&mut self, channel_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.messages
            .retain(|_, message| message.channel_id != channel_id);
    }

    pub fn tombstone_bulk<'a>(
        &mut self,
        channel_id: &str,
        message_ids: impl IntoIterator<Item = &'a str>,
        now_ms: u64,
    ) {
        for message_id in message_ids {
            self.tombstone_message(channel_id, message_id, now_ms);
        }
    }

    pub fn upsert_channel(&mut self, snapshot: ChannelSnapshot) {
        self.prune(snapshot.captured_at_ms);
        let key = snapshot.channel_id.clone();
        self.channel_tombstones.remove(&key);
        self.channels.insert(key, snapshot);
        self.enforce_limits();
    }

    pub fn tombstone_channel(&mut self, channel_id: &str, now_ms: u64) {
        self.invalidate_channel_messages(channel_id, now_ms);
        self.channels.remove(channel_id);
        if self.resource_tombstone_ttl_ms == 0 {
            self.enforce_limits();
            return;
        }
        self.channel_tombstones.insert_expiring(
            channel_id.to_string(),
            Tombstone,
            Some(now_ms.saturating_add(self.resource_tombstone_ttl_ms)),
        );
        self.enforce_limits();
    }

    pub fn patch_channel(&mut self, patch: ChannelSnapshotPatch, now_ms: u64) {
        self.prune(now_ms);
        if self.channel_tombstones.contains_key(&patch.channel_id) {
            return;
        }
        let key = patch.channel_id.clone();
        let complete = patch.has_complete_state();
        if !self.channels.contains_key(&key) {
            let mut snapshot = ChannelSnapshot::new(
                patch.guild_id.as_deref().unwrap_or(""),
                &key,
                patch.channel_type.unwrap_or(0),
                now_ms,
            );
            snapshot.complete = false;
            self.channels.insert(key.clone(), snapshot);
        }
        self.channels.update(&key, |snapshot| {
            patch.apply(snapshot, now_ms);
            if complete {
                snapshot.complete = true;
                snapshot.captured_at_ms = now_ms;
            }
        });
        self.enforce_limits();
    }

    pub fn set_current_user_thread_joined(
        &mut self,
        channel_id: &str,
        joined: bool,
        now_ms: u64,
    ) -> bool {
        self.prune(now_ms);
        if self.channel_tombstones.contains_key(channel_id) {
            return false;
        }
        self.channels
            .update(channel_id, |snapshot| {
                snapshot.current_user_thread_joined = Some(joined);
                snapshot.current_user_thread_joined_at_ms = Some(now_ms);
            })
            .is_some()
    }

    pub fn clear_private_thread_access_for_channel(&mut self, channel_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.channels.for_each_mut(|snapshot| {
            if snapshot.channel_type == 12
                && (snapshot.channel_id == channel_id
                    || snapshot.parent_channel_id.as_deref() == Some(channel_id))
            {
                snapshot.current_user_thread_joined = None;
                snapshot.current_user_thread_joined_at_ms = None;
            }
        });
    }

    pub(crate) fn clear_current_user_thread_joined(&mut self, channel_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.channels.update(channel_id, |channel| {
            channel.current_user_thread_joined = None;
            channel.current_user_thread_joined_at_ms = None;
        });
    }

    pub(crate) fn reconcile_active_threads(
        &mut self,
        guild_id: &str,
        parents: Option<&std::collections::HashSet<String>>,
        returned: &std::collections::HashSet<String>,
        now_ms: u64,
    ) {
        self.prune(now_ms);
        self.channels.for_each_mut(|channel| {
            if channel.guild_id == guild_id
                && matches!(channel.channel_type, 10..=12)
                && !channel.archived
                && parents.is_none_or(|parents| {
                    channel
                        .parent_channel_id
                        .as_ref()
                        .is_some_and(|parent| parents.contains(parent))
                })
                && !returned.contains(&channel.channel_id)
            {
                channel.complete = false;
                channel.current_user_thread_joined = None;
                channel.current_user_thread_joined_at_ms = None;
            }
        });
    }

    pub fn clear_private_thread_access_for_guild(&mut self, guild_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.channels.for_each_mut(|snapshot| {
            if snapshot.channel_type == 12 && snapshot.guild_id == guild_id {
                snapshot.current_user_thread_joined = None;
                snapshot.current_user_thread_joined_at_ms = None;
            }
        });
    }

    pub fn get_channel(
        &mut self,
        channel_id: &str,
        now_ms: u64,
    ) -> SnapshotLookup<ChannelSnapshot> {
        self.prune(now_ms);
        if self.channel_tombstones.contains_key(channel_id) {
            return SnapshotLookup::Tombstone;
        }
        self.channels.update(channel_id, |snapshot| {
            if snapshot
                .current_user_thread_joined_at_ms
                .is_some_and(|captured_at| {
                    now_ms.saturating_sub(captured_at) > self.thread_member_ttl_ms
                })
            {
                snapshot.current_user_thread_joined = None;
                snapshot.current_user_thread_joined_at_ms = None;
            }
        });
        let Some(snapshot) = self.channels.get(channel_id).cloned() else {
            return SnapshotLookup::Missing;
        };
        if now_ms.saturating_sub(snapshot.captured_at_ms) > self.resource_ttl_ms
            || self
                .channels
                .rest_guard_at(channel_id)
                .is_some_and(|at_ms| now_ms.saturating_sub(at_ms) > self.resource_ttl_ms)
        {
            SnapshotLookup::Stale(snapshot)
        } else {
            SnapshotLookup::Found(snapshot)
        }
    }

    pub fn upsert_member(&mut self, snapshot: MemberSnapshot) {
        self.prune(snapshot.captured_at_ms);
        let key = member_key(&snapshot.guild_id, &snapshot.user_id);
        self.member_tombstones.remove(&key);
        self.user_tombstones.remove(&snapshot.user_id);
        self.members.insert(key, StoredMember::new(snapshot));
        self.enforce_limits();
    }

    pub fn patch_member(&mut self, patch: MemberSnapshotPatch, now_ms: u64) {
        self.prune(now_ms);
        let key = member_key(&patch.guild_id, &patch.user_id);
        if self.member_tombstones.contains_key(&key)
            || self.user_tombstones.contains_key(&patch.user_id)
        {
            return;
        }
        if !self.members.contains_key(&key) {
            let mut member =
                MemberSnapshot::new(&patch.guild_id, &patch.user_id, Vec::new(), now_ms);
            member.complete = false;
            member.roles_complete = false;
            member.timeout_known = false;
            self.members.insert(key.clone(), StoredMember::new(member));
        }
        self.members.update(&key, |member| {
            member.patch(patch, now_ms);
        });
        self.enforce_limits();
    }

    pub fn tombstone_member(&mut self, guild_id: &str, user_id: &str, now_ms: u64) {
        self.prune(now_ms);
        self.invalidate_owner_membership(guild_id, user_id, now_ms);
        let key = member_key(guild_id, user_id);
        self.members.remove(&key);
        if self.member_tombstone_ttl_ms == 0 {
            self.enforce_limits();
            return;
        }
        self.member_tombstones.insert_expiring(
            key,
            Tombstone,
            Some(now_ms.saturating_add(self.member_tombstone_ttl_ms)),
        );
        self.enforce_limits();
    }

    pub fn tombstone_user(&mut self, user_id: &str, now_ms: u64) {
        self.prune(now_ms);
        let owned_guilds = self
            .guilds
            .values()
            .filter(|guild| guild.complete && guild.owner_id.as_deref() == Some(user_id))
            .map(|guild| guild.guild_id.clone())
            .collect::<Vec<_>>();
        for guild_id in owned_guilds {
            self.invalidate_owner_membership(&guild_id, user_id, now_ms);
        }
        self.members
            .retain(|_, member| member.snapshot.user_id.as_str() != user_id);
        let member_suffix = format!(":{}", user_id);
        self.member_tombstones
            .retain(|key, _| !key.ends_with(&member_suffix));
        if self.resource_tombstone_ttl_ms == 0 {
            self.enforce_limits();
            return;
        }
        self.user_tombstones.insert_expiring(
            user_id.to_string(),
            Tombstone,
            Some(now_ms.saturating_add(self.resource_tombstone_ttl_ms)),
        );
        self.enforce_limits();
    }

    pub fn is_user_tombstoned(&mut self, user_id: &str, now_ms: u64) -> bool {
        self.prune(now_ms);
        self.user_tombstones.contains_key(user_id)
    }

    pub fn get_member(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> SnapshotLookup<MemberSnapshot> {
        self.member_lookup(guild_id, user_id, now_ms, self.member_ttl_ms)
    }

    pub fn get_bot_member(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> SnapshotLookup<MemberSnapshot> {
        self.member_lookup(guild_id, user_id, now_ms, self.bot_member_ttl_ms)
    }

    fn member_lookup(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> SnapshotLookup<MemberSnapshot> {
        self.prune(now_ms);
        let key = member_key(guild_id, user_id);
        if self.member_tombstones.contains_key(&key) {
            return SnapshotLookup::Tombstone;
        }
        let Some(member) = self.members.get(&key) else {
            return SnapshotLookup::Missing;
        };
        let stale = member.stale(now_ms, ttl_ms)
            || self
                .members
                .rest_guard_at(&key)
                .is_some_and(|at_ms| now_ms.saturating_sub(at_ms) > ttl_ms);
        let mut snapshot = member.snapshot.clone();
        snapshot.refresh_timeout(now_ms);
        if stale {
            SnapshotLookup::Stale(snapshot)
        } else {
            SnapshotLookup::Found(snapshot)
        }
    }

    pub fn upsert_guild_roles(&mut self, snapshot: GuildRolesSnapshot) {
        self.prune(snapshot.captured_at_ms);
        let key = snapshot.guild_id.clone();
        self.guild_roles.insert(key, snapshot);
        self.enforce_limits();
    }

    pub fn upsert_guild_role(&mut self, role: RoleSnapshot, now_ms: u64) {
        self.prune(now_ms);
        let key = role.guild_id.clone();
        if self.guild_roles.contains_key(&key) {
            self.guild_roles.update(&key, |snapshot| {
                snapshot
                    .roles
                    .retain(|stored| stored.role_id != role.role_id);
                snapshot.roles.push(role);
            });
        } else {
            let mut snapshot = GuildRolesSnapshot::new(&key, vec![role], now_ms);
            snapshot.complete = false;
            self.guild_roles.insert(key, snapshot);
        }
        self.enforce_limits();
    }

    pub fn remove_guild_role(&mut self, guild_id: &str, role_id: &str, now_ms: u64) {
        self.prune(now_ms);
        if self.guild_roles.contains_key(guild_id) {
            self.guild_roles.update(guild_id, |snapshot| {
                snapshot.roles.retain(|role| role.role_id != role_id);
            });
        } else {
            let mut snapshot = GuildRolesSnapshot::new(guild_id, Vec::new(), now_ms);
            snapshot.complete = false;
            self.guild_roles.insert(guild_id.to_string(), snapshot);
        }
        self.members.for_each_mut(|member| {
            let member = &mut member.snapshot;
            if member.guild_id == guild_id {
                member.role_ids.retain(|stored| stored != role_id);
            }
        });
        self.enforce_limits();
    }

    pub fn get_guild_roles(
        &mut self,
        guild_id: &str,
        now_ms: u64,
    ) -> SnapshotLookup<GuildRolesSnapshot> {
        self.prune(now_ms);
        let Some(snapshot) = self.guild_roles.get(guild_id).cloned() else {
            return SnapshotLookup::Missing;
        };
        if now_ms.saturating_sub(snapshot.captured_at_ms) > self.resource_ttl_ms
            || self
                .guild_roles
                .rest_guard_at(guild_id)
                .is_some_and(|at_ms| now_ms.saturating_sub(at_ms) > self.resource_ttl_ms)
        {
            SnapshotLookup::Stale(snapshot)
        } else {
            SnapshotLookup::Found(snapshot)
        }
    }

    pub fn get_message(
        &mut self,
        channel_id: &str,
        message_id: &str,
        now_ms: u64,
    ) -> SnapshotLookup<MessageSnapshot> {
        self.prune(now_ms);
        let key = message_key(channel_id, message_id);
        if self.tombstones.contains_key(&key) {
            return SnapshotLookup::Tombstone;
        }
        let Some(snapshot) = self.messages.get(&key).map(|message| (**message).clone()) else {
            return SnapshotLookup::Missing;
        };
        if now_ms.saturating_sub(snapshot.captured_at_ms) > self.message_ttl_ms {
            SnapshotLookup::Stale(snapshot)
        } else {
            SnapshotLookup::Found(snapshot)
        }
    }

    pub fn messages_for_user(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> Vec<MessageSnapshot> {
        self.message_refs_for_user(guild_id, user_id, now_ms)
            .cloned()
            .collect()
    }

    pub(crate) fn message_refs_for_user<'a>(
        &'a mut self,
        guild_id: &'a str,
        user_id: &'a str,
        now_ms: u64,
    ) -> impl Iterator<Item = &'a MessageSnapshot> {
        self.prune(now_ms);
        let ttl_ms = self.message_ttl_ms;
        self.messages
            .values()
            .map(|message| &**message)
            .filter(move |snapshot| {
                snapshot.guild_id == guild_id
                    && snapshot.user_id == user_id
                    && now_ms.saturating_sub(snapshot.captured_at_ms) <= ttl_ms
            })
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn tombstone_count(&self) -> usize {
        self.tombstones.len()
    }

    pub fn set_max_bytes(&mut self, max_bytes: usize) {
        self.max_bytes = max_bytes.max(1);
        self.messages.set_max_entry_bytes(self.max_bytes);
        self.webhook_originals.set_max_entry_bytes(self.max_bytes);
        self.tombstones.set_max_entry_bytes(self.max_bytes);
        self.channels.set_max_entry_bytes(self.max_bytes);
        self.channel_tombstones.set_max_entry_bytes(self.max_bytes);
        self.members.set_max_entry_bytes(self.max_bytes);
        self.member_tombstones.set_max_entry_bytes(self.max_bytes);
        self.user_tombstones.set_max_entry_bytes(self.max_bytes);
        self.guild_roles.set_max_entry_bytes(self.max_bytes);
        self.guilds.set_max_entry_bytes(self.max_bytes);
        self.guild_tombstones.set_max_entry_bytes(self.max_bytes);

        self.enforce_limits();
    }

    pub fn retained_bytes(&self) -> usize {
        [
            self.messages.bytes(),
            self.tombstones.bytes(),
            self.channels.bytes(),
            self.channel_tombstones.bytes(),
            self.members.bytes(),
            self.member_tombstones.bytes(),
            self.user_tombstones.bytes(),
            self.guild_roles.bytes(),
            self.guilds.bytes(),
            self.guild_tombstones.bytes(),
            self.webhook_originals.bytes(),
        ]
        .into_iter()
        .fold(0, usize::saturating_add)
    }

    fn prune(&mut self, now_ms: u64) {
        self.webhook_originals.expire_before(now_ms);
        self.tombstones.expire_before(now_ms);
        self.channel_tombstones.expire_before(now_ms);
        self.member_tombstones.expire_before(now_ms);
        self.user_tombstones.expire_before(now_ms);
        self.guild_tombstones.expire_before(now_ms);
    }

    fn enforce_limits(&mut self) {
        while self.webhook_originals.len() > self.max_messages {
            self.webhook_originals.pop_oldest();
        }
        while self.messages.len() > self.max_messages {
            self.messages.pop_oldest();
        }
        while self.tombstones.len() > self.max_message_tombstones {
            self.tombstones.pop_oldest();
        }
        while self.channels.len() > self.max_resources {
            self.channels.pop_oldest();
        }
        while self.channel_tombstones.len() > self.max_resource_tombstones {
            self.channel_tombstones.pop_oldest();
        }
        while self.members.len() > self.max_members {
            self.members.pop_oldest();
        }
        while self.member_tombstones.len() > self.max_member_tombstones {
            self.member_tombstones.pop_oldest();
        }
        while self.user_tombstones.len() > self.max_resource_tombstones {
            self.user_tombstones.pop_oldest();
        }
        while self.guild_roles.len() > self.max_resources {
            self.guild_roles.pop_oldest();
        }
        while self.guilds.len() > self.max_resources {
            self.guilds.pop_oldest();
        }
        while self.guild_tombstones.len() > self.max_resource_tombstones {
            self.guild_tombstones.pop_oldest();
        }
        while self.retained_bytes() > self.max_bytes {
            let largest = [
                self.messages.bytes(),
                self.tombstones.bytes(),
                self.channels.bytes(),
                self.channel_tombstones.bytes(),
                self.members.bytes(),
                self.member_tombstones.bytes(),
                self.user_tombstones.bytes(),
                self.guild_roles.bytes(),
                self.guilds.bytes(),
                self.guild_tombstones.bytes(),
                self.webhook_originals.bytes(),
            ]
            .into_iter()
            .enumerate()
            .max_by_key(|(_, bytes)| *bytes)
            .map(|(index, _)| index);
            match largest {
                Some(0) => {
                    self.messages.pop_oldest();
                }
                Some(1) => {
                    self.tombstones.pop_oldest();
                }
                Some(2) => {
                    self.channels.pop_oldest();
                }
                Some(3) => {
                    self.channel_tombstones.pop_oldest();
                }
                Some(4) => {
                    self.members.pop_oldest();
                }
                Some(5) => {
                    self.member_tombstones.pop_oldest();
                }
                Some(6) => {
                    self.user_tombstones.pop_oldest();
                }
                Some(7) => {
                    self.guild_roles.pop_oldest();
                }
                Some(8) => {
                    self.guilds.pop_oldest();
                }
                Some(9) => {
                    self.guild_tombstones.pop_oldest();
                }
                Some(10) => {
                    self.webhook_originals.pop_oldest();
                }
                _ => break,
            }
        }
    }
}

fn message_key(channel_id: &str, message_id: &str) -> String {
    format!("{}:{}", channel_id, message_id)
}

fn member_key(guild_id: &str, user_id: &str) -> String {
    format!("{}:{}", guild_id, user_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_membership_removal_invalidates_ownership_even_without_negative_caching() {
        for ttl in [0, 100] {
            for global in [false, true] {
                let mut store = SnapshotStore::new(1000, ttl, 10, 10);
                for (guild_id, owner_id) in [("10", "40"), ("11", "40"), ("12", "41")] {
                    let mut guild = GuildSnapshot::new(guild_id, false, 0);
                    guild.owner_id = Some(owner_id.into());
                    store.upsert_guild(guild);
                }
                if global {
                    store.tombstone_user("40", 1);
                } else {
                    store.tombstone_member("10", "40", 1);
                }
                for guild_id in ["10", "11", "12"] {
                    let SnapshotLookup::Found(guild) = store.get_guild(guild_id, 1) else {
                        panic!("expected guild context");
                    };
                    assert_eq!(
                        guild.complete,
                        guild_id == "12" || guild_id == "11" && !global,
                        "ttl={ttl} global={global} guild={guild_id}"
                    );
                }
            }
        }
    }

    #[test]
    fn partial_guild_updates_do_not_renew_or_complete_retained_owner_information() {
        for complete in [false, true] {
            let mut store = SnapshotStore::new(100, 100, 10, 10);
            let mut guild = GuildSnapshot::new("10", false, 0);
            guild.owner_id = Some("99".into());
            guild.complete = complete;
            store.upsert_guild(guild);
            store.mark_guild_unavailable("10", 50);
            let SnapshotLookup::Found(guild) = store.get_guild("10", 50) else {
                panic!("fresh ownership must be retained");
            };
            assert!(guild.unavailable);
            assert_eq!(guild.owner_id.as_deref(), Some("99"));
            assert_eq!(guild.captured_at_ms, 0);
            assert_eq!(guild.complete, complete);
            store.mark_guild_available("10", 101);
            let SnapshotLookup::Stale(guild) = store.get_guild("10", 101) else {
                panic!("availability must not renew ownership");
            };
            assert!(!guild.unavailable);
            assert_eq!(guild.owner_id.as_deref(), Some("99"));
            let mut update = GuildSnapshot::new("10", false, 101);
            update.owner_id = Some("98".into());
            store.upsert_guild(update);
            let SnapshotLookup::Found(guild) = store.get_guild("10", 101) else {
                panic!("explicit ownership must renew the snapshot");
            };
            assert!(guild.complete);
            assert_eq!(guild.owner_id.as_deref(), Some("98"));
            assert_eq!(guild.captured_at_ms, 101);
        }
    }

    #[test]
    fn ownership_changes_invalidate_derived_private_thread_access() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let mut guild = GuildSnapshot::new("10", false, 0);
        guild.owner_id = Some("99".into());
        store.upsert_guild(guild.clone());
        store.upsert_channel(ChannelSnapshot::new("10", "30", 12, 0));
        assert!(store.set_current_user_thread_joined("30", true, 1));
        guild.owner_id = Some("98".into());
        guild.captured_at_ms = 2;
        store.upsert_guild(guild);
        let SnapshotLookup::Found(channel) = store.get_channel("30", 2) else {
            panic!("thread state must remain available");
        };
        assert_eq!(channel.current_user_thread_joined, None);
        assert_eq!(channel.current_user_thread_joined_at_ms, None);
    }

    #[test]
    fn role_changes_do_not_refresh_the_age_of_the_whole_role_list() {
        for remove in [false, true] {
            let mut store = SnapshotStore::new(100, 100, 10, 10);
            let everyone = RoleSnapshot {
                guild_id: "10".into(),
                role_id: "10".into(),
                permissions: PermissionSet::from_bits(&[10, 11]),
                position: 0,
            };
            let changed = RoleSnapshot {
                guild_id: "10".into(),
                role_id: "20".into(),
                permissions: PermissionSet::from_bits(&[3]),
                position: 1,
            };
            store.upsert_guild_roles(GuildRolesSnapshot::new(
                "10",
                vec![everyone.clone(), changed.clone()],
                0,
            ));
            for now in [99, 150] {
                if remove {
                    store.remove_guild_role("10", "20", now);
                } else {
                    store.upsert_guild_role(changed.clone(), now);
                }
                let snapshot = match store.get_guild_roles("10", now) {
                    SnapshotLookup::Found(snapshot) if now == 99 => snapshot,
                    SnapshotLookup::Stale(snapshot) if now == 150 => snapshot,
                    _ => panic!("a role change must not renew the other roles"),
                };
                assert!(snapshot.complete);
                assert_eq!(snapshot.captured_at_ms, 0);
                assert_eq!(snapshot.roles.contains(&changed), !remove);
                assert!(snapshot.roles.contains(&everyone));
            }
            store.upsert_guild_roles(GuildRolesSnapshot::new("10", vec![everyone], 150));
            let SnapshotLookup::Found(snapshot) = store.get_guild_roles("10", 151) else {
                panic!("a full role list must renew the snapshot");
            };
            assert!(snapshot.complete);
            assert_eq!(snapshot.captured_at_ms, 150);
        }
    }

    #[test]
    fn partial_role_lists_remain_incomplete_and_keep_their_original_age() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let role = RoleSnapshot {
            guild_id: "10".into(),
            role_id: "20".into(),
            permissions: PermissionSet::zero(),
            position: 1,
        };
        store.upsert_guild_role(role.clone(), 10);
        store.remove_guild_role("10", "30", 111);
        store.upsert_guild_role(role, 150);
        let SnapshotLookup::Stale(snapshot) = store.get_guild_roles("10", 150) else {
            panic!("partial role events must not keep a role list fresh");
        };
        assert!(!snapshot.complete);
        assert_eq!(snapshot.captured_at_ms, 10);
        assert_eq!(snapshot.roles.len(), 1);
    }

    #[test]
    fn deleting_a_role_removes_member_assignments_without_renewing_member_state() {
        let mut store = SnapshotStore::new(100, 100, 100, 100);
        store.upsert_guild_roles(GuildRolesSnapshot::new(
            "10",
            vec![RoleSnapshot {
                guild_id: "10".into(),
                role_id: "20".into(),
                permissions: PermissionSet::from_bits(&[3]),
                position: 1,
            }],
            0,
        ));
        let mut member = MemberSnapshot::new("10", "99", vec!["20".into(), "30".into()], 0);
        member.set_timeout_until(Some(200), 0);
        store.upsert_member(member);
        store.upsert_member(MemberSnapshot::new("10", "98", vec!["30".into()], 0));
        store.upsert_member(MemberSnapshot::new("11", "99", vec!["20".into()], 0));
        store.tombstone_member("10", "97", 0);
        let bytes_before = store.retained_bytes();
        store.remove_guild_role("10", "20", 50);
        let SnapshotLookup::Found(member) = store.get_member("10", "99", 50) else {
            panic!("the member must remain available after a role deletion");
        };
        assert_eq!(member.role_ids, ["30"]);
        assert_eq!(member.captured_at_ms, 0);
        assert!(member.complete && member.roles_complete && member.timeout_known);
        assert_eq!(member.timeout_until_ms, Some(200));
        assert!(member.timed_out);
        let SnapshotLookup::Found(other_member) = store.get_member("11", "99", 50) else {
            panic!("the other guild must remain unchanged");
        };
        assert_eq!(other_member.role_ids, ["20"]);
        assert_eq!(store.get_member("10", "97", 50), SnapshotLookup::Tombstone);
        assert!(matches!(
            store.get_member("10", "99", 101),
            SnapshotLookup::Stale(_)
        ));
        assert!(store.retained_bytes() <= bytes_before);
        store.remove_guild_role("10", "20", 101);
        assert_eq!(store.get_member("10", "97", 101), SnapshotLookup::Missing);
        assert!(matches!(
            store.get_member("10", "99", 101),
            SnapshotLookup::Stale(_)
        ));
    }

    #[test]
    fn byte_limits_cover_all_cache_kinds_and_survive_gap_and_clone() {
        let mut store = SnapshotStore::new(100, 100, 1000, 1000);
        store.set_max_bytes(4096);
        for index in 0..200 {
            let id = index.to_string();
            let mut message = MessageSnapshot::new("guild", "channel", &id, "user", 0);
            message.content = "text".repeat(100);
            store.upsert_message(message);
            store.upsert_channel(ChannelSnapshot::new("guild", &id, 0, 0));
            store.upsert_member(MemberSnapshot::new("guild", &id, vec!["role".into()], 0));
            store.upsert_guild_roles(GuildRolesSnapshot::new(
                &id,
                vec![RoleSnapshot {
                    guild_id: id.clone(),
                    role_id: id.clone(),
                    permissions: PermissionSet::from_bits(&[3]),
                    position: 1,
                }],
                0,
            ));
            store.mark_guild_available(&id, 0);
            store.tombstone_message("channel", &id, 1);
            store.tombstone_channel(&id, 1);
            store.tombstone_member("guild", &id, 1);
            store.tombstone_user(&id, 1);
            store.tombstone_guild(&id, 1);
            assert!(store.retained_bytes() <= 4096);
        }
        let mut clone = store.clone();
        assert!(clone.retained_bytes() <= 4096);
        clone.set_max_bytes(1);
        assert_eq!(clone.retained_bytes(), 0);
        store.mark_gap();
        assert_eq!(store.retained_bytes(), 0);
        let mut message = MessageSnapshot::new("g", "c", "m", "u", 0);
        message.content = "huge".repeat(4096);
        store.upsert_message(message);
        assert_eq!(store.message_count(), 0);
        assert_eq!(store.retained_bytes(), 0);
    }

    #[test]
    fn oversized_updates_drop_the_changed_value_without_flushing_unrelated_snapshots() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.set_max_bytes(2048);
        store.upsert_message(MessageSnapshot::new("g", "c", "one", "u", 0));
        store.upsert_message(MessageSnapshot::new("g", "c", "two", "u", 0));
        store.patch_message(
            MessageSnapshotPatch {
                channel_id: "c".into(),
                message_id: "two".into(),
                content: Some("x".repeat(4096)),
                ..MessageSnapshotPatch::default()
            },
            1,
        );
        assert!(matches!(
            store.get_message("c", "one", 1),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            store.get_message("c", "two", 1),
            SnapshotLookup::Missing
        ));
        assert!(store.retained_bytes() <= 2048);
    }

    #[test]
    fn patching_a_message_preserves_its_insertion_order() {
        let mut store = SnapshotStore::new(100, 100, 2, 2);
        store.upsert_message(MessageSnapshot::new("g", "c", "one", "u", 0));
        store.upsert_message(MessageSnapshot::new("g", "c", "two", "u", 0));
        store.patch_message(
            MessageSnapshotPatch {
                channel_id: "c".into(),
                message_id: "one".into(),
                content: Some("edited".into()),
                ..MessageSnapshotPatch::default()
            },
            1,
        );
        store.upsert_message(MessageSnapshot::new("g", "c", "three", "u", 2));
        assert!(matches!(
            store.get_message("c", "one", 2),
            SnapshotLookup::Missing
        ));
        assert_eq!(
            store
                .messages_for_user("g", "u", 2)
                .iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["two", "three"]
        );
    }

    #[test]
    fn replacing_tombstones_uses_the_latest_exact_expiry() {
        let mut store = SnapshotStore::new(100, 10, 10, 10);
        store.tombstone_message("c", "m", 0);
        store.tombstone_message("c", "m", 8);
        assert!(matches!(
            store.get_message("c", "m", 18),
            SnapshotLookup::Tombstone
        ));
        assert!(matches!(
            store.get_message("c", "m", 19),
            SnapshotLookup::Missing
        ));
        assert_eq!(store.retained_bytes(), 0);
    }

    #[test]
    fn message_snapshot_can_be_found_and_stale() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.upsert_message(MessageSnapshot::new("g", "c", "m", "u", 0));
        assert!(matches!(
            store.get_message("c", "m", 50),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            store.get_message("c", "m", 101),
            SnapshotLookup::Stale(_)
        ));
    }

    #[test]
    fn delete_creates_tombstone_and_removes_message() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.upsert_message(MessageSnapshot::new("g", "c", "m", "u", 0));
        store.tombstone_message("c", "m", 10);
        assert!(matches!(
            store.get_message("c", "m", 20),
            SnapshotLookup::Tombstone
        ));
        assert_eq!(store.message_count(), 0);
        assert_eq!(store.tombstone_count(), 1);
    }

    #[test]
    fn bulk_delete_creates_tombstones() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.tombstone_bulk("c", ["m1", "m2"], 0);
        assert!(matches!(
            store.get_message("c", "m1", 0),
            SnapshotLookup::Tombstone
        ));
        assert!(matches!(
            store.get_message("c", "m2", 0),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn channel_snapshot_can_be_found_stale_and_tombstoned() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.upsert_channel(ChannelSnapshot::new("g", "c", 0, 0));
        assert!(matches!(
            store.get_channel("c", 50),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            store.get_channel("c", 101),
            SnapshotLookup::Stale(_)
        ));
        store.tombstone_channel("c", 110);
        assert!(matches!(
            store.get_channel("c", 111),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn channel_patches_preserve_omitted_state_without_refreshing_its_age() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let mut channel = ChannelSnapshot::new("10", "20", 12, 0);
        channel.parent_channel_id = Some("30".into());
        channel.archived = true;
        channel.locked = true;
        channel.current_user_thread_joined = Some(true);
        channel.current_user_thread_joined_at_ms = Some(0);
        channel
            .permission_overwrites
            .push(ChannelPermissionOverwrite {
                overwrite_id: "10".into(),
                kind: 0,
                allow: PermissionSet::zero(),
                deny: PermissionSet::from_bits(&[10]),
            });
        channel.has_permission_overwrites = true;
        store.upsert_channel(channel.clone());
        store.patch_channel(
            channel::patch(
                &crate::json::parse_json(
                    r#"{"id":"20","name":"renamed","thread_metadata":{"archived":false}}"#,
                )
                .unwrap(),
            )
            .unwrap(),
            90,
        );
        let SnapshotLookup::Found(updated) = store.get_channel("20", 90) else {
            panic!("expected cached thread");
        };
        channel.archived = false;
        assert_eq!(updated, channel);
        assert!(matches!(
            store.get_channel("20", 101),
            SnapshotLookup::Stale(_)
        ));
        store.patch_channel(channel::patch(&crate::json::parse_json(r#"{"id":"20","parent_id":null,"permission_overwrites":[],"thread_metadata":{"locked":false}}"#).unwrap()).unwrap(), 102);
        let SnapshotLookup::Stale(updated) = store.get_channel("20", 102) else {
            panic!("partial updates must not renew omitted fields");
        };
        assert_eq!(updated.parent_channel_id, None);
        assert!(updated.permission_overwrites.is_empty());
        assert!(!updated.has_permission_overwrites);
        assert!(!updated.locked);
    }

    #[test]
    fn channel_patches_cannot_resurrect_deleted_resources_or_complete_unknown_state() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let patch = channel::patch(
            &crate::json::parse_json(r#"{"id":"20","guild_id":"10","type":0}"#).unwrap(),
        )
        .unwrap();
        store.patch_channel(patch.clone(), 0);
        let SnapshotLookup::Found(channel) = store.get_channel("20", 0) else {
            panic!("expected incomplete channel");
        };
        assert!(!channel.complete);
        store.tombstone_channel("20", 1);
        store.patch_channel(patch, 2);
        assert_eq!(store.get_channel("20", 2), SnapshotLookup::Tombstone);
        store.upsert_channel(ChannelSnapshot::new("10", "20", 0, 3));
        store.set_max_bytes(2048);
        store.patch_channel(
            ChannelSnapshotPatch {
                channel_id: "20".into(),
                permission_overwrites: Some(
                    (0..100)
                        .map(|i| ChannelPermissionOverwrite {
                            overwrite_id: i.to_string(),
                            kind: 0,
                            allow: PermissionSet::zero(),
                            deny: PermissionSet::zero(),
                        })
                        .collect(),
                ),
                ..Default::default()
            },
            4,
        );
        assert!(store.retained_bytes() <= 2048);
        assert_eq!(store.get_channel("20", 4), SnapshotLookup::Missing);
    }

    #[test]
    fn complete_channel_updates_refresh_state_and_do_not_inherit_access_from_another_kind() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let mut channel = ChannelSnapshot::new("10", "20", 0, 0);
        channel.current_user_thread_joined = Some(true);
        channel.current_user_thread_joined_at_ms = Some(0);
        store.upsert_channel(channel);
        store.patch_channel(channel::patch(&crate::json::parse_json(r#"{"id":"20","guild_id":"10","type":12,"parent_id":"30","flags":0,"permission_overwrites":[],"thread_metadata":{"archived":false,"locked":false}}"#).unwrap()).unwrap(), 101);
        let SnapshotLookup::Found(channel) = store.get_channel("20", 101) else {
            panic!("complete update must renew the cache");
        };
        assert!(channel.complete);
        assert_eq!(channel.channel_type, 12);
        assert_eq!(channel.captured_at_ms, 101);
        assert_eq!(channel.current_user_thread_joined, None);
        assert_eq!(channel.current_user_thread_joined_at_ms, None);
    }

    #[test]
    fn member_and_guild_roles_snapshots_can_be_found_and_stale() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.upsert_member(MemberSnapshot::new("g", "u", vec!["r".to_string()], 0));
        store.upsert_guild_roles(GuildRolesSnapshot::new(
            "g",
            vec![RoleSnapshot {
                guild_id: "g".to_string(),
                role_id: "r".to_string(),
                permissions: PermissionSet::from_bits(&[10]),
                position: 1,
            }],
            0,
        ));
        assert!(matches!(
            store.get_member("g", "u", 50),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            store.get_member("g", "u", 101),
            SnapshotLookup::Stale(_)
        ));
        assert!(matches!(
            store.get_guild_roles("g", 50),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            store.get_guild_roles("g", 101),
            SnapshotLookup::Stale(_)
        ));
        store.tombstone_member("g", "u", 110);
        assert!(matches!(
            store.get_member("g", "u", 111),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn cached_member_timeouts_expire_at_the_deadline_without_an_update() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let mut member = MemberSnapshot::new("g", "u", vec!["r".into()], 0);
        member.set_timeout_until(Some(50), 0);
        store.upsert_member(member);
        for now in [49, 50, 51, 101] {
            for lookup in [
                store.get_member("g", "u", now),
                store.get_bot_member("g", "u", now),
            ] {
                let member = match lookup {
                    SnapshotLookup::Found(member) | SnapshotLookup::Stale(member) => member,
                    other => panic!("expected cached member: {other:?}"),
                };
                assert_eq!(member.timed_out, now < 50);
                assert_eq!(member.timeout_until_ms, Some(50));
                assert_eq!(member.captured_at_ms, 0);
            }
        }
    }

    #[test]
    fn member_updates_preserve_omitted_fields_and_do_not_renew_unobserved_roles() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let mut member = MemberSnapshot::new("g", "u", vec!["r".into()], 0);
        member.set_timeout_until(Some(200), 0);
        store.upsert_member(member);
        store.patch_member(
            MemberSnapshotPatch {
                guild_id: "g".into(),
                user_id: "u".into(),
                role_ids: None,
                timeout: MemberTimeoutUpdate::Clear,
            },
            101,
        );
        let SnapshotLookup::Stale(member) = store.get_member("g", "u", 101) else {
            panic!("timeout updates must not make old roles fresh");
        };
        assert_eq!(member.role_ids, vec!["r"]);
        assert!(!member.timed_out);
        assert_eq!(member.timeout_until_ms, None);
        assert_eq!(member.captured_at_ms, 0);
        assert!(member.complete);

        store.patch_member(
            MemberSnapshotPatch {
                guild_id: "g".into(),
                user_id: "u".into(),
                role_ids: Some(vec!["new-role".into()]),
                timeout: MemberTimeoutUpdate::Until(300),
            },
            110,
        );
        for (now, roles) in [(120, Some(Vec::new())), (130, None)] {
            store.patch_member(
                MemberSnapshotPatch {
                    guild_id: "g".into(),
                    user_id: "u".into(),
                    role_ids: roles,
                    timeout: MemberTimeoutUpdate::Missing,
                },
                now,
            );
        }
        let SnapshotLookup::Found(member) = store.get_member("g", "u", 130) else {
            panic!("expected updated member");
        };
        assert!(member.role_ids.is_empty());
        assert!(member.timed_out);
        assert_eq!(member.timeout_until_ms, Some(300));
        assert_eq!(member.captured_at_ms, 120);
    }

    #[test]
    fn role_updates_do_not_renew_unobserved_timeout_state() {
        let mut settings = SnapshotSettings::compact(100, 100, 10, 10);
        settings.member_ttl_ms = 10;
        settings.bot_member_ttl_ms = 20;
        let mut store = SnapshotStore::with_settings(settings);
        store.upsert_member(MemberSnapshot::new("g", "u", vec![], 0));
        for now in [10, 11, 20, 21] {
            store.patch_member(
                MemberSnapshotPatch {
                    guild_id: "g".into(),
                    user_id: "u".into(),
                    role_ids: Some(vec!["r".into()]),
                    timeout: MemberTimeoutUpdate::Missing,
                },
                now,
            );
            assert_eq!(
                matches!(store.get_member("g", "u", now), SnapshotLookup::Found(_)),
                now <= 10,
                "member at {now}"
            );
            assert_eq!(
                matches!(
                    store.get_bot_member("g", "u", now),
                    SnapshotLookup::Found(_)
                ),
                now <= 20,
                "bot at {now}"
            );
            assert_eq!(
                store.clone().get_member("g", "u", now),
                store.get_member("g", "u", now)
            );
        }
        store.patch_member(
            MemberSnapshotPatch {
                guild_id: "g".into(),
                user_id: "u".into(),
                role_ids: None,
                timeout: MemberTimeoutUpdate::Until(100),
            },
            22,
        );
        let SnapshotLookup::Found(member) = store.get_member("g", "u", 22) else {
            panic!("separately observed fresh fields must remain usable");
        };
        assert!(member.complete);
        assert!(member.timed_out);
        assert_eq!(member.captured_at_ms, 21);
    }

    #[test]
    fn partial_member_fields_expire_independently_and_full_reads_refresh_both() {
        for roles_first in [false, true] {
            let mut store = SnapshotStore::new(10, 10, 10, 10);
            for (index, now) in [0, 11].into_iter().enumerate() {
                let roles = (index == 0) == roles_first;
                store.patch_member(
                    MemberSnapshotPatch {
                        guild_id: "g".into(),
                        user_id: "u".into(),
                        role_ids: roles.then(Vec::new),
                        timeout: if roles {
                            MemberTimeoutUpdate::Missing
                        } else {
                            MemberTimeoutUpdate::Clear
                        },
                    },
                    now,
                );
            }
            let SnapshotLookup::Stale(member) = store.get_member("g", "u", 11) else {
                panic!("a later partial update must not renew an older field");
            };
            assert!(member.complete);
            store.upsert_member(MemberSnapshot::new("g", "u", vec![], 12));
            assert!(matches!(
                store.get_member("g", "u", 22),
                SnapshotLookup::Found(_)
            ));
            assert!(matches!(
                store.get_member("g", "u", 23),
                SnapshotLookup::Stale(_)
            ));
        }
    }

    #[test]
    fn partial_member_records_stay_incomplete_until_roles_and_timeout_are_known() {
        for roles_first in [false, true] {
            let mut store = SnapshotStore::new(100, 100, 10, 10);
            for index in 0..2 {
                let roles_update = (index == 0) == roles_first;
                store.patch_member(
                    MemberSnapshotPatch {
                        guild_id: "g".into(),
                        user_id: "u".into(),
                        role_ids: roles_update.then(Vec::new),
                        timeout: if roles_update {
                            MemberTimeoutUpdate::Missing
                        } else {
                            MemberTimeoutUpdate::Until(50)
                        },
                    },
                    index,
                );
                let SnapshotLookup::Found(member) = store.get_member("g", "u", index) else {
                    panic!("expected partial member");
                };
                assert_eq!(member.complete, index == 1);
                assert_eq!(member.roles_complete, roles_update || index == 1);
                assert_eq!(member.timeout_known, !roles_update || index == 1);
            }
        }
    }

    #[test]
    fn member_updates_do_not_resurrect_tombstones_and_obey_the_cache_budget() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        for user_tombstone in [false, true] {
            store.upsert_member(MemberSnapshot::new("g", "u", vec![], 0));
            if user_tombstone {
                store.tombstone_user("u", 1);
            } else {
                store.tombstone_member("g", "u", 1);
            }
            store.patch_member(
                MemberSnapshotPatch {
                    guild_id: "g".into(),
                    user_id: "u".into(),
                    role_ids: Some(vec!["r".into()]),
                    timeout: MemberTimeoutUpdate::Clear,
                },
                2,
            );
            assert!(!matches!(
                store.get_member("g", "u", 2),
                SnapshotLookup::Found(_)
            ));
            store.upsert_member(MemberSnapshot::new("g", "u", vec![], 3));
            assert!(matches!(
                store.get_member("g", "u", 3),
                SnapshotLookup::Found(_)
            ));
        }
        store.set_max_bytes(2048);
        store.patch_member(
            MemberSnapshotPatch {
                guild_id: "g".into(),
                user_id: "u".into(),
                role_ids: Some(vec!["r".repeat(4096)]),
                timeout: MemberTimeoutUpdate::Clear,
            },
            4,
        );
        assert!(store.retained_bytes() <= 2048);
        assert_eq!(store.get_member("g", "u", 4), SnapshotLookup::Missing);
    }

    #[test]
    fn snapshot_settings_apply_member_bot_and_thread_member_ttls() {
        let mut store = SnapshotStore::with_settings(SnapshotSettings {
            message_ttl_ms: 100,
            message_tombstone_ttl_ms: 100,
            message_max_entries: 10,
            message_tombstone_max_entries: 10,
            resource_ttl_ms: 100,
            resource_tombstone_ttl_ms: 100,
            resource_max_entries: 10,
            resource_tombstone_max_entries: 10,
            member_ttl_ms: 10,
            bot_member_ttl_ms: 50,
            thread_member_ttl_ms: 20,
            member_tombstone_ttl_ms: 100,
            member_max_entries: 10,
            member_tombstone_max_entries: 10,
        });
        store.upsert_member(MemberSnapshot::new("g", "u", vec!["r".to_string()], 0));
        assert!(matches!(
            store.get_member("g", "u", 11),
            SnapshotLookup::Stale(_)
        ));
        assert!(matches!(
            store.get_bot_member("g", "u", 11),
            SnapshotLookup::Found(_)
        ));

        store.upsert_channel(ChannelSnapshot::new("g", "thread", 12, 0));
        assert!(store.set_current_user_thread_joined("thread", true, 5));
        let SnapshotLookup::Found(joined) = store.get_channel("thread", 25) else {
            panic!("expected thread channel");
        };
        assert_eq!(joined.current_user_thread_joined, Some(true));
        let SnapshotLookup::Found(expired) = store.get_channel("thread", 26) else {
            panic!("expected thread channel");
        };
        assert_eq!(expired.current_user_thread_joined, None);
    }

    #[test]
    fn private_thread_access_can_be_cleared_by_channel_or_guild() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let mut thread = ChannelSnapshot::new("g", "thread", 12, 0);
        thread.parent_channel_id = Some("parent".to_string());
        store.upsert_channel(thread);
        assert!(store.set_current_user_thread_joined("thread", true, 5));
        store.clear_private_thread_access_for_channel("parent", 6);
        let SnapshotLookup::Found(cleared) = store.get_channel("thread", 6) else {
            panic!("expected thread channel");
        };
        assert_eq!(cleared.current_user_thread_joined, None);

        assert!(store.set_current_user_thread_joined("thread", true, 7));
        store.clear_private_thread_access_for_guild("g", 8);
        let SnapshotLookup::Found(cleared) = store.get_channel("thread", 8) else {
            panic!("expected thread channel");
        };
        assert_eq!(cleared.current_user_thread_joined, None);
    }

    #[test]
    fn snapshot_settings_apply_separate_tombstone_ttls() {
        let mut store = SnapshotStore::with_settings(SnapshotSettings {
            message_ttl_ms: 100,
            message_tombstone_ttl_ms: 10,
            message_max_entries: 10,
            message_tombstone_max_entries: 10,
            resource_ttl_ms: 100,
            resource_tombstone_ttl_ms: 30,
            resource_max_entries: 10,
            resource_tombstone_max_entries: 10,
            member_ttl_ms: 100,
            bot_member_ttl_ms: 100,
            thread_member_ttl_ms: 100,
            member_tombstone_ttl_ms: 20,
            member_max_entries: 10,
            member_tombstone_max_entries: 10,
        });
        store.tombstone_message("c", "m", 0);
        store.tombstone_member("g", "u", 0);
        store.tombstone_channel("c", 0);
        store.tombstone_user("user", 0);
        store.tombstone_guild("g2", 0);

        assert!(matches!(
            store.get_message("c", "m", 11),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            store.get_member("g", "u", 11),
            SnapshotLookup::Tombstone
        ));
        assert!(matches!(
            store.get_channel("c", 11),
            SnapshotLookup::Tombstone
        ));
        assert!(store.is_user_tombstoned("user", 11));
        assert!(matches!(
            store.get_guild("g2", 11),
            SnapshotLookup::Tombstone
        ));

        assert!(matches!(
            store.get_member("g", "u", 21),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            store.get_channel("c", 21),
            SnapshotLookup::Tombstone
        ));
        assert!(!store.is_user_tombstoned("user", 31));
        assert!(matches!(store.get_guild("g2", 31), SnapshotLookup::Missing));
    }

    #[test]
    fn zero_tombstone_ttl_keeps_no_tombstones() {
        let mut store = SnapshotStore::with_settings(SnapshotSettings {
            message_ttl_ms: 100,
            message_tombstone_ttl_ms: 0,
            message_max_entries: 10,
            message_tombstone_max_entries: 10,
            resource_ttl_ms: 100,
            resource_tombstone_ttl_ms: 0,
            resource_max_entries: 10,
            resource_tombstone_max_entries: 10,
            member_ttl_ms: 100,
            bot_member_ttl_ms: 100,
            thread_member_ttl_ms: 100,
            member_tombstone_ttl_ms: 0,
            member_max_entries: 10,
            member_tombstone_max_entries: 10,
        });
        store.upsert_message(MessageSnapshot::new("g", "c", "m", "u", 0));
        store.upsert_channel(ChannelSnapshot::new("g", "c", 0, 0));
        store.upsert_member(MemberSnapshot::new("g", "u", vec![], 0));
        store.mark_guild_available("g", 0);
        store.tombstone_message("c", "m", 1);
        store.tombstone_channel("c", 1);
        store.tombstone_member("g", "u", 1);
        store.tombstone_user("u2", 1);
        store.tombstone_guild("g", 1);
        assert_eq!(store.tombstone_count(), 0);
        assert!(matches!(
            store.get_message("c", "m", 1),
            SnapshotLookup::Missing
        ));
        assert!(matches!(store.get_channel("c", 1), SnapshotLookup::Missing));
        assert!(matches!(
            store.get_member("g", "u", 1),
            SnapshotLookup::Missing
        ));
        assert!(!store.is_user_tombstoned("u2", 1));
        assert!(matches!(store.get_guild("g", 1), SnapshotLookup::Missing));
    }

    #[test]
    fn snapshot_settings_apply_separate_max_entries() {
        let mut store = SnapshotStore::with_settings(SnapshotSettings {
            message_ttl_ms: 100,
            message_tombstone_ttl_ms: 100,
            message_max_entries: 3,
            message_tombstone_max_entries: 3,
            resource_ttl_ms: 100,
            resource_tombstone_ttl_ms: 100,
            resource_max_entries: 1,
            resource_tombstone_max_entries: 1,
            member_ttl_ms: 100,
            bot_member_ttl_ms: 100,
            thread_member_ttl_ms: 100,
            member_tombstone_ttl_ms: 100,
            member_max_entries: 1,
            member_tombstone_max_entries: 1,
        });
        store.upsert_message(MessageSnapshot::new("g", "c", "m1", "u", 0));
        store.upsert_message(MessageSnapshot::new("g", "c", "m2", "u", 1));
        store.upsert_channel(ChannelSnapshot::new("g", "c1", 0, 2));
        store.upsert_channel(ChannelSnapshot::new("g", "c2", 0, 3));
        store.upsert_member(MemberSnapshot::new("g", "u1", vec![], 4));
        store.upsert_member(MemberSnapshot::new("g", "u2", vec![], 5));
        assert!(matches!(
            store.get_message("c", "m1", 5),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            store.get_channel("c1", 5),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            store.get_channel("c2", 5),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            store.get_member("g", "u1", 5),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            store.get_member("g", "u2", 5),
            SnapshotLookup::Found(_)
        ));
    }

    #[test]
    fn user_tombstone_removes_member_snapshots_and_expires() {
        let mut store = SnapshotStore::new(100, 10, 10, 10);
        store.upsert_member(MemberSnapshot::new("g1", "u", vec!["r".to_string()], 0));
        store.upsert_member(MemberSnapshot::new("g2", "u", vec!["r".to_string()], 0));
        store.tombstone_member("g1", "u", 1);

        store.tombstone_user("u", 2);

        assert!(store.is_user_tombstoned("u", 2));
        assert!(matches!(
            store.get_member("g1", "u", 2),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            store.get_member("g2", "u", 2),
            SnapshotLookup::Missing
        ));
        assert!(!store.is_user_tombstoned("u", 13));
    }

    #[test]
    fn guild_snapshot_can_be_available_unavailable_and_tombstoned() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.mark_guild_available("g", 0);
        let SnapshotLookup::Found(snapshot) = store.get_guild("g", 50) else {
            panic!("expected guild snapshot");
        };
        assert!(!snapshot.unavailable);
        store.mark_guild_unavailable("g", 60);
        let SnapshotLookup::Found(snapshot) = store.get_guild("g", 61) else {
            panic!("expected guild unavailable snapshot");
        };
        assert!(snapshot.unavailable);
        store.tombstone_guild("g", 70);
        assert!(matches!(
            store.get_guild("g", 71),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn gap_marks_all_snapshots_unknown() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.upsert_message(MessageSnapshot::new("g", "c", "m", "u", 0));
        store.tombstone_message("c", "deleted", 1);
        store.upsert_channel(ChannelSnapshot::new("g", "c", 0, 2));
        store.tombstone_channel("gone-c", 3);
        store.upsert_member(MemberSnapshot::new("g", "u", vec!["r".to_string()], 4));
        store.tombstone_member("g", "gone-u", 5);
        store.tombstone_user("gone-user", 6);
        store.upsert_guild_roles(GuildRolesSnapshot::new(
            "g",
            vec![RoleSnapshot {
                guild_id: "g".to_string(),
                role_id: "r".to_string(),
                permissions: PermissionSet::from_bits(&[10]),
                position: 1,
            }],
            6,
        ));
        store.mark_guild_available("g", 7);
        store.tombstone_guild("gone-g", 8);

        store.mark_gap();

        assert!(matches!(
            store.get_message("c", "m", 9),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            store.get_message("c", "deleted", 9),
            SnapshotLookup::Missing
        ));
        assert!(matches!(store.get_channel("c", 9), SnapshotLookup::Missing));
        assert!(matches!(
            store.get_channel("gone-c", 9),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            store.get_member("g", "u", 9),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            store.get_member("g", "gone-u", 9),
            SnapshotLookup::Missing
        ));
        assert!(!store.is_user_tombstoned("gone-user", 9));
        assert!(matches!(
            store.get_guild_roles("g", 9),
            SnapshotLookup::Missing
        ));
        assert!(matches!(store.get_guild("g", 9), SnapshotLookup::Missing));
        assert!(matches!(
            store.get_guild("gone-g", 9),
            SnapshotLookup::Missing
        ));
    }

    #[test]
    fn tombstone_ttl_expires() {
        let mut store = SnapshotStore::new(100, 10, 10, 10);
        store.tombstone_message("c", "m", 0);
        assert!(matches!(
            store.get_message("c", "m", 11),
            SnapshotLookup::Missing
        ));
    }

    #[test]
    fn max_entries_are_enforced() {
        let mut store = SnapshotStore::new(100, 100, 2, 2);
        store.upsert_message(MessageSnapshot::new("g", "c", "m1", "u", 0));
        store.upsert_message(MessageSnapshot::new("g", "c", "m2", "u", 1));
        store.upsert_message(MessageSnapshot::new("g", "c", "m3", "u", 2));
        assert_eq!(store.message_count(), 2);
        assert!(matches!(
            store.get_message("c", "m1", 2),
            SnapshotLookup::Missing
        ));
    }

    #[test]
    fn patch_message_preserves_existing_fields() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        let mut snapshot = MessageSnapshot::new("g", "c", "m", "u", 0);
        snapshot.message_type = 21;
        store.upsert_message(snapshot);
        store.patch_message(
            MessageSnapshotPatch {
                channel_id: "c".to_string(),
                message_id: "m".to_string(),
                content: Some("edited".to_string()),
                ..MessageSnapshotPatch::default()
            },
            10,
        );
        let SnapshotLookup::Found(snapshot) = store.get_message("c", "m", 10) else {
            panic!("expected patched message");
        };
        assert_eq!(snapshot.guild_id, "g");
        assert_eq!(snapshot.user_id, "u");
        assert_eq!(snapshot.message_type, 21);
        assert_eq!(snapshot.content, "edited");
        assert!(snapshot.complete);
    }

    #[test]
    fn patch_message_without_required_fields_is_incomplete() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.patch_message(
            MessageSnapshotPatch {
                channel_id: "c".to_string(),
                message_id: "m".to_string(),
                content: Some("edited".to_string()),
                ..MessageSnapshotPatch::default()
            },
            10,
        );
        let SnapshotLookup::Found(snapshot) = store.get_message("c", "m", 10) else {
            panic!("expected patched message");
        };
        assert!(!snapshot.complete);
    }

    #[test]
    fn message_search_obeys_exact_ttl_and_partial_updates_do_not_renew_old_fields() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.upsert_message(MessageSnapshot::new("g", "c", "m", "u", 0));
        assert_eq!(store.messages_for_user("g", "u", 100).len(), 1);
        assert!(store.patch_message_checked(
            MessageSnapshotPatch {
                channel_id: "c".into(),
                message_id: "m".into(),
                content: Some("edited".into()),
                ..Default::default()
            },
            100
        ));
        assert!(store.messages_for_user("g", "u", 101).is_empty());
        let SnapshotLookup::Stale(old) = store.get_message("c", "m", 101) else {
            panic!("expected stale message");
        };
        assert_eq!(old.captured_at_ms, 0);
        assert_eq!(old.content, "edited");
        store.patch_message(
            MessageSnapshotPatch {
                channel_id: "c".into(),
                message_id: "m".into(),
                user_id: Some("u".into()),
                content: Some("complete update".into()),
                image_attachment_count: Some(3),
                message_type: Some(0),
                ..Default::default()
            },
            102,
        );
        assert_eq!(store.messages_for_user("g", "u", 102).len(), 1);
        let SnapshotLookup::Found(fresh) = store.get_message("c", "m", 102) else {
            panic!("expected refreshed message");
        };
        assert_eq!(fresh.captured_at_ms, 102);
        assert_eq!(fresh.image_attachment_count, 3);
    }

    #[test]
    fn late_message_updates_do_not_resurrect_deleted_messages() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.upsert_message(MessageSnapshot::new("g", "c", "m", "u", 0));
        store.tombstone_message("c", "m", 10);
        store.patch_message(
            MessageSnapshotPatch {
                guild_id: Some("g".into()),
                channel_id: "c".into(),
                message_id: "m".into(),
                user_id: Some("u".into()),
                content: Some("late".into()),
                message_type: Some(0),
                image_attachment_count: Some(4),
                ..Default::default()
            },
            11,
        );
        assert_eq!(store.get_message("c", "m", 11), SnapshotLookup::Tombstone);
        assert!(store.messages_for_user("g", "u", 11).is_empty());
    }

    #[test]
    fn conflicting_message_identity_discards_the_record_and_role_updates_clear_old_permissions() {
        for guild_conflict in [false, true] {
            let mut store = SnapshotStore::new(100, 100, 10, 10);
            let mut message = MessageSnapshot::new("g", "c", "m", "u", 0);
            message.member_permissions = Some(PermissionSet::from_bits(&[3]));
            store.upsert_message(message);
            assert!(store.patch_message_checked(
                MessageSnapshotPatch {
                    channel_id: "c".into(),
                    message_id: "m".into(),
                    role_ids: Some(vec!["r".into()]),
                    ..Default::default()
                },
                1
            ));
            let SnapshotLookup::Found(member_changed) = store.get_message("c", "m", 1) else {
                panic!("expected message");
            };
            assert_eq!(member_changed.member_permissions, None);
            assert!(!store.patch_message_checked(
                MessageSnapshotPatch {
                    channel_id: "c".into(),
                    message_id: "m".into(),
                    guild_id: guild_conflict.then(|| "other".into()),
                    user_id: (!guild_conflict).then(|| "other".into()),
                    ..Default::default()
                },
                2
            ));
            assert_eq!(store.get_message("c", "m", 2), SnapshotLookup::Missing);
        }
    }

    #[test]
    fn complete_direct_message_updates_can_fill_an_unknown_message() {
        let mut store = SnapshotStore::new(100, 100, 10, 10);
        store.upsert_channel(ChannelSnapshot::new("", "c", 1, 0));
        store.patch_message(
            MessageSnapshotPatch {
                channel_id: "c".into(),
                message_id: "m".into(),
                ..Default::default()
            },
            0,
        );
        let SnapshotLookup::Found(unknown) = store.get_message("c", "m", 0) else {
            panic!("expected partial message");
        };
        assert!(!unknown.complete);
        assert!(!unknown.content_available);
        store.patch_message(
            MessageSnapshotPatch {
                channel_id: "c".into(),
                message_id: "m".into(),
                user_id: Some("u".into()),
                content: Some("".into()),
                message_type: Some(0),
                image_attachment_count: Some(0),
                ..Default::default()
            },
            1,
        );
        let SnapshotLookup::Found(message) = store.get_message("c", "m", 1) else {
            panic!("expected complete direct message");
        };
        assert!(message.complete);
        assert!(message.content_available);
        assert!(message.guild_id.is_empty());
    }
}
