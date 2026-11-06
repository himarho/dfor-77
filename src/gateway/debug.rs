use std::fmt;

use super::{
    GatewayBotInfo, GatewayDispatch, GatewayEvent, GatewayEventData, GatewayIngestor,
    GatewayResourceEvent, GatewayResumeSession, GatewayStateMachine, InteractionSnapshot,
    MockGatewayTransport, ThreadMembershipChange, TungsteniteGatewayTransport, WsEndpoint, WsFrame,
    WsGatewayTransport,
};

impl fmt::Debug for InteractionSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InteractionSnapshot")
            .field("interaction_type", &self.interaction_type)
            .field("name_bytes", &self.name.len())
            .field("has_token", &!self.token.is_empty())
            .finish()
    }
}

impl fmt::Debug for MockGatewayTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MockGatewayTransport")
            .field("connected", &self.connected_to.is_some())
            .field("sent_count", &self.sent_texts.len())
            .field("incoming_count", &self.incoming.len())
            .field("connect_error", &self.connect_error)
            .field("send_error", &self.send_error)
            .field("last_close_code", &self.last_close_code)
            .field("graceful_close_count", &self.graceful_close_codes.len())
            .field("resume_disconnects", &self.resume_disconnects)
            .finish()
    }
}

impl fmt::Debug for GatewayEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayEvent")
            .field("event_type", &self.event_type)
            .field("sequence", &self.sequence)
            .field("description_bytes", &self.description.len())
            .field("opcode", &self.opcode)
            .field("retry_ms", &self.retry_ms)
            .field("data", &self.data)
            .finish()
    }
}

impl fmt::Debug for GatewayEventData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Message(message) => formatter.debug_tuple("Message").field(message).finish(),
            Self::Interaction(interaction) => formatter
                .debug_tuple("Interaction")
                .field(interaction)
                .finish(),
            Self::MessageDelete { guild_id, .. } => formatter
                .debug_struct("MessageDelete")
                .field("has_guild", &guild_id.is_some())
                .finish(),
            Self::Reaction { emoji, .. } => formatter
                .debug_struct("Reaction")
                .field("emoji_bytes", &emoji.len())
                .finish(),
            Self::Member { .. } => formatter.debug_struct("Member").finish_non_exhaustive(),
            Self::RateLimit(limit) => formatter.debug_tuple("RateLimit").field(limit).finish(),
            Self::MembersChunk {
                user_ids, metadata, ..
            } => formatter
                .debug_struct("MembersChunk")
                .field("member_count", &user_ids.len())
                .field("metadata_bytes", &metadata.len())
                .finish(),
            Self::Resource {
                guild_id,
                channel_id,
                message_id,
                user_id,
                role_id,
                text,
            } => formatter
                .debug_struct("Resource")
                .field("has_guild", &guild_id.is_some())
                .field("has_channel", &channel_id.is_some())
                .field("has_message", &message_id.is_some())
                .field("has_user", &user_id.is_some())
                .field("has_role", &role_id.is_some())
                .field("text_bytes", &text.as_ref().map(String::len))
                .finish(),
        }
    }
}

impl fmt::Debug for GatewayResourceEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayResourceEvent")
            .field("event_type", &self.event_type)
            .field("sequence", &self.sequence)
            .field("description_bytes", &self.description.len())
            .field("has_guild", &self.guild_id.is_some())
            .field("has_channel", &self.channel_id.is_some())
            .field("has_message", &self.message_id.is_some())
            .field("has_user", &self.user_id.is_some())
            .field("has_role", &self.role_id.is_some())
            .field("text_bytes", &self.text.as_ref().map(String::len))
            .finish()
    }
}

impl fmt::Debug for GatewayResumeSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayResumeSession")
            .field("has_session_id", &!self.session_id.is_empty())
            .field("resume_gateway_url_bytes", &self.resume_gateway_url.len())
            .field("last_seen_seq", &self.last_seen_seq)
            .field("has_bot_user_id", &self.bot_user_id.is_some())
            .finish()
    }
}

