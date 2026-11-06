use std::fmt;

use super::{
    ChannelPermissionOverwrite, ChannelSnapshot, ChannelSnapshotPatch, GuildRolesSnapshot,
    GuildSnapshot, MemberSnapshot, MemberSnapshotPatch, MessageSnapshot, MessageSnapshotPatch,
    RoleSnapshot,
};

impl fmt::Debug for MessageSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessageSnapshot")
            .field("has_guild", &!self.guild_id.is_empty())
            .field("content_bytes", &self.content.len())
            .field("content_available", &self.content_available)
            .field("author_is_bot", &self.author_is_bot)
            .field("role_count", &self.role_ids.len())
            .field("image_attachment_count", &self.image_attachment_count)
            .field("has_member_permissions", &self.member_permissions.is_some())
            .field("message_type", &self.message_type)
            .field("captured_at_ms", &self.captured_at_ms)
            .field("complete", &self.complete)
            .finish()
    }
}

impl fmt::Debug for MessageSnapshotPatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessageSnapshotPatch")
            .field("has_guild", &self.guild_id.is_some())
            .field("has_user", &self.user_id.is_some())
            .field("content_bytes", &self.content.as_ref().map(String::len))
            .field("author_is_bot", &self.author_is_bot)
            .field("role_count", &self.role_ids.as_ref().map(Vec::len))
            .field("image_attachment_count", &self.image_attachment_count)
            .field("has_member_permissions", &self.member_permissions.is_some())
            .field("message_type", &self.message_type)
            .finish()
    }
}

impl fmt::Debug for ChannelSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChannelSnapshot")
            .field("has_guild", &!self.guild_id.is_empty())
            .field("channel_type", &self.channel_type)
            .field("obfuscated", &self.obfuscated)
            .field("has_parent", &self.parent_channel_id.is_some())
            .field("archived", &self.archived)
            .field("locked", &self.locked)
            .field(
                "current_user_thread_joined",
                &self.current_user_thread_joined,
            )
            .field(
                "current_user_thread_joined_at_ms",
                &self.current_user_thread_joined_at_ms,
            )
            .field("has_permission_overwrites", &self.has_permission_overwrites)
            .field(
                "permission_overwrite_count",
                &self.permission_overwrites.len(),
            )
            .field("captured_at_ms", &self.captured_at_ms)
            .field("complete", &self.complete)
            .finish()
    }
}

impl fmt::Debug for ChannelSnapshotPatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChannelSnapshotPatch")
            .field("has_guild", &self.guild_id.is_some())
            .field("channel_type", &self.channel_type)
            .field(
                "has_parent",
                &self.parent_channel_id.as_ref().map(Option::is_some),
            )
            .field("archived", &self.archived)
            .field("locked", &self.locked)
            .field(
                "current_user_thread_joined",
                &self.current_user_thread_joined,
            )
            .field(
                "permission_overwrite_count",
                &self.permission_overwrites.as_ref().map(Vec::len),
            )
            .field("obfuscated", &self.obfuscated)
            .finish()
    }
}

impl fmt::Debug for ChannelPermissionOverwrite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChannelPermissionOverwrite")
            .field("kind", &self.kind)
            .field("allow", &self.allow)
            .field("deny", &self.deny)
            .finish()
    }
}

impl fmt::Debug for MemberSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemberSnapshot")
            .field("role_count", &self.role_ids.len())
            .field("timed_out", &self.timed_out)
            .field("timeout_until_ms", &self.timeout_until_ms)
            .field("roles_complete", &self.roles_complete)
            .field("timeout_known", &self.timeout_known)
            .field("captured_at_ms", &self.captured_at_ms)
            .field("complete", &self.complete)
            .finish()
    }
}

impl fmt::Debug for MemberSnapshotPatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemberSnapshotPatch")
            .field("role_count", &self.role_ids.as_ref().map(Vec::len))
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl fmt::Debug for RoleSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RoleSnapshot")
            .field("permissions", &self.permissions)
            .field("position", &self.position)
            .finish()
    }
}