impl fmt::Debug for GatewayStateMachine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayStateMachine")
            .field("state", &self.state)
            .field("ready_seen", &self.ready_seen)
            .field(
                "resume_session_invalidated",
                &self.resume_session_invalidated,
            )
            .field("has_session_id", &self.session_id.is_some())
            .field("has_resume_gateway_url", &self.resume_gateway_url.is_some())
            .field("has_bot_user_id", &self.bot_user_id.is_some())
            .field("last_seen_seq", &self.last_seen_seq)
            .field("resume_seq_used", &self.resume_seq_used)
            .field("backoff_info", &self.backoff_info)
            .field("backoff_attempt_count", &self.backoff_attempts.len())
            .field("bug_close_event_count", &self.bug_close_events.len())
            .field("queue", &self.queue)
            .finish()
    }
}

impl fmt::Debug for GatewayBotInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayBotInfo")
            .field("url_bytes", &self.url.len())
            .field("session_start_limit", &self.session_start_limit)
            .finish()
    }
}

impl fmt::Debug for GatewayIngestor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayIngestor")
            .field("machine", &self.machine)
            .field("snapshot", &self.snapshot)
            .field("heartbeat", &self.heartbeat)
            .field("snapshot_gap_epoch", &self.snapshot_gap_epoch)
            .field(
                "message_snapshot_channel_allowlist_count",
                &self.message_snapshot_channel_allowlist.len(),
            )
            .finish()
    }
}

impl fmt::Debug for TungsteniteGatewayTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TungsteniteGatewayTransport")
            .field("connected", &self.socket.is_some())
            .field("timeout_ms", &self.timeout_ms)
            .field("last_close_code", &self.last_close_code)
            .finish()
    }
}

impl fmt::Debug for WsGatewayTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WsGatewayTransport")
            .field("connected", &self.stream.is_some())
            .field("timeout_ms", &self.timeout_ms)
            .field("last_close_code", &self.last_close_code)
            .finish()
    }
}

impl fmt::Debug for WsEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WsEndpoint")
            .field("host_bytes", &self.host.len())
            .field("port", &self.port)
            .field("path_bytes", &self.path.len())
            .finish()
    }
}

impl fmt::Debug for WsFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WsFrame")
            .field("opcode", &self.opcode)
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

impl fmt::Debug for ThreadMembershipChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadMembershipChange")
            .field("has_user", &self.user_id.is_some())
            .field("joined", &self.joined)
            .finish()
    }
}