impl fmt::Debug for GuildRolesSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuildRolesSnapshot")
            .field("role_count", &self.roles.len())
            .field("captured_at_ms", &self.captured_at_ms)
            .field("complete", &self.complete)
            .finish()
    }
}

impl fmt::Debug for GuildSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuildSnapshot")
            .field("has_owner", &self.owner_id.is_some())
            .field("unavailable", &self.unavailable)
            .field("captured_at_ms", &self.captured_at_ms)
            .field("complete", &self.complete)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use std::fmt::Debug;

    #[test]
    fn snapshot_diagnostics_exclude_content_and_identifiers_without_expanding_collections() {
        let mut message = MessageSnapshot::new(
            "private-guild-id",
            "private-channel-id",
            "private-message-id",
            "private-user-id",
            0,
        );
        message.content = "private-message-content".repeat(50_000);
        message.role_ids = vec!["private-role-id".into(); 1000];
        let patch = MessageSnapshotPatch {
            channel_id: message.channel_id.clone(),
            message_id: message.message_id.clone(),
            content: Some(message.content.clone()),
            role_ids: Some(message.role_ids.clone()),
            ..Default::default()
        };
        let mut channel = ChannelSnapshot::new("private-guild-id", "private-channel-id", 0, 0);
        channel.permission_overwrites = vec![
            ChannelPermissionOverwrite {
                overwrite_id: "private-overwrite-id".into(),
                kind: 0,
                allow: PermissionSet::from_bits(&[3]),
                deny: PermissionSet::zero(),
            };
            1000
        ];
        let member = MemberSnapshot::new(
            "private-guild-id",
            "private-user-id",
            message.role_ids.clone(),
            0,
        );
        let member_patch = MemberSnapshotPatch {
            guild_id: member.guild_id.clone(),
            user_id: member.user_id.clone(),
            role_ids: Some(member.role_ids.clone()),
            timeout: MemberTimeoutUpdate::Missing,
        };
        let channel_patch = ChannelSnapshotPatch {
            channel_id: channel.channel_id.clone(),
            permission_overwrites: Some(channel.permission_overwrites.clone()),
            ..Default::default()
        };
        let role = RoleSnapshot {
            guild_id: "private-guild-id".into(),
            role_id: "private-role-id".into(),
            permissions: PermissionSet::from_bits(&[3]),
            position: 1,
        };
        let roles = GuildRolesSnapshot::new("private-guild-id", vec![role.clone(); 1000], 0);
        let mut guild = GuildSnapshot::new("private-guild-id", false, 0);
        guild.owner_id = Some("private-owner-id".into());
        let values: [&dyn Debug; 10] = [
            &message,
            &patch,
            &channel,
            &channel.permission_overwrites[0],
            &member,
            &member_patch,
            &channel_patch,
            &role,
            &roles,
            &guild,
        ];
        for value in values {
            for output in [format!("{value:?}"), format!("{value:#?}")] {
                assert!(!output.contains("private-"));
                assert!(output.len() < 2048);
            }
        }
        assert_eq!(patch.content.as_deref(), Some(message.content.as_str()));
        assert_eq!(member.role_ids.len(), 1000);
    }

    #[test]
    fn cache_diagnostics_remain_bounded_for_many_live_entries() {
        let mut store = SnapshotStore::new(1000, 1000, 1000, 1000);
        for index in 0..256 {
            let mut message = MessageSnapshot::new(
                "private-guild-id",
                "private-channel-id",
                format!("private-message-{index}"),
                "private-user-id",
                0,
            );
            message.content = "private-cached-content".repeat(100);
            store.upsert_message(message);
        }
        for output in [format!("{store:?}"), format!("{store:#?}")] {
            assert!(!output.contains("private-"));
            assert!(output.len() < 8192);
            assert!(output.contains("SnapshotCache"));
        }
        assert_eq!(store.message_count(), 256);
        let SnapshotLookup::Found(message) =
            store.get_message("private-channel-id", "private-message-0", 0)
        else {
            panic!("message was lost");
        };
        assert_eq!(message.content, "private-cached-content".repeat(100));
    }
}