impl fmt::Debug for GatewayDispatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Ready { .. } => "Ready",
            Self::Resumed { .. } => "Resumed",
            Self::MessageCreate { .. } => "MessageCreate",
            Self::InteractionCreate { .. } => "InteractionCreate",
            Self::MessageDelete { .. } => "MessageDelete",
            Self::MessageDeleteBulk { .. } => "MessageDeleteBulk",
            Self::MessageReaction { .. } => "MessageReaction",
            Self::MessageUpdate { .. } => "MessageUpdate",
            Self::ChannelUpsert { .. } => "ChannelUpsert",
            Self::ChannelUpdate { .. } => "ChannelUpdate",
            Self::ChannelDelete { .. } => "ChannelDelete",
            Self::ChannelSync { .. } => "ChannelSync",
            Self::ThreadMemberUpdate { .. } => "ThreadMemberUpdate",
            Self::ThreadMembersUpdate { .. } => "ThreadMembersUpdate",
            Self::RoleUpsert { .. } => "RoleUpsert",
            Self::RoleDelete { .. } => "RoleDelete",
            Self::MemberUpsert { .. } => "MemberUpsert",
            Self::MemberUpdate { .. } => "MemberUpdate",
            Self::MembersChunk { .. } => "MembersChunk",
            Self::MemberDelete { .. } => "MemberDelete",
            Self::GuildCreate { .. } => "GuildCreate",
            Self::GuildUpdate { .. } => "GuildUpdate",
            Self::GuildDelete { .. } => "GuildDelete",
            Self::GatewayRateLimited { .. } => "GatewayRateLimited",
            Self::RateLimited { .. } => "RateLimited",
            Self::Unknown { .. } => "Unknown",
        };
        let mut output = formatter.debug_struct(name);
        match self {
            Self::Ready {
                sequence,
                session_id,
                resume_gateway_url,
                bot_user_id,
            } => {
                output
                    .field("sequence", sequence)
                    .field("has_session_id", &!session_id.is_empty())
                    .field("resume_gateway_url_bytes", &resume_gateway_url.len())
                    .field("has_bot_user_id", &bot_user_id.is_some());
            }
            Self::Resumed { sequence } => {
                output.field("sequence", sequence);
            }
            Self::MessageCreate { sequence, message } => {
                output.field("sequence", sequence).field("message", message);
            }
            Self::InteractionCreate {
                sequence,
                interaction,
            } => {
                output
                    .field("sequence", sequence)
                    .field("interaction", interaction);
            }
            Self::MessageDelete {
                sequence, guild_id, ..
            } => {
                output
                    .field("sequence", sequence)
                    .field("has_guild", &guild_id.is_some());
            }
            Self::MessageDeleteBulk {
                sequence,
                guild_id,
                message_ids,
                ..
            } => {
                output
                    .field("sequence", sequence)
                    .field("has_guild", &guild_id.is_some())
                    .field("message_count", &message_ids.len());
            }
            Self::MessageReaction {
                sequence,
                added,
                emoji,
                ..
            } => {
                output
                    .field("sequence", sequence)
                    .field("added", added)
                    .field("emoji_bytes", &emoji.len());
            }
            Self::MessageUpdate { sequence, patch } => {
                output.field("sequence", sequence).field("patch", patch);
            }
            Self::ChannelUpsert {
                sequence,
                event_type,
                channel,
                ..
            } => {
                output
                    .field("sequence", sequence)
                    .field("event_type", event_type)
                    .field("channel", channel);
            }
            Self::ChannelUpdate {
                sequence,
                event_type,
                patch,
                ..
            } => {
                output
                    .field("sequence", sequence)
                    .field("event_type", event_type)
                    .field("patch", patch);
            }
            Self::ChannelDelete {
                sequence,
                event_type,
                guild_id,
                ..
            } => {
                output
                    .field("sequence", sequence)
                    .field("event_type", event_type)
                    .field("has_guild", &guild_id.is_some());
            }
            Self::ChannelSync {
                sequence,
                guild_id,
                channels,
                memberships,
            } => {
                output
                    .field("sequence", sequence)
                    .field("has_guild", &guild_id.is_some())
                    .field("channel_count", &channels.len())
                    .field("membership_count", &memberships.len());
            }
            Self::ThreadMemberUpdate {
                sequence,
                guild_id,
                change,
            } => {
                output
                    .field("sequence", sequence)
                    .field("has_guild", &guild_id.is_some())
                    .field("change", change);
            }
            Self::ThreadMembersUpdate {
                sequence,
                guild_id,
                changes,
            } => {
                output
                    .field("sequence", sequence)
                    .field("has_guild", &guild_id.is_some())
                    .field("change_count", &changes.len());
            }
            Self::RoleUpsert {
                sequence,
                event_type,
                role,
                ..
            } => {
                output
                    .field("sequence", sequence)
                    .field("event_type", event_type)
                    .field("role", role);
            }
            Self::RoleDelete { sequence, .. } | Self::MemberDelete { sequence, .. } => {
                output.field("sequence", sequence);
            }
            Self::MemberUpsert {
                sequence,
                event_type,
                member,
                ..
            } => {
                output
                    .field("sequence", sequence)
                    .field("event_type", event_type)
                    .field("member", member);
            }
            Self::MemberUpdate { sequence, patch } => {
                output.field("sequence", sequence).field("patch", patch);
            }
            Self::RateLimited { sequence, limit } => {
                output.field("sequence", sequence).field("limit", limit);
            }
            Self::MembersChunk { sequence, chunk } => {
                output.field("sequence", sequence).field("chunk", chunk);
            }
            Self::GuildCreate {
                sequence,
                guild,
                channels,
                roles,
                members,
            } => {
                output
                    .field("sequence", sequence)
                    .field("guild", guild)
                    .field("channel_count", &channels.len())
                    .field("role_count", &roles.as_ref().map(|roles| roles.roles.len()))
                    .field("member_count", &members.len());
            }
            Self::GuildUpdate { sequence, guild } => {
                output.field("sequence", sequence).field("guild", guild);
            }
            Self::GuildDelete {
                sequence,
                unavailable,
                ..
            } => {
                output
                    .field("sequence", sequence)
                    .field("unavailable", unavailable);
            }
            Self::GatewayRateLimited { opcode, retry_ms } => {
                output.field("opcode", opcode).field("retry_ms", retry_ms);
            }
            Self::Unknown { sequence, name } => {
                output
                    .field("sequence", sequence)
                    .field("name_bytes", &name.len());
            }
        }
        output.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use std::fmt::Debug;

    #[test]
    fn event_queue_diagnostics_remain_small_for_many_retained_events() {
        let description = "private-queue-description".repeat(20);
        let mut queue = GatewayEventQueue::new(2048);
        for sequence in 0..2048 {
            assert!(!queue.push_normal(GatewayEvent::generic(Some(sequence), description.clone())));
        }
        assert_eq!(queue.len(), 2048);
        for output in [format!("{queue:?}"), format!("{queue:#?}")] {
            assert!(!output.contains("private-"));
            assert!(output.len() < 1024);
            assert!(output.contains("2048"));
        }
        assert_eq!(queue.pop().unwrap().description, description);
    }

    #[test]
    fn bulk_dispatch_diagnostics_keep_counts_without_expanding_payloads() {
        let channel = ChannelSnapshot::new("private-guild", "private-channel", 0, 0);
        let member = MemberSnapshot::new("private-guild", "private-user", Vec::new(), 0);
        let role = RoleSnapshot {
            guild_id: "private-guild".into(),
            role_id: "private-role".into(),
            permissions: crate::permission::PermissionSet::zero(),
            position: 1,
        };
        let memberships = vec![
            ThreadMembershipChange {
                thread_id: "private-thread".into(),
                user_id: Some("private-user".into()),
                joined: true,
            };
            1000
        ];
        let dispatches = [
            GatewayDispatch::GuildCreate {
                sequence: 1,
                guild: GuildSnapshot::new("private-guild", false, 0),
                channels: vec![channel.clone(); 1000],
                roles: Some(GuildRolesSnapshot::new(
                    "private-guild",
                    vec![role; 1000],
                    0,
                )),
                members: vec![member; 1000],
            },
            GatewayDispatch::ChannelSync {
                sequence: 2,
                guild_id: Some("private-guild".into()),
                channels: vec![channel; 1000],
                memberships: memberships.clone(),
            },
            GatewayDispatch::ThreadMembersUpdate {
                sequence: 3,
                guild_id: Some("private-guild".into()),
                changes: memberships,
            },
            GatewayDispatch::MessageDeleteBulk {
                sequence: 4,
                guild_id: Some("private-guild".into()),
                channel_id: "private-channel".into(),
                message_ids: vec!["private-message".into(); 10_000],
            },
            GatewayDispatch::MessageReaction {
                sequence: 5,
                added: true,
                guild_id: "private-guild".into(),
                channel_id: "private-channel".into(),
                message_id: "private-message".into(),
                user_id: "private-user".into(),
                emoji: "private-emoji".repeat(1000),
            },
            GatewayDispatch::Unknown {
                sequence: Some(6),
                name: "private-future-dispatch".repeat(1000),
            },
        ];
        for dispatch in &dispatches {
            for output in [format!("{dispatch:?}"), format!("{dispatch:#?}")] {
                assert!(!output.contains("private-"));
                assert!(output.len() < 1024);
            }
        }
        let GatewayDispatch::MessageDeleteBulk { message_ids, .. } = &dispatches[3] else {
            unreachable!();
        };
        assert_eq!(message_ids.len(), 10_000);
    }

    #[test]
    fn nested_gateway_diagnostics_hide_session_and_message_data() {
        let session = GatewayResumeSession::new(
            "private-session-id",
            "wss://gateway.discord.gg/?query=private-session-url",
            42,
            Some("17".into()),
        )
        .unwrap();
        let ready = GatewayDispatch::Ready {
            sequence: 42,
            session_id: session.session_id.clone(),
            resume_gateway_url: session.resume_gateway_url.clone(),
            bot_user_id: session.bot_user_id.clone(),
        };
        let frame = GatewayFrame::Dispatch(ready.clone());
        let mut machine = GatewayStateMachine::new(16);
        machine.restore_resume_session(session.clone()).unwrap();
        let mut message = MessageSnapshot::new("1", "2", "3", "4", 0);
        message.content = "private-message-content".repeat(100);
        machine
            .queue
            .push_normal(GatewayEvent::message_create(43, message.clone()));
        let resource = GatewayResourceEvent {
            event_type: D7GEV,
            sequence: 44,
            description: "private-event-description".into(),
            guild_id: Some("1".into()),
            channel_id: Some("2".into()),
            message_id: None,
            user_id: None,
            role_id: None,
            text: Some("private-resource-text".repeat(100)),
        };
        let event = GatewayEvent::resource(resource.clone());
        machine.queue.push_normal(event.clone());
        let manager = GatewayManager {
            machine: machine.clone(),
            gateway_bot: Some(GatewayBotInfo {
                url: "wss://gateway.discord.gg/?query=private-bot-url".into(),
                session_start_limit: SessionStartLimit {
                    total: 1000,
                    remaining: 999,
                    reset_after_ms: 5000,
                    max_concurrency: 1,
                },
            }),
            ..Default::default()
        };
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 100, 100);
        ingestor.machine = machine.clone();
        ingestor.snapshot.upsert_message(message.clone());
        ingestor.set_message_snapshot_channel_allowlist(["private-allowlist-value".into()]);
        let values: [&dyn Debug; 9] = [
            &session, &ready, &frame, &machine, &manager, &ingestor, &message, &resource, &event,
        ];
        for value in values {
            for output in [format!("{value:?}"), format!("{value:#?}")] {
                for secret in [
                    "private-session-id",
                    "private-session-url",
                    "private-bot-url",
                    "private-message-content",
                    "private-event-description",
                    "private-resource-text",
                    "private-allowlist-value",
                ] {
                    assert!(!output.contains(secret), "diagnostic contains {secret}");
                }
                assert!(output.len() < 8192);
            }
        }
        assert_eq!(
            GatewayResumeSession::from_json(&session.to_json()).unwrap(),
            session
        );
        assert_eq!(manager.machine.session_id(), Some("private-session-id"));
        assert!(matches!(
            machine.queue.pop().unwrap().data,
            GatewayEventData::Message(value) if value == message
        ));
    }

    #[test]
    fn websocket_frame_diagnostics_do_not_expand_binary_payloads_or_endpoint_paths() {
        let frame = WsFrame {
            opcode: 1,
            payload: b"private-frame-secret".repeat(50_000),
        };
        let endpoint = WsEndpoint {
            host: "private-endpoint-host".into(),
            port: 8080,
            path: "/?query=private-endpoint-path".into(),
        };
        for value in [&frame as &dyn Debug, &endpoint as &dyn Debug] {
            for output in [format!("{value:?}"), format!("{value:#?}")] {
                assert!(output.len() < 256);
                assert!(!output.contains("private-"));
            }
        }
        assert!(frame.payload.starts_with(b"private-frame-secret"));
        assert!(endpoint.path.contains("private-endpoint-path"));
    }

    #[test]
    fn transport_diagnostics_do_not_format_buffered_websocket_credentials() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stream =
            NetworkStream::connect("127.0.0.1", address.port(), Duration::from_secs(1), true)
                .unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let config = WebSocketConfig::default().write_buffer_size(1024 * 1024);
        let mut socket = WebSocket::from_raw_socket(
            MaybeTlsStream::Plain(stream),
            tungstenite::protocol::Role::Client,
            Some(config),
        );
        socket
            .write(Message::Text(
                "private-buffered-credential".repeat(10_000).into(),
            ))
            .unwrap();
        let transport = TungsteniteGatewayTransport {
            socket: Some(socket),
            timeout_ms: 1000,
            last_close_code: None,
        };
        for output in [format!("{transport:?}"), format!("{transport:#?}")] {
            assert!(output.len() < 256);
            assert!(!output.contains("private-buffered-credential"));
            assert!(output.contains("connected: true"));
        }
    }
}
