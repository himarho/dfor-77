use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::time::Duration;

use crate::audit::json_escape;
use crate::constants::{
    D7ECC, D7ECD, D7ECU, D7EGC, D7EGU, D7EGX, D7EMB, D7EMU, D7ERC, D7ERD, D7ERS, D7ERU, D7ETC,
    D7ETD, D7ETM, D7ETS, D7ETU, D7GAP, D7GEV, D7GRL, D7ITR, D7MBA, D7MBR, D7MBU, D7MDE, D7MSG,
    D7RDY, D7REA, D7RER,
};
use crate::event_queue::{EventQueue, QueueEvent, DEFAULT_EVENT_QUEUE_MAX_BYTES};
use crate::json::{parse_json, JsonValue};
use crate::memory::HeapSize;
use crate::network::NetworkStream;
use crate::snapshot::revision::{MessageCachePolicy, MessageRevision};
use crate::snapshot::webhook::{MessageOrigin, WebhookMessageTarget};
use crate::snapshot::{
    ChannelSnapshot, ChannelSnapshotPatch, GuildRolesSnapshot, GuildSnapshot, MemberSnapshot,
    MemberSnapshotPatch, MessageSnapshot, MessageSnapshotPatch, RoleSnapshot, SnapshotSettings,
    SnapshotStore,
};
use crate::validation::validate_snowflake;

use tungstenite::protocol::{frame::coding::CloseCode, CloseFrame, WebSocketConfig};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{client_tls_with_config, Message, WebSocket};

mod authority;
mod debug;
mod guild;
mod limits;
mod members;
pub(crate) mod payload;
pub(crate) mod request_limit;
mod threads;
use threads::{apply_thread_membership_change, parse_thread_member, parse_thread_member_array};

pub use members::GuildMembersChunk;
pub use request_limit::GatewayRateLimit;

pub use limits::{
    DailyIdentifyBudget, GatewayOutboundLimiter, GatewaySendDecision, GatewaySendKind,
};

pub const MAX_GATEWAY_PAYLOAD_BYTES: usize = 4096;
const BUG_CLOSE_WINDOW_MS: u64 = 60_000;
const BUG_CLOSE_FATAL_COUNT: usize = 3;
const BACKOFF_ATTEMPT_WINDOW_MS: u64 = 60_000;
const BACKOFF_BASE_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 60_000;
const DAILY_IDENTIFY_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;
const IDENTIFY_BUCKET_WINDOW_MS: u64 = 5_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayPayloads {
    intents: u64,
    shard_id: u32,
    shard_count: u32,
}

impl GatewayPayloads {
    pub fn new(intents: u64, shard_id: u32, shard_count: u32) -> Self {
        Self {
            intents,
            shard_id,
            shard_count,
        }
    }

    pub fn identify(&self, token: &str) -> String {
        format!(
            "{{\"op\":2,\"d\":{{\"token\":\"{}\",\"intents\":{},\"properties\":{{\"os\":\"{}\",\"browser\":\"dfor77\",\"device\":\"dfor77\"}},\"compress\":false,\"large_threshold\":50,\"shard\":[{},{}]}}}}",
            json_escape(token),
            self.intents,
            std::env::consts::OS,
            self.shard_id,
            self.shard_count
        )
    }

    pub fn resume(&self, token: &str, session_id: &str, seq: i64) -> String {
        format!(
            "{{\"op\":6,\"d\":{{\"token\":\"{}\",\"session_id\":\"{}\",\"seq\":{}}}}}",
            json_escape(token),
            json_escape(session_id),
            seq
        )
    }

    pub fn heartbeat(seq: Option<i64>) -> String {
        match seq {
            Some(seq) => format!("{{\"op\":1,\"d\":{}}}", seq),
            None => "{\"op\":1,\"d\":null}".to_string(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct GatewayEvent {
    pub event_type: i32,
    pub sequence: Option<i64>,
    pub description: String,
    pub opcode: Option<i32>,
    pub retry_ms: Option<u64>,
    pub data: GatewayEventData,
}

#[derive(Clone, Eq, PartialEq)]
pub enum GatewayEventData {
    None,
    Message(MessageSnapshot),
    Interaction(InteractionSnapshot),
    MessageDelete {
        guild_id: Option<String>,
        channel_id: String,
        message_id: String,
    },
    Reaction {
        guild_id: String,
        channel_id: String,
        message_id: String,
        user_id: String,
        emoji: String,
    },
    Member {
        guild_id: String,
        user_id: String,
    },
    MembersChunk {
        guild_id: String,
        user_ids: Vec<String>,
        metadata: String,
    },
    RateLimit(GatewayRateLimit),
    Resource {
        guild_id: Option<String>,
        channel_id: Option<String>,
        message_id: Option<String>,
        user_id: Option<String>,
        role_id: Option<String>,
        text: Option<String>,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct GatewayResourceEvent {
    pub event_type: i32,
    pub sequence: i64,
    pub description: String,
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub user_id: Option<String>,
    pub role_id: Option<String>,
    pub text: Option<String>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct InteractionSnapshot {
    pub guild_id: String,
    pub channel_id: String,
    pub interaction_id: String,
    pub application_id: String,
    pub token: String,
    pub user_id: String,
    pub interaction_type: i32,
    pub name: String,
}

impl GatewayEvent {
    pub fn ready(sequence: i64) -> Self {
        Self {
            event_type: D7RDY,
            sequence: Some(sequence),
            description: "READY".to_string(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::None,
        }
    }

    pub fn resumed(sequence: Option<i64>) -> Self {
        Self {
            event_type: D7ERS,
            sequence,
            description: "RESUMED".to_string(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::None,
        }
    }

    pub fn message(sequence: i64, description: impl Into<String>) -> Self {
        Self {
            event_type: D7MSG,
            sequence: Some(sequence),
            description: description.into(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::None,
        }
    }

    pub fn generic(sequence: Option<i64>, description: impl Into<String>) -> Self {
        Self {
            event_type: D7GEV,
            sequence,
            description: description.into(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::None,
        }
    }

    pub fn message_create(sequence: i64, message: MessageSnapshot) -> Self {
        Self {
            event_type: D7MSG,
            sequence: Some(sequence),
            description: "MESSAGE_CREATE".to_string(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::Message(message),
        }
    }

    pub fn interaction_create(sequence: i64, interaction: InteractionSnapshot) -> Self {
        Self {
            event_type: D7ITR,
            sequence: Some(sequence),
            description: "INTERACTION_CREATE".to_string(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::Interaction(interaction),
        }
    }

    pub fn message_delete(
        sequence: i64,
        guild_id: Option<String>,
        channel_id: impl Into<String>,
        message_id: impl Into<String>,
    ) -> Self {
        Self {
            event_type: D7MDE,
            sequence: Some(sequence),
            description: "MESSAGE_DELETE".to_string(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::MessageDelete {
                guild_id,
                channel_id: channel_id.into(),
                message_id: message_id.into(),
            },
        }
    }

    pub fn reaction(
        event_type: i32,
        sequence: i64,
        guild_id: impl Into<String>,
        channel_id: impl Into<String>,
        message_id: impl Into<String>,
        user_id: impl Into<String>,
        emoji: impl Into<String>,
    ) -> Self {
        let description = if event_type == D7REA {
            "MESSAGE_REACTION_ADD"
        } else {
            "MESSAGE_REACTION_REMOVE"
        };
        Self {
            event_type,
            sequence: Some(sequence),
            description: description.to_string(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::Reaction {
                guild_id: guild_id.into(),
                channel_id: channel_id.into(),
                message_id: message_id.into(),
                user_id: user_id.into(),
                emoji: emoji.into(),
            },
        }
    }

    pub fn member(
        event_type: i32,
        description: impl Into<String>,
        sequence: i64,
        guild_id: impl Into<String>,
        user_id: impl Into<String>,
    ) -> Self {
        Self {
            event_type,
            sequence: Some(sequence),
            description: description.into(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::Member {
                guild_id: guild_id.into(),
                user_id: user_id.into(),
            },
        }
    }

    pub fn resource(event: GatewayResourceEvent) -> Self {
        Self {
            event_type: event.event_type,
            sequence: Some(event.sequence),
            description: event.description,
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::Resource {
                guild_id: event.guild_id,
                channel_id: event.channel_id,
                message_id: event.message_id,
                user_id: event.user_id,
                role_id: event.role_id,
                text: event.text,
            },
        }
    }

    pub fn gap(reason: impl Into<String>) -> Self {
        Self {
            event_type: D7GAP,
            sequence: None,
            description: reason.into(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::None,
        }
    }

    pub fn rate_limited(opcode: i32, retry_ms: u64) -> Self {
        Self {
            event_type: D7GRL,
            sequence: None,
            description: format!("OPCODE {}", opcode),
            opcode: Some(opcode),
            retry_ms: Some(retry_ms),
            data: GatewayEventData::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayState {
    Idle,
    FetchingUrl,
    Identifying,
    Ready,
    Resuming,
    Backoff,
    Fatal,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayAction {
    None,
    FetchGatewayBot,
    SendIdentify,
    SendResume { seq: i64 },
    SendHeartbeat,
    Backoff { delay_ms: u64 },
    CloseAndResume,
    Fatal(&'static str),
}

#[derive(Clone, Eq, PartialEq)]
pub struct GatewayResumeSession {
    pub session_id: String,
    pub resume_gateway_url: String,
    pub last_seen_seq: i64,
    pub bot_user_id: Option<String>,
}

impl GatewayResumeSession {
    pub fn new(
        session_id: impl Into<String>,
        resume_gateway_url: impl Into<String>,
        last_seen_seq: i64,
        bot_user_id: Option<String>,
    ) -> Result<Self, &'static str> {
        let session = Self {
            session_id: session_id.into(),
            resume_gateway_url: resume_gateway_url.into(),
            last_seen_seq,
            bot_user_id,
        };
        session.validate()?;
        Ok(session)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        validate_session_id(&self.session_id)?;
        validate_gateway_url(&self.resume_gateway_url)?;
        if self.last_seen_seq < 0 {
            return Err("last_seen_seq invalid");
        }
        if self
            .bot_user_id
            .as_deref()
            .is_some_and(|user_id| !validate_snowflake(user_id))
        {
            return Err("bot_user_id invalid");
        }
        Ok(())
    }

    pub fn to_json(&self) -> String {
        let bot_user_id = self
            .bot_user_id
            .as_deref()
            .map(|value| format!("\"{}\"", json_escape(value)))
            .unwrap_or_else(|| "null".to_string());
        format!(
            "{{\"session_id\":\"{}\",\"resume_gateway_url\":\"{}\",\"last_seen_seq\":{},\"bot_user_id\":{}}}",
            json_escape(&self.session_id),
            json_escape(&self.resume_gateway_url),
            self.last_seen_seq,
            bot_user_id
        )
    }

    pub fn from_json(input: &str) -> Result<Self, &'static str> {
        let value = parse_json(input).map_err(|_| "resume session JSON invalid")?;
        let JsonValue::Object(_) = value else {
            return Err("resume session JSON invalid");
        };
        let session_id = value
            .get("session_id")
            .and_then(JsonValue::as_str)
            .ok_or("session_id missing")?
            .to_string();
        let resume_gateway_url = value
            .get("resume_gateway_url")
            .and_then(JsonValue::as_str)
            .ok_or("resume_gateway_url missing")?
            .to_string();
        let last_seen_seq = value
            .get("last_seen_seq")
            .and_then(JsonValue::as_i64)
            .ok_or("last_seen_seq missing")?;
        let bot_user_id = match value.get("bot_user_id") {
            Some(JsonValue::String(value)) => Some(value.clone()),
            Some(JsonValue::Null) | None => None,
            _ => return Err("bot_user_id invalid"),
        };
        Self::new(session_id, resume_gateway_url, last_seen_seq, bot_user_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayBackoffNext {
    Resume,
    NewIdentify,
    RefetchGatewayBot,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GatewayBackoffInfo {
    pub reason: &'static str,
    pub next: GatewayBackoffNext,
    pub delay_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GatewayEventOutcome {
    accepted: bool,
    action: GatewayAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReactionEventParts {
    sequence: i64,
    added: bool,
    guild_id: String,
    channel_id: String,
    message_id: String,
    user_id: String,
    emoji: String,
}

fn channel_event_type(name: &str) -> i32 {
    match name {
        "CHANNEL_CREATE" => D7ECC,
        "CHANNEL_UPDATE" => D7ECU,
        "THREAD_CREATE" => D7ETC,
        "THREAD_UPDATE" => D7ETU,
        _ => D7GEV,
    }
}

fn channel_delete_event_type(name: &str) -> i32 {
    match name {
        "THREAD_DELETE" => D7ETD,
        _ => D7ECD,
    }
}

fn role_upsert_event_type(name: &str) -> i32 {
    match name {
        "GUILD_ROLE_CREATE" => D7ERC,
        "GUILD_ROLE_UPDATE" => D7ERU,
        _ => D7GEV,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GatewayBackoffAttempt {
    reason: &'static str,
    next: GatewayBackoffNext,
    recorded_at_ms: u64,
}

impl GatewayEventOutcome {
    fn ignored() -> Self {
        Self {
            accepted: false,
            action: GatewayAction::None,
        }
    }

    fn accepted(action: GatewayAction) -> Self {
        Self {
            accepted: true,
            action,
        }
    }
}

#[derive(Clone)]
pub struct GatewayStateMachine {
    state: GatewayState,
    ready_seen: bool,
    resume_session_invalidated: bool,
    session_id: Option<String>,
    resume_gateway_url: Option<String>,
    bot_user_id: Option<String>,
    last_seen_seq: Option<i64>,
    resume_seq_used: Option<i64>,
    backoff_info: Option<GatewayBackoffInfo>,
    backoff_attempts: VecDeque<GatewayBackoffAttempt>,
    bug_close_events: VecDeque<(u16, u64)>,
    queue: GatewayEventQueue,
}

impl GatewayStateMachine {
    pub fn new(event_queue_capacity: usize) -> Self {
        Self {
            state: GatewayState::Idle,
            ready_seen: false,
            resume_session_invalidated: false,
            session_id: None,
            resume_gateway_url: None,
            bot_user_id: None,
            last_seen_seq: None,
            resume_seq_used: None,
            backoff_info: None,
            backoff_attempts: VecDeque::new(),
            bug_close_events: VecDeque::new(),
            queue: GatewayEventQueue::new(event_queue_capacity),
        }
    }

    pub fn state(&self) -> GatewayState {
        self.state
    }

    pub fn last_seen_seq(&self) -> Option<i64> {
        self.last_seen_seq
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn resume_gateway_url(&self) -> Option<&str> {
        self.resume_gateway_url.as_deref()
    }

    pub fn bot_user_id(&self) -> Option<&str> {
        self.bot_user_id.as_deref()
    }

    pub fn resume_session(&self) -> Option<GatewayResumeSession> {
        let session_id = self.session_id.as_ref()?;
        let resume_gateway_url = self.resume_gateway_url.as_ref()?;
        let last_seen_seq = self.last_seen_seq?;
        GatewayResumeSession::new(
            session_id.clone(),
            resume_gateway_url.clone(),
            last_seen_seq,
            self.bot_user_id.clone(),
        )
        .ok()
    }

    pub fn resume_session_invalidated(&self) -> bool {
        self.resume_session_invalidated
    }

    pub fn restore_resume_session(
        &mut self,
        session: GatewayResumeSession,
    ) -> Result<GatewayAction, &'static str> {
        session.validate()?;
        let same_session = self.session_id.as_deref() == Some(session.session_id.as_str());
        if same_session && self.bot_user_id_conflicts(session.bot_user_id.as_deref()) {
            return Err("resume bot user id mismatch");
        }
        self.invalidate_replaced_session(&session.session_id);
        if same_session
            && self
                .last_seen_seq
                .is_some_and(|seen| session.last_seen_seq > seen)
        {
            self.gap_if_ready("GATEWAY_RESUME_CURSOR_ADVANCED");
        }
        self.ready_seen = true;
        self.resume_session_invalidated = false;
        self.state = GatewayState::Resuming;
        self.backoff_info = None;
        self.session_id = Some(session.session_id);
        self.resume_gateway_url = Some(session.resume_gateway_url);
        self.last_seen_seq = Some(if same_session {
            self.last_seen_seq.map_or(session.last_seen_seq, |seen| {
                seen.max(session.last_seen_seq)
            })
        } else {
            session.last_seen_seq
        });
        self.resume_seq_used = None;
        if !same_session || self.bot_user_id.is_none() {
            self.bot_user_id = session.bot_user_id;
        }
        Ok(GatewayAction::SendResume {
            seq: self.prepare_resume_seq(),
        })
    }

    pub fn gap_epoch(&self) -> u64 {
        self.queue.gap_epoch()
    }

    pub fn set_event_queue_max_bytes(&mut self, max_bytes: usize) -> bool {
        self.queue.set_max_bytes(max_bytes)
    }

    pub fn backoff_info(&self) -> Option<GatewayBackoffInfo> {
        if self.state == GatewayState::Backoff {
            self.backoff_info
        } else {
            None
        }
    }

    pub fn start(&mut self) -> GatewayAction {
        if self.session_id.is_some() {
            self.gap_if_ready("GATEWAY_NEW_IDENTIFY");
        }
        self.invalidate_resume_session();
        self.state = GatewayState::FetchingUrl;
        self.backoff_info = None;
        GatewayAction::FetchGatewayBot
    }

    pub fn gateway_url_fetched(&mut self) -> GatewayAction {
        if self.session_id.is_some() {
            self.gap_if_ready("GATEWAY_NEW_IDENTIFY");
        }
        self.invalidate_resume_session();
        self.state = GatewayState::Identifying;
        self.backoff_info = None;
        GatewayAction::SendIdentify
    }

    pub fn ready(
        &mut self,
        sequence: i64,
        session_id: impl Into<String>,
        resume_gateway_url: impl Into<String>,
    ) {
        self.ready_with_bot_user(sequence, session_id, resume_gateway_url, None::<String>);
    }

    fn ready_with_bot_user(
        &mut self,
        sequence: i64,
        session_id: impl Into<String>,
        resume_gateway_url: impl Into<String>,
        bot_user_id: Option<String>,
    ) {
        let session_id = session_id.into();
        if sequence < 0
            || self.session_id.as_deref() == Some(session_id.as_str())
                && self
                    .last_seen_seq
                    .is_some_and(|previous| sequence <= previous)
        {
            return;
        }
        if self.session_id.as_deref() == Some(session_id.as_str())
            && self.bot_user_id_conflicts(bot_user_id.as_deref())
        {
            self.queue
                .push_control(GatewayEvent::gap("READY_IDENTITY_MISMATCH"));
            return;
        }
        let replaced = self.invalidate_replaced_session(&session_id);
        self.ready_seen = true;
        self.resume_session_invalidated = false;
        self.state = GatewayState::Ready;
        self.backoff_info = None;
        self.last_seen_seq = Some(sequence);
        self.resume_seq_used = None;
        self.session_id = Some(session_id);
        self.resume_gateway_url = Some(resume_gateway_url.into());
        if replaced || self.bot_user_id.is_none() {
            self.bot_user_id = bot_user_id;
        }
        self.queue.push_control(GatewayEvent::ready(sequence));
    }

    pub fn dispatch(&mut self, sequence: i64, description: impl Into<String>) -> bool {
        self.dispatch_outcome(sequence, description).accepted
    }

    pub fn message_create(&mut self, sequence: i64, message: MessageSnapshot) -> bool {
        self.message_create_outcome(sequence, message).accepted
    }

    pub fn message_delete(
        &mut self,
        sequence: i64,
        guild_id: Option<String>,
        channel_id: impl Into<String>,
        message_id: impl Into<String>,
    ) -> bool {
        self.message_delete_outcome(sequence, guild_id, channel_id, message_id)
            .accepted
    }

    pub fn observe_dispatch(&mut self, sequence: i64) -> bool {
        self.accept_sequence(sequence)
    }

    pub fn resumed_event(&mut self, sequence: Option<i64>) {
        if self.resume_session_invalidated {
            return;
        }
        if let Some(sequence) = sequence {
            if sequence < 0 {
                return;
            }
            if self.last_seen_seq.is_some_and(|last| {
                sequence < last || sequence == last && self.state != GatewayState::Resuming
            }) {
                if self.state == GatewayState::Resuming {
                    self.resume_succeeded();
                }
                return;
            }
            self.last_seen_seq = Some(sequence);
        }
        self.resume_succeeded();
        self.queue.push_control(GatewayEvent::resumed(sequence));
    }

    pub fn reconnect_opcode(&mut self) -> GatewayAction {
        if self.can_resume() {
            self.state = GatewayState::Resuming;
            self.backoff_info = None;
            GatewayAction::SendResume {
                seq: self.prepare_resume_seq(),
            }
        } else {
            self.gap_if_ready("RECONNECT_WITHOUT_RESUME_STATE");
            self.state = GatewayState::FetchingUrl;
            self.backoff_info = None;
            GatewayAction::FetchGatewayBot
        }
    }

    pub fn invalid_session(&mut self, resumable: bool) -> GatewayAction {
        if resumable && self.can_resume() {
            self.state = GatewayState::Resuming;
            self.backoff_info = None;
            GatewayAction::SendResume {
                seq: self.prepare_resume_seq(),
            }
        } else {
            self.gap_if_ready("INVALID_SESSION_NOT_RESUMABLE");
            self.invalidate_resume_session();
            self.enter_backoff(
                "INVALID_SESSION_NOT_RESUMABLE",
                GatewayBackoffNext::NewIdentify,
                1_000,
            )
        }
    }

    pub fn close(&mut self, code: Option<u16>, for_explicit_shutdown: bool) -> GatewayAction {
        self.close_at(code, for_explicit_shutdown, 0)
    }

    pub fn close_at(
        &mut self,
        code: Option<u16>,
        for_explicit_shutdown: bool,
        now_ms: u64,
    ) -> GatewayAction {
        if for_explicit_shutdown {
            self.invalidate_resume_session();
            self.state = GatewayState::Closed;
            self.backoff_info = None;
            return GatewayAction::None;
        }
        match code {
            Some(4004) | Some(4010) | Some(4011) | Some(4012) | Some(4013) | Some(4014) => {
                self.gap_if_ready("GATEWAY_FATAL_CLOSE");
                self.invalidate_resume_session();
                self.state = GatewayState::Fatal;
                self.backoff_info = None;
                GatewayAction::Fatal("GATEWAY_FATAL_CLOSE")
            }
            Some(4003) | Some(4007) | Some(4009) => {
                self.backoff_new_identify_with_gap("GATEWAY_SESSION_INVALIDATED", 1_000)
            }
            Some(code @ (4001 | 4002 | 4005)) => self.bug_close(code, now_ms),
            Some(4008) => {
                if self.can_resume() {
                    self.enter_exponential_backoff(
                        "RATE_LIMITED_4008",
                        GatewayBackoffNext::Resume,
                        now_ms,
                    )
                } else {
                    self.backoff_new_identify_with_gap("GATEWAY_CLOSED", 1_000)
                }
            }
            Some(1000 | 1001) => self.new_identify_with_gap("GATEWAY_CLOSED"),
            Some(4000) => {
                if self.can_resume() {
                    self.enter_backoff("GATEWAY_CLOSED", GatewayBackoffNext::Resume, 1_000)
                } else {
                    self.backoff_new_identify_with_gap("GATEWAY_CLOSED", 1_000)
                }
            }
            None => {
                if self.can_resume() {
                    self.state = GatewayState::Resuming;
                    self.backoff_info = None;
                    GatewayAction::SendResume {
                        seq: self.prepare_resume_seq(),
                    }
                } else {
                    self.new_identify_with_gap("GATEWAY_CLOSED")
                }
            }
            _ => {
                if self.can_resume() {
                    self.state = GatewayState::Resuming;
                    self.backoff_info = None;
                    GatewayAction::SendResume {
                        seq: self.prepare_resume_seq(),
                    }
                } else {
                    self.new_identify_with_gap("GATEWAY_CLOSED")
                }
            }
        }
    }

    pub fn resume_succeeded(&mut self) {
        if self.resume_session_invalidated {
            return;
        }
        self.state = GatewayState::Ready;
        self.backoff_info = None;
    }

    pub fn resume_failed(&mut self, reason: &'static str) -> GatewayAction {
        self.resume_failed_at(reason, 0)
    }

    pub fn resume_failed_at(&mut self, reason: &'static str, now_ms: u64) -> GatewayAction {
        self.gap_if_ready(reason);
        self.invalidate_resume_session();
        self.enter_exponential_backoff(reason, GatewayBackoffNext::NewIdentify, now_ms)
    }

    pub fn resume_send_wait(&mut self, delay_ms: u64) -> GatewayAction {
        self.enter_backoff(
            "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED",
            GatewayBackoffNext::Resume,
            delay_ms,
        )
    }

    pub fn backoff_elapsed(&mut self) -> GatewayAction {
        let next = self
            .backoff_info
            .map(|info| info.next)
            .unwrap_or(GatewayBackoffNext::NewIdentify);
        self.backoff_info = None;
        match next {
            GatewayBackoffNext::Resume => {
                if self.can_resume() {
                    self.state = GatewayState::Resuming;
                    GatewayAction::SendResume {
                        seq: self.prepare_resume_seq(),
                    }
                } else {
                    self.new_identify_with_gap("BACKOFF_WITHOUT_RESUME_STATE")
                }
            }
            GatewayBackoffNext::NewIdentify => self.new_identify_without_gap(),
            GatewayBackoffNext::RefetchGatewayBot => {
                self.state = GatewayState::FetchingUrl;
                GatewayAction::FetchGatewayBot
            }
            GatewayBackoffNext::Shutdown => {
                self.invalidate_resume_session();
                self.state = GatewayState::Closed;
                GatewayAction::None
            }
        }
    }

    pub fn backoff_refetch_gateway_bot(
        &mut self,
        reason: &'static str,
        delay_ms: u64,
    ) -> GatewayAction {
        self.enter_backoff(reason, GatewayBackoffNext::RefetchGatewayBot, delay_ms)
    }

    pub fn backoff_shutdown(&mut self, reason: &'static str, delay_ms: u64) -> GatewayAction {
        self.enter_backoff(reason, GatewayBackoffNext::Shutdown, delay_ms)
    }

    pub fn event_queue_overflow(&mut self) -> GatewayAction {
        self.queue_overflow_action(true)
    }

    pub fn discord_rate_limited(&mut self, opcode: i32, retry_ms: u64) {
        self.queue
            .push_control(GatewayEvent::rate_limited(opcode, retry_ms));
    }

    fn rate_limited(&mut self, sequence: Option<i64>, limit: GatewayRateLimit) {
        if matches!(self.state, GatewayState::Closed | GatewayState::Fatal)
            || sequence.is_some_and(|sequence| !self.accept_sequence(sequence))
        {
            return;
        }
        self.queue.push_control(limit.event(sequence));
    }

    pub fn pop_event(&mut self) -> Option<GatewayEvent> {
        self.queue.pop()
    }

    pub fn handle_frame(&mut self, frame: GatewayFrame) -> GatewayAction {
        match frame {
            GatewayFrame::Hello { .. }
            | GatewayFrame::HeartbeatAck
            | GatewayFrame::Unknown { .. } => GatewayAction::None,
            GatewayFrame::HeartbeatRequest => GatewayAction::SendHeartbeat,
            GatewayFrame::Reconnect => self.reconnect_opcode(),
            GatewayFrame::InvalidSession { resumable } => self.invalid_session(resumable),
            GatewayFrame::Dispatch(dispatch) => self.handle_dispatch(dispatch),
        }
    }

    fn handle_dispatch(&mut self, dispatch: GatewayDispatch) -> GatewayAction {
        match dispatch {
            GatewayDispatch::Ready {
                sequence,
                session_id,
                resume_gateway_url,
                bot_user_id,
            } => {
                self.ready_with_bot_user(sequence, session_id, resume_gateway_url, bot_user_id);
                GatewayAction::None
            }
            GatewayDispatch::Resumed { sequence } => {
                self.resumed_event(sequence);
                GatewayAction::None
            }
            GatewayDispatch::MessageCreate { sequence, message } => {
                self.message_create_outcome(sequence, message).action
            }
            GatewayDispatch::InteractionCreate {
                sequence,
                interaction,
            } => {
                self.interaction_create_outcome(sequence, interaction)
                    .action
            }
            GatewayDispatch::MessageDelete {
                sequence,
                guild_id,
                channel_id,
                message_id,
            } => {
                self.message_delete_outcome(sequence, guild_id, channel_id, message_id)
                    .action
            }
            GatewayDispatch::MessageDeleteBulk {
                sequence,
                guild_id,
                channel_id,
                message_ids,
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type: D7EMB,
                    sequence,
                    description: "MESSAGE_DELETE_BULK".to_string(),
                    guild_id,
                    channel_id: Some(channel_id),
                    message_id: message_ids.first().cloned(),
                    user_id: None,
                    role_id: None,
                    text: Some(message_ids.join(",")),
                })
                .action
            }
            GatewayDispatch::MessageReaction {
                sequence,
                added,
                guild_id,
                channel_id,
                message_id,
                user_id,
                emoji,
            } => {
                self.reaction_outcome(ReactionEventParts {
                    sequence,
                    added,
                    guild_id,
                    channel_id,
                    message_id,
                    user_id,
                    emoji,
                })
                .action
            }
            GatewayDispatch::MessageUpdate { sequence, patch } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type: D7EMU,
                    sequence,
                    description: "MESSAGE_UPDATE".to_string(),
                    guild_id: patch.guild_id,
                    channel_id: Some(patch.channel_id),
                    message_id: Some(patch.message_id),
                    user_id: patch.user_id,
                    role_id: None,
                    text: patch.content,
                })
                .action
            }
            GatewayDispatch::ChannelUpsert {
                sequence,
                event_type,
                event_name,
                channel,
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type,
                    sequence,
                    description: event_name,
                    guild_id: Some(channel.guild_id),
                    channel_id: Some(channel.channel_id),
                    message_id: None,
                    user_id: None,
                    role_id: None,
                    text: None,
                })
                .action
            }
            GatewayDispatch::ChannelUpdate {
                sequence,
                event_type,
                event_name,
                patch,
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type,
                    sequence,
                    description: event_name,
                    guild_id: patch.guild_id,
                    channel_id: Some(patch.channel_id),
                    message_id: None,
                    user_id: None,
                    role_id: None,
                    text: None,
                })
                .action
            }
            GatewayDispatch::ChannelDelete {
                sequence,
                event_type,
                event_name,
                guild_id,
                channel_id,
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type,
                    sequence,
                    description: event_name,
                    guild_id,
                    channel_id: Some(channel_id),
                    message_id: None,
                    user_id: None,
                    role_id: None,
                    text: None,
                })
                .action
            }
            GatewayDispatch::ChannelSync {
                sequence,
                guild_id,
                channels,
                memberships,
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type: D7ETS,
                    sequence,
                    description: "THREAD_LIST_SYNC".to_string(),
                    guild_id,
                    channel_id: channels
                        .first()
                        .map(|channel| channel.channel_id.clone())
                        .or_else(|| memberships.first().map(|change| change.thread_id.clone())),
                    message_id: None,
                    user_id: memberships
                        .first()
                        .and_then(|change| change.user_id.clone()),
                    role_id: None,
                    text: None,
                })
                .action
            }
            GatewayDispatch::ThreadMemberUpdate {
                sequence,
                guild_id,
                change,
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type: D7ETM,
                    sequence,
                    description: "THREAD_MEMBER_UPDATE".to_string(),
                    guild_id,
                    channel_id: Some(change.thread_id),
                    message_id: None,
                    user_id: change.user_id,
                    role_id: None,
                    text: None,
                })
                .action
            }
            GatewayDispatch::ThreadMembersUpdate {
                sequence,
                guild_id,
                changes,
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type: D7ETM,
                    sequence,
                    description: "THREAD_MEMBERS_UPDATE".to_string(),
                    guild_id,
                    channel_id: changes.first().map(|change| change.thread_id.clone()),
                    message_id: None,
                    user_id: changes.first().and_then(|change| change.user_id.clone()),
                    role_id: None,
                    text: None,
                })
                .action
            }
            GatewayDispatch::RoleUpsert {
                sequence,
                event_type,
                event_name,
                role,
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type,
                    sequence,
                    description: event_name,
                    guild_id: Some(role.guild_id),
                    channel_id: None,
                    message_id: None,
                    user_id: None,
                    role_id: Some(role.role_id),
                    text: None,
                })
                .action
            }
            GatewayDispatch::RoleDelete {
                sequence,
                guild_id,
                role_id,
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type: D7ERD,
                    sequence,
                    description: "GUILD_ROLE_DELETE".to_string(),
                    guild_id: Some(guild_id),
                    channel_id: None,
                    message_id: None,
                    user_id: None,
                    role_id: Some(role_id),
                    text: None,
                })
                .action
            }
            GatewayDispatch::MemberUpsert {
                sequence,
                event_type,
                event_name,
                member,
            } => {
                self.member_outcome(
                    sequence,
                    event_type,
                    event_name,
                    &member.guild_id,
                    &member.user_id,
                )
                .action
            }
            GatewayDispatch::MembersChunk { sequence, chunk } => {
                self.member_chunk_outcome(sequence, &chunk).action
            }
            GatewayDispatch::MemberUpdate { sequence, patch } => {
                self.member_outcome(
                    sequence,
                    D7MBU,
                    "GUILD_MEMBER_UPDATE",
                    &patch.guild_id,
                    &patch.user_id,
                )
                .action
            }
            GatewayDispatch::MemberDelete {
                sequence,
                guild_id,
                user_id,
            } => {
                self.member_outcome(sequence, D7MBR, "GUILD_MEMBER_REMOVE", guild_id, user_id)
                    .action
            }
            GatewayDispatch::GuildCreate {
                sequence, guild, ..
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type: D7EGC,
                    sequence,
                    description: "GUILD_CREATE".to_string(),
                    guild_id: Some(guild.guild_id),
                    channel_id: None,
                    message_id: None,
                    user_id: guild.owner_id,
                    role_id: None,
                    text: None,
                })
                .action
            }
            GatewayDispatch::GuildUpdate { sequence, guild } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type: D7EGU,
                    sequence,
                    description: "GUILD_UPDATE".to_string(),
                    guild_id: Some(guild.guild_id),
                    channel_id: None,
                    message_id: None,
                    user_id: guild.owner_id,
                    role_id: None,
                    text: None,
                })
                .action
            }
            GatewayDispatch::GuildDelete {
                sequence, guild_id, ..
            } => {
                self.resource_outcome(GatewayResourceEvent {
                    event_type: D7EGX,
                    sequence,
                    description: "GUILD_DELETE".to_string(),
                    guild_id: Some(guild_id),
                    channel_id: None,
                    message_id: None,
                    user_id: None,
                    role_id: None,
                    text: None,
                })
                .action
            }
            GatewayDispatch::GatewayRateLimited { opcode, retry_ms } => {
                self.discord_rate_limited(opcode, retry_ms);
                GatewayAction::None
            }
            GatewayDispatch::RateLimited { sequence, limit } => {
                self.rate_limited(sequence, limit);
                GatewayAction::None
            }
            GatewayDispatch::Unknown { sequence, name } => {
                self.generic_dispatch_outcome(sequence, name).action
            }
        }
    }

    fn accept_sequence(&mut self, sequence: i64) -> bool {
        if self.resume_session_invalidated
            || sequence < 0
            || self
                .last_seen_seq
                .is_some_and(|previous| sequence <= previous)
            || self
                .resume_seq_used
                .is_some_and(|resume_seq| sequence <= resume_seq)
        {
            return false;
        }
        self.last_seen_seq = Some(sequence);
        true
    }

    fn dispatch_outcome(
        &mut self,
        sequence: i64,
        description: impl Into<String>,
    ) -> GatewayEventOutcome {
        if !self.accept_sequence(sequence) {
            return GatewayEventOutcome::ignored();
        }
        let overflowed = self
            .queue
            .push_normal(GatewayEvent::message(sequence, description));
        GatewayEventOutcome::accepted(self.queue_overflow_action(overflowed))
    }

    fn generic_dispatch_outcome(
        &mut self,
        sequence: Option<i64>,
        description: impl Into<String>,
    ) -> GatewayEventOutcome {
        if let Some(sequence) = sequence {
            if !self.accept_sequence(sequence) {
                return GatewayEventOutcome::ignored();
            }
        }
        let description = description.into();
        if let Some(reason) = invalid_dispatch_gap_reason(&description) {
            self.queue.push_control(GatewayEvent::gap(reason));
        }
        let overflowed = self
            .queue
            .push_normal(GatewayEvent::generic(sequence, description));
        GatewayEventOutcome::accepted(self.queue_overflow_action(overflowed))
    }

    fn resource_outcome(&mut self, event: GatewayResourceEvent) -> GatewayEventOutcome {
        if !self.accept_sequence(event.sequence) {
            return GatewayEventOutcome::ignored();
        }
        let overflowed = self.queue.push_normal(GatewayEvent::resource(event));
        GatewayEventOutcome::accepted(self.queue_overflow_action(overflowed))
    }

    fn member_chunk_outcome(
        &mut self,
        sequence: i64,
        chunk: &GuildMembersChunk,
    ) -> GatewayEventOutcome {
        if !self.accept_sequence(sequence) {
            return GatewayEventOutcome::ignored();
        }
        let overflowed = self.queue.push_normal(chunk.event(sequence));
        GatewayEventOutcome::accepted(self.queue_overflow_action(overflowed))
    }

    fn message_create_outcome(
        &mut self,
        sequence: i64,
        message: MessageSnapshot,
    ) -> GatewayEventOutcome {
        if !self.accept_sequence(sequence) {
            return GatewayEventOutcome::ignored();
        }
        let overflowed = self
            .queue
            .push_normal(GatewayEvent::message_create(sequence, message));
        GatewayEventOutcome::accepted(self.queue_overflow_action(overflowed))
    }

    fn interaction_create_outcome(
        &mut self,
        sequence: i64,
        interaction: InteractionSnapshot,
    ) -> GatewayEventOutcome {
        if !self.accept_sequence(sequence) {
            return GatewayEventOutcome::ignored();
        }
        let overflowed = self
            .queue
            .push_normal(GatewayEvent::interaction_create(sequence, interaction));
        GatewayEventOutcome::accepted(self.queue_overflow_action(overflowed))
    }

    fn message_delete_outcome(
        &mut self,
        sequence: i64,
        guild_id: Option<String>,
        channel_id: impl Into<String>,
        message_id: impl Into<String>,
    ) -> GatewayEventOutcome {
        if !self.accept_sequence(sequence) {
            return GatewayEventOutcome::ignored();
        }
        let overflowed = self.queue.push_normal(GatewayEvent::message_delete(
            sequence, guild_id, channel_id, message_id,
        ));
        GatewayEventOutcome::accepted(self.queue_overflow_action(overflowed))
    }

    fn reaction_outcome(&mut self, event: ReactionEventParts) -> GatewayEventOutcome {
        if !self.accept_sequence(event.sequence) {
            return GatewayEventOutcome::ignored();
        }
        let event_type = if event.added { D7REA } else { D7RER };
        let overflowed = self.queue.push_normal(GatewayEvent::reaction(
            event_type,
            event.sequence,
            event.guild_id,
            event.channel_id,
            event.message_id,
            event.user_id,
            event.emoji,
        ));
        GatewayEventOutcome::accepted(self.queue_overflow_action(overflowed))
    }

    fn member_outcome(
        &mut self,
        sequence: i64,
        event_type: i32,
        description: impl Into<String>,
        guild_id: impl Into<String>,
        user_id: impl Into<String>,
    ) -> GatewayEventOutcome {
        if !self.accept_sequence(sequence) {
            return GatewayEventOutcome::ignored();
        }
        let overflowed = self.queue.push_normal(GatewayEvent::member(
            event_type,
            description,
            sequence,
            guild_id,
            user_id,
        ));
        GatewayEventOutcome::accepted(self.queue_overflow_action(overflowed))
    }

    fn queue_overflow_action(&mut self, overflowed: bool) -> GatewayAction {
        if !overflowed {
            return GatewayAction::None;
        }
        if self.can_resume() {
            self.state = GatewayState::Resuming;
            self.backoff_info = None;
            GatewayAction::SendResume {
                seq: self.prepare_resume_seq(),
            }
        } else {
            self.state = GatewayState::FetchingUrl;
            self.backoff_info = None;
            GatewayAction::FetchGatewayBot
        }
    }

    fn new_identify_with_gap(&mut self, reason: &'static str) -> GatewayAction {
        self.gap_if_ready(reason);
        self.invalidate_resume_session();
        self.state = GatewayState::FetchingUrl;
        self.backoff_info = None;
        GatewayAction::FetchGatewayBot
    }

    fn backoff_new_identify_with_gap(
        &mut self,
        reason: &'static str,
        delay_ms: u64,
    ) -> GatewayAction {
        self.gap_if_ready(reason);
        self.invalidate_resume_session();
        self.enter_backoff(reason, GatewayBackoffNext::NewIdentify, delay_ms)
    }

    fn new_identify_without_gap(&mut self) -> GatewayAction {
        self.invalidate_resume_session();
        self.state = GatewayState::FetchingUrl;
        self.backoff_info = None;
        GatewayAction::FetchGatewayBot
    }

    fn enter_backoff(
        &mut self,
        reason: &'static str,
        next: GatewayBackoffNext,
        delay_ms: u64,
    ) -> GatewayAction {
        self.state = GatewayState::Backoff;
        self.backoff_info = Some(GatewayBackoffInfo {
            reason,
            next,
            delay_ms,
        });
        GatewayAction::Backoff { delay_ms }
    }

    fn bug_close(&mut self, code: u16, now_ms: u64) -> GatewayAction {
        let cutoff = now_ms.saturating_sub(BUG_CLOSE_WINDOW_MS);
        self.bug_close_events
            .retain(|(_, recorded_at)| *recorded_at >= cutoff);
        self.bug_close_events.push_back((code, now_ms));
        if self.bug_close_events.len() >= BUG_CLOSE_FATAL_COUNT {
            self.gap_if_ready("GATEWAY_BUG_CLOSE_REPEATED");
            self.invalidate_resume_session();
            self.state = GatewayState::Fatal;
            GatewayAction::Fatal("GATEWAY_BUG_CLOSE_REPEATED")
        } else {
            let next = if self.can_resume() {
                GatewayBackoffNext::Resume
            } else {
                GatewayBackoffNext::NewIdentify
            };
            self.enter_exponential_backoff("GATEWAY_BUG_CLOSE", next, now_ms)
        }
    }

    fn enter_exponential_backoff(
        &mut self,
        reason: &'static str,
        next: GatewayBackoffNext,
        now_ms: u64,
    ) -> GatewayAction {
        let attempt = self.record_backoff_attempt(reason, next, now_ms);
        let delay_ms = exponential_backoff_delay_ms(reason, next, attempt, now_ms);
        self.enter_backoff(reason, next, delay_ms)
    }

    fn record_backoff_attempt(
        &mut self,
        reason: &'static str,
        next: GatewayBackoffNext,
        now_ms: u64,
    ) -> u32 {
        let cutoff = now_ms.saturating_sub(BACKOFF_ATTEMPT_WINDOW_MS);
        self.backoff_attempts
            .retain(|attempt| attempt.recorded_at_ms >= cutoff);
        let attempt = self
            .backoff_attempts
            .iter()
            .filter(|attempt| attempt.reason == reason && attempt.next == next)
            .count() as u32;
        self.backoff_attempts.push_back(GatewayBackoffAttempt {
            reason,
            next,
            recorded_at_ms: now_ms,
        });
        attempt
    }

    fn can_resume(&self) -> bool {
        self.session_id.is_some()
            && self.resume_gateway_url.is_some()
            && self.last_seen_seq.is_some()
    }

    fn invalidate_resume_session(&mut self) {
        self.resume_session_invalidated |= self.ready_seen || self.session_id.is_some();
        self.session_id = None;
        self.resume_gateway_url = None;
        self.bot_user_id = None;
        self.last_seen_seq = None;
        self.resume_seq_used = None;
    }

    fn prepare_resume_seq(&mut self) -> i64 {
        let seq = self.last_seen_seq.unwrap_or(0);
        self.resume_seq_used = Some(seq);
        seq
    }

    fn gap_if_ready(&mut self, reason: &'static str) {
        if self.ready_seen {
            self.queue.push_control(GatewayEvent::gap(reason));
        }
    }

    fn invalidate_replaced_session(&mut self, session_id: &str) -> bool {
        let replaced = self.session_id.as_deref() != Some(session_id);
        if replaced {
            self.gap_if_ready("GATEWAY_SESSION_REPLACED");
        }
        replaced
    }

    fn bot_user_id_conflicts(&self, user_id: Option<&str>) -> bool {
        self.bot_user_id
            .as_deref()
            .zip(user_id)
            .is_some_and(|(current, candidate)| {
                current.trim_start_matches('0') != candidate.trim_start_matches('0')
            })
    }
}

fn exponential_backoff_delay_ms(
    reason: &'static str,
    next: GatewayBackoffNext,
    attempt: u32,
    now_ms: u64,
) -> u64 {
    let shift = attempt.min(16);
    let base = BACKOFF_BASE_MS.saturating_mul(1_u64 << shift);
    let capped = base.min(BACKOFF_MAX_MS);
    if attempt == 0 {
        return capped;
    }
    let jitter = backoff_jitter_per_mille(reason, next, attempt, now_ms);
    capped.saturating_mul(jitter) / 1_000
}

fn backoff_jitter_per_mille(
    reason: &'static str,
    next: GatewayBackoffNext,
    attempt: u32,
    now_ms: u64,
) -> u64 {
    let mut seed = now_ms ^ u64::from(attempt).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    seed ^= match next {
        GatewayBackoffNext::Resume => 0x52,
        GatewayBackoffNext::NewIdentify => 0x4E,
        GatewayBackoffNext::RefetchGatewayBot => 0x47,
        GatewayBackoffNext::Shutdown => 0x53,
    };
    for byte in reason.bytes() {
        seed ^= u64::from(byte);
        seed = seed.wrapping_mul(0x1000_0000_01B3);
        seed ^= seed >> 32;
    }
    500 + (seed % 1_001)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HeartbeatTracker {
    interval_ms: u64,
    next_due_ms: Option<u64>,
    pending_since_ms: Option<u64>,
    last_ack_at_ms: Option<u64>,
}

impl HeartbeatTracker {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms: interval_ms.max(1),
            next_due_ms: None,
            pending_since_ms: None,
            last_ack_at_ms: None,
        }
    }

    pub fn hello(&mut self, now_ms: u64) {
        self.next_due_ms =
            Some(now_ms.saturating_add(heartbeat_initial_delay_ms(self.interval_ms, now_ms)));
    }

    pub fn due(&self, now_ms: u64) -> bool {
        self.next_due_ms.is_some_and(|due| now_ms >= due)
    }

    pub fn mark_sent(&mut self, now_ms: u64) {
        self.pending_since_ms.get_or_insert(now_ms);
        self.next_due_ms = Some(now_ms.saturating_add(self.interval_ms));
    }

    pub fn ack(&mut self, now_ms: u64) {
        self.last_ack_at_ms = Some(now_ms);
        self.pending_since_ms = None;
    }

    pub fn missed_ack(&self, now_ms: u64) -> bool {
        self.pending_since_ms
            .is_some_and(|sent| now_ms.saturating_sub(sent) >= self.interval_ms)
    }

    pub fn last_ack_at_ms(&self) -> Option<u64> {
        self.last_ack_at_ms
    }

    pub fn next_due_ms(&self) -> Option<u64> {
        self.next_due_ms
    }

    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }
}

fn heartbeat_initial_delay_ms(interval_ms: u64, now_ms: u64) -> u64 {
    let interval = interval_ms.max(1);
    let mut seed = now_ms ^ interval.rotate_left(17);
    seed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    seed ^= seed >> 33;
    seed % interval.saturating_add(1)
}

#[derive(Clone, Debug, PartialEq)]
pub enum GatewayFrame {
    Hello { heartbeat_interval_ms: u64 },
    HeartbeatRequest,
    HeartbeatAck,
    Reconnect,
    InvalidSession { resumable: bool },
    Dispatch(GatewayDispatch),
    Unknown { opcode: i64 },
}

#[derive(Clone, PartialEq)]
pub enum GatewayDispatch {
    Ready {
        sequence: i64,
        session_id: String,
        resume_gateway_url: String,
        bot_user_id: Option<String>,
    },
    Resumed {
        sequence: Option<i64>,
    },
    MessageCreate {
        sequence: i64,
        message: MessageSnapshot,
    },
    InteractionCreate {
        sequence: i64,
        interaction: InteractionSnapshot,
    },
    MessageDelete {
        sequence: i64,
        guild_id: Option<String>,
        channel_id: String,
        message_id: String,
    },
    MessageDeleteBulk {
        sequence: i64,
        guild_id: Option<String>,
        channel_id: String,
        message_ids: Vec<String>,
    },
    MessageReaction {
        sequence: i64,
        added: bool,
        guild_id: String,
        channel_id: String,
        message_id: String,
        user_id: String,
        emoji: String,
    },
    MessageUpdate {
        sequence: i64,
        patch: MessageSnapshotPatch,
    },
    ChannelUpsert {
        sequence: i64,
        event_type: i32,
        event_name: String,
        channel: ChannelSnapshot,
    },
    ChannelUpdate {
        sequence: i64,
        event_type: i32,
        event_name: String,
        patch: ChannelSnapshotPatch,
    },
    ChannelDelete {
        sequence: i64,
        event_type: i32,
        event_name: String,
        guild_id: Option<String>,
        channel_id: String,
    },
    ChannelSync {
        sequence: i64,
        guild_id: Option<String>,
        channels: Vec<ChannelSnapshot>,
        memberships: Vec<ThreadMembershipChange>,
    },
    ThreadMemberUpdate {
        sequence: i64,
        guild_id: Option<String>,
        change: ThreadMembershipChange,
    },
    ThreadMembersUpdate {
        sequence: i64,
        guild_id: Option<String>,
        changes: Vec<ThreadMembershipChange>,
    },
    RoleUpsert {
        sequence: i64,
        event_type: i32,
        event_name: String,
        role: RoleSnapshot,
    },
    RoleDelete {
        sequence: i64,
        guild_id: String,
        role_id: String,
    },
    MemberUpsert {
        sequence: i64,
        event_type: i32,
        event_name: String,
        member: MemberSnapshot,
    },
    MemberUpdate {
        sequence: i64,
        patch: MemberSnapshotPatch,
    },
    MembersChunk {
        sequence: i64,
        chunk: GuildMembersChunk,
    },
    MemberDelete {
        sequence: i64,
        guild_id: String,
        user_id: String,
    },
    GuildCreate {
        sequence: i64,
        guild: GuildSnapshot,
        channels: Vec<ChannelSnapshot>,
        roles: Option<GuildRolesSnapshot>,
        members: Vec<MemberSnapshot>,
    },
    GuildUpdate {
        sequence: i64,
        guild: GuildSnapshot,
    },
    GuildDelete {
        sequence: i64,
        guild_id: String,
        unavailable: bool,
    },
    GatewayRateLimited {
        opcode: i32,
        retry_ms: u64,
    },
    RateLimited {
        sequence: Option<i64>,
        limit: GatewayRateLimit,
    },
    Unknown {
        sequence: Option<i64>,
        name: String,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct ThreadMembershipChange {
    pub thread_id: String,
    pub user_id: Option<String>,
    pub joined: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayNormalizeError {
    message: String,
}

impl GatewayNormalizeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GatewayNormalizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GatewayNormalizeError {}

fn validate_gateway_url(url: &str) -> Result<(), &'static str> {
    if url.is_empty() {
        return Err("gateway url must not be empty");
    }
    if url.len() > 2048 {
        return Err("gateway url too long");
    }
    if url.trim() != url
        || url
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err("gateway url contains unsafe characters");
    }
    let rest = if let Some(rest) = url.strip_prefix("wss://") {
        rest
    } else if let Some(rest) = url.strip_prefix("ws://") {
        rest
    } else {
        return Err("gateway url scheme must be ws or wss");
    };
    if rest.contains('#') {
        return Err("gateway url fragment not allowed");
    }
    let authority_len = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_len];
    if authority.is_empty() {
        return Err("gateway url host missing");
    }
    let _ = parse_gateway_authority(authority, None)?;
    Ok(())
}

fn validate_session_id(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.len() > 512 {
        return Err("session_id invalid");
    }
    if value
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err("session_id invalid");
    }
    Ok(())
}

fn parse_gateway_authority(
    authority: &str,
    default_port: Option<u16>,
) -> Result<(String, Option<u16>), &'static str> {
    if authority.contains('@') {
        return Err("gateway url credentials not allowed");
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after_host)) = rest.split_once(']') else {
            return Err("gateway url host invalid");
        };
        if host.is_empty() || host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("gateway url host invalid");
        }
        let port = if after_host.is_empty() {
            default_port
        } else if let Some(port_text) = after_host.strip_prefix(':') {
            Some(parse_gateway_port(port_text)?)
        } else {
            return Err("gateway url host invalid");
        };
        return Ok((host.to_string(), port));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(parse_gateway_port(port)?)),
        None => (authority, default_port),
    };
    if host.is_empty() || host.len() > 253 {
        return Err("gateway url host invalid");
    }
    if !host
        .bytes()
        .all(|byte| byte == b'.' || byte == b'-' || byte.is_ascii_alphanumeric())
    {
        return Err("gateway url host invalid");
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 || label.starts_with('-') || label.ends_with('-') {
            return Err("gateway url host invalid");
        }
    }
    Ok((host.to_string(), port))
}

fn parse_gateway_port(value: &str) -> Result<u16, &'static str> {
    if value.is_empty() {
        return Err("gateway url port invalid");
    }
    let port = value
        .parse::<u16>()
        .map_err(|_| "gateway url port invalid")?;
    if port == 0 {
        return Err("gateway url port invalid");
    }
    Ok(port)
}

pub fn parse_gateway_frame(
    input: &str,
    now_ms: u64,
) -> Result<GatewayFrame, GatewayNormalizeError> {
    let root = parse_json(input).map_err(|error| GatewayNormalizeError::new(error.to_string()))?;
    parse_gateway_frame_root(&root, now_ms, false)
}

pub fn parse_gateway_frame_tolerant(
    input: &str,
    now_ms: u64,
) -> Result<GatewayFrame, GatewayNormalizeError> {
    let root = parse_json(input).map_err(|error| GatewayNormalizeError::new(error.to_string()))?;
    parse_gateway_frame_root(&root, now_ms, true)
}

struct GatewayMessageOrigin {
    sequence: i64,
    target: WebhookMessageTarget,
    origin: Option<MessageOrigin>,
    revision: Option<MessageRevision>,
    changes_content: bool,
    full: bool,
}

struct ParsedGatewayFrame {
    frame: GatewayFrame,
    message_origin: Option<GatewayMessageOrigin>,
    channel_context: Option<threads::ChannelContext>,
    guild_context: Option<guild::GuildContext>,
}

fn parse_gateway_frame_for_ingest(
    input: &str,
    now_ms: u64,
    tolerant: bool,
    bot_user_id: Option<&str>,
) -> Result<ParsedGatewayFrame, GatewayNormalizeError> {
    let root = parse_json(input).map_err(|error| GatewayNormalizeError::new(error.to_string()))?;
    let frame = parse_gateway_frame_root(&root, now_ms, tolerant)?;
    let fields = match &frame {
        GatewayFrame::Dispatch(GatewayDispatch::MessageCreate { sequence, message }) => {
            Some((*sequence, &message.channel_id, &message.message_id, true))
        }
        GatewayFrame::Dispatch(GatewayDispatch::MessageUpdate { sequence, patch }) => {
            Some((*sequence, &patch.channel_id, &patch.message_id, false))
        }
        _ => None,
    };
    let message_origin = fields.and_then(|(sequence, channel_id, message_id, full)| {
        Some(GatewayMessageOrigin {
            sequence,
            target: WebhookMessageTarget {
                channel_id: channel_id.clone(),
                message_id: message_id.clone(),
            },
            origin: MessageOrigin::from_data(root.get("d")?, full),
            revision: MessageRevision::parse(root.get("d")?)?,
            changes_content: ["content", "attachments", "type"]
                .iter()
                .any(|key| root.get("d").and_then(|data| data.get(key)).is_some()),
            full,
        })
    });
    Ok(ParsedGatewayFrame {
        guild_context: guild::GuildContext::capture(&frame, root.get("d"), now_ms),
        channel_context: threads::ChannelContext::capture(&frame, root.get("d"), bot_user_id)?,
        frame,
        message_origin,
    })
}

fn parse_gateway_frame_root(
    root: &JsonValue,
    now_ms: u64,
    tolerant_dispatch: bool,
) -> Result<GatewayFrame, GatewayNormalizeError> {
    let opcode = required_i64(root, "op")?;
    match opcode {
        0 if tolerant_dispatch => parse_dispatch_tolerant(root, now_ms).map(GatewayFrame::Dispatch),
        0 => parse_dispatch(root, now_ms).map(GatewayFrame::Dispatch),
        1 => Ok(GatewayFrame::HeartbeatRequest),
        7 => Ok(GatewayFrame::Reconnect),
        9 => Ok(GatewayFrame::InvalidSession {
            resumable: root
                .get("d")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| GatewayNormalizeError::new("Invalid Session d must be boolean"))?,
        }),
        10 => Ok(GatewayFrame::Hello {
            heartbeat_interval_ms: root
                .get("d")
                .and_then(|d| d.get("heartbeat_interval"))
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| GatewayNormalizeError::new("Hello heartbeat_interval missing"))?,
        }),
        11 => Ok(GatewayFrame::HeartbeatAck),
        _ => Ok(GatewayFrame::Unknown { opcode }),
    }
}

pub fn apply_dispatch_to_snapshot(
    dispatch: &GatewayDispatch,
    snapshot: &mut SnapshotStore,
    now_ms: u64,
) {
    let authority = authority::AuthorityContext::capture_dispatch(dispatch, snapshot, now_ms);
    match dispatch {
        GatewayDispatch::Unknown { name, .. } if invalid_dispatch_gap_reason(name).is_some() => {
            snapshot.mark_gap()
        }
        GatewayDispatch::MessageCreate { message, .. } => snapshot.upsert_message(message.clone()),
        GatewayDispatch::MessageDelete {
            channel_id,
            message_id,
            ..
        } => snapshot.tombstone_message(channel_id, message_id, now_ms),
        GatewayDispatch::MessageDeleteBulk {
            channel_id,
            message_ids,
            ..
        } => snapshot.tombstone_bulk(channel_id, message_ids.iter().map(String::as_str), now_ms),
        GatewayDispatch::MessageUpdate { patch, .. } => {
            if !snapshot.patch_message_checked(patch.clone(), now_ms) {
                snapshot.mark_gap();
            }
        }
        GatewayDispatch::ChannelUpsert { channel, .. } => {
            snapshot.clear_private_thread_access_for_channel(&channel.channel_id, now_ms);
            snapshot.upsert_channel(channel.clone());
        }
        GatewayDispatch::ChannelUpdate { patch, .. } => {
            if patch.affects_thread_access() {
                snapshot.clear_private_thread_access_for_channel(&patch.channel_id, now_ms);
            }
            snapshot.patch_channel(patch.clone(), now_ms);
        }
        GatewayDispatch::ChannelDelete { channel_id, .. } => {
            snapshot.gateway_channel_removed(channel_id, now_ms)
        }
        GatewayDispatch::ChannelSync {
            guild_id,
            channels,
            memberships,
            ..
        } => {
            for channel in channels {
                snapshot.upsert_channel(channel.clone());
            }
            for change in memberships {
                apply_thread_membership_change(snapshot, None, guild_id.as_deref(), change, now_ms);
            }
        }
        GatewayDispatch::ThreadMemberUpdate {
            guild_id, change, ..
        } => {
            apply_thread_membership_change(snapshot, None, guild_id.as_deref(), change, now_ms);
        }
        GatewayDispatch::ThreadMembersUpdate {
            guild_id, changes, ..
        } => {
            for change in changes {
                apply_thread_membership_change(snapshot, None, guild_id.as_deref(), change, now_ms);
            }
        }
        GatewayDispatch::RoleUpsert { role, .. } => {
            snapshot.clear_private_thread_access_for_guild(&role.guild_id, now_ms);
            snapshot.upsert_guild_role(role.clone(), now_ms)
        }
        GatewayDispatch::RoleDelete {
            guild_id, role_id, ..
        } => {
            snapshot.clear_private_thread_access_for_guild(guild_id, now_ms);
            snapshot.gateway_role_removed(guild_id, role_id, now_ms);
        }
        GatewayDispatch::MemberUpsert { member, .. } => snapshot.upsert_member(member.clone()),
        GatewayDispatch::MembersChunk { chunk, .. } => {
            chunk.clone().apply(snapshot, None, now_ms);
        }
        GatewayDispatch::MemberUpdate { patch, .. } => snapshot.patch_member(patch.clone(), now_ms),
        GatewayDispatch::MemberDelete {
            guild_id, user_id, ..
        } => snapshot.gateway_member_removed(guild_id, user_id, now_ms),
        GatewayDispatch::GuildCreate {
            guild,
            channels,
            roles,
            members,
            ..
        } => {
            let guild_id = guild.guild_id.clone();
            snapshot.upsert_guild(guild.clone());
            if let Some(roles) = roles {
                snapshot.upsert_guild_roles(roles.clone());
            }
            snapshot.upsert_guild_channels(&guild_id, channels.iter().cloned(), now_ms);
            for member in members {
                snapshot.upsert_member(member.clone());
            }
        }
        GatewayDispatch::GuildUpdate { guild, .. } => snapshot.upsert_guild(guild.clone()),
        GatewayDispatch::GuildDelete {
            guild_id,
            unavailable,
            ..
        } => {
            snapshot.gateway_guild_removed(guild_id, *unavailable, now_ms);
        }
        _ => {}
    }
    authority.apply(snapshot, now_ms);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStartLimit {
    pub total: u32,
    pub remaining: u32,
    pub reset_after_ms: u64,
    pub max_concurrency: u32,
}

#[derive(Clone, Eq, PartialEq)]
pub struct GatewayBotInfo {
    pub url: String,
    pub session_start_limit: SessionStartLimit,
}

pub fn parse_gateway_bot_info(input: &str) -> Result<GatewayBotInfo, GatewayNormalizeError> {
    let root = parse_json(input).map_err(|error| GatewayNormalizeError::new(error.to_string()))?;
    let url = required_str(&root, "url")?;
    validate_gateway_url(url).map_err(GatewayNormalizeError::new)?;
    let limit = root
        .get("session_start_limit")
        .ok_or_else(|| GatewayNormalizeError::new("session_start_limit missing"))?;
    let max_concurrency = required_u32(limit, "max_concurrency")?;
    if max_concurrency == 0 {
        return Err(GatewayNormalizeError::new(
            "max_concurrency must be positive",
        ));
    }
    Ok(GatewayBotInfo {
        url: url.to_string(),
        session_start_limit: SessionStartLimit {
            total: required_u32(limit, "total")?,
            remaining: required_u32(limit, "remaining")?,
            reset_after_ms: required_u64(limit, "reset_after")?,
            max_concurrency,
        },
    })
}

#[derive(Clone, Debug)]
pub struct GatewayManager {
    payloads: GatewayPayloads,
    machine: GatewayStateMachine,
    daily_budget: DailyIdentifyBudget,
    identify_max_wait_ms: u64,
    gateway_bot: Option<GatewayBotInfo>,
    last_identify_sent_at_ms: Option<u64>,
}

impl GatewayManager {
    pub fn new(
        intents: u64,
        shard_id: u32,
        shard_count: u32,
        daily_warn: u32,
        daily_stop: u32,
        identify_max_wait_ms: u64,
        event_queue_capacity: usize,
    ) -> Result<Self, &'static str> {
        Ok(Self {
            payloads: GatewayPayloads::new(intents, shard_id, shard_count),
            machine: GatewayStateMachine::new(event_queue_capacity),
            daily_budget: DailyIdentifyBudget::new(daily_warn, daily_stop)?,
            identify_max_wait_ms,
            gateway_bot: None,
            last_identify_sent_at_ms: None,
        })
    }

    pub fn start(&mut self) -> GatewayAction {
        self.machine.start()
    }

    pub fn observe_gateway_bot_body(
        &mut self,
        body: &str,
    ) -> Result<GatewayAction, GatewayNormalizeError> {
        parse_gateway_bot_info(body).map(|info| self.observe_gateway_bot_info(info))
    }

    pub fn observe_gateway_bot_body_at(
        &mut self,
        body: &str,
        now_ms: u64,
    ) -> Result<GatewayAction, GatewayNormalizeError> {
        parse_gateway_bot_info(body).map(|info| self.observe_gateway_bot_info_at(info, now_ms))
    }

    pub fn observe_gateway_bot_info(&mut self, info: GatewayBotInfo) -> GatewayAction {
        let action = self.next_identify_action(&info.session_start_limit);
        self.gateway_bot = Some(info);
        action
    }

    pub fn observe_gateway_bot_info_at(
        &mut self,
        info: GatewayBotInfo,
        now_ms: u64,
    ) -> GatewayAction {
        let action = self.next_identify_action_at(&info.session_start_limit, now_ms);
        self.gateway_bot = Some(info);
        action
    }

    pub fn identify_bucket(&self) -> Option<u32> {
        self.gateway_bot
            .as_ref()
            .map(|info| self.payloads.shard_id % info.session_start_limit.max_concurrency.max(1))
    }

    pub fn identify_payload(&mut self, token: &str) -> Result<String, &'static str> {
        let Some(info) = &self.gateway_bot else {
            return Err("GATEWAY_BOT_INFO_MISSING");
        };
        if info.session_start_limit.remaining == 0 {
            return Err("IDENTIFY_LIMIT_EXHAUSTED");
        }
        Ok(self.payloads.identify(token))
    }

    pub fn record_identify_sent_at(&mut self, now_ms: u64) -> Result<(), &'static str> {
        let Some(info) = &mut self.gateway_bot else {
            return Err("GATEWAY_BOT_INFO_MISSING");
        };
        if info.session_start_limit.remaining == 0 {
            return Err("IDENTIFY_LIMIT_EXHAUSTED");
        }
        self.daily_budget.record_identify_sent_at(now_ms)?;
        info.session_start_limit.remaining -= 1;
        self.last_identify_sent_at_ms = Some(now_ms);
        Ok(())
    }

    pub fn identify_limiter_backoff_at(&mut self, now_ms: u64) -> GatewayAction {
        let wait_ms = self
            .identify_bucket_wait_ms(now_ms)
            .unwrap_or(IDENTIFY_BUCKET_WINDOW_MS);
        self.machine
            .backoff_refetch_gateway_bot("IDENTIFY_BUCKET_SERIALIZED", wait_ms)
    }

    pub fn resume_payload(&self, token: &str, session_id: &str, seq: i64) -> String {
        self.payloads.resume(token, session_id, seq)
    }

    pub fn session_id(&self) -> Option<&str> {
        self.machine.session_id()
    }

    pub fn resume_gateway_url(&self) -> Option<&str> {
        self.machine.resume_gateway_url()
    }

    pub fn gateway_bot_info(&self) -> Option<&GatewayBotInfo> {
        self.gateway_bot.as_ref()
    }

    pub fn daily_budget(&self) -> &DailyIdentifyBudget {
        &self.daily_budget
    }

    pub fn machine(&self) -> &GatewayStateMachine {
        &self.machine
    }

    pub fn machine_mut(&mut self) -> &mut GatewayStateMachine {
        &mut self.machine
    }

    pub fn backoff_elapsed(&mut self) -> GatewayAction {
        self.machine.backoff_elapsed()
    }

    fn next_identify_action(&mut self, limit: &SessionStartLimit) -> GatewayAction {
        if !self.daily_budget.can_identify() {
            self.machine.state = GatewayState::Fatal;
            return GatewayAction::Fatal("DAILY_IDENTIFY_BUDGET_EXHAUSTED");
        }
        self.next_identify_action_after_daily_check(limit)
    }

    fn next_identify_action_at(&mut self, limit: &SessionStartLimit, now_ms: u64) -> GatewayAction {
        if !self.daily_budget.can_identify_at(now_ms) {
            self.machine.state = GatewayState::Fatal;
            return GatewayAction::Fatal("DAILY_IDENTIFY_BUDGET_EXHAUSTED");
        }
        self.next_identify_action_after_daily_check_at(limit, now_ms)
    }

    fn next_identify_action_after_daily_check(
        &mut self,
        limit: &SessionStartLimit,
    ) -> GatewayAction {
        if limit.remaining > 0 {
            return self.machine.gateway_url_fetched();
        }
        if limit.reset_after_ms <= self.identify_max_wait_ms {
            return self
                .machine
                .backoff_refetch_gateway_bot("IDENTIFY_LIMIT", limit.reset_after_ms);
        }
        self.machine.state = GatewayState::Fatal;
        GatewayAction::Fatal("IDENTIFY_LIMIT_EXHAUSTED")
    }

    fn next_identify_action_after_daily_check_at(
        &mut self,
        limit: &SessionStartLimit,
        now_ms: u64,
    ) -> GatewayAction {
        if limit.remaining > 0 {
            if let Some(wait_ms) = self.identify_bucket_wait_ms(now_ms) {
                return self
                    .machine
                    .backoff_refetch_gateway_bot("IDENTIFY_BUCKET_SERIALIZED", wait_ms);
            }
            return self.machine.gateway_url_fetched();
        }
        if limit.reset_after_ms <= self.identify_max_wait_ms {
            return self
                .machine
                .backoff_refetch_gateway_bot("IDENTIFY_LIMIT", limit.reset_after_ms);
        }
        self.machine.state = GatewayState::Fatal;
        GatewayAction::Fatal("IDENTIFY_LIMIT_EXHAUSTED")
    }

    fn identify_bucket_wait_ms(&self, now_ms: u64) -> Option<u64> {
        let last_sent = self.last_identify_sent_at_ms?;
        let elapsed = now_ms.saturating_sub(last_sent);
        if elapsed >= IDENTIFY_BUCKET_WINDOW_MS {
            None
        } else {
            Some(IDENTIFY_BUCKET_WINDOW_MS - elapsed)
        }
    }
}

impl Default for GatewayManager {
    fn default() -> Self {
        Self {
            payloads: GatewayPayloads::new(513, 0, 1),
            machine: GatewayStateMachine::new(1024),
            daily_budget: DailyIdentifyBudget::default(),
            identify_max_wait_ms: 60_000,
            gateway_bot: None,
            last_identify_sent_at_ms: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayTransportError {
    ConnectFailed,
    SendFailed,
    ReceiveFailed,
    NoFrame,
    Closed,
}

impl GatewayTransportError {
    pub fn why(&self) -> &'static str {
        match self {
            Self::ConnectFailed => "GATEWAY_CONNECT_FAILED",
            Self::SendFailed => "GATEWAY_SEND_FAILED",
            Self::ReceiveFailed => "GATEWAY_RECEIVE_FAILED",
            Self::NoFrame => "GATEWAY_NO_FRAME",
            Self::Closed => "GATEWAY_CLOSED",
        }
    }
}

pub trait GatewayTransport {
    fn connect(&mut self, url: &str) -> Result<(), GatewayTransportError>;
    fn send_text(&mut self, payload: &str) -> Result<(), GatewayTransportError>;
    fn receive_text(&mut self) -> Result<Option<String>, GatewayTransportError>;

    fn disconnect_for_resume(&mut self) -> Result<(), GatewayTransportError> {
        Ok(())
    }

    fn close_gracefully(&mut self) -> Result<(), GatewayTransportError> {
        Ok(())
    }

    fn last_close_code(&self) -> Option<u16> {
        None
    }
}

type TungsteniteSocket = WebSocket<MaybeTlsStream<NetworkStream>>;

#[derive(Clone, Default)]
pub struct MockGatewayTransport {
    connected_to: Option<String>,
    sent_texts: Vec<String>,
    incoming: VecDeque<Result<Option<String>, GatewayTransportError>>,
    connect_error: Option<GatewayTransportError>,
    send_error: Option<GatewayTransportError>,
    last_close_code: Option<u16>,
    graceful_close_codes: Vec<u16>,
    resume_disconnects: usize,
}

impl MockGatewayTransport {
    pub fn push_incoming(&mut self, payload: impl Into<String>) {
        self.incoming.push_back(Ok(Some(payload.into())));
    }

    pub fn push_close(&mut self) {
        self.incoming.push_back(Ok(None));
    }

    pub fn push_close_code(&mut self, code: u16) {
        self.last_close_code = Some(code);
        self.incoming.push_back(Ok(None));
    }

    pub fn push_error(&mut self, error: GatewayTransportError) {
        self.incoming.push_back(Err(error));
    }

    pub fn set_connect_error(&mut self, error: GatewayTransportError) {
        self.connect_error = Some(error);
    }

    pub fn set_send_error(&mut self, error: GatewayTransportError) {
        self.send_error = Some(error);
    }

    pub fn connected_to(&self) -> Option<&str> {
        self.connected_to.as_deref()
    }

    pub fn sent_texts(&self) -> &[String] {
        &self.sent_texts
    }

    pub fn graceful_close_codes(&self) -> &[u16] {
        &self.graceful_close_codes
    }

    pub fn resume_disconnects(&self) -> usize {
        self.resume_disconnects
    }
}

impl GatewayTransport for MockGatewayTransport {
    fn connect(&mut self, url: &str) -> Result<(), GatewayTransportError> {
        if let Some(error) = self.connect_error.clone() {
            return Err(error);
        }
        self.last_close_code = None;
        self.connected_to = Some(url.to_string());
        Ok(())
    }

    fn send_text(&mut self, payload: &str) -> Result<(), GatewayTransportError> {
        if let Some(error) = self.send_error.clone() {
            return Err(error);
        }
        self.sent_texts.push(payload.to_string());
        Ok(())
    }

    fn receive_text(&mut self) -> Result<Option<String>, GatewayTransportError> {
        self.incoming.pop_front().unwrap_or(Ok(None))
    }

    fn disconnect_for_resume(&mut self) -> Result<(), GatewayTransportError> {
        self.resume_disconnects += 1;
        self.last_close_code = None;
        self.connected_to = None;
        Ok(())
    }

    fn close_gracefully(&mut self) -> Result<(), GatewayTransportError> {
        self.graceful_close_codes.push(1000);
        self.last_close_code = Some(1000);
        Ok(())
    }

    fn last_close_code(&self) -> Option<u16> {
        self.last_close_code
    }
}

pub struct TungsteniteGatewayTransport {
    socket: Option<TungsteniteSocket>,
    timeout_ms: u64,
    last_close_code: Option<u16>,
}

impl TungsteniteGatewayTransport {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            socket: None,
            timeout_ms: timeout_ms.max(1),
            last_close_code: None,
        }
    }

    pub fn set_timeout_ms(&mut self, timeout_ms: u64) {
        self.timeout_ms = timeout_ms.max(1);
        if let Some(socket) = &mut self.socket {
            set_tungstenite_timeouts(socket, self.timeout_ms);
        }
    }
}

impl GatewayTransport for TungsteniteGatewayTransport {
    fn connect(&mut self, url: &str) -> Result<(), GatewayTransportError> {
        validate_gateway_url(url).map_err(|_| GatewayTransportError::ConnectFailed)?;
        let secure = url.starts_with("wss://");
        let rest = &url[if secure { 6 } else { 5 }..];
        let authority = rest.split(['/', '?']).next().unwrap_or("");
        let (host, port) = parse_gateway_authority(authority, Some(if secure { 443 } else { 80 }))
            .map_err(|_| GatewayTransportError::ConnectFailed)?;
        if secure {
            let host_lower = host.to_ascii_lowercase();
            if !host_lower.ends_with(".discord.gg") || port != Some(443) {
                return Err(GatewayTransportError::ConnectFailed);
            }
        } else if !loopback_gateway_host(&host) {
            return Err(GatewayTransportError::ConnectFailed);
        }
        let stream = NetworkStream::connect(
            &host,
            port.unwrap_or(443),
            Duration::from_millis(self.timeout_ms),
            !secure,
        )
        .map_err(|_| GatewayTransportError::ConnectFailed)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(crate::json::MAX_JSON_BYTES))
            .max_frame_size(Some(crate::json::MAX_JSON_BYTES))
            .max_write_buffer_size(256 * 1024);
        let (socket, _) = client_tls_with_config(url, stream, Some(config), None)
            .map_err(|_| GatewayTransportError::ConnectFailed)?;
        self.last_close_code = None;
        self.socket = Some(socket);
        Ok(())
    }

    fn send_text(&mut self, payload: &str) -> Result<(), GatewayTransportError> {
        let Some(socket) = &mut self.socket else {
            return Err(GatewayTransportError::Closed);
        };
        if payload.len() > MAX_GATEWAY_PAYLOAD_BYTES {
            return Err(GatewayTransportError::SendFailed);
        }
        set_tungstenite_timeouts(socket, self.timeout_ms);
        socket
            .send(Message::Text(payload.to_string().into()))
            .map_err(|_| GatewayTransportError::SendFailed)
    }

    fn receive_text(&mut self) -> Result<Option<String>, GatewayTransportError> {
        let Some(socket) = &mut self.socket else {
            return Err(GatewayTransportError::Closed);
        };
        set_tungstenite_timeouts(socket, self.timeout_ms);
        for _ in 0..8 {
            let message = socket.read().map_err(tungstenite_receive_error)?;
            match message {
                Message::Text(text) => return Ok(Some(text.to_string())),
                Message::Binary(bytes) => {
                    let text = String::from_utf8(bytes.to_vec())
                        .map_err(|_| GatewayTransportError::ReceiveFailed)?;
                    return Ok(Some(text));
                }
                Message::Close(frame) => {
                    self.last_close_code = frame.map(|frame| websocket_close_code(frame.code));
                    return Ok(None);
                }
                Message::Ping(payload) => socket
                    .send(Message::Pong(payload))
                    .map_err(|_| GatewayTransportError::SendFailed)?,
                Message::Pong(_) => {}
                Message::Frame(_) => {}
            }
        }
        Err(GatewayTransportError::ReceiveFailed)
    }

    fn disconnect_for_resume(&mut self) -> Result<(), GatewayTransportError> {
        self.socket = None;
        self.last_close_code = None;
        Ok(())
    }

    fn close_gracefully(&mut self) -> Result<(), GatewayTransportError> {
        let Some(socket) = &mut self.socket else {
            return Ok(());
        };
        set_tungstenite_timeouts(socket, self.timeout_ms);
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        };
        socket
            .close(Some(frame))
            .map_err(|_| GatewayTransportError::SendFailed)?;
        socket
            .flush()
            .map_err(|_| GatewayTransportError::SendFailed)?;
        self.last_close_code = Some(1000);
        self.socket = None;
        Ok(())
    }

    fn last_close_code(&self) -> Option<u16> {
        self.last_close_code
    }
}

fn websocket_close_code(code: tungstenite::protocol::frame::coding::CloseCode) -> u16 {
    use tungstenite::protocol::frame::coding::CloseCode;
    match code {
        CloseCode::Normal => 1000,
        CloseCode::Away => 1001,
        CloseCode::Protocol => 1002,
        CloseCode::Unsupported => 1003,
        CloseCode::Status => 1005,
        CloseCode::Abnormal => 1006,
        CloseCode::Invalid => 1007,
        CloseCode::Policy => 1008,
        CloseCode::Size => 1009,
        CloseCode::Extension => 1010,
        CloseCode::Error => 1011,
        CloseCode::Restart => 1012,
        CloseCode::Again => 1013,
        CloseCode::Tls => 1015,
        CloseCode::Reserved(value) | CloseCode::Iana(value) | CloseCode::Library(value) => value,
        CloseCode::Bad(value) => value,
    }
}

fn set_tungstenite_timeouts(socket: &mut TungsteniteSocket, timeout_ms: u64) {
    let timeout = Duration::from_millis(timeout_ms.max(1));
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => {
            stream.renew_deadline(timeout);
        }
        MaybeTlsStream::NativeTls(stream) => {
            let tcp = stream.get_mut();
            tcp.renew_deadline(timeout);
        }
        #[allow(unreachable_patterns)]
        _ => {}
    }
}

fn tungstenite_receive_error(error: tungstenite::Error) -> GatewayTransportError {
    match error {
        tungstenite::Error::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            GatewayTransportError::NoFrame
        }
        _ => GatewayTransportError::ReceiveFailed,
    }
}

pub struct WsGatewayTransport {
    stream: Option<NetworkStream>,
    timeout_ms: u64,
    nonce_seed: u64,
    last_close_code: Option<u16>,
}

impl WsGatewayTransport {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            stream: None,
            timeout_ms: timeout_ms.max(1),
            nonce_seed: 0,
            last_close_code: None,
        }
    }
}

impl GatewayTransport for WsGatewayTransport {
    fn connect(&mut self, url: &str) -> Result<(), GatewayTransportError> {
        let endpoint = parse_ws_url(url)?;
        if !loopback_gateway_host(&endpoint.host) {
            return Err(GatewayTransportError::ConnectFailed);
        }
        let mut stream = NetworkStream::connect(
            &endpoint.host,
            endpoint.port,
            Duration::from_millis(self.timeout_ms),
            true,
        )
        .map_err(|_| GatewayTransportError::ConnectFailed)?;
        let key = next_websocket_key(&mut self.nonce_seed);
        let request = websocket_handshake_request(&endpoint, &key);
        stream
            .write_all(request.as_bytes())
            .map_err(|_| GatewayTransportError::ConnectFailed)?;
        stream
            .flush()
            .map_err(|_| GatewayTransportError::ConnectFailed)?;
        let response =
            read_http_head(&mut stream).map_err(|_| GatewayTransportError::ConnectFailed)?;
        if !websocket_handshake_ok(&response, &key) {
            return Err(GatewayTransportError::ConnectFailed);
        }
        self.last_close_code = None;
        self.stream = Some(stream);
        Ok(())
    }

    fn send_text(&mut self, payload: &str) -> Result<(), GatewayTransportError> {
        let Some(stream) = &mut self.stream else {
            return Err(GatewayTransportError::Closed);
        };
        stream.renew_deadline(Duration::from_millis(self.timeout_ms));
        if payload.len() > MAX_GATEWAY_PAYLOAD_BYTES {
            return Err(GatewayTransportError::SendFailed);
        }
        let frame =
            websocket_client_text_frame(payload.as_bytes(), next_mask(&mut self.nonce_seed))
                .ok_or(GatewayTransportError::SendFailed)?;
        stream
            .write_all(&frame)
            .map_err(|_| GatewayTransportError::SendFailed)?;
        stream
            .flush()
            .map_err(|_| GatewayTransportError::SendFailed)
    }

    fn receive_text(&mut self) -> Result<Option<String>, GatewayTransportError> {
        let Some(stream) = &mut self.stream else {
            return Err(GatewayTransportError::Closed);
        };
        stream.renew_deadline(Duration::from_millis(self.timeout_ms));
        for _ in 0..8 {
            let frame = read_websocket_frame(stream)?;
            match frame.opcode {
                0x1 => {
                    let text = String::from_utf8(frame.payload)
                        .map_err(|_| GatewayTransportError::ReceiveFailed)?;
                    return Ok(Some(text));
                }
                0x8 => {
                    self.last_close_code = websocket_close_code_from_payload(&frame.payload);
                    return Ok(None);
                }
                0x9 => {
                    let pong = websocket_control_frame(0xA, &frame.payload)
                        .ok_or(GatewayTransportError::SendFailed)?;
                    stream
                        .write_all(&pong)
                        .map_err(|_| GatewayTransportError::SendFailed)?;
                    stream
                        .flush()
                        .map_err(|_| GatewayTransportError::SendFailed)?;
                }
                0xA => {}
                _ => return Err(GatewayTransportError::ReceiveFailed),
            }
        }
        Err(GatewayTransportError::ReceiveFailed)
    }

    fn disconnect_for_resume(&mut self) -> Result<(), GatewayTransportError> {
        self.stream = None;
        self.last_close_code = None;
        Ok(())
    }

    fn close_gracefully(&mut self) -> Result<(), GatewayTransportError> {
        let Some(stream) = &mut self.stream else {
            return Ok(());
        };
        stream.renew_deadline(Duration::from_millis(self.timeout_ms));
        let payload = 1000_u16.to_be_bytes();
        let frame = websocket_masked_frame(0x8, &payload, next_mask(&mut self.nonce_seed))
            .ok_or(GatewayTransportError::SendFailed)?;
        stream
            .write_all(&frame)
            .map_err(|_| GatewayTransportError::SendFailed)?;
        stream
            .flush()
            .map_err(|_| GatewayTransportError::SendFailed)?;
        self.last_close_code = Some(1000);
        self.stream = None;
        Ok(())
    }

    fn last_close_code(&self) -> Option<u16> {
        self.last_close_code
    }
}

fn websocket_close_code_from_payload(payload: &[u8]) -> Option<u16> {
    if payload.len() < 2 {
        return None;
    }
    Some(u16::from_be_bytes([payload[0], payload[1]]))
}

#[derive(Clone, Eq, PartialEq)]
struct WsEndpoint {
    host: String,
    port: u16,
    path: String,
}

#[derive(Clone, Eq, PartialEq)]
struct WsFrame {
    opcode: u8,
    payload: Vec<u8>,
}

fn parse_ws_url(url: &str) -> Result<WsEndpoint, GatewayTransportError> {
    validate_gateway_url(url).map_err(|_| GatewayTransportError::ConnectFailed)?;
    let rest = url
        .strip_prefix("ws://")
        .ok_or(GatewayTransportError::ConnectFailed)?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(index) => {
            let path = &rest[index..];
            let path = if path.starts_with('?') {
                format!("/{path}")
            } else {
                path.to_string()
            };
            (&rest[..index], path)
        }
        None => (rest, "/".to_string()),
    };
    let (host, port) = parse_gateway_authority(authority, Some(80))
        .map_err(|_| GatewayTransportError::ConnectFailed)?;
    let port = port.ok_or(GatewayTransportError::ConnectFailed)?;
    Ok(WsEndpoint { host, port, path })
}

fn websocket_handshake_request(endpoint: &WsEndpoint, key: &str) -> String {
    let host = if endpoint.host.contains(':') {
        format!("[{}]", endpoint.host)
    } else {
        endpoint.host.clone()
    };
    format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
        endpoint.path, host, endpoint.port, key
    )
}

fn loopback_gateway_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn websocket_handshake_ok(response: &str, key: &str) -> bool {
    let mut lines = response.lines();
    let Some(status) = lines.next() else {
        return false;
    };
    if !status.starts_with("HTTP/1.1 101 ") {
        return false;
    }
    let mut saw_upgrade = false;
    let mut saw_connection = false;
    let mut saw_accept = false;
    let expected_accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.eq_ignore_ascii_case("Sec-WebSocket-Accept") {
            if saw_accept || value.trim() != expected_accept {
                return false;
            }
            saw_accept = true;
        }
        if key.trim().eq_ignore_ascii_case("Upgrade")
            && value.trim().eq_ignore_ascii_case("websocket")
        {
            saw_upgrade = true;
        }
        if key.trim().eq_ignore_ascii_case("Connection")
            && value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        {
            saw_connection = true;
        }
    }
    saw_upgrade && saw_connection && saw_accept
}

fn read_http_head(stream: &mut impl Read) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while bytes.len() < 16 * 1024 {
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            return Ok(String::from_utf8_lossy(&bytes).to_string());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "http head too large",
    ))
}

fn websocket_client_text_frame(payload: &[u8], mask: [u8; 4]) -> Option<Vec<u8>> {
    websocket_masked_frame(0x1, payload, mask)
}

fn websocket_masked_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Option<Vec<u8>> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | (opcode & 0x0F));
    if payload.len() <= 125 {
        frame.push(0x80 | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        return None;
    }
    frame.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }
    Some(frame)
}

fn websocket_control_frame(opcode: u8, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > 125 {
        return None;
    }
    let mut frame = Vec::with_capacity(payload.len() + 2);
    frame.push(0x80 | (opcode & 0x0F));
    frame.push(payload.len() as u8);
    frame.extend_from_slice(payload);
    Some(frame)
}

fn read_websocket_frame(stream: &mut impl Read) -> Result<WsFrame, GatewayTransportError> {
    let mut head = [0_u8; 2];
    stream
        .read_exact(&mut head)
        .map_err(|_| GatewayTransportError::ReceiveFailed)?;
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0F;
    if !fin || head[0] & 0x70 != 0 {
        return Err(GatewayTransportError::ReceiveFailed);
    }
    let masked = head[1] & 0x80 != 0;
    let mut len = u64::from(head[1] & 0x7F);
    if len == 126 {
        let mut bytes = [0_u8; 2];
        stream
            .read_exact(&mut bytes)
            .map_err(|_| GatewayTransportError::ReceiveFailed)?;
        len = u64::from(u16::from_be_bytes(bytes));
    } else if len == 127 {
        let mut bytes = [0_u8; 8];
        stream
            .read_exact(&mut bytes)
            .map_err(|_| GatewayTransportError::ReceiveFailed)?;
        len = u64::from_be_bytes(bytes);
    }
    if len > 1024 * 1024 {
        return Err(GatewayTransportError::ReceiveFailed);
    }
    if opcode >= 0x8 && len > 125 {
        return Err(GatewayTransportError::ReceiveFailed);
    }
    let mask = if masked {
        let mut mask = [0_u8; 4];
        stream
            .read_exact(&mut mask)
            .map_err(|_| GatewayTransportError::ReceiveFailed)?;
        Some(mask)
    } else {
        None
    };
    let mut payload = vec![0_u8; len as usize];
    stream
        .read_exact(&mut payload)
        .map_err(|_| GatewayTransportError::ReceiveFailed)?;
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok(WsFrame { opcode, payload })
}

fn next_websocket_key(seed: &mut u64) -> String {
    *seed = seed.wrapping_add(1);
    tungstenite::handshake::client::generate_key()
}

fn next_mask(seed: &mut u64) -> [u8; 4] {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let key = next_websocket_key(seed);
    let mut sextets = [0u8; 6];
    for (target, byte) in sextets.iter_mut().zip(key.bytes()) {
        *target = TABLE
            .iter()
            .position(|candidate| *candidate == byte)
            .unwrap_or(0) as u8;
    }
    [
        sextets[0] << 2 | sextets[1] >> 4,
        sextets[1] << 4 | sextets[2] >> 2,
        sextets[2] << 6 | sextets[3],
        sextets[4] << 2 | sextets[5] >> 4,
    ]
}

#[derive(Clone)]
pub struct GatewayIngestor {
    machine: GatewayStateMachine,
    snapshot: SnapshotStore,
    heartbeat: Option<HeartbeatTracker>,
    snapshot_gap_epoch: u64,
    message_snapshot_channel_allowlist: HashSet<String>,
}

impl GatewayIngestor {
    pub fn new(
        event_queue_capacity: usize,
        message_ttl_ms: u64,
        tombstone_ttl_ms: u64,
        max_messages: usize,
        max_tombstones: usize,
    ) -> Self {
        Self::with_snapshot_settings(
            event_queue_capacity,
            SnapshotSettings::compact(
                message_ttl_ms,
                tombstone_ttl_ms,
                max_messages,
                max_tombstones,
            ),
        )
    }

    pub fn with_snapshot_settings(
        event_queue_capacity: usize,
        snapshot_settings: SnapshotSettings,
    ) -> Self {
        Self {
            machine: GatewayStateMachine::new(event_queue_capacity),
            snapshot: SnapshotStore::with_settings(snapshot_settings),
            heartbeat: None,
            snapshot_gap_epoch: 0,
            message_snapshot_channel_allowlist: HashSet::new(),
        }
    }

    pub fn set_message_snapshot_channel_allowlist<I>(&mut self, channel_ids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.message_snapshot_channel_allowlist = channel_ids.into_iter().collect();
    }

    pub fn ingest_json(
        &mut self,
        input: &str,
        now_ms: u64,
    ) -> Result<GatewayAction, GatewayNormalizeError> {
        self.ingest_parsed_frame(
            parse_gateway_frame_for_ingest(input, now_ms, false, self.machine.bot_user_id()),
            now_ms,
        )
    }

    pub fn ingest_json_tolerant(
        &mut self,
        input: &str,
        now_ms: u64,
    ) -> Result<GatewayAction, GatewayNormalizeError> {
        self.ingest_parsed_frame(
            parse_gateway_frame_for_ingest(input, now_ms, true, self.machine.bot_user_id()),
            now_ms,
        )
    }

    fn ingest_parsed_frame(
        &mut self,
        frame: Result<ParsedGatewayFrame, GatewayNormalizeError>,
        now_ms: u64,
    ) -> Result<GatewayAction, GatewayNormalizeError> {
        match frame {
            Ok(parsed) => {
                let before_sequence = self.machine.last_seen_seq();
                let before_gap = self.machine.gap_epoch();
                let compatible = parsed.message_origin.as_ref().is_none_or(|message| {
                    message.origin.as_ref().is_none_or(|origin| {
                        self.snapshot.message_origin_matches(
                            &message.target.channel_id,
                            &message.target.message_id,
                            origin,
                            message.full,
                        )
                    })
                });
                let policy =
                    parsed
                        .message_origin
                        .as_ref()
                        .map_or(MessageCachePolicy::Apply, |message| {
                            self.snapshot.message_cache_policy(
                                &message.target.channel_id,
                                &message.target.message_id,
                                message.revision.as_ref(),
                                message.changes_content,
                            )
                        });
                let action = self.ingest_frame_with_policy(parsed.frame, policy, now_ms);
                let accepted_sequence = self.machine.last_seen_seq().filter(|sequence| {
                    before_sequence.is_none_or(|before| *sequence > before)
                        && self.machine.gap_epoch() == before_gap
                });
                if let Some(context) = parsed
                    .guild_context
                    .filter(|context| accepted_sequence == Some(context.sequence))
                {
                    context.apply(&mut self.snapshot, now_ms);
                }
                if let Some(context) = parsed
                    .channel_context
                    .filter(|context| accepted_sequence == Some(context.sequence))
                {
                    context.apply(&mut self.snapshot, now_ms);
                }
                if let Some(message) = parsed
                    .message_origin
                    .filter(|message| accepted_sequence == Some(message.sequence))
                {
                    if !compatible
                        || message.origin.is_some_and(|origin| {
                            !self.snapshot.set_message_origin(
                                &message.target.channel_id,
                                &message.target.message_id,
                                origin,
                                message.full,
                            )
                        })
                    {
                        self.machine
                            .queue
                            .push_control(GatewayEvent::gap("MESSAGE_ORIGIN_MISMATCH"));
                        self.invalidate_snapshot_if_gap_advanced();
                    } else if policy == MessageCachePolicy::Apply {
                        self.snapshot.set_message_revision(
                            &message.target.channel_id,
                            &message.target.message_id,
                            message.revision,
                        );
                    }
                }
                Ok(action)
            }
            Err(error) => {
                self.machine
                    .queue
                    .push_control(GatewayEvent::gap("INVALID_GATEWAY_FRAME"));
                self.invalidate_snapshot_if_gap_advanced();
                Err(error)
            }
        }
    }

    pub fn ingest_frame(&mut self, frame: GatewayFrame, now_ms: u64) -> GatewayAction {
        self.ingest_frame_with_policy(frame, MessageCachePolicy::Apply, now_ms)
    }

    fn ingest_frame_with_policy(
        &mut self,
        frame: GatewayFrame,
        policy: MessageCachePolicy,
        now_ms: u64,
    ) -> GatewayAction {
        let before_sequence = self.machine.last_seen_seq();
        let before_gap = self.machine.gap_epoch();
        let authority = authority::AuthorityContext::capture(&frame, &mut self.snapshot, now_ms);
        let action = match frame {
            GatewayFrame::Hello {
                heartbeat_interval_ms,
            } => {
                let mut heartbeat = HeartbeatTracker::new(heartbeat_interval_ms);
                heartbeat.hello(now_ms);
                self.heartbeat = Some(heartbeat);
                let action = self.machine.handle_frame(GatewayFrame::Hello {
                    heartbeat_interval_ms,
                });
                self.invalidate_snapshot_if_gap_advanced();
                action
            }
            GatewayFrame::HeartbeatAck => {
                if let Some(heartbeat) = &mut self.heartbeat {
                    heartbeat.ack(now_ms);
                }
                let action = self.machine.handle_frame(GatewayFrame::HeartbeatAck);
                self.invalidate_snapshot_if_gap_advanced();
                action
            }
            GatewayFrame::Dispatch(dispatch) => {
                let action = self.ingest_dispatch(dispatch, policy, now_ms);
                self.invalidate_snapshot_if_gap_advanced();
                action
            }
            other => {
                let action = self.machine.handle_frame(other);
                self.invalidate_snapshot_if_gap_advanced();
                action
            }
        };
        if self.machine.last_seen_seq().is_some_and(|sequence| {
            before_sequence.is_none_or(|before| sequence > before)
                && self.machine.gap_epoch() == before_gap
        }) {
            authority.apply(&mut self.snapshot, now_ms);
        }
        action
    }

    pub fn pop_event(&mut self) -> Option<GatewayEvent> {
        self.machine.pop_event()
    }

    pub fn snapshot(&self) -> &SnapshotStore {
        &self.snapshot
    }

    pub fn snapshot_mut(&mut self) -> &mut SnapshotStore {
        &mut self.snapshot
    }

    pub fn machine(&self) -> &GatewayStateMachine {
        &self.machine
    }

    pub fn machine_mut(&mut self) -> &mut GatewayStateMachine {
        &mut self.machine
    }

    pub fn heartbeat(&self) -> Option<&HeartbeatTracker> {
        self.heartbeat.as_ref()
    }

    pub fn heartbeat_mut(&mut self) -> Option<&mut HeartbeatTracker> {
        self.heartbeat.as_mut()
    }

    pub fn sync_snapshot_gap(&mut self) -> bool {
        self.invalidate_snapshot_if_gap_advanced()
    }

    fn ingest_dispatch(
        &mut self,
        dispatch: GatewayDispatch,
        policy: MessageCachePolicy,
        now_ms: u64,
    ) -> GatewayAction {
        match dispatch {
            GatewayDispatch::Ready {
                sequence,
                session_id,
                resume_gateway_url,
                bot_user_id,
            } => {
                self.machine.ready_with_bot_user(
                    sequence,
                    session_id,
                    resume_gateway_url,
                    bot_user_id,
                );
                GatewayAction::None
            }
            GatewayDispatch::Resumed { sequence } => {
                self.machine.resumed_event(sequence);
                GatewayAction::None
            }
            GatewayDispatch::MessageCreate { sequence, message } => {
                let mut message = message;
                if policy != MessageCachePolicy::Apply {
                    message.member_permissions = None;
                }
                let outcome = self
                    .machine
                    .message_create_outcome(sequence, message.clone());
                let gap_advanced = self.invalidate_snapshot_if_gap_advanced();
                if outcome.accepted
                    && !gap_advanced
                    && self.should_store_message_snapshot(&message.channel_id)
                {
                    if !self.snapshot.message_identity_matches(
                        &message.channel_id,
                        &message.message_id,
                        Some(&message.guild_id),
                        Some(&message.user_id),
                    ) {
                        self.snapshot.invalidate_message(
                            &message.channel_id,
                            &message.message_id,
                            now_ms,
                        );
                        self.machine
                            .queue
                            .push_control(GatewayEvent::gap("MESSAGE_IDENTITY_CONFLICT"));
                    } else {
                        match policy {
                            MessageCachePolicy::Apply => self.snapshot.upsert_message(message),
                            MessageCachePolicy::Ignore => {}
                            MessageCachePolicy::Invalidate => self
                                .snapshot
                                .discard_message_content(&message.channel_id, &message.message_id),
                        }
                    }
                }
                outcome.action
            }
            GatewayDispatch::InteractionCreate {
                sequence,
                interaction,
            } => {
                let outcome = self
                    .machine
                    .interaction_create_outcome(sequence, interaction);
                self.invalidate_snapshot_if_gap_advanced();
                outcome.action
            }
            GatewayDispatch::MessageDelete {
                sequence,
                guild_id,
                channel_id,
                message_id,
            } => {
                let outcome = self.machine.message_delete_outcome(
                    sequence,
                    guild_id,
                    channel_id.clone(),
                    message_id.clone(),
                );
                let gap_advanced = self.invalidate_snapshot_if_gap_advanced();
                if outcome.accepted
                    && !gap_advanced
                    && self.should_store_message_snapshot(&channel_id)
                {
                    self.snapshot
                        .tombstone_message(&channel_id, &message_id, now_ms);
                }
                outcome.action
            }
            GatewayDispatch::MessageDeleteBulk {
                sequence,
                guild_id,
                channel_id,
                message_ids,
            } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type: D7EMB,
                    sequence,
                    description: "MESSAGE_DELETE_BULK".to_string(),
                    guild_id,
                    channel_id: Some(channel_id.clone()),
                    message_id: message_ids.first().cloned(),
                    user_id: None,
                    role_id: None,
                    text: Some(message_ids.join(",")),
                });
                if outcome.accepted && self.should_store_message_snapshot(&channel_id) {
                    self.snapshot.tombstone_bulk(
                        &channel_id,
                        message_ids.iter().map(String::as_str),
                        now_ms,
                    );
                }
                outcome.action
            }
            GatewayDispatch::MessageReaction {
                sequence,
                added,
                guild_id,
                channel_id,
                message_id,
                user_id,
                emoji,
            } => {
                let outcome = self.machine.reaction_outcome(ReactionEventParts {
                    sequence,
                    added,
                    guild_id,
                    channel_id,
                    message_id,
                    user_id,
                    emoji,
                });
                self.invalidate_snapshot_if_gap_advanced();
                outcome.action
            }
            GatewayDispatch::MessageUpdate { sequence, patch } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type: D7EMU,
                    sequence,
                    description: "MESSAGE_UPDATE".to_string(),
                    guild_id: patch.guild_id.clone(),
                    channel_id: Some(patch.channel_id.clone()),
                    message_id: Some(patch.message_id.clone()),
                    user_id: patch.user_id.clone(),
                    role_id: None,
                    text: patch.content.clone(),
                });
                if outcome.accepted && self.should_store_message_snapshot(&patch.channel_id) {
                    let compatible = self.snapshot.message_identity_matches(
                        &patch.channel_id,
                        &patch.message_id,
                        patch.guild_id.as_deref(),
                        patch.user_id.as_deref(),
                    );
                    if !compatible {
                        self.snapshot.invalidate_message(
                            &patch.channel_id,
                            &patch.message_id,
                            now_ms,
                        );
                    } else if policy == MessageCachePolicy::Invalidate {
                        self.snapshot
                            .discard_message_content(&patch.channel_id, &patch.message_id);
                    }
                    if !compatible
                        || policy == MessageCachePolicy::Apply
                            && !self.snapshot.patch_message_checked(patch, now_ms)
                    {
                        self.machine
                            .queue
                            .push_control(GatewayEvent::gap("MESSAGE_IDENTITY_CONFLICT"));
                    }
                }
                outcome.action
            }
            GatewayDispatch::ChannelUpsert {
                sequence,
                event_type,
                event_name,
                channel,
            } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type,
                    sequence,
                    description: event_name,
                    guild_id: Some(channel.guild_id.clone()),
                    channel_id: Some(channel.channel_id.clone()),
                    message_id: None,
                    user_id: None,
                    role_id: None,
                    text: None,
                });
                if outcome.accepted {
                    self.snapshot
                        .clear_private_thread_access_for_channel(&channel.channel_id, now_ms);
                    self.snapshot.upsert_channel(channel);
                }
                outcome.action
            }
            GatewayDispatch::ChannelUpdate {
                sequence,
                event_type,
                event_name,
                patch,
            } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type,
                    sequence,
                    description: event_name,
                    guild_id: patch.guild_id.clone(),
                    channel_id: Some(patch.channel_id.clone()),
                    message_id: None,
                    user_id: None,
                    role_id: None,
                    text: None,
                });
                if outcome.accepted {
                    if patch.affects_thread_access() {
                        self.snapshot
                            .clear_private_thread_access_for_channel(&patch.channel_id, now_ms);
                    }
                    self.snapshot.patch_channel(patch, now_ms);
                }
                outcome.action
            }
            GatewayDispatch::ChannelDelete {
                sequence,
                event_type,
                event_name,
                guild_id,
                channel_id,
            } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type,
                    sequence,
                    description: event_name,
                    guild_id,
                    channel_id: Some(channel_id.clone()),
                    message_id: None,
                    user_id: None,
                    role_id: None,
                    text: None,
                });
                if outcome.accepted {
                    self.snapshot.gateway_channel_removed(&channel_id, now_ms);
                }
                outcome.action
            }
            GatewayDispatch::ChannelSync {
                sequence,
                guild_id,
                channels,
                memberships,
            } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type: D7ETS,
                    sequence,
                    description: "THREAD_LIST_SYNC".to_string(),
                    guild_id: guild_id.clone(),
                    channel_id: channels
                        .first()
                        .map(|channel| channel.channel_id.clone())
                        .or_else(|| memberships.first().map(|change| change.thread_id.clone())),
                    message_id: None,
                    user_id: memberships
                        .first()
                        .and_then(|change| change.user_id.clone()),
                    role_id: None,
                    text: None,
                });
                if outcome.accepted {
                    let bot_user_id = self.machine.bot_user_id().map(ToOwned::to_owned);
                    for channel in channels {
                        self.snapshot.upsert_channel(channel);
                    }
                    for change in memberships {
                        apply_thread_membership_change(
                            &mut self.snapshot,
                            bot_user_id.as_deref(),
                            guild_id.as_deref(),
                            &change,
                            now_ms,
                        );
                    }
                }
                outcome.action
            }
            GatewayDispatch::ThreadMemberUpdate {
                sequence,
                guild_id,
                change,
            } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type: D7ETM,
                    sequence,
                    description: "THREAD_MEMBER_UPDATE".to_string(),
                    guild_id: guild_id.clone(),
                    channel_id: Some(change.thread_id.clone()),
                    message_id: None,
                    user_id: change.user_id.clone(),
                    role_id: None,
                    text: None,
                });
                if outcome.accepted {
                    let bot_user_id = self.machine.bot_user_id().map(ToOwned::to_owned);
                    apply_thread_membership_change(
                        &mut self.snapshot,
                        bot_user_id.as_deref(),
                        guild_id.as_deref(),
                        &change,
                        now_ms,
                    );
                }
                outcome.action
            }
            GatewayDispatch::ThreadMembersUpdate {
                sequence,
                guild_id,
                changes,
            } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type: D7ETM,
                    sequence,
                    description: "THREAD_MEMBERS_UPDATE".to_string(),
                    guild_id: guild_id.clone(),
                    channel_id: changes.first().map(|change| change.thread_id.clone()),
                    message_id: None,
                    user_id: changes.first().and_then(|change| change.user_id.clone()),
                    role_id: None,
                    text: None,
                });
                if outcome.accepted {
                    let bot_user_id = self.machine.bot_user_id().map(ToOwned::to_owned);
                    for change in changes {
                        apply_thread_membership_change(
                            &mut self.snapshot,
                            bot_user_id.as_deref(),
                            guild_id.as_deref(),
                            &change,
                            now_ms,
                        );
                    }
                }
                outcome.action
            }
            GatewayDispatch::RoleUpsert {
                sequence,
                event_type,
                event_name,
                role,
            } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type,
                    sequence,
                    description: event_name,
                    guild_id: Some(role.guild_id.clone()),
                    channel_id: None,
                    message_id: None,
                    user_id: None,
                    role_id: Some(role.role_id.clone()),
                    text: None,
                });
                if outcome.accepted {
                    self.snapshot
                        .clear_private_thread_access_for_guild(&role.guild_id, now_ms);
                    self.snapshot.upsert_guild_role(role, now_ms);
                }
                outcome.action
            }
            GatewayDispatch::RoleDelete {
                sequence,
                guild_id,
                role_id,
            } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type: D7ERD,
                    sequence,
                    description: "GUILD_ROLE_DELETE".to_string(),
                    guild_id: Some(guild_id.clone()),
                    channel_id: None,
                    message_id: None,
                    user_id: None,
                    role_id: Some(role_id.clone()),
                    text: None,
                });
                if outcome.accepted {
                    self.snapshot
                        .clear_private_thread_access_for_guild(&guild_id, now_ms);
                    self.snapshot
                        .gateway_role_removed(&guild_id, &role_id, now_ms);
                }
                outcome.action
            }
            GatewayDispatch::MemberUpsert {
                sequence,
                event_type,
                event_name,
                member,
            } => {
                let outcome = self.machine.member_outcome(
                    sequence,
                    event_type,
                    event_name,
                    &member.guild_id,
                    &member.user_id,
                );
                let gap_advanced = self.invalidate_snapshot_if_gap_advanced();
                if outcome.accepted && !gap_advanced {
                    if self
                        .machine
                        .bot_user_id()
                        .is_some_and(|bot_user_id| bot_user_id == member.user_id)
                    {
                        self.snapshot
                            .clear_private_thread_access_for_guild(&member.guild_id, now_ms);
                    }
                    self.snapshot.upsert_member(member);
                }
                outcome.action
            }
            GatewayDispatch::MembersChunk { sequence, chunk } => {
                let outcome = self.machine.member_chunk_outcome(sequence, &chunk);
                let gap_advanced = self.invalidate_snapshot_if_gap_advanced();
                if outcome.accepted && !gap_advanced {
                    chunk.apply(&mut self.snapshot, self.machine.bot_user_id(), now_ms);
                }
                outcome.action
            }
            GatewayDispatch::MemberUpdate { sequence, patch } => {
                let outcome = self.machine.member_outcome(
                    sequence,
                    D7MBU,
                    "GUILD_MEMBER_UPDATE",
                    &patch.guild_id,
                    &patch.user_id,
                );
                let gap_advanced = self.invalidate_snapshot_if_gap_advanced();
                if outcome.accepted && !gap_advanced {
                    if self
                        .machine
                        .bot_user_id()
                        .is_some_and(|id| id == patch.user_id)
                    {
                        self.snapshot
                            .clear_private_thread_access_for_guild(&patch.guild_id, now_ms);
                    }
                    self.snapshot.patch_member(patch, now_ms);
                }
                outcome.action
            }
            GatewayDispatch::MemberDelete {
                sequence,
                guild_id,
                user_id,
            } => {
                let outcome = self.machine.member_outcome(
                    sequence,
                    D7MBR,
                    "GUILD_MEMBER_REMOVE",
                    &guild_id,
                    &user_id,
                );
                let gap_advanced = self.invalidate_snapshot_if_gap_advanced();
                if outcome.accepted && !gap_advanced {
                    if self
                        .machine
                        .bot_user_id()
                        .is_some_and(|bot_user_id| bot_user_id == user_id)
                    {
                        self.snapshot
                            .clear_private_thread_access_for_guild(&guild_id, now_ms);
                    }
                    self.snapshot
                        .gateway_member_removed(&guild_id, &user_id, now_ms);
                }
                outcome.action
            }
            GatewayDispatch::GuildCreate {
                sequence,
                guild,
                channels,
                roles,
                members,
            } => {
                let guild_id = guild.guild_id.clone();
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type: D7EGC,
                    sequence,
                    description: "GUILD_CREATE".to_string(),
                    guild_id: Some(guild.guild_id.clone()),
                    channel_id: None,
                    message_id: None,
                    user_id: guild.owner_id.clone(),
                    role_id: None,
                    text: None,
                });
                if outcome.accepted {
                    self.snapshot.upsert_guild(guild);
                    if let Some(roles) = roles {
                        self.snapshot.upsert_guild_roles(roles);
                    }
                    self.snapshot
                        .upsert_guild_channels(&guild_id, channels, now_ms);
                    for member in members {
                        self.snapshot.upsert_member(member);
                    }
                }
                outcome.action
            }
            GatewayDispatch::GuildUpdate { sequence, guild } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type: D7EGU,
                    sequence,
                    description: "GUILD_UPDATE".to_string(),
                    guild_id: Some(guild.guild_id.clone()),
                    channel_id: None,
                    message_id: None,
                    user_id: guild.owner_id.clone(),
                    role_id: None,
                    text: None,
                });
                if outcome.accepted {
                    self.snapshot.upsert_guild(guild);
                }
                outcome.action
            }
            GatewayDispatch::GuildDelete {
                sequence,
                guild_id,
                unavailable,
            } => {
                let outcome = self.machine.resource_outcome(GatewayResourceEvent {
                    event_type: D7EGX,
                    sequence,
                    description: "GUILD_DELETE".to_string(),
                    guild_id: Some(guild_id.clone()),
                    channel_id: None,
                    message_id: None,
                    user_id: None,
                    role_id: None,
                    text: None,
                });
                if outcome.accepted {
                    self.snapshot
                        .gateway_guild_removed(&guild_id, unavailable, now_ms);
                }
                outcome.action
            }
            GatewayDispatch::GatewayRateLimited { opcode, retry_ms } => {
                self.machine.discord_rate_limited(opcode, retry_ms);
                GatewayAction::None
            }
            GatewayDispatch::RateLimited { sequence, limit } => {
                self.machine.rate_limited(sequence, limit);
                self.invalidate_snapshot_if_gap_advanced();
                GatewayAction::None
            }
            GatewayDispatch::Unknown { sequence, name } => {
                let outcome = self.machine.generic_dispatch_outcome(sequence, name);
                self.invalidate_snapshot_if_gap_advanced();
                outcome.action
            }
        }
    }

    fn invalidate_snapshot_if_gap_advanced(&mut self) -> bool {
        let gap_epoch = self.machine.gap_epoch();
        if gap_epoch == self.snapshot_gap_epoch {
            return false;
        }
        self.snapshot.mark_gap();
        self.snapshot_gap_epoch = gap_epoch;
        true
    }

    fn should_store_message_snapshot(&self, channel_id: &str) -> bool {
        self.message_snapshot_channel_allowlist.is_empty()
            || self.message_snapshot_channel_allowlist.contains(channel_id)
    }
}

fn parse_dispatch(root: &JsonValue, now_ms: u64) -> Result<GatewayDispatch, GatewayNormalizeError> {
    let name = required_str(root, "t")?;
    let sequence = dispatch_sequence(root)?;
    let data = root
        .get("d")
        .ok_or_else(|| GatewayNormalizeError::new("dispatch d missing"))?;
    match name {
        "READY" => {
            let resume_gateway_url = required_str(data, "resume_gateway_url")?;
            validate_gateway_url(resume_gateway_url).map_err(GatewayNormalizeError::new)?;
            let session_id = required_str(data, "session_id")?;
            validate_session_id(session_id).map_err(GatewayNormalizeError::new)?;
            let bot_user_id = data.get("user").and_then(|user| optional_str(user, "id"));
            if bot_user_id.is_some_and(|id| !validate_snowflake(id)) {
                return Err(GatewayNormalizeError::new("READY user id invalid"));
            }
            Ok(GatewayDispatch::Ready {
                sequence: sequence
                    .ok_or_else(|| GatewayNormalizeError::new("READY sequence missing"))?,
                session_id: session_id.to_string(),
                resume_gateway_url: resume_gateway_url.to_string(),
                bot_user_id: bot_user_id.map(ToOwned::to_owned),
            })
        }
        "RESUMED" => Ok(GatewayDispatch::Resumed { sequence }),
        "MESSAGE_CREATE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new("MESSAGE_CREATE sequence missing"))?;
            Ok(GatewayDispatch::MessageCreate {
                sequence,
                message: parse_message_create(data, now_ms)?,
            })
        }
        "INTERACTION_CREATE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new("INTERACTION_CREATE sequence missing"))?;
            Ok(GatewayDispatch::InteractionCreate {
                sequence,
                interaction: parse_interaction_create(data)?,
            })
        }
        "MESSAGE_DELETE" => Ok(GatewayDispatch::MessageDelete {
            sequence: sequence
                .ok_or_else(|| GatewayNormalizeError::new("MESSAGE_DELETE sequence missing"))?,
            guild_id: optional_str(data, "guild_id").map(ToOwned::to_owned),
            channel_id: required_str(data, "channel_id")?.to_string(),
            message_id: required_str(data, "id")?.to_string(),
        }),
        "MESSAGE_DELETE_BULK" => Ok(GatewayDispatch::MessageDeleteBulk {
            sequence: sequence.ok_or_else(|| {
                GatewayNormalizeError::new("MESSAGE_DELETE_BULK sequence missing")
            })?,
            guild_id: optional_str(data, "guild_id").map(ToOwned::to_owned),
            channel_id: required_str(data, "channel_id")?.to_string(),
            message_ids: required_snowflake_array(data, "ids")?,
        }),
        "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new(format!("{name} sequence missing")))?;
            let emoji = data
                .get("emoji")
                .ok_or_else(|| GatewayNormalizeError::new("reaction emoji missing"))?;
            Ok(GatewayDispatch::MessageReaction {
                sequence,
                added: name == "MESSAGE_REACTION_ADD",
                guild_id: optional_str(data, "guild_id").unwrap_or("").to_string(),
                channel_id: required_str(data, "channel_id")?.to_string(),
                message_id: required_str(data, "message_id")?.to_string(),
                user_id: required_str(data, "user_id")?.to_string(),
                emoji: reaction_emoji_label(emoji)?,
            })
        }
        "MESSAGE_UPDATE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new("MESSAGE_UPDATE sequence missing"))?;
            Ok(GatewayDispatch::MessageUpdate {
                sequence,
                patch: parse_message_update(data)?,
            })
        }
        "CHANNEL_CREATE" | "THREAD_CREATE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new(format!("{name} sequence missing")))?;
            Ok(GatewayDispatch::ChannelUpsert {
                sequence,
                event_type: channel_event_type(name),
                event_name: name.to_string(),
                channel: parse_channel_snapshot(data, None, now_ms)?,
            })
        }
        "CHANNEL_UPDATE" | "THREAD_UPDATE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new(format!("{name} sequence missing")))?;
            Ok(GatewayDispatch::ChannelUpdate {
                sequence,
                event_type: channel_event_type(name),
                event_name: name.to_string(),
                patch: crate::snapshot::channel::patch(data)
                    .ok_or_else(|| GatewayNormalizeError::new("channel update fields invalid"))?,
            })
        }
        "CHANNEL_DELETE" | "THREAD_DELETE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new(format!("{name} sequence missing")))?;
            Ok(GatewayDispatch::ChannelDelete {
                sequence,
                event_type: channel_delete_event_type(name),
                event_name: name.to_string(),
                guild_id: optional_str(data, "guild_id").map(ToOwned::to_owned),
                channel_id: required_str(data, "id")?.to_string(),
            })
        }
        "THREAD_LIST_SYNC" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new("THREAD_LIST_SYNC sequence missing"))?;
            let guild_id = threads::optional_id(data, "guild_id")?;
            let channels = parse_channel_array(data, "threads", guild_id, now_ms)?;
            let memberships = parse_thread_member_array(data, "members", None)?;
            threads::validate_sync(guild_id, &channels, &memberships)?;
            threads::validate_sync_scope(data, &channels)?;
            Ok(GatewayDispatch::ChannelSync {
                sequence,
                guild_id: guild_id.map(ToOwned::to_owned),
                channels,
                memberships,
            })
        }
        "THREAD_MEMBER_UPDATE" => {
            let sequence = sequence.ok_or_else(|| {
                GatewayNormalizeError::new("THREAD_MEMBER_UPDATE sequence missing")
            })?;
            Ok(GatewayDispatch::ThreadMemberUpdate {
                sequence,
                guild_id: threads::optional_id(data, "guild_id")?.map(ToOwned::to_owned),
                change: parse_thread_member(data, None, true)?,
            })
        }
        "THREAD_MEMBERS_UPDATE" => {
            let sequence = sequence.ok_or_else(|| {
                GatewayNormalizeError::new("THREAD_MEMBERS_UPDATE sequence missing")
            })?;
            let thread_id = required_str(data, "id")?;
            let mut changes = parse_thread_member_array(data, "added_members", Some(thread_id))?;
            changes.extend(
                optional_snowflake_array(data, "removed_member_ids")?
                    .into_iter()
                    .map(|user_id| ThreadMembershipChange {
                        thread_id: thread_id.to_string(),
                        user_id: Some(user_id),
                        joined: false,
                    }),
            );
            Ok(GatewayDispatch::ThreadMembersUpdate {
                sequence,
                guild_id: threads::optional_id(data, "guild_id")?.map(ToOwned::to_owned),
                changes,
            })
        }
        "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new(format!("{name} sequence missing")))?;
            let guild_id = required_str(data, "guild_id")?;
            let role = data
                .get("role")
                .ok_or_else(|| GatewayNormalizeError::new("role missing"))?;
            Ok(GatewayDispatch::RoleUpsert {
                sequence,
                event_type: role_upsert_event_type(name),
                event_name: name.to_string(),
                role: parse_role_snapshot(role, guild_id)?,
            })
        }
        "GUILD_ROLE_DELETE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new("GUILD_ROLE_DELETE sequence missing"))?;
            Ok(GatewayDispatch::RoleDelete {
                sequence,
                guild_id: required_str(data, "guild_id")?.to_string(),
                role_id: required_str(data, "role_id")?.to_string(),
            })
        }
        "GUILD_MEMBER_ADD" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new(format!("{name} sequence missing")))?;
            let guild_id = required_str(data, "guild_id")?;
            Ok(GatewayDispatch::MemberUpsert {
                sequence,
                event_type: D7MBA,
                event_name: name.to_string(),
                member: parse_member_snapshot(data, guild_id, now_ms)?,
            })
        }
        "GUILD_MEMBERS_CHUNK" => {
            let sequence = sequence.ok_or_else(|| {
                GatewayNormalizeError::new("GUILD_MEMBERS_CHUNK sequence missing")
            })?;
            Ok(GatewayDispatch::MembersChunk {
                sequence,
                chunk: GuildMembersChunk::parse(data, now_ms)?,
            })
        }
        "GUILD_MEMBER_UPDATE" => {
            let sequence = sequence.ok_or_else(|| {
                GatewayNormalizeError::new("GUILD_MEMBER_UPDATE sequence missing")
            })?;
            let user = data
                .get("user")
                .ok_or_else(|| GatewayNormalizeError::new("user missing"))?;
            let role_ids = data
                .get("roles")
                .map(|value| {
                    crate::snapshot::parse::snowflakes(value)
                        .ok_or_else(|| GatewayNormalizeError::new("member roles invalid"))
                })
                .transpose()?;
            Ok(GatewayDispatch::MemberUpdate {
                sequence,
                patch: MemberSnapshotPatch {
                    guild_id: required_str(data, "guild_id")?.to_string(),
                    user_id: required_str(user, "id")?.to_string(),
                    role_ids,
                    timeout: crate::snapshot::member::timeout(data)
                        .ok_or_else(|| GatewayNormalizeError::new("member timeout invalid"))?,
                },
            })
        }
        "GUILD_MEMBER_REMOVE" => {
            let sequence = sequence.ok_or_else(|| {
                GatewayNormalizeError::new("GUILD_MEMBER_REMOVE sequence missing")
            })?;
            let user = data
                .get("user")
                .ok_or_else(|| GatewayNormalizeError::new("user missing"))?;
            Ok(GatewayDispatch::MemberDelete {
                sequence,
                guild_id: required_str(data, "guild_id")?.to_string(),
                user_id: required_str(user, "id")?.to_string(),
            })
        }
        "GUILD_CREATE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new("GUILD_CREATE sequence missing"))?;
            let guild_id = required_str(data, "id")?;
            let mut channels = parse_channel_array_lossy(data, "channels", Some(guild_id), now_ms);
            let threads = parse_channel_array_lossy(data, "threads", Some(guild_id), now_ms);
            channels.extend(threads);
            let roles = parse_roles_snapshot_lossy(data, "roles", guild_id, now_ms);
            let members = parse_member_array_lossy(data, "members", guild_id, now_ms);
            Ok(GatewayDispatch::GuildCreate {
                sequence,
                guild: parse_guild_snapshot(data, now_ms)?,
                channels,
                roles,
                members,
            })
        }
        "GUILD_UPDATE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new("GUILD_UPDATE sequence missing"))?;
            Ok(GatewayDispatch::GuildUpdate {
                sequence,
                guild: parse_guild_snapshot(data, now_ms)?,
            })
        }
        "GUILD_DELETE" => {
            let sequence = sequence
                .ok_or_else(|| GatewayNormalizeError::new("GUILD_DELETE sequence missing"))?;
            let guild_id = required_str(data, "id")?.to_string();
            let unavailable = data
                .get("unavailable")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            Ok(GatewayDispatch::GuildDelete {
                sequence,
                guild_id,
                unavailable,
            })
        }
        "RATE_LIMITED" => Ok(GatewayDispatch::RateLimited {
            sequence,
            limit: GatewayRateLimit::parse(data, false)?,
        }),
        "GATEWAY_RATE_LIMITED" if sequence.is_some() => Ok(GatewayDispatch::RateLimited {
            sequence,
            limit: GatewayRateLimit::parse(data, true)?,
        }),
        "GATEWAY_RATE_LIMITED" => Ok(GatewayDispatch::GatewayRateLimited {
            opcode: required_i32(data, "opcode")?,
            retry_ms: required_u64(data, "retry_ms")?,
        }),
        other => Ok(GatewayDispatch::Unknown {
            sequence,
            name: other.to_string(),
        }),
    }
}

fn dispatch_sequence(root: &JsonValue) -> Result<Option<i64>, GatewayNormalizeError> {
    match root.get("s") {
        None | Some(JsonValue::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|sequence| *sequence >= 0)
            .map(Some)
            .ok_or_else(|| GatewayNormalizeError::new("dispatch sequence invalid")),
    }
}

fn parse_dispatch_tolerant(
    root: &JsonValue,
    now_ms: u64,
) -> Result<GatewayDispatch, GatewayNormalizeError> {
    let name = required_str(root, "t")?;
    let sequence = dispatch_sequence(root)?;
    if name == "READY" {
        return parse_dispatch(root, now_ms);
    }
    match parse_dispatch(root, now_ms) {
        Ok(dispatch) => Ok(dispatch),
        Err(_) => Ok(GatewayDispatch::Unknown {
            sequence,
            name: invalid_dispatch_name(name),
        }),
    }
}

fn invalid_dispatch_name(name: &str) -> String {
    let mut output = String::from("INVALID_");
    for character in name.chars().take(80) {
        if character.is_ascii_alphanumeric() || character == '_' {
            output.push(character);
        } else {
            output.push('_');
        }
    }
    if output == "INVALID_" {
        output.push_str("DISPATCH");
    }
    output
}

fn invalid_dispatch_gap_reason(name: &str) -> Option<&'static str> {
    if invalid_permission_dispatch(name) {
        Some("INVALID_PERMISSION_DISPATCH")
    } else if matches!(
        name,
        "INVALID_MESSAGE_CREATE"
            | "INVALID_MESSAGE_UPDATE"
            | "INVALID_MESSAGE_DELETE"
            | "INVALID_MESSAGE_DELETE_BULK"
    ) {
        Some("INVALID_MESSAGE_DISPATCH")
    } else {
        None
    }
}

fn invalid_permission_dispatch(name: &str) -> bool {
    matches!(
        name,
        "INVALID_GUILD_CREATE"
            | "INVALID_GUILD_UPDATE"
            | "INVALID_GUILD_DELETE"
            | "INVALID_GUILD_ROLE_CREATE"
            | "INVALID_GUILD_ROLE_UPDATE"
            | "INVALID_GUILD_ROLE_DELETE"
            | "INVALID_GUILD_MEMBER_ADD"
            | "INVALID_GUILD_MEMBER_UPDATE"
            | "INVALID_GUILD_MEMBERS_CHUNK"
            | "INVALID_GUILD_MEMBER_REMOVE"
            | "INVALID_CHANNEL_CREATE"
            | "INVALID_CHANNEL_UPDATE"
            | "INVALID_CHANNEL_DELETE"
            | "INVALID_THREAD_CREATE"
            | "INVALID_THREAD_UPDATE"
            | "INVALID_THREAD_DELETE"
            | "INVALID_THREAD_LIST_SYNC"
            | "INVALID_THREAD_MEMBER_UPDATE"
            | "INVALID_THREAD_MEMBERS_UPDATE"
    )
}

fn parse_interaction_create(
    data: &JsonValue,
) -> Result<InteractionSnapshot, GatewayNormalizeError> {
    let interaction_id = required_str(data, "id")?;
    let application_id = required_str(data, "application_id")?;
    if !validate_snowflake(interaction_id) || !validate_snowflake(application_id) {
        return Err(GatewayNormalizeError::new("INTERACTION_CREATE id invalid"));
    }
    let user_id = data
        .get("member")
        .and_then(|member| member.get("user"))
        .and_then(|user| optional_str(user, "id"))
        .or_else(|| data.get("user").and_then(|user| optional_str(user, "id")))
        .unwrap_or("");
    if !user_id.is_empty() && !validate_snowflake(user_id) {
        return Err(GatewayNormalizeError::new(
            "INTERACTION_CREATE user id invalid",
        ));
    }
    let name = data
        .get("data")
        .and_then(|value| optional_str(value, "name"))
        .unwrap_or("");
    Ok(InteractionSnapshot {
        guild_id: optional_str(data, "guild_id").unwrap_or("").to_string(),
        channel_id: optional_str(data, "channel_id").unwrap_or("").to_string(),
        interaction_id: interaction_id.to_string(),
        application_id: application_id.to_string(),
        token: required_str(data, "token")?.to_string(),
        user_id: user_id.to_string(),
        interaction_type: required_i32(data, "type")?,
        name: name.to_string(),
    })
}

fn reaction_emoji_label(emoji: &JsonValue) -> Result<String, GatewayNormalizeError> {
    let name = optional_str(emoji, "name").unwrap_or("");
    let id = optional_str(emoji, "id").unwrap_or("");
    if !id.is_empty() {
        if !validate_snowflake(id) {
            return Err(GatewayNormalizeError::new("reaction emoji id invalid"));
        }
        if name.is_empty() {
            return Ok(id.to_string());
        }
        return Ok(format!("{name}:{id}"));
    }
    if name.is_empty() {
        return Err(GatewayNormalizeError::new("reaction emoji name missing"));
    }
    Ok(name.to_string())
}

fn parse_message_create(
    data: &JsonValue,
    now_ms: u64,
) -> Result<MessageSnapshot, GatewayNormalizeError> {
    required_i32(data, "type")?;
    crate::snapshot::message::full(data, "", "", now_ms)
        .filter(|message| message.complete)
        .ok_or_else(|| GatewayNormalizeError::new("MESSAGE_CREATE fields invalid"))
}

fn parse_message_update(data: &JsonValue) -> Result<MessageSnapshotPatch, GatewayNormalizeError> {
    crate::snapshot::message::patch(data)
        .ok_or_else(|| GatewayNormalizeError::new("MESSAGE_UPDATE fields invalid"))
}

fn parse_guild_snapshot(
    data: &JsonValue,
    now_ms: u64,
) -> Result<GuildSnapshot, GatewayNormalizeError> {
    crate::snapshot::parse::guild(data, now_ms)
        .ok_or_else(|| GatewayNormalizeError::new("guild fields invalid"))
}

fn parse_channel_array(
    data: &JsonValue,
    key: &str,
    fallback_guild_id: Option<&str>,
    now_ms: u64,
) -> Result<Vec<ChannelSnapshot>, GatewayNormalizeError> {
    let Some(values) = data.get(key) else {
        return Ok(Vec::new());
    };
    values
        .as_array()
        .ok_or_else(|| GatewayNormalizeError::new(format!("{key} must be array")))?
        .iter()
        .map(|value| parse_channel_snapshot(value, fallback_guild_id, now_ms))
        .collect()
}

fn parse_channel_array_lossy(
    data: &JsonValue,
    key: &str,
    fallback_guild_id: Option<&str>,
    now_ms: u64,
) -> Vec<ChannelSnapshot> {
    let Some(values) = data.get(key) else {
        return Vec::new();
    };
    let Some(values) = values.as_array() else {
        return Vec::new();
    };
    let mut snapshots = Vec::new();
    for value in values {
        if let Ok(snapshot) = parse_channel_snapshot_lossy(value, fallback_guild_id, now_ms) {
            if fallback_guild_id.is_none_or(|guild_id| guild_id == snapshot.guild_id)
                && (key != "threads" || matches!(snapshot.channel_type, 10..=12))
            {
                snapshots.push(snapshot);
            }
        }
    }
    snapshots
}

fn parse_channel_snapshot(
    data: &JsonValue,
    fallback_guild_id: Option<&str>,
    now_ms: u64,
) -> Result<ChannelSnapshot, GatewayNormalizeError> {
    required_i32(data, "type")?;
    crate::snapshot::channel::full(data, fallback_guild_id.unwrap_or(""), "", now_ms, false)
        .ok_or_else(|| GatewayNormalizeError::new("channel fields invalid"))
}

fn parse_channel_snapshot_lossy(
    data: &JsonValue,
    fallback_guild_id: Option<&str>,
    now_ms: u64,
) -> Result<ChannelSnapshot, GatewayNormalizeError> {
    required_i32(data, "type")?;
    crate::snapshot::channel::full(data, fallback_guild_id.unwrap_or(""), "", now_ms, true)
        .ok_or_else(|| GatewayNormalizeError::new("channel fields invalid"))
}

fn parse_roles_snapshot_lossy(
    data: &JsonValue,
    key: &str,
    guild_id: &str,
    now_ms: u64,
) -> Option<GuildRolesSnapshot> {
    let values = data.get(key)?;
    let Some(values) = values.as_array() else {
        let mut snapshot = GuildRolesSnapshot::new(guild_id, Vec::new(), now_ms);
        snapshot.complete = false;
        return Some(snapshot);
    };
    let mut complete = true;
    let mut role_ids = HashSet::new();
    let mut roles = Vec::new();
    for value in values {
        match parse_role_snapshot(value, guild_id) {
            Ok(role) => {
                if !role_ids.insert(role.role_id.clone()) {
                    complete = false;
                }
                roles.push(role);
            }
            Err(_) => complete = false,
        }
    }
    let mut snapshot = GuildRolesSnapshot::new(guild_id, roles, now_ms);
    snapshot.complete = complete;
    Some(snapshot)
}

fn parse_role_snapshot(
    data: &JsonValue,
    guild_id: &str,
) -> Result<RoleSnapshot, GatewayNormalizeError> {
    crate::snapshot::parse::role(data, guild_id)
        .ok_or_else(|| GatewayNormalizeError::new("role invalid"))
}

fn parse_member_snapshot(
    data: &JsonValue,
    guild_id: &str,
    now_ms: u64,
) -> Result<MemberSnapshot, GatewayNormalizeError> {
    let user = data
        .get("user")
        .ok_or_else(|| GatewayNormalizeError::new("user missing"))?;
    let user_id = required_str(user, "id")?;
    let (role_ids, complete) = match data.get("roles") {
        Some(value) => crate::snapshot::parse::snowflakes_lossy(value),
        None => (Vec::new(), false),
    };
    let mut snapshot = MemberSnapshot::new(guild_id, user_id, role_ids, now_ms);
    snapshot.complete = complete;
    snapshot.roles_complete = complete;
    match crate::snapshot::member::timeout(data) {
        Some(timeout) => timeout.apply(&mut snapshot, now_ms),
        None => {
            snapshot.timeout_known = false;
            snapshot.timed_out = true;
            snapshot.complete = false;
        }
    }
    Ok(snapshot)
}

fn parse_member_array_lossy(
    data: &JsonValue,
    key: &str,
    guild_id: &str,
    now_ms: u64,
) -> Vec<MemberSnapshot> {
    let Some(values) = data.get(key) else {
        return Vec::new();
    };
    let Some(values) = values.as_array() else {
        return Vec::new();
    };
    let mut snapshots = Vec::new();
    for value in values {
        if let Ok(snapshot) = parse_member_snapshot(value, guild_id, now_ms) {
            snapshots.push(snapshot);
        }
    }
    snapshots
}

fn required_str<'a>(value: &'a JsonValue, key: &str) -> Result<&'a str, GatewayNormalizeError> {
    let text = optional_str(value, key)
        .ok_or_else(|| GatewayNormalizeError::new(format!("{key} missing")))?;
    if matches!(
        key,
        "id" | "guild_id"
            | "channel_id"
            | "message_id"
            | "user_id"
            | "application_id"
            | "role_id"
            | "owner_id"
    ) && !validate_snowflake(text)
    {
        return Err(GatewayNormalizeError::new(format!("{key} invalid")));
    }
    Ok(text)
}

fn optional_str<'a>(value: &'a JsonValue, key: &str) -> Option<&'a str> {
    value.get(key).and_then(JsonValue::as_str)
}

fn required_i64(value: &JsonValue, key: &str) -> Result<i64, GatewayNormalizeError> {
    value
        .get(key)
        .and_then(JsonValue::as_i64)
        .ok_or_else(|| GatewayNormalizeError::new(format!("{key} missing")))
}

fn required_u64(value: &JsonValue, key: &str) -> Result<u64, GatewayNormalizeError> {
    value
        .get(key)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| GatewayNormalizeError::new(format!("{key} missing")))
}

fn required_u32(value: &JsonValue, key: &str) -> Result<u32, GatewayNormalizeError> {
    let number = required_u64(value, key)?;
    u32::try_from(number).map_err(|_| GatewayNormalizeError::new(format!("{key} too large")))
}

fn required_i32(value: &JsonValue, key: &str) -> Result<i32, GatewayNormalizeError> {
    let number = required_i64(value, key)?;
    i32::try_from(number).map_err(|_| GatewayNormalizeError::new(format!("{key} too large")))
}

fn required_snowflake_array(
    value: &JsonValue,
    key: &str,
) -> Result<Vec<String>, GatewayNormalizeError> {
    value
        .get(key)
        .and_then(crate::snapshot::parse::snowflakes)
        .ok_or_else(|| GatewayNormalizeError::new(format!("{key} invalid Snowflake array")))
}

fn optional_snowflake_array(
    value: &JsonValue,
    key: &str,
) -> Result<Vec<String>, GatewayNormalizeError> {
    if value.get(key).is_none() {
        Ok(Vec::new())
    } else {
        required_snowflake_array(value, key)
    }
}

impl HeapSize for InteractionSnapshot {
    fn heap_size(&self) -> usize {
        self.guild_id.heap_size()
            + self.channel_id.heap_size()
            + self.interaction_id.heap_size()
            + self.application_id.heap_size()
            + self.token.heap_size()
            + self.user_id.heap_size()
            + self.name.heap_size()
    }
}

impl HeapSize for GatewayEvent {
    fn heap_size(&self) -> usize {
        self.description.heap_size()
            + match &self.data {
                GatewayEventData::None => 0,
                GatewayEventData::Message(value) => value.heap_size(),
                GatewayEventData::Interaction(value) => value.heap_size(),
                GatewayEventData::MessageDelete {
                    guild_id,
                    channel_id,
                    message_id,
                } => guild_id.heap_size() + channel_id.heap_size() + message_id.heap_size(),
                GatewayEventData::Reaction {
                    guild_id,
                    channel_id,
                    message_id,
                    user_id,
                    emoji,
                } => {
                    guild_id.heap_size()
                        + channel_id.heap_size()
                        + message_id.heap_size()
                        + user_id.heap_size()
                        + emoji.heap_size()
                }
                GatewayEventData::Member { guild_id, user_id } => {
                    guild_id.heap_size() + user_id.heap_size()
                }
                GatewayEventData::MembersChunk {
                    guild_id,
                    user_ids,
                    metadata,
                } => guild_id.heap_size() + user_ids.heap_size() + metadata.heap_size(),
                GatewayEventData::RateLimit(limit) => {
                    limit.guild_id.heap_size() + limit.nonce.heap_size()
                }
                GatewayEventData::Resource {
                    guild_id,
                    channel_id,
                    message_id,
                    user_id,
                    role_id,
                    text,
                } => {
                    guild_id.heap_size()
                        + channel_id.heap_size()
                        + message_id.heap_size()
                        + user_id.heap_size()
                        + role_id.heap_size()
                        + text.heap_size()
                }
            }
    }
}

impl QueueEvent for GatewayEvent {
    fn rate_limit_scope(&self) -> Option<&str> {
        match &self.data {
            GatewayEventData::RateLimit(limit) if limit.opcode == 8 => limit.guild_id.as_deref(),
            _ => None,
        }
    }

    fn kind(&self) -> i32 {
        self.event_type
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn description_mut(&mut self) -> &mut String {
        &mut self.description
    }
    fn opcode(&self) -> Option<i32> {
        self.opcode
    }
    fn gap(reason: String) -> Self {
        Self::gap(reason)
    }
}

#[derive(Clone, Debug)]
pub struct GatewayEventQueue {
    inner: EventQueue<GatewayEvent>,
}

impl GatewayEventQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: EventQueue::new(capacity, DEFAULT_EVENT_QUEUE_MAX_BYTES, true),
        }
    }
    pub fn set_max_bytes(&mut self, max_bytes: usize) -> bool {
        self.inner.set_max_bytes(max_bytes)
    }
    pub fn push_normal(&mut self, event: GatewayEvent) -> bool {
        self.inner.push_normal(event)
    }
    pub fn push_control(&mut self, event: GatewayEvent) {
        self.inner.push_control(event);
    }
    pub fn pop(&mut self) -> Option<GatewayEvent> {
        self.inner.pop_front()
    }
    pub fn len(&self) -> usize {
        self.inner.len()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    pub fn gap_epoch(&self) -> u64 {
        self.inner.gap_epoch()
    }
    pub fn retained_bytes(&self) -> usize {
        self.inner.retained_bytes()
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn malformed_guild_authority_fields_invalidate_cached_permission_state() {
        for data in [
            r#"{"id":"10","owner_id":"invalid"}"#,
            r#"{"id":"10","owner_id":true}"#,
            r#"{"id":"10","owner_id":null}"#,
            r#"{"id":"10","owner_id":""}"#,
            r#"{"id":"10","unavailable":"false"}"#,
            r#"{"id":"10","unavailable":null}"#,
            r#"{"id":"invalid","owner_id":"99"}"#,
        ] {
            let mut ingestor = GatewayIngestor::new(16, 100, 100, 10, 10);
            ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#, 0).unwrap();
            let mut guild = GuildSnapshot::new("10", false, 0);
            guild.owner_id = Some("99".into());
            ingestor.snapshot_mut().upsert_guild(guild);
            let input = format!(r#"{{"op":0,"s":2,"t":"GUILD_UPDATE","d":{data}}}"#);
            assert!(parse_gateway_frame(&input, 1).is_err());
            ingestor.ingest_json_tolerant(&input, 1).unwrap();
            assert_eq!(ingestor.machine().gap_epoch(), 1);
            assert_eq!(
                ingestor.snapshot_mut().get_guild("10", 1),
                SnapshotLookup::Missing
            );
        }
    }
    use super::*;
    use crate::permission::PermissionSet;

    fn session_ingestor() -> GatewayIngestor {
        let mut ingestor = GatewayIngestor::new(32, 1000, 1000, 100, 100);
        ingestor.ingest_json(r#"{"op":0,"s":10,"t":"READY","d":{"session_id":"old","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":11,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"99","roles":[{"id":"10","permissions":"8","position":0}],"members":[{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}]}}"#, 1).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":12,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","author":{"id":"40"},"content":"old session","type":0,"member":{"permissions":"8"}}}"#, 2).unwrap();
        ingestor
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("10", "20", 0, 2));
        ingestor
    }

    #[test]
    fn new_ready_session_discards_old_snapshots_and_pending_messages() {
        for user in ["", r#", "user":{"id":"88"}"#] {
            let mut ingestor = session_ingestor();
            let store = ingestor.snapshot_mut();
            store.tombstone_guild("11", 2);
            store.tombstone_channel("21", 2);
            store.tombstone_member("10", "41", 2);
            store.tombstone_user("42", 2);
            store.tombstone_message("20", "31", 2);
            assert_eq!(ingestor.machine().gap_epoch(), 0);
            let ready = format!(
                r#"{{"op":0,"s":1,"t":"READY","d":{{"session_id":"new","resume_gateway_url":"wss://new"{user}}}}}"#
            );
            ingestor.ingest_json(&ready, 3).unwrap();
            assert_eq!(ingestor.snapshot().message_count(), 0);
            assert_eq!(ingestor.snapshot().tombstone_count(), 0);
            let store = ingestor.snapshot_mut();
            for guild in ["10", "11"] {
                assert_eq!(store.get_guild(guild, 3), SnapshotLookup::Missing);
            }
            for channel in ["20", "21"] {
                assert_eq!(store.get_channel(channel, 3), SnapshotLookup::Missing);
            }
            for member in ["40", "41"] {
                assert_eq!(store.get_member("10", member, 3), SnapshotLookup::Missing);
            }
            assert_eq!(store.get_guild_roles("10", 3), SnapshotLookup::Missing);
            assert!(!store.is_user_tombstoned("42", 3));
            assert_eq!(ingestor.machine().session_id(), Some("new"));
            assert_eq!(ingestor.machine().last_seen_seq(), Some(1));
            assert_eq!(
                ingestor.machine().bot_user_id(),
                if user.is_empty() { None } else { Some("88") }
            );
            let mut events = Vec::new();
            while let Some(event) = ingestor.pop_event() {
                events.push(event);
            }
            assert_eq!(events[0].event_type, D7GAP);
            assert_eq!(events[0].description, "GATEWAY_SESSION_REPLACED");
            assert!(events.iter().all(|event| event.event_type != D7MSG));
            assert!(events
                .iter()
                .any(|event| event.event_type == D7RDY && event.sequence == Some(1)));
            ingestor.ingest_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"32","author":{"id":"40"},"content":"new session","type":0}}"#, 4).unwrap();
            assert_eq!(ingestor.snapshot().message_count(), 1);
            let event = ingestor.pop_event().unwrap();
            assert_eq!(event.event_type, D7MSG);
            assert_eq!(event.sequence, Some(2));
        }
    }

    #[test]
    fn duplicate_ready_and_successful_resume_preserve_current_session_snapshots() {
        let mut ingestor = session_ingestor();
        ingestor.ingest_json(r#"{"op":0,"s":10,"t":"READY","d":{"session_id":"old","resume_gateway_url":"wss://other","user":{"id":"88"}}}"#, 3).unwrap();
        assert_eq!(
            ingestor.machine_mut().reconnect_opcode(),
            GatewayAction::SendResume { seq: 12 }
        );
        ingestor
            .ingest_json(r#"{"op":0,"s":13,"t":"RESUMED","d":{}}"#, 4)
            .unwrap();
        assert_eq!(ingestor.machine().gap_epoch(), 0);
        assert_eq!(ingestor.machine().bot_user_id(), Some("99"));
        assert_eq!(ingestor.snapshot().message_count(), 1);
        assert!(matches!(
            ingestor.snapshot_mut().get_guild_roles("10", 4),
            SnapshotLookup::Found(_)
        ));
        let mut messages = 0;
        let mut resumed = 0;
        while let Some(event) = ingestor.pop_event() {
            assert_ne!(event.event_type, D7GAP);
            messages += usize::from(event.event_type == D7MSG);
            resumed += usize::from(event.event_type == D7ERS);
        }
        assert_eq!(messages, 1);
        assert_eq!(resumed, 1);
    }

    #[test]
    fn restoring_a_different_session_invalidates_cached_state_before_resume() {
        let mut ingestor = session_ingestor();
        let session = GatewayResumeSession::new("new", "wss://new", 5, None).unwrap();
        assert_eq!(
            ingestor.machine_mut().restore_resume_session(session),
            Ok(GatewayAction::SendResume { seq: 5 })
        );
        assert!(ingestor.sync_snapshot_gap());
        assert_eq!(ingestor.snapshot().message_count(), 0);
        assert_eq!(ingestor.machine().bot_user_id(), None);
        let gap = ingestor.pop_event().unwrap();
        assert_eq!(gap.event_type, D7GAP);
        assert_eq!(gap.description, "GATEWAY_SESSION_REPLACED");
        while let Some(event) = ingestor.pop_event() {
            assert_ne!(event.event_type, D7MSG);
        }
    }

    #[test]
    fn restoring_the_current_session_preserves_the_cursor_and_known_identity() {
        for saved_sequence in [5, 12] {
            let mut ingestor = session_ingestor();
            let session =
                GatewayResumeSession::new("old", "wss://resume", saved_sequence, None).unwrap();
            assert_eq!(
                ingestor.machine_mut().restore_resume_session(session),
                Ok(GatewayAction::SendResume {
                    seq: saved_sequence.max(12)
                })
            );
            assert!(!ingestor.sync_snapshot_gap());
            assert_eq!(ingestor.snapshot().message_count(), 1);
            assert_eq!(ingestor.machine().bot_user_id(), Some("99"));
            assert!(!ingestor.machine_mut().dispatch(12, "old message"));
        }
        let mut ingestor = session_ingestor();
        let session =
            GatewayResumeSession::new("old", "wss://other", 5, Some("88".into())).unwrap();
        assert_eq!(
            ingestor.machine_mut().restore_resume_session(session),
            Err("resume bot user id mismatch")
        );
        assert_eq!(ingestor.machine().state(), GatewayState::Ready);
        assert_eq!(ingestor.machine().last_seen_seq(), Some(12));
        assert_eq!(
            ingestor.machine().resume_gateway_url(),
            Some("wss://resume")
        );
        assert_eq!(ingestor.machine().bot_user_id(), Some("99"));
        assert_eq!(ingestor.machine().gap_epoch(), 0);
    }

    #[test]
    fn restoring_a_later_cursor_cannot_preserve_snapshots_of_skipped_events() {
        let mut ingestor = session_ingestor();
        let session =
            GatewayResumeSession::new("old", "wss://resume", 14, Some("99".into())).unwrap();
        assert_eq!(
            ingestor.machine_mut().restore_resume_session(session),
            Ok(GatewayAction::SendResume { seq: 14 })
        );
        assert!(ingestor.sync_snapshot_gap());
        assert_eq!(ingestor.snapshot().message_count(), 0);
        assert_eq!(ingestor.machine().bot_user_id(), Some("99"));
        let gap = ingestor.pop_event().unwrap();
        assert_eq!(gap.event_type, D7GAP);
        assert_eq!(gap.description, "GATEWAY_RESUME_CURSOR_ADVANCED");
        while let Some(event) = ingestor.pop_event() {
            assert_ne!(event.event_type, D7MSG);
        }
    }

    #[test]
    fn conflicting_ready_identity_cannot_rebind_an_existing_session() {
        let mut ingestor = session_ingestor();
        ingestor.ingest_json(r#"{"op":0,"s":13,"t":"READY","d":{"session_id":"old","resume_gateway_url":"wss://other","user":{"id":"88"}}}"#, 3).unwrap();
        assert_eq!(ingestor.machine().last_seen_seq(), Some(12));
        assert_eq!(
            ingestor.machine().resume_gateway_url(),
            Some("wss://resume")
        );
        assert_eq!(ingestor.machine().bot_user_id(), Some("99"));
        assert_eq!(ingestor.snapshot().message_count(), 0);
        let gap = ingestor.pop_event().unwrap();
        assert_eq!(gap.event_type, D7GAP);
        assert_eq!(gap.description, "READY_IDENTITY_MISMATCH");
        while let Some(event) = ingestor.pop_event() {
            assert_ne!(event.event_type, D7MSG);
        }
    }

    #[test]
    fn malformed_permission_updates_invalidate_cached_permissions_and_report_a_gap() {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 100, 100);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"9"}}}"#, 0).unwrap();
        ingestor
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("1", "20", 0, 0));
        ingestor
            .snapshot_mut()
            .upsert_guild_roles(GuildRolesSnapshot::new(
                "1",
                vec![RoleSnapshot {
                    guild_id: "1".into(),
                    role_id: "1".into(),
                    permissions: PermissionSet::from_bits(&[3]),
                    position: 0,
                }],
                0,
            ));
        while ingestor.pop_event().is_some() {}
        ingestor.ingest_json_tolerant(r#"{"op":0,"s":2,"t":"CHANNEL_UPDATE","d":{"id":"20","guild_id":"1","type":0,"permission_overwrites":[{"id":"1","type":2,"allow":"0","deny":"1024"}]}}"#, 1).unwrap();
        assert!(matches!(
            ingestor.snapshot_mut().get_channel("20", 1),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            ingestor.snapshot_mut().get_guild_roles("1", 1),
            SnapshotLookup::Missing
        ));
        assert_eq!(ingestor.machine().gap_epoch(), 1);
        assert_eq!(ingestor.pop_event().unwrap().event_type, D7GAP);
        let notification = ingestor.pop_event().unwrap();
        assert_eq!(notification.event_type, D7GEV);
        assert_eq!(notification.description, "INVALID_CHANNEL_UPDATE");
    }

    #[test]
    fn partial_thread_updates_preserve_cached_access_and_publish_the_existing_event() {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 10, 10);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"THREAD_CREATE","d":{"id":"20","guild_id":"10","type":12,"parent_id":"30","thread_metadata":{"archived":false,"locked":true},"member":{}}}"#, 1).unwrap();
        while ingestor.pop_event().is_some() {}
        ingestor.ingest_json(r#"{"op":0,"s":3,"t":"THREAD_UPDATE","d":{"id":"20","guild_id":"10","type":12,"name":"new name","thread_metadata":{"archived":true}}}"#, 2).unwrap();
        let SnapshotLookup::Found(channel) = ingestor.snapshot_mut().get_channel("20", 2) else {
            panic!("expected cached thread");
        };
        assert!(channel.complete);
        assert!(channel.archived);
        assert!(channel.locked);
        assert_eq!(channel.parent_channel_id.as_deref(), Some("30"));
        assert_eq!(channel.current_user_thread_joined, Some(true));
        assert_eq!(channel.current_user_thread_joined_at_ms, Some(1));
        let event = ingestor.pop_event().unwrap();
        assert_eq!(event.event_type, D7ETU);
        assert_eq!(event.description, "THREAD_UPDATE");
        assert_eq!(event.sequence, Some(3));
        ingestor
            .ingest_json(
                r#"{"op":0,"s":4,"t":"CHANNEL_CREATE","d":{"id":"30","guild_id":"10","type":0}}"#,
                3,
            )
            .unwrap();
        ingestor
            .ingest_json(
                r#"{"op":0,"s":5,"t":"CHANNEL_UPDATE","d":{"id":"30","permission_overwrites":[]}}"#,
                4,
            )
            .unwrap();
        let SnapshotLookup::Found(channel) = ingestor.snapshot_mut().get_channel("20", 4) else {
            panic!("expected cached thread after parent update");
        };
        assert_eq!(channel.current_user_thread_joined, None);
    }

    #[test]
    fn invalid_thread_metadata_triggers_a_gap_and_obfuscated_guild_channels_are_retained() {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 10, 10);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","channels":[{"id":"20","type":0,"flags":131072,"permission_overwrites":[{"id":"10","type":0,"allow":"0","deny":"1024"}]}]}}"#, 1).unwrap();
        let SnapshotLookup::Found(channel) = ingestor.snapshot_mut().get_channel("20", 1) else {
            panic!("expected obfuscated channel");
        };
        assert!(channel.obfuscated);
        while ingestor.pop_event().is_some() {}
        ingestor.ingest_json_tolerant(r#"{"op":0,"s":3,"t":"THREAD_UPDATE","d":{"id":"30","thread_metadata":{"locked":"false"}}}"#, 2).unwrap();
        assert_eq!(
            ingestor.snapshot_mut().get_channel("20", 2),
            SnapshotLookup::Missing
        );
        assert_eq!(ingestor.pop_event().unwrap().event_type, D7GAP);
        assert_eq!(
            ingestor.pop_event().unwrap().description,
            "INVALID_THREAD_UPDATE"
        );
    }

    #[test]
    fn malformed_message_fields_report_a_gap_and_never_keep_old_moderation_state() {
        for (event, fields) in [
            (
                "MESSAGE_CREATE",
                r#""author":{"id":"40"},"content":"missing type""#,
            ),
            ("MESSAGE_UPDATE", r#""content":null"#),
            ("MESSAGE_UPDATE", r#""author":{"id":"40","bot":"false"}"#),
            ("MESSAGE_UPDATE", r#""member":{"roles":["50",false]}"#),
            ("MESSAGE_UPDATE", r#""attachments":[{"filename":false}]"#),
            ("MESSAGE_DELETE_BULK", r#""ids":["30","not-an-id"]"#),
        ] {
            let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 10, 10);
            ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume"}}"#, 0).unwrap();
            ingestor.ingest_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","author":{"id":"40"},"type":0,"attachments":[{"filename":"image.png"}]}}"#, 1).unwrap();
            while ingestor.pop_event().is_some() {}
            ingestor.ingest_json_tolerant(&format!(r#"{{"op":0,"s":3,"t":"{event}","d":{{"guild_id":"10","channel_id":"20","id":"30",{fields}}}}}"#), 2).unwrap();
            assert_eq!(ingestor.snapshot().retained_bytes(), 0);
            let gap = ingestor.pop_event().unwrap();
            assert_eq!(gap.event_type, D7GAP);
            assert_eq!(gap.description, "INVALID_MESSAGE_DISPATCH");
            assert_eq!(ingestor.machine().gap_epoch(), 1);
        }
    }

    #[test]
    fn message_identity_conflicts_report_a_gap_and_discard_mixed_state() {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 10, 10);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume"}}"#, 0).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","author":{"id":"40"},"type":0,"attachments":[{"filename":"image.png"}]}}"#, 1).unwrap();
        while ingestor.pop_event().is_some() {}
        ingestor.ingest_json(r#"{"op":0,"s":3,"t":"MESSAGE_UPDATE","d":{"channel_id":"20","id":"30","author":{"id":"99"}}}"#, 2).unwrap();
        assert_eq!(ingestor.snapshot().retained_bytes(), 0);
        let gap = ingestor.pop_event().unwrap();
        assert_eq!(gap.event_type, D7GAP);
        assert_eq!(gap.description, "MESSAGE_IDENTITY_CONFLICT");
    }

    #[test]
    fn every_resource_dispatch_invalidates_all_snapshots_when_its_event_overflows() {
        for (name, data) in [
            ("CHANNEL_UPDATE", r#"{"id":"20","name":"renamed"}"#),
            ("CHANNEL_CREATE", r#"{"id":"20","guild_id":"10","type":0}"#),
            ("CHANNEL_DELETE", r#"{"id":"20","guild_id":"10"}"#),
            (
                "THREAD_LIST_SYNC",
                r#"{"guild_id":"10","threads":[{"id":"30","type":12}]}"#,
            ),
            (
                "THREAD_MEMBER_UPDATE",
                r#"{"guild_id":"10","id":"20","user_id":"99"}"#,
            ),
            (
                "THREAD_MEMBERS_UPDATE",
                r#"{"guild_id":"10","id":"20","removed_member_ids":["99"]}"#,
            ),
            (
                "GUILD_ROLE_CREATE",
                r#"{"guild_id":"10","role":{"id":"40","permissions":"8","position":1}}"#,
            ),
            ("GUILD_ROLE_DELETE", r#"{"guild_id":"10","role_id":"40"}"#),
            (
                "GUILD_MEMBER_ADD",
                r#"{"guild_id":"10","user":{"id":"99"},"roles":[]}"#,
            ),
            (
                "GUILD_MEMBER_UPDATE",
                r#"{"guild_id":"10","user":{"id":"99"},"roles":[]}"#,
            ),
            (
                "GUILD_MEMBER_REMOVE",
                r#"{"guild_id":"10","user":{"id":"99"}}"#,
            ),
            (
                "GUILD_CREATE",
                r#"{"id":"10","roles":[{"id":"10","permissions":"8","position":0}]}"#,
            ),
            ("GUILD_UPDATE", r#"{"id":"10","owner_id":"99"}"#),
            ("GUILD_DELETE", r#"{"id":"10","unavailable":false}"#),
            (
                "MESSAGE_UPDATE",
                r#"{"id":"50","channel_id":"20","content":"updated"}"#,
            ),
            ("MESSAGE_DELETE_BULK", r#"{"channel_id":"20","ids":["50"]}"#),
        ] {
            let mut ingestor = GatewayIngestor::new(1, 1000, 1000, 10, 10);
            ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
            ingestor
                .snapshot_mut()
                .upsert_channel(ChannelSnapshot::new("10", "20", 12, 0));
            ingestor
                .snapshot_mut()
                .upsert_member(MemberSnapshot::new("10", "99", vec![], 0));
            ingestor.snapshot_mut().mark_guild_available("10", 0);
            ingestor
                .snapshot_mut()
                .upsert_guild_roles(GuildRolesSnapshot::new("10", vec![], 0));
            ingestor
                .ingest_json(r#"{"op":0,"s":2,"t":"FUTURE_EVENT","d":{}}"#, 1)
                .unwrap();
            assert_eq!(ingestor.machine().gap_epoch(), 0, "{name}");
            ingestor
                .ingest_json(&format!(r#"{{"op":0,"s":3,"t":"{name}","d":{data}}}"#), 2)
                .unwrap();
            assert_eq!(ingestor.machine().gap_epoch(), 1, "{name}");
            assert_eq!(ingestor.snapshot_mut().retained_bytes(), 0, "{name}");
        }
    }

    #[test]
    fn future_dispatches_and_ignored_replays_do_not_discard_valid_snapshots() {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 100, 100);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume"}}"#, 0).unwrap();
        ingestor
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("1", "20", 0, 0));
        ingestor
            .ingest_json_tolerant(r#"{"op":0,"s":2,"t":"FUTURE_EVENT","d":{}}"#, 1)
            .unwrap();
        assert!(matches!(
            ingestor.snapshot_mut().get_channel("20", 1),
            SnapshotLookup::Found(_)
        ));
        ingestor.machine.resume_seq_used = Some(3);
        ingestor.ingest_json_tolerant(r#"{"op":0,"s":3,"t":"CHANNEL_UPDATE","d":{"id":"20","guild_id":"1","type":"bad"}}"#, 2).unwrap();
        assert_eq!(ingestor.machine().gap_epoch(), 0);
        assert!(matches!(
            ingestor.snapshot_mut().get_channel("20", 2),
            SnapshotLookup::Found(_)
        ));
    }

    #[test]
    fn malformed_role_arrays_keep_useful_parts_but_never_complete_permission_state() {
        let data = parse_json(r#"{"user":{"id":"9"},"roles":["10",false,"20"]}"#).unwrap();
        let member = parse_member_snapshot(&data, "1", 0).unwrap();
        assert!(!member.complete);
        assert_eq!(member.role_ids, vec!["10", "20"]);
        let data = parse_json(r#"{"roles":[{"id":"1","permissions":"0","position":0},{"id":"1","permissions":"8","position":1}]}"#).unwrap();
        assert!(
            !parse_roles_snapshot_lossy(&data, "roles", "1", 0)
                .unwrap()
                .complete
        );
    }

    #[test]
    fn interaction_and_transport_diagnostics_hide_credentials_through_nested_values() {
        let interaction = InteractionSnapshot {
            guild_id: "10".into(),
            channel_id: "20".into(),
            interaction_id: "30".into(),
            application_id: "40".into(),
            token: "private-interaction-token".into(),
            user_id: "50".into(),
            interaction_type: 2,
            name: "private-command-name".into(),
        };
        let frame = GatewayFrame::Dispatch(GatewayDispatch::InteractionCreate {
            sequence: 1,
            interaction: interaction.clone(),
        });
        let event = GatewayEvent {
            event_type: D7ITR,
            sequence: Some(1),
            description: "INTERACTION_CREATE".into(),
            opcode: None,
            retry_ms: None,
            data: GatewayEventData::Interaction(interaction.clone()),
        };
        let mut transport = MockGatewayTransport::default();
        transport
            .connect("ws://127.0.0.1/?private-url-secret")
            .unwrap();
        transport
            .send_text(r#"{"token":"private-bot-token"}"#)
            .unwrap();
        transport.push_incoming(r#"{"token":"private-interaction-token"}"#);
        let output = format!("{interaction:?} {frame:#?} {event:#?} {transport:#?}");
        for secret in [
            "private-interaction-token",
            "private-command-name",
            "private-bot-token",
            "private-url-secret",
        ] {
            assert!(!output.contains(secret), "{output}");
        }
        assert!(output.len() < 4096);
        assert_eq!(interaction.token, "private-interaction-token");
        assert!(transport.sent_texts()[0].contains("private-bot-token"));
    }
    use crate::snapshot::{SnapshotLookup, SnapshotStore};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn transport_rejects_token_exfiltration_destinations_before_connecting() {
        assert!(
            validate_gateway_url(&format!("wss://gateway.discord.gg/{}", "x".repeat(2048)))
                .is_err()
        );
        for url in [
            "ws://gateway.discord.gg",
            "ws://192.0.2.1",
            "wss://attacker.example",
            "wss://gateway.discord.gg.attacker.example",
            "wss://gateway.discord.gg:444",
        ] {
            let mut transport = TungsteniteGatewayTransport::new(100);
            assert_eq!(
                transport.connect(url),
                Err(GatewayTransportError::ConnectFailed),
                "{url}"
            );
        }
    }

    #[test]
    fn transport_handshake_cannot_exceed_its_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/gateway", listener.local_addr().unwrap());
        let (release, released) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_ws_http_request(&mut stream);
            let _ = released.recv_timeout(Duration::from_secs(2));
        });
        let started = std::time::Instant::now();
        let mut transport = TungsteniteGatewayTransport::new(50);
        assert_eq!(
            transport.connect(&url),
            Err(GatewayTransportError::ConnectFailed)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn legacy_handshake_checks_the_server_accept_key() {
        let headers =
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n";
        assert!(!websocket_handshake_ok(
            &format!("{headers}\r\n"),
            "test-key"
        ));
        assert!(!websocket_handshake_ok(
            &format!("{headers}Sec-WebSocket-Accept: wrong\r\n\r\n"),
            "test-key"
        ));
        let accept = tungstenite::handshake::derive_accept_key(b"test-key");
        assert!(websocket_handshake_ok(
            &format!("{headers}Sec-WebSocket-Accept: {accept}\r\n\r\n"),
            "test-key"
        ));
    }

    #[test]
    fn gateway_payloads_match_required_opcodes() {
        let payloads = GatewayPayloads::new(513, 0, 1);
        let identify = payloads.identify("secret");
        assert!(identify.contains("\"op\":2"));
        assert!(identify.contains("\"token\":\"secret\""));
        assert!(identify.contains("\"intents\":513"));
        assert!(identify.contains("\"compress\":false"));
        assert!(identify.contains("\"large_threshold\":50"));
        assert_eq!(
            payloads.resume("secret", "session", 42),
            "{\"op\":6,\"d\":{\"token\":\"secret\",\"session_id\":\"session\",\"seq\":42}}"
        );
        assert_eq!(GatewayPayloads::heartbeat(Some(7)), "{\"op\":1,\"d\":7}");
        assert_eq!(GatewayPayloads::heartbeat(None), "{\"op\":1,\"d\":null}");
    }

    #[test]
    fn gateway_outbound_limiter_allows_120_per_60_seconds() {
        let mut limiter = GatewayOutboundLimiter::new();
        for _ in 0..120 {
            assert!(
                limiter
                    .check_and_record(GatewaySendKind::Heartbeat, 10, 0)
                    .allowed
            );
        }
        let denied = limiter.check_and_record(GatewaySendKind::Heartbeat, 10, 0);
        assert!(!denied.allowed);
        assert_eq!(denied.why, "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED");
        assert!(
            limiter
                .check_and_record(GatewaySendKind::Heartbeat, 10, 60_000)
                .allowed
        );
    }

    #[test]
    fn gateway_outbound_limiter_reserves_heartbeat_slots() {
        let mut limiter = GatewayOutboundLimiter::new();
        limiter.configure_heartbeat_interval_ms(1000);
        assert_eq!(limiter.heartbeat_reserve(), 61);
        for _ in 0..58 {
            assert!(
                limiter
                    .check_and_record(GatewaySendKind::Other, 10, 0)
                    .allowed
            );
        }
        let denied = limiter.check_and_record(GatewaySendKind::Other, 10, 0);
        assert!(!denied.allowed);
        assert_eq!(denied.remaining, 62);
        assert_eq!(denied.why, "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED");
        assert!(
            limiter
                .check_and_record(GatewaySendKind::Identify, 10, 0)
                .allowed
        );
        assert!(
            limiter
                .check_and_record(GatewaySendKind::Heartbeat, 10, 0)
                .allowed
        );
    }

    #[test]
    fn gateway_outbound_limiter_lets_resume_use_reserved_slots() {
        let mut limiter = GatewayOutboundLimiter::new();
        limiter.configure_heartbeat_interval_ms(1);
        assert_eq!(limiter.heartbeat_reserve(), 120);
        assert!(
            !limiter
                .check_and_record(GatewaySendKind::Identify, 10, 0)
                .allowed
        );
        assert!(
            limiter
                .check_and_record(GatewaySendKind::Resume, 10, 0)
                .allowed
        );
    }

    #[test]
    fn gateway_outbound_limiter_reports_retry_after_for_resume_slots() {
        let mut limiter = GatewayOutboundLimiter::new();
        for _ in 0..120 {
            assert!(
                limiter
                    .check_and_record(GatewaySendKind::Resume, 10, 0)
                    .allowed
            );
        }
        let denied = limiter.check_and_record(GatewaySendKind::Resume, 10, 59_950);
        assert!(!denied.allowed);
        assert_eq!(denied.why, "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED");
        assert_eq!(denied.retry_after_ms, Some(50));
        assert!(
            limiter
                .check_and_record(GatewaySendKind::Resume, 10, 60_000)
                .allowed
        );
    }

    #[test]
    fn gateway_outbound_limiter_resets_per_connection() {
        let mut limiter = GatewayOutboundLimiter::new();
        limiter.configure_heartbeat_interval_ms(1000);
        assert_eq!(limiter.heartbeat_reserve(), 61);
        assert!(
            limiter
                .check_and_record(GatewaySendKind::Heartbeat, 10, 0)
                .allowed
        );
        limiter.reset_connection();
        assert_eq!(limiter.heartbeat_reserve(), 2);
        assert_eq!(limiter.remaining(), 120);
    }

    #[test]
    fn gateway_outbound_limiter_rejects_large_payload() {
        let mut limiter = GatewayOutboundLimiter::new();
        let denied =
            limiter.check_and_record(GatewaySendKind::Identify, MAX_GATEWAY_PAYLOAD_BYTES + 1, 0);
        assert!(!denied.allowed);
        assert_eq!(denied.why, "GATEWAY_PAYLOAD_TOO_LARGE");
    }

    #[test]
    fn daily_identify_budget_counts_only_sent_identify() {
        let mut budget = DailyIdentifyBudget::new(2, 3).unwrap();
        budget.record_identify_sent_at(0).unwrap();
        budget.record_identify_sent_at(1).unwrap();
        assert!(budget.should_warn_at(1));
        budget.record_identify_sent_at(2).unwrap();
        assert!(!budget.can_identify_at(2));
        assert_eq!(budget.remaining(), 0);
        assert!(budget.record_identify_sent_at(3).is_err());
    }

    #[test]
    fn daily_identify_budget_uses_rolling_window() {
        let mut budget = DailyIdentifyBudget::with_window(2, 3, 100).unwrap();
        budget.record_identify_sent_at(0).unwrap();
        budget.record_identify_sent_at(10).unwrap();
        budget.record_identify_sent_at(20).unwrap();
        assert!(!budget.can_identify_at(99));
        assert!(budget.can_identify_at(100));
        assert_eq!(budget.count(), 2);
        assert_eq!(budget.remaining(), 1);
        budget.record_identify_sent_at(100).unwrap();
        assert!(!budget.can_identify_at(109));
        assert!(budget.can_identify_at(110));
    }

    #[test]
    fn parses_gateway_bot_info_response() {
        let info = parse_gateway_bot_info(
            r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":42,"reset_after":60000,"max_concurrency":16}}"#,
        )
        .unwrap();
        assert_eq!(info.url, "wss://gateway.discord.gg");
        assert_eq!(
            info.session_start_limit,
            SessionStartLimit {
                total: 1000,
                remaining: 42,
                reset_after_ms: 60_000,
                max_concurrency: 16
            }
        );
    }

    #[test]
    fn gateway_bot_info_accepts_path_and_query_url() {
        let info = parse_gateway_bot_info(
            r#"{"url":"wss://gateway.discord.gg/?v=10&encoding=json","session_start_limit":{"total":1000,"remaining":42,"reset_after":60000,"max_concurrency":16}}"#,
        )
        .unwrap();
        assert_eq!(info.url, "wss://gateway.discord.gg/?v=10&encoding=json");

        let info = parse_gateway_bot_info(
            r#"{"url":"wss://[2001:db8::1]/?v=10","session_start_limit":{"total":1000,"remaining":42,"reset_after":60000,"max_concurrency":16}}"#,
        )
        .unwrap();
        assert_eq!(info.url, "wss://[2001:db8::1]/?v=10");
    }

    #[test]
    fn gateway_bot_info_rejects_invalid_gateway_urls() {
        for (url, expected) in [
            ("", "must not be empty"),
            ("https://gateway.discord.gg", "scheme"),
            ("wss://user@gateway.discord.gg", "credentials"),
            ("wss://gateway.discord.gg/#fragment", "fragment"),
            ("wss://gateway.discord.gg:0", "port"),
            ("wss://gateway.discord.gg:70000", "port"),
            ("wss://bad host", "unsafe"),
            ("wss://-gateway.discord.gg", "host"),
            ("wss://gateway..discord.gg", "host"),
            ("wss://::1:443", "host"),
            ("wss://[::1", "host"),
            ("wss://[::1]:0", "port"),
        ] {
            let body = format!(
                r#"{{"url":"{}","session_start_limit":{{"total":1000,"remaining":42,"reset_after":60000,"max_concurrency":16}}}}"#,
                url
            );
            let error = parse_gateway_bot_info(&body).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "{url} produced {error}"
            );
        }
    }

    #[test]
    fn gateway_manager_identifies_when_session_limit_available() {
        let mut manager = GatewayManager::new(513, 5, 8, 900, 999, 60_000, 8).unwrap();
        assert_eq!(manager.start(), GatewayAction::FetchGatewayBot);
        let action = manager
            .observe_gateway_bot_body(
                r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":1,"reset_after":60000,"max_concurrency":4}}"#,
            )
            .unwrap();
        assert_eq!(action, GatewayAction::SendIdentify);
        assert_eq!(manager.machine().state(), GatewayState::Identifying);
        assert_eq!(manager.identify_bucket(), Some(1));
        let payload = manager.identify_payload("secret").unwrap();
        assert!(payload.contains("\"op\":2"));
        assert_eq!(manager.daily_budget().count(), 0);
        assert_eq!(
            manager
                .gateway_bot_info()
                .unwrap()
                .session_start_limit
                .remaining,
            1
        );
        manager.record_identify_sent_at(0).unwrap();
        assert_eq!(manager.daily_budget().count(), 1);
        assert_eq!(
            manager
                .gateway_bot_info()
                .unwrap()
                .session_start_limit
                .remaining,
            0
        );
    }

    #[test]
    fn gateway_manager_stops_when_daily_identify_budget_is_exhausted() {
        let mut manager = GatewayManager::new(513, 0, 1, 1, 2, 60_000, 8).unwrap();
        let body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":3,"reset_after":60000,"max_concurrency":1}}"#;
        assert_eq!(
            manager.observe_gateway_bot_body_at(body, 0).unwrap(),
            GatewayAction::SendIdentify
        );
        manager.identify_payload("secret").unwrap();
        manager.record_identify_sent_at(0).unwrap();
        assert_eq!(
            manager.observe_gateway_bot_body_at(body, 5_000).unwrap(),
            GatewayAction::SendIdentify
        );
        manager.identify_payload("secret").unwrap();
        manager.record_identify_sent_at(5_000).unwrap();
        assert_eq!(
            manager.observe_gateway_bot_body_at(body, 10_000).unwrap(),
            GatewayAction::Fatal("DAILY_IDENTIFY_BUDGET_EXHAUSTED")
        );
        assert_eq!(manager.machine().state(), GatewayState::Fatal);
    }

    #[test]
    fn gateway_manager_serializes_identify_bucket_for_five_seconds() {
        let mut manager = GatewayManager::new(513, 0, 1, 900, 999, 60_000, 8).unwrap();
        let body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":3,"reset_after":60000,"max_concurrency":1}}"#;
        assert_eq!(
            manager.observe_gateway_bot_body_at(body, 0).unwrap(),
            GatewayAction::SendIdentify
        );
        manager.record_identify_sent_at(0).unwrap();
        assert_eq!(
            manager.observe_gateway_bot_body_at(body, 1_000).unwrap(),
            GatewayAction::Backoff { delay_ms: 4_000 }
        );
        assert_eq!(manager.machine().state(), GatewayState::Backoff);
        assert_eq!(
            manager.machine().backoff_info(),
            Some(GatewayBackoffInfo {
                reason: "IDENTIFY_BUCKET_SERIALIZED",
                next: GatewayBackoffNext::RefetchGatewayBot,
                delay_ms: 4_000
            })
        );
        assert_eq!(
            manager.observe_gateway_bot_body_at(body, 5_000).unwrap(),
            GatewayAction::SendIdentify
        );
        assert_eq!(manager.machine().state(), GatewayState::Identifying);
    }

    #[test]
    fn gateway_manager_waits_when_session_limit_resets_soon() {
        let mut manager = GatewayManager::new(513, 0, 1, 900, 999, 60_000, 8).unwrap();
        let action = manager
            .observe_gateway_bot_body(
                r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":0,"reset_after":5000,"max_concurrency":1}}"#,
            )
            .unwrap();
        assert_eq!(action, GatewayAction::Backoff { delay_ms: 5000 });
        assert_eq!(manager.machine().state(), GatewayState::Backoff);
        assert_eq!(
            manager.machine().backoff_info(),
            Some(GatewayBackoffInfo {
                reason: "IDENTIFY_LIMIT",
                next: GatewayBackoffNext::RefetchGatewayBot,
                delay_ms: 5000
            })
        );
        assert_eq!(manager.backoff_elapsed(), GatewayAction::FetchGatewayBot);
        assert_eq!(manager.machine().state(), GatewayState::FetchingUrl);
        assert_eq!(manager.machine().backoff_info(), None);
        assert_eq!(manager.daily_budget().count(), 0);
    }

    #[test]
    fn gateway_manager_fails_when_session_limit_wait_is_too_long() {
        let mut manager = GatewayManager::new(513, 0, 1, 900, 999, 60_000, 8).unwrap();
        let action = manager
            .observe_gateway_bot_body(
                r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":0,"reset_after":60001,"max_concurrency":1}}"#,
            )
            .unwrap();
        assert_eq!(action, GatewayAction::Fatal("IDENTIFY_LIMIT_EXHAUSTED"));
        assert_eq!(manager.machine().state(), GatewayState::Fatal);
    }

    #[test]
    fn ws_gateway_transport_connects_sends_and_receives_text() {
        let Some((url, received, server)) =
            serve_websocket(vec![r#"{"op":10,"d":{"heartbeat_interval":1000}}"#])
        else {
            return;
        };
        let mut transport = WsGatewayTransport::new(1000);
        transport.connect(&url).unwrap();
        transport.send_text(r#"{"op":2}"#).unwrap();
        let frame = transport.receive_text().unwrap();
        assert_eq!(
            frame.as_deref(),
            Some(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#)
        );
        let request = received.recv().unwrap();
        assert!(request.starts_with("GET /gateway HTTP/1.1"));
        assert!(request.contains("\r\nUpgrade: websocket\r\n"));
        assert!(request.contains("\r\nSec-WebSocket-Version: 13\r\n"));
        let sent_payload = received.recv().unwrap();
        assert_eq!(sent_payload, r#"{"op":2}"#);
        transport.close_gracefully().unwrap();
        let close = received.recv().unwrap();
        assert_eq!(close, "close:1000");
        server.join().unwrap();
    }

    #[test]
    fn ws_url_parser_preserves_gateway_query_targets() {
        let endpoint = parse_ws_url("ws://gateway.discord.gg:443?v=10&encoding=json").unwrap();
        assert_eq!(endpoint.host, "gateway.discord.gg");
        assert_eq!(endpoint.port, 443);
        assert_eq!(endpoint.path, "/?v=10&encoding=json");
        assert!(websocket_handshake_request(&endpoint, "abc")
            .starts_with("GET /?v=10&encoding=json HTTP/1.1\r\nHost: gateway.discord.gg:443\r\n"));

        let endpoint = parse_ws_url("ws://gateway.discord.gg:443/gateway?v=10").unwrap();
        assert_eq!(endpoint.path, "/gateway?v=10");
        assert!(websocket_handshake_request(&endpoint, "abc")
            .starts_with("GET /gateway?v=10 HTTP/1.1\r\n"));

        let endpoint = parse_ws_url("ws://[::1]:18080/gateway?v=10").unwrap();
        assert_eq!(endpoint.host, "::1");
        assert_eq!(endpoint.port, 18080);
        assert_eq!(endpoint.path, "/gateway?v=10");
        assert!(websocket_handshake_request(&endpoint, "abc")
            .starts_with("GET /gateway?v=10 HTTP/1.1\r\nHost: [::1]:18080\r\n"));

        let endpoint = parse_ws_url("ws://[2001:db8::1]/gateway").unwrap();
        assert_eq!(endpoint.host, "2001:db8::1");
        assert_eq!(endpoint.port, 80);
        assert!(websocket_handshake_request(&endpoint, "abc")
            .starts_with("GET /gateway HTTP/1.1\r\nHost: [2001:db8::1]:80\r\n"));
    }

    #[test]
    fn tungstenite_gateway_transport_connects_sends_and_receives_text() {
        let Some((url, received, server)) =
            serve_websocket(vec![r#"{"op":10,"d":{"heartbeat_interval":1000}}"#])
        else {
            return;
        };
        let mut transport = TungsteniteGatewayTransport::new(1000);
        transport.connect(&url).unwrap();
        transport.send_text(r#"{"op":2}"#).unwrap();
        let frame = transport.receive_text().unwrap();
        assert_eq!(
            frame.as_deref(),
            Some(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#)
        );
        let request = received.recv().unwrap();
        assert!(request.starts_with("GET /gateway HTTP/1.1"));
        assert!(request.contains("\r\nUpgrade: websocket\r\n"));
        assert!(request.contains("\r\nSec-WebSocket-Version: 13\r\n"));
        let sent_payload = received.recv().unwrap();
        assert_eq!(sent_payload, r#"{"op":2}"#);
        transport.close_gracefully().unwrap();
        let close = received.recv().unwrap();
        assert_eq!(close, "close:1000");
        server.join().unwrap();
    }

    #[test]
    fn tungstenite_gateway_transport_rejects_invalid_gateway_url() {
        let mut transport = TungsteniteGatewayTransport::new(1000);
        assert_eq!(
            transport.connect("wss://bad host").unwrap_err(),
            GatewayTransportError::ConnectFailed
        );
    }

    #[test]
    fn mock_gateway_transport_records_graceful_close() {
        let mut transport = MockGatewayTransport::default();
        transport.close_gracefully().unwrap();
        assert_eq!(transport.graceful_close_codes(), &[1000]);
        assert_eq!(transport.last_close_code(), Some(1000));
    }

    #[test]
    fn ws_gateway_transport_rejects_non_ws_url() {
        let mut transport = WsGatewayTransport::new(1000);
        assert_eq!(
            transport.connect("wss://gateway.discord.gg").unwrap_err(),
            GatewayTransportError::ConnectFailed
        );
    }

    #[test]
    fn websocket_close_code_payload_is_decoded() {
        assert_eq!(websocket_close_code_from_payload(&[0x0f, 0xa8]), Some(4008));
        assert_eq!(websocket_close_code_from_payload(&[]), None);
    }

    #[test]
    fn rejects_oversized_identifiers_before_expanding_guild_snapshots() {
        let identifier = "1".repeat(128 * 1024);
        let roles = (0..128)
            .map(|index| format!(r#"{{"id":"{}","permissions":"0","position":0}}"#, index + 1))
            .collect::<Vec<_>>()
            .join(",");
        let frame = format!(
            r#"{{"op":0,"s":2,"t":"GUILD_CREATE","d":{{"id":"{identifier}","roles":[{roles}]}}}}"#
        );
        assert!(parse_gateway_frame(&frame, 0)
            .unwrap_err()
            .to_string()
            .contains("id invalid"));
        let GatewayFrame::Dispatch(GatewayDispatch::Unknown { name, .. }) =
            parse_gateway_frame_tolerant(&frame, 0).unwrap()
        else {
            panic!("expected a rejected dispatch notification");
        };
        assert_eq!(name, "INVALID_GUILD_CREATE");
        let frame = format!(
            r#"{{"op":0,"s":2,"t":"THREAD_LIST_SYNC","d":{{"guild_id":"{identifier}","threads":[{{"id":"20","type":11}}]}}}}"#
        );
        assert!(parse_gateway_frame(&frame, 0).is_err());
        for data in [
            format!(
                r#"{{"session_id":"{}","resume_gateway_url":"wss://resume"}}"#,
                "x".repeat(513)
            ),
            format!(
                r#"{{"session_id":"s","resume_gateway_url":"wss://resume","user":{{"id":"{identifier}"}}}}"#
            ),
        ] {
            let frame = format!(r#"{{"op":0,"s":1,"t":"READY","d":{data}}}"#);
            assert!(parse_gateway_frame(&frame, 0).is_err());
            assert!(parse_gateway_frame_tolerant(&frame, 0).is_err());
        }
    }

    #[test]
    fn repeated_overflow_invalidates_snapshots_even_while_a_gap_is_pending() {
        let mut ingestor = GatewayIngestor::new(1, 1000, 1000, 10, 10);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume"}}"#, 0).unwrap();
        ingestor.pop_event();
        for sequence in 2..=5 {
            ingestor.ingest_json(&format!(r#"{{"op":0,"s":{sequence},"t":"MESSAGE_CREATE","d":{{"type":0,"guild_id":"10","channel_id":"20","id":"{sequence}","author":{{"id":"40"}},"content":"message"}}}}"#), sequence).unwrap();
            assert_eq!(
                ingestor.snapshot().message_count(),
                if sequence % 2 == 0 { 1 } else { 0 }
            );
        }
        assert_eq!(ingestor.machine().gap_epoch(), 2);
        assert_eq!(ingestor.pop_event().unwrap().event_type, D7GAP);
        assert!(ingestor.pop_event().is_none());
    }

    #[test]
    fn event_queue_coalesces_gap_and_preserves_one_delivery() {
        let mut queue = GatewayEventQueue::new(2);
        assert!(!queue.push_normal(GatewayEvent::message(1, "a")));
        assert!(!queue.push_normal(GatewayEvent::message(2, "b")));
        assert!(queue.push_normal(GatewayEvent::message(3, "c")));
        queue.push_control(GatewayEvent::gap("ANOTHER_GAP"));
        assert_eq!(queue.len(), 1);
        let event = queue.pop().unwrap();
        assert_eq!(event.event_type, D7GAP);
        assert_eq!(event.description, "QUEUE_FULL;ANOTHER_GAP");
        assert!(queue.pop().is_none());
    }

    #[test]
    fn event_queue_prioritizes_gap_over_rate_limited_and_normal_events() {
        let mut queue = GatewayEventQueue::new(2);
        assert!(!queue.push_normal(GatewayEvent::message(1, "normal")));
        queue.push_control(GatewayEvent::rate_limited(2, 1500));
        queue.push_control(GatewayEvent::gap("STATE_GAP"));

        assert_eq!(queue.pop().unwrap().event_type, D7GAP);
        assert_eq!(queue.pop().unwrap().event_type, D7GRL);
        assert!(queue.pop().is_none());
    }

    #[test]
    fn event_queue_drops_normal_events_from_before_gap() {
        let mut queue = GatewayEventQueue::new(4);
        assert!(!queue.push_normal(GatewayEvent::message(1, "before")));
        queue.push_control(GatewayEvent::gap("STATE_GAP"));
        assert!(!queue.push_normal(GatewayEvent::message(2, "after")));

        assert_eq!(queue.pop().unwrap().event_type, D7GAP);
        let event = queue.pop().unwrap();
        assert_eq!(event.event_type, D7MSG);
        assert_eq!(event.sequence, Some(2));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn event_queue_coalesces_rate_limited_by_opcode() {
        let mut queue = GatewayEventQueue::new(2);
        queue.push_control(GatewayEvent::rate_limited(2, 1500));
        queue.push_control(GatewayEvent::rate_limited(2, 2500));
        queue.push_control(GatewayEvent::rate_limited(6, 500));

        assert_eq!(queue.len(), 2);
        let first = queue.pop().unwrap();
        assert_eq!(first.event_type, D7GRL);
        assert_eq!(first.opcode, Some(2));
        assert_eq!(first.retry_ms, Some(2500));
        let second = queue.pop().unwrap();
        assert_eq!(second.event_type, D7GRL);
        assert_eq!(second.opcode, Some(6));
        assert_eq!(second.retry_ms, Some(500));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn event_queue_fixed_control_capacity_keeps_gap_deliverable() {
        let mut queue = GatewayEventQueue::new(2);
        for opcode in 0..8 {
            queue.push_control(GatewayEvent::rate_limited(opcode, 1000));
        }
        queue.push_control(GatewayEvent::gap("CONTROL_FULL"));

        assert_eq!(queue.len(), 8);
        let gap = queue.pop().unwrap();
        assert_eq!(gap.event_type, D7GAP);
        let remaining: Vec<i32> = (0..7)
            .map(|_| queue.pop().unwrap().opcode.unwrap())
            .collect();
        assert!(!remaining.contains(&0));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn state_machine_queue_full_requests_resume_and_keeps_gap() {
        let mut machine = GatewayStateMachine::new(1);
        machine.ready(1, "session", "wss://resume");
        let _ = machine.pop_event();
        let first = parse_gateway_frame(
            r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"30","author":{"id":"40"},"content":"one"}}"#,
            0,
        )
        .unwrap();
        assert_eq!(machine.handle_frame(first), GatewayAction::None);
        let second = parse_gateway_frame(
            r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"31","author":{"id":"40"},"content":"two"}}"#,
            0,
        )
        .unwrap();
        assert_eq!(
            machine.handle_frame(second),
            GatewayAction::SendResume { seq: 3 }
        );
        assert_eq!(machine.state(), GatewayState::Resuming);
        let event = machine.pop_event().unwrap();
        assert_eq!(event.event_type, D7GAP);
        assert_eq!(event.description, "QUEUE_FULL");
        assert!(machine.pop_event().is_none());
    }

    #[test]
    fn state_machine_identifies_then_becomes_ready() {
        let mut machine = GatewayStateMachine::new(8);
        assert_eq!(machine.start(), GatewayAction::FetchGatewayBot);
        assert_eq!(machine.state(), GatewayState::FetchingUrl);
        assert_eq!(machine.gateway_url_fetched(), GatewayAction::SendIdentify);
        machine.ready(10, "session", "wss://resume");
        assert_eq!(machine.state(), GatewayState::Ready);
        let event = machine.pop_event().unwrap();
        assert_eq!(event.event_type, D7RDY);
        assert_eq!(event.sequence, Some(10));
    }

    #[test]
    fn resume_session_json_roundtrips_and_restores_state() {
        let session = GatewayResumeSession::new(
            "session",
            "wss://resume.gateway",
            42,
            Some("100000000000000001".to_string()),
        )
        .unwrap();
        let json = session.to_json();
        assert_eq!(
            GatewayResumeSession::from_json(&json).unwrap(),
            GatewayResumeSession {
                session_id: "session".to_string(),
                resume_gateway_url: "wss://resume.gateway".to_string(),
                last_seen_seq: 42,
                bot_user_id: Some("100000000000000001".to_string()),
            }
        );

        let mut machine = GatewayStateMachine::new(2);
        let action = machine.restore_resume_session(session).unwrap();
        assert_eq!(action, GatewayAction::SendResume { seq: 42 });
        assert_eq!(machine.state(), GatewayState::Resuming);
        assert_eq!(machine.session_id(), Some("session"));
        assert_eq!(machine.resume_gateway_url(), Some("wss://resume.gateway"));
        assert_eq!(machine.last_seen_seq(), Some(42));
        assert_eq!(machine.bot_user_id(), Some("100000000000000001"));
    }

    #[test]
    fn resume_session_rejects_unsafe_values() {
        assert!(GatewayResumeSession::new("bad session", "wss://resume", 1, None).is_err());
        assert!(GatewayResumeSession::new("session", "https://resume", 1, None).is_err());
        assert!(GatewayResumeSession::new("session", "wss://resume", -1, None).is_err());
        assert!(
            GatewayResumeSession::new("session", "wss://resume", 1, Some("abc".to_string()))
                .is_err()
        );
    }

    #[test]
    fn reconnect_opcode_resumes_without_gap_when_possible() {
        let mut machine = GatewayStateMachine::new(8);
        machine.ready(42, "session", "wss://resume");
        let action = machine.reconnect_opcode();
        assert_eq!(action, GatewayAction::SendResume { seq: 42 });
        assert_eq!(machine.state(), GatewayState::Resuming);
        assert!(machine.pop_event().is_some());
        assert!(machine.pop_event().is_none());
    }

    #[test]
    fn invalidated_sessions_cannot_be_exported_or_automatically_resumed() {
        for cause in 0..14 {
            let mut machine = GatewayStateMachine::new(16);
            machine.ready_with_bot_user(10, "old", "wss://resume", Some("99".into()));
            machine.pop_event();
            machine.dispatch(12, "pending");
            match cause {
                0 => {
                    machine.invalid_session(false);
                }
                1 => {
                    machine.resume_failed("GATEWAY_SEND_FAILED");
                }
                2 => {
                    machine.close(Some(4003), false);
                }
                3 => {
                    machine.close(Some(4007), false);
                }
                4 => {
                    machine.close(Some(4009), false);
                }
                5 => {
                    machine.close(Some(1000), false);
                }
                6 => {
                    machine.close(Some(1001), false);
                }
                7 => {
                    machine.close(Some(4004), false);
                }
                8 => {
                    machine.close(Some(4014), false);
                }
                9 => {
                    machine.close(Some(1000), true);
                }
                10 => {
                    for _ in 0..3 {
                        machine.close(Some(4001), false);
                    }
                }
                11 => {
                    machine.start();
                }
                12 => {
                    machine.gateway_url_fetched();
                }
                _ => {
                    machine.backoff_shutdown("SHUTDOWN", 1);
                    machine.backoff_elapsed();
                }
            }
            assert!(machine.resume_session_invalidated(), "cause={cause}");
            assert!(machine.resume_session().is_none(), "cause={cause}");
            assert_eq!(machine.session_id(), None);
            assert_eq!(machine.resume_gateway_url(), None);
            assert_eq!(machine.last_seen_seq(), None);
            assert_eq!(machine.bot_user_id(), None);
            assert_ne!(
                machine.reconnect_opcode(),
                GatewayAction::SendResume { seq: 12 }
            );
        }
    }

    #[test]
    fn late_dispatches_cannot_repopulate_state_after_session_invalidation() {
        let mut ingestor = session_ingestor();
        ingestor.ingest_json(r#"{"op":9,"d":false}"#, 3).unwrap();
        while ingestor.pop_event().is_some() {}
        assert_eq!(ingestor.snapshot().message_count(), 0);
        ingestor.ingest_json(r#"{"op":0,"s":13,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"31","author":{"id":"40"},"content":"late","type":0}}"#, 4).unwrap();
        ingestor
            .ingest_json(r#"{"op":0,"s":14,"t":"RESUMED","d":{}}"#, 5)
            .unwrap();
        assert_eq!(ingestor.snapshot().message_count(), 0);
        assert!(ingestor.pop_event().is_none());
        assert_ne!(ingestor.machine().state(), GatewayState::Ready);
        assert_eq!(ingestor.machine().last_seen_seq(), None);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"new","resume_gateway_url":"wss://new","user":{"id":"99"}}}"#, 6).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"32","author":{"id":"40"},"content":"new","type":0}}"#, 7).unwrap();
        assert_eq!(ingestor.machine().state(), GatewayState::Ready);
        assert_eq!(ingestor.snapshot().message_count(), 1);
        assert_eq!(ingestor.machine().last_seen_seq(), Some(2));
    }

    #[test]
    fn application_context_can_arrive_later_without_losing_the_webhook_origin() {
        let mut ingestor = session_ingestor();
        let gap = ingestor.machine().gap_epoch();
        ingestor.ingest_json(r#"{"op":0,"s":13,"t":"MESSAGE_CREATE","d":{"id":"34","guild_id":"10","channel_id":"20","type":0,"author":{"id":"40"},"webhook_id":"50","content":"original","attachments":[]}}"#, 3).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":14,"t":"MESSAGE_UPDATE","d":{"id":"34","channel_id":"20","application_id":"60","content":"updated"}}"#, 4).unwrap();
        assert_eq!(ingestor.machine().gap_epoch(), gap);
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "34", 4),
            SnapshotLookup::Found(_)
        ));
        ingestor
            .snapshot_mut()
            .invalidate_unresolved_webhook_messages("60", 4);
        assert_eq!(
            ingestor.snapshot_mut().get_message("20", "34", 4),
            SnapshotLookup::Missing
        );
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "30", 4),
            SnapshotLookup::Found(_)
        ));
    }

    #[test]
    fn webhook_origins_ignore_replays_preserve_partial_fields_and_reject_identity_changes() {
        for conflict in [false, true] {
            let mut ingestor = session_ingestor();
            let before_gap = ingestor.machine().gap_epoch();
            ingestor.ingest_json(r#"{"op":0,"s":13,"t":"MESSAGE_CREATE","d":{"id":"34","guild_id":"10","channel_id":"20","type":0,"author":{"id":"40"},"webhook_id":"50","application_id":"60","content":"original","attachments":[]}}"#, 3).unwrap();
            ingestor.ingest_json(r#"{"op":0,"s":13,"t":"MESSAGE_CREATE","d":{"id":"34","guild_id":"10","channel_id":"20","type":0,"author":{"id":"40"},"webhook_id":"51","application_id":"61","content":"replayed","attachments":[]}}"#, 4).unwrap();
            ingestor.ingest_json(r#"{"op":0,"s":14,"t":"MESSAGE_UPDATE","d":{"id":"34","channel_id":"20","application_id":"60","content":"partial"}}"#, 5).unwrap();
            assert_eq!(ingestor.machine().gap_epoch(), before_gap);
            let SnapshotLookup::Found(message) = ingestor.snapshot_mut().get_message("20", "34", 5)
            else {
                panic!("expected cached webhook message");
            };
            assert_eq!(message.content, "partial");
            if conflict {
                ingestor.ingest_json(r#"{"op":0,"s":15,"t":"MESSAGE_UPDATE","d":{"id":"34","channel_id":"20","webhook_id":"51","content":"conflicting"}}"#, 6).unwrap();
                assert!(ingestor.machine().gap_epoch() > before_gap);
                assert_eq!(ingestor.snapshot().message_count(), 0);
            } else {
                ingestor
                    .snapshot_mut()
                    .invalidate_unresolved_webhook_messages("50", 6);
                assert_eq!(
                    ingestor.snapshot_mut().get_message("20", "34", 6),
                    SnapshotLookup::Missing
                );
                assert!(matches!(
                    ingestor.snapshot_mut().get_message("20", "30", 6),
                    SnapshotLookup::Found(_)
                ));
            }
        }
    }

    #[test]
    fn invalid_session_false_generates_gap_and_identifies() {
        let mut machine = GatewayStateMachine::new(8);
        machine.ready(42, "session", "wss://resume");
        let _ = machine.pop_event();
        let action = machine.invalid_session(false);
        assert_eq!(action, GatewayAction::Backoff { delay_ms: 1_000 });
        assert_eq!(machine.state(), GatewayState::Backoff);
        assert_eq!(
            machine.backoff_info(),
            Some(GatewayBackoffInfo {
                reason: "INVALID_SESSION_NOT_RESUMABLE",
                next: GatewayBackoffNext::NewIdentify,
                delay_ms: 1_000
            })
        );
        let event = machine.pop_event().unwrap();
        assert_eq!(event.event_type, D7GAP);
        assert_eq!(event.description, "INVALID_SESSION_NOT_RESUMABLE");
        assert_eq!(machine.backoff_elapsed(), GatewayAction::FetchGatewayBot);
        assert_eq!(machine.state(), GatewayState::FetchingUrl);
        assert_eq!(machine.backoff_info(), None);
    }

    #[test]
    fn dispatch_order_rejects_duplicates_and_keeps_the_latest_resume_cursor() {
        let mut machine = GatewayStateMachine::new(16);
        machine.ready(42, "session", "wss://resume");
        machine.pop_event();
        assert!(machine.dispatch(44, "new"));
        assert!(!machine.dispatch(44, "duplicate"));
        assert!(!machine.dispatch(43, "old"));
        assert!(!machine.observe_dispatch(-1));
        assert_eq!(machine.last_seen_seq(), Some(44));
        assert_eq!(
            machine.reconnect_opcode(),
            GatewayAction::SendResume { seq: 44 }
        );
        assert!(!machine.dispatch(1, "old replay"));
        assert_eq!(machine.last_seen_seq(), Some(44));
        assert!(machine.dispatch(45, "first replay"));
        assert!(!machine.dispatch(45, "duplicate replay"));
        assert_eq!(
            machine.reconnect_opcode(),
            GatewayAction::SendResume { seq: 45 }
        );
        assert_eq!(machine.pop_event().unwrap().sequence, Some(44));
        assert_eq!(machine.pop_event().unwrap().sequence, Some(45));
        assert!(machine.pop_event().is_none());
    }

    #[test]
    fn duplicate_ready_and_resume_completion_do_not_rewind_the_session() {
        let mut machine = GatewayStateMachine::new(16);
        machine.ready_with_bot_user(1, "session", "wss://resume", Some("99".into()));
        machine.pop_event();
        assert!(machine.observe_dispatch(42));
        machine.ready_with_bot_user(1, "session", "wss://other", Some("88".into()));
        assert_eq!(machine.last_seen_seq(), Some(42));
        assert_eq!(machine.bot_user_id(), Some("99"));
        assert_eq!(machine.resume_gateway_url(), Some("wss://resume"));
        assert!(machine.pop_event().is_none());
        assert_eq!(
            machine.reconnect_opcode(),
            GatewayAction::SendResume { seq: 42 }
        );
        assert!(machine.observe_dispatch(43));
        machine.resumed_event(Some(41));
        assert_eq!(machine.state(), GatewayState::Ready);
        assert_eq!(machine.last_seen_seq(), Some(43));
        assert!(machine.pop_event().is_none());
        assert_eq!(
            machine.reconnect_opcode(),
            GatewayAction::SendResume { seq: 43 }
        );
        machine.resumed_event(Some(43));
        assert_eq!(machine.state(), GatewayState::Ready);
        assert_eq!(machine.pop_event().unwrap().sequence, Some(43));
        machine.resumed_event(Some(43));
        assert!(machine.pop_event().is_none());
        machine.resumed_event(None);
        assert_eq!(machine.last_seen_seq(), Some(43));
        assert_eq!(machine.pop_event().unwrap().sequence, None);
        machine.ready_with_bot_user(-1, "invalid", "wss://invalid", None);
        assert_eq!(machine.session_id(), Some("session"));
        machine.ready(1, "new-session", "wss://new");
        assert_eq!(machine.session_id(), Some("new-session"));
        assert_eq!(machine.last_seen_seq(), Some(1));
        assert!(machine.observe_dispatch(2));
    }

    #[test]
    fn replayed_role_updates_cannot_restore_revoked_administrator_permissions() {
        let mut ingestor = GatewayIngestor::new(32, 1000, 1000, 100, 100);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}]}}"#, 1).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":3,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":"0","position":1}}}"#, 2).unwrap();
        while ingestor.pop_event().is_some() {}
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":"8","position":1}}}"#, 3).unwrap();
        let SnapshotLookup::Found(roles) = ingestor.snapshot_mut().get_guild_roles("10", 3) else {
            panic!("expected the current role snapshot");
        };
        assert!(roles.complete);
        assert!(!roles
            .roles
            .iter()
            .find(|role| role.role_id == "50")
            .unwrap()
            .permissions
            .has(3));
        assert_eq!(ingestor.machine().last_seen_seq(), Some(3));
        assert!(ingestor.pop_event().is_none());
        assert_eq!(ingestor.machine().gap_epoch(), 0);
    }

    #[test]
    fn malformed_dispatch_sequences_are_rejected_before_tolerant_normalisation() {
        for sequence in ["-1", "1.5", r#""42""#, "true", "9223372036854775808"] {
            for name in ["READY", "RESUMED", "GUILD_ROLE_UPDATE", "FUTURE_EVENT"] {
                let frame = format!(r#"{{"op":0,"s":{sequence},"t":"{name}","d":{{}}}}"#);
                assert!(parse_gateway_frame(&frame, 0).is_err(), "{frame}");
                assert!(parse_gateway_frame_tolerant(&frame, 0).is_err(), "{frame}");
            }
        }
        for frame in [
            r#"{"op":0,"s":null,"t":"RESUMED","d":{}}"#,
            r#"{"op":0,"t":"FUTURE_EVENT","d":{}}"#,
            r#"{"op":0,"s":9223372036854775807,"t":"FUTURE_EVENT","d":{}}"#,
        ] {
            assert!(parse_gateway_frame(frame, 0).is_ok());
            assert!(parse_gateway_frame_tolerant(frame, 0).is_ok());
        }
    }

    #[test]
    fn rejected_wire_frames_invalidate_cached_permissions_without_advancing_the_cursor() {
        for tolerant in [false, true] {
            for frame in [
                r#"{"op":0,"s":-1,"t":"GUILD_ROLE_UPDATE","d":{}}"#,
                r#"{"op":0,"s":"3","t":"GUILD_ROLE_UPDATE","d":{}}"#,
                r#"{"op":0,"s":9223372036854775808,"t":"GUILD_ROLE_UPDATE","d":{}}"#,
                r#"{"op":0,"s":3,"t":"GUILD_ROLE_UPDATE","d":{"#,
            ] {
                let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 100, 100);
                ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
                ingestor.ingest_json(r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","roles":[{"id":"10","permissions":"8","position":0}]}}"#, 0).unwrap();
                while ingestor.pop_event().is_some() {}
                let result = if tolerant {
                    ingestor.ingest_json_tolerant(frame, 1)
                } else {
                    ingestor.ingest_json(frame, 1)
                };
                assert!(result.is_err());
                assert_eq!(ingestor.machine().last_seen_seq(), Some(2));
                assert_eq!(ingestor.machine().gap_epoch(), 1);
                assert!(matches!(
                    ingestor.snapshot_mut().get_guild_roles("10", 1),
                    SnapshotLookup::Missing
                ));
                let gap = ingestor.pop_event().unwrap();
                assert_eq!(gap.event_type, D7GAP);
                assert_eq!(gap.description, "INVALID_GATEWAY_FRAME");
                assert!(ingestor.pop_event().is_none());
            }
        }
    }

    #[test]
    fn resume_success_discards_duplicate_dispatches() {
        let mut machine = GatewayStateMachine::new(8);
        machine.ready(42, "session", "wss://resume");
        let _ = machine.pop_event();
        assert_eq!(
            machine.close(Some(4008), false),
            GatewayAction::Backoff { delay_ms: 1_000 }
        );
        assert_eq!(
            machine.backoff_elapsed(),
            GatewayAction::SendResume { seq: 42 }
        );
        machine.resume_succeeded();
        assert!(!machine.dispatch(41, "old"));
        assert!(!machine.dispatch(42, "same"));
        assert!(machine.dispatch(43, "new"));
        let event = machine.pop_event().unwrap();
        assert_eq!(event.event_type, D7MSG);
        assert_eq!(event.sequence, Some(43));
    }

    #[test]
    fn resume_failed_generates_gap_and_backs_off_to_new_identify() {
        let mut machine = GatewayStateMachine::new(8);
        machine.ready(42, "session", "wss://resume");
        let _ = machine.pop_event();
        assert_eq!(
            machine.resume_failed("GATEWAY_SEND_FAILED"),
            GatewayAction::Backoff { delay_ms: 1_000 }
        );
        assert_eq!(machine.state(), GatewayState::Backoff);
        let event = machine.pop_event().unwrap();
        assert_eq!(event.event_type, D7GAP);
        assert_eq!(event.description, "GATEWAY_SEND_FAILED");
        assert_eq!(machine.backoff_elapsed(), GatewayAction::FetchGatewayBot);
        assert_eq!(machine.state(), GatewayState::FetchingUrl);
    }

    #[test]
    fn close_session_invalidating_codes_backoff_then_new_identify() {
        for code in [4003, 4007, 4009] {
            let mut machine = GatewayStateMachine::new(8);
            machine.ready(42, "session", "wss://resume");
            let _ = machine.pop_event();
            assert_eq!(
                machine.close(Some(code), false),
                GatewayAction::Backoff { delay_ms: 1_000 }
            );
            assert_eq!(machine.state(), GatewayState::Backoff);
            let event = machine.pop_event().unwrap();
            assert_eq!(event.event_type, D7GAP);
            assert_eq!(event.description, "GATEWAY_SESSION_INVALIDATED");
            assert_eq!(machine.backoff_elapsed(), GatewayAction::FetchGatewayBot);
            assert_eq!(machine.state(), GatewayState::FetchingUrl);
        }
    }

    #[test]
    fn close_4008_without_resume_state_starts_new_identify() {
        let mut machine = GatewayStateMachine::new(8);
        assert_eq!(
            machine.close(Some(4008), false),
            GatewayAction::Backoff { delay_ms: 1_000 }
        );
        assert_eq!(machine.state(), GatewayState::Backoff);
        assert!(machine.pop_event().is_none());
        assert_eq!(machine.backoff_elapsed(), GatewayAction::FetchGatewayBot);
        assert_eq!(machine.state(), GatewayState::FetchingUrl);
    }

    #[test]
    fn close_4000_without_resume_state_starts_new_identify() {
        let mut machine = GatewayStateMachine::new(8);
        assert_eq!(
            machine.close(Some(4000), false),
            GatewayAction::Backoff { delay_ms: 1_000 }
        );
        assert_eq!(machine.state(), GatewayState::Backoff);
        assert!(machine.pop_event().is_none());
        assert_eq!(machine.backoff_elapsed(), GatewayAction::FetchGatewayBot);
        assert_eq!(machine.state(), GatewayState::FetchingUrl);
    }

    #[test]
    fn close_4000_with_resume_state_backs_off_then_resumes() {
        let mut machine = GatewayStateMachine::new(8);
        machine.ready(42, "session", "wss://resume");
        let _ = machine.pop_event();
        assert_eq!(
            machine.close(Some(4000), false),
            GatewayAction::Backoff { delay_ms: 1_000 }
        );
        assert_eq!(machine.state(), GatewayState::Backoff);
        assert_eq!(
            machine.backoff_info(),
            Some(GatewayBackoffInfo {
                reason: "GATEWAY_CLOSED",
                next: GatewayBackoffNext::Resume,
                delay_ms: 1_000
            })
        );
        assert!(machine.pop_event().is_none());
        assert_eq!(
            machine.backoff_elapsed(),
            GatewayAction::SendResume { seq: 42 }
        );
        assert_eq!(machine.state(), GatewayState::Resuming);
    }

    #[test]
    fn close_4008_repeated_backoff_uses_exponential_jitter() {
        let mut machine = GatewayStateMachine::new(8);
        machine.ready(42, "session", "wss://resume");
        let _ = machine.pop_event();
        assert_eq!(
            machine.close_at(Some(4008), false, 1_000),
            GatewayAction::Backoff { delay_ms: 1_000 }
        );
        assert_eq!(
            machine.backoff_elapsed(),
            GatewayAction::SendResume { seq: 42 }
        );
        machine.resume_succeeded();
        let second = machine.close_at(Some(4008), false, 30_000);
        let GatewayAction::Backoff { delay_ms } = second else {
            panic!("expected 4008 backoff");
        };
        assert!((1_000..=3_000).contains(&delay_ms));
        assert_eq!(
            machine.backoff_info(),
            Some(GatewayBackoffInfo {
                reason: "RATE_LIMITED_4008",
                next: GatewayBackoffNext::Resume,
                delay_ms
            })
        );
    }

    #[test]
    fn bug_close_codes_backoff_then_fatal_on_third_within_sixty_seconds() {
        let mut machine = GatewayStateMachine::new(8);
        machine.ready(42, "session", "wss://resume");
        let _ = machine.pop_event();
        assert_eq!(
            machine.close_at(Some(4001), false, 1_000),
            GatewayAction::Backoff { delay_ms: 1_000 }
        );
        assert_eq!(
            machine.backoff_elapsed(),
            GatewayAction::SendResume { seq: 42 }
        );
        machine.resume_succeeded();
        let second = machine.close_at(Some(4002), false, 30_000);
        let GatewayAction::Backoff { delay_ms } = second else {
            panic!("expected bug close backoff");
        };
        assert!((1_000..=3_000).contains(&delay_ms));
        machine.resume_succeeded();
        assert_eq!(
            machine.close_at(Some(4005), false, 60_500),
            GatewayAction::Fatal("GATEWAY_BUG_CLOSE_REPEATED")
        );
        assert_eq!(machine.state(), GatewayState::Fatal);
    }

    #[test]
    fn bug_close_counter_uses_sixty_second_window() {
        let mut machine = GatewayStateMachine::new(8);
        machine.ready(42, "session", "wss://resume");
        let _ = machine.pop_event();
        assert_eq!(
            machine.close_at(Some(4001), false, 0),
            GatewayAction::Backoff { delay_ms: 1_000 }
        );
        machine.resume_succeeded();
        let second = machine.close_at(Some(4001), false, 30_000);
        let GatewayAction::Backoff { delay_ms } = second else {
            panic!("expected second bug close backoff");
        };
        assert!((1_000..=3_000).contains(&delay_ms));
        machine.resume_succeeded();
        let third = machine.close_at(Some(4001), false, 61_000);
        let GatewayAction::Backoff { delay_ms } = third else {
            panic!("expected third bug close backoff");
        };
        assert!((1_000..=3_000).contains(&delay_ms));
        assert_eq!(machine.state(), GatewayState::Backoff);
    }

    #[test]
    fn explicit_shutdown_does_not_generate_gap() {
        let mut machine = GatewayStateMachine::new(8);
        machine.ready(42, "session", "wss://resume");
        let _ = machine.pop_event();
        assert_eq!(machine.close(Some(1000), true), GatewayAction::None);
        assert_eq!(machine.state(), GatewayState::Closed);
        assert!(machine.pop_event().is_none());
    }

    #[test]
    fn backoff_shutdown_records_next_action_and_closes_on_elapsed() {
        let mut machine = GatewayStateMachine::new(8);
        assert_eq!(
            machine.backoff_shutdown("GATEWAY_SHUTDOWN", 250),
            GatewayAction::Backoff { delay_ms: 250 }
        );
        assert_eq!(
            machine.backoff_info(),
            Some(GatewayBackoffInfo {
                reason: "GATEWAY_SHUTDOWN",
                next: GatewayBackoffNext::Shutdown,
                delay_ms: 250
            })
        );
        assert_eq!(machine.backoff_elapsed(), GatewayAction::None);
        assert_eq!(machine.state(), GatewayState::Closed);
        assert_eq!(machine.backoff_info(), None);
    }

    #[test]
    fn discord_rate_limited_event_is_control_event() {
        let mut machine = GatewayStateMachine::new(8);
        machine.discord_rate_limited(2, 1500);
        let event = machine.pop_event().unwrap();
        assert_eq!(event.event_type, D7GRL);
        assert_eq!(event.opcode, Some(2));
        assert_eq!(event.retry_ms, Some(1500));
    }

    #[test]
    fn heartbeat_ack_deadline_survives_additional_sends_and_expires_at_the_interval() {
        let mut heartbeat = HeartbeatTracker::new(1000);
        heartbeat.hello(0);
        heartbeat.mark_sent(100);
        heartbeat.mark_sent(600);
        heartbeat.mark_sent(1099);
        assert!(!heartbeat.missed_ack(1099));
        assert!(heartbeat.missed_ack(1100));
        heartbeat.ack(1100);
        assert!(!heartbeat.missed_ack(3000));
        heartbeat.mark_sent(3000);
        assert!(!heartbeat.missed_ack(3999));
        assert!(heartbeat.missed_ack(4000));
    }

    #[test]
    fn heartbeat_tracker_tracks_due_ack_and_missed_ack() {
        let mut heartbeat = HeartbeatTracker::new(1000);
        heartbeat.hello(100);
        let initial_due = heartbeat.next_due_ms.unwrap();
        assert!((100..=1100).contains(&initial_due));
        if initial_due > 100 {
            assert!(!heartbeat.due(initial_due - 1));
        }
        assert!(heartbeat.due(initial_due));
        heartbeat.mark_sent(initial_due);
        assert_eq!(heartbeat.next_due_ms, Some(initial_due + 1000));
        assert!(!heartbeat.missed_ack(initial_due + 900));
        assert!(heartbeat.missed_ack(initial_due + 1001));
        heartbeat.ack(initial_due + 1010);
        assert_eq!(heartbeat.last_ack_at_ms(), Some(initial_due + 1010));
        assert!(!heartbeat.missed_ack(initial_due + 2000));
    }

    #[test]
    fn parses_ready_gateway_dispatch() {
        let frame = parse_gateway_frame(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            GatewayFrame::Dispatch(GatewayDispatch::Ready {
                sequence: 1,
                session_id: "abc".to_string(),
                resume_gateway_url: "wss://resume".to_string(),
                bot_user_id: Some("99".to_string())
            })
        );
    }

    #[test]
    fn rejects_ready_dispatch_with_invalid_resume_gateway_url() {
        let error = parse_gateway_frame(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"https://resume","user":{"id":"99"}}}"#,
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("scheme"));
    }

    #[test]
    fn tolerant_parser_keeps_ready_strict_but_downgrades_other_dispatch_errors() {
        let ready_error = parse_gateway_frame_tolerant(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"https://resume","user":{"id":"99"}}}"#,
            0,
        )
        .unwrap_err();
        assert!(ready_error.to_string().contains("scheme"));

        let frame =
            parse_gateway_frame_tolerant(r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":123}}"#, 0)
                .unwrap();
        assert_eq!(
            frame,
            GatewayFrame::Dispatch(GatewayDispatch::Unknown {
                sequence: Some(2),
                name: "INVALID_GUILD_CREATE".to_string(),
            })
        );

        let strict_error =
            parse_gateway_frame(r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":123}}"#, 0)
                .unwrap_err();
        assert!(strict_error.to_string().contains("id"));
    }

    #[test]
    fn parses_resumed_gateway_dispatch() {
        let frame = parse_gateway_frame(r#"{"op":0,"s":9,"t":"RESUMED","d":{}}"#, 0).unwrap();
        assert_eq!(
            frame,
            GatewayFrame::Dispatch(GatewayDispatch::Resumed { sequence: Some(9) })
        );
    }

    #[test]
    fn parses_message_create_and_updates_snapshot() {
        let frame = parse_gateway_frame(
            r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","content":"hello","type":0,"attachments":[{"filename":"a.png","content_type":"image/png"},{"filename":"b.txt","content_type":"text/plain"},{"filename":"c.webp"}],"author":{"id":"40","bot":false},"member":{"roles":["50","60"],"permissions":"8"}}}"#,
            1234,
        )
        .unwrap();
        let GatewayFrame::Dispatch(dispatch) = frame else {
            panic!("expected dispatch");
        };
        let GatewayDispatch::MessageCreate { sequence, message } = &dispatch else {
            panic!("expected message create");
        };
        assert_eq!(*sequence, 2);
        assert_eq!(message.guild_id, "10");
        assert_eq!(message.role_ids, vec!["50", "60"]);
        assert_eq!(message.image_attachment_count, 2);
        assert!(message
            .member_permissions
            .as_ref()
            .is_some_and(|permissions| permissions.has(crate::permission::BIT_ADMINISTRATOR)));

        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        apply_dispatch_to_snapshot(&dispatch, &mut store, 1234);
        assert!(matches!(
            store.get_message("20", "30", 1234),
            SnapshotLookup::Found(_)
        ));
    }

    #[test]
    fn message_snapshot_channel_allowlist_keeps_event_but_skips_snapshot() {
        let mut ingestor = GatewayIngestor::new(10, 1000, 1000, 10, 10);
        ingestor.set_message_snapshot_channel_allowlist(vec!["20".to_string()]);

        let action = ingestor
            .ingest_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"21","id":"30","content":"skip","type":0,"author":{"id":"40","bot":false}}}"#,
                100,
            )
            .unwrap();
        assert_eq!(action, GatewayAction::None);
        let event = ingestor.pop_event().expect("event is still delivered");
        assert_eq!(event.event_type, D7MSG);
        let GatewayEventData::Message(message) = event.data else {
            panic!("expected message event");
        };
        assert_eq!(message.channel_id, "21");
        assert!(matches!(
            ingestor.snapshot_mut().get_message("21", "30", 100),
            SnapshotLookup::Missing
        ));

        ingestor
            .ingest_json(
                r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"31","content":"keep","type":0,"author":{"id":"40","bot":false}}}"#,
                110,
            )
            .unwrap();
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "31", 110),
            SnapshotLookup::Found(_)
        ));
    }

    #[test]
    fn rejects_message_create_type_outside_i32_range() {
        let error = parse_gateway_frame(
            r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","content":"hello","type":2147483648,"author":{"id":"40"}}}"#,
            1234,
        )
        .unwrap_err();
        assert!(error.to_string().contains("type too large"));
    }

    #[test]
    fn rejects_gateway_rate_limited_opcode_outside_i32_range() {
        let error = parse_gateway_frame(
            r#"{"op":0,"s":null,"t":"GATEWAY_RATE_LIMITED","d":{"opcode":2147483648,"retry_ms":1500}}"#,
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("opcode too large"));
    }

    #[test]
    fn direct_messages_channels_and_reactions_do_not_require_a_guild() {
        let mut ingestor = GatewayIngestor::new(8, 1000, 1000, 10, 10);
        for frame in [
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#,
            r#"{"op":0,"s":2,"t":"CHANNEL_CREATE","d":{"id":"20","type":1}}"#,
            r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"channel_id":"20","id":"30","content":"dm","type":0,"author":{"id":"40"}}}"#,
            r#"{"op":0,"s":4,"t":"MESSAGE_REACTION_ADD","d":{"channel_id":"20","message_id":"30","user_id":"40","emoji":{"name":"ok"}}}"#,
        ] {
            ingestor.ingest_json(frame, 100).unwrap();
        }
        assert_eq!(ingestor.pop_event().unwrap().event_type, D7RDY);
        assert_eq!(ingestor.pop_event().unwrap().event_type, D7ECC);
        let message = ingestor.pop_event().unwrap();
        assert_eq!(message.event_type, D7MSG);
        let GatewayEventData::Message(message) = message.data else {
            panic!("expected message")
        };
        assert!(message.guild_id.is_empty());
        assert_eq!(message.content, "dm");
        assert_eq!(ingestor.pop_event().unwrap().event_type, D7REA);
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "30", 100),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            ingestor.snapshot_mut().get_channel("20", 100),
            SnapshotLookup::Found(_)
        ));
        assert!(parse_gateway_frame(
            r#"{"op":0,"s":5,"t":"CHANNEL_CREATE","d":{"id":"21","type":0}}"#,
            100
        )
        .is_err());
    }

    #[test]
    fn parses_message_update_and_patches_snapshot() {
        let create = parse_gateway_frame(
            r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","content":"hello","type":21,"author":{"id":"40","bot":false}}}"#,
            100,
        )
        .unwrap();
        let update = parse_gateway_frame(
            r#"{"op":0,"s":3,"t":"MESSAGE_UPDATE","d":{"channel_id":"20","id":"30","content":"edited"}}"#,
            110,
        )
        .unwrap();
        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        if let GatewayFrame::Dispatch(dispatch) = create {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 100);
        }
        if let GatewayFrame::Dispatch(dispatch) = update {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 110);
        }
        let SnapshotLookup::Found(snapshot) = store.get_message("20", "30", 110) else {
            panic!("expected patched message");
        };
        assert_eq!(snapshot.content, "edited");
        assert_eq!(snapshot.message_type, 21);
        assert_eq!(snapshot.user_id, "40");
        assert!(snapshot.complete);
    }

    #[test]
    fn message_updates_replace_image_counts_only_when_attachments_are_present() {
        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        let mut message = MessageSnapshot::new("10", "20", "30", "40", 100);
        message.image_attachment_count = 2;
        message.content = "original".to_string();
        store.upsert_message(message);
        for (sequence, fields, expected_count) in [
            (1, r#""content":"edited""#, 2),
            (
                2,
                r#""attachments":[{"filename":"new.AVIF"},{"content_type":"image/png"},{"filename":"text.txt"}]"#,
                2,
            ),
            (3, r#""attachments":[{"filename":"only.webp"}]"#, 1),
            (4, r#""attachments":[]"#, 0),
            (5, r#""content":"edited again""#, 0),
        ] {
            let frame = parse_gateway_frame(
                &format!(r#"{{"op":0,"s":{sequence},"t":"MESSAGE_UPDATE","d":{{"channel_id":"20","id":"30",{fields}}}}}"#),
                110,
            ).unwrap();
            let GatewayFrame::Dispatch(dispatch) = frame else {
                panic!("expected dispatch");
            };
            apply_dispatch_to_snapshot(&dispatch, &mut store, 110);
            let SnapshotLookup::Found(snapshot) = store.get_message("20", "30", 110) else {
                panic!("expected message");
            };
            assert_eq!(snapshot.image_attachment_count, expected_count);
            assert_eq!(snapshot.user_id, "40");
            assert!(snapshot.complete);
        }
    }

    #[test]
    fn rejects_malformed_message_update_fields() {
        for fields in [
            r#""type":2147483648"#,
            r#""type":"wrong""#,
            r#""attachments":null"#,
            r#""attachments":{}"#,
        ] {
            let frame = format!(
                r#"{{"op":0,"s":1,"t":"MESSAGE_UPDATE","d":{{"channel_id":"20","id":"30",{fields}}}}}"#
            );
            assert!(parse_gateway_frame(&frame, 0).is_err(), "{fields}");
        }
    }

    #[test]
    fn parses_channel_and_thread_dispatches_into_snapshot() {
        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        let channel = parse_gateway_frame(
            r#"{"op":0,"s":7,"t":"CHANNEL_UPDATE","d":{"guild_id":"10","id":"20","type":0,"permission_overwrites":[{"id":"10","type":0,"allow":"2048","deny":"0"}]}}"#,
            100,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = channel {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 100);
        }
        let SnapshotLookup::Found(snapshot) = store.get_channel("20", 100) else {
            panic!("expected channel snapshot");
        };
        assert_eq!(snapshot.guild_id, "10");
        assert_eq!(snapshot.channel_type, 0);
        assert!(snapshot.has_permission_overwrites);
        assert!(snapshot.permission_overwrites[0]
            .allow
            .has(crate::permission::BIT_SEND_MESSAGES));

        let threads = parse_gateway_frame(
            r#"{"op":0,"s":8,"t":"THREAD_LIST_SYNC","d":{"guild_id":"10","threads":[{"id":"30","parent_id":"20","type":12,"thread_metadata":{"archived":true,"locked":false},"member":{}}]}}"#,
            110,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = threads {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 110);
        }
        let SnapshotLookup::Found(snapshot) = store.get_channel("30", 110) else {
            panic!("expected thread snapshot");
        };
        assert_eq!(snapshot.guild_id, "10");
        assert_eq!(snapshot.channel_type, 12);
        assert!(snapshot.archived);
        assert_eq!(snapshot.current_user_thread_joined, Some(true));
        let parent_update = parse_gateway_frame(
            r#"{"op":0,"s":9,"t":"CHANNEL_UPDATE","d":{"guild_id":"10","id":"20","type":0,"permission_overwrites":[]}}"#,
            115,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = parent_update {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 115);
        }
        let SnapshotLookup::Found(snapshot) = store.get_channel("30", 115) else {
            panic!("expected thread snapshot");
        };
        assert_eq!(snapshot.current_user_thread_joined, None);

        let delete =
            parse_gateway_frame(r#"{"op":0,"s":9,"t":"THREAD_DELETE","d":{"id":"30"}}"#, 120)
                .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = delete {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 120);
        }
        assert!(matches!(
            store.get_channel("30", 120),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn parses_guild_create_initial_channels_and_threads() {
        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        let create = parse_gateway_frame(
            r#"{"op":0,"s":5,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"99","roles":[{"id":"10","permissions":"1024","position":0},{"id":"40","permissions":"2048","position":1}],"members":[{"user":{"id":"99"},"roles":["40"],"communication_disabled_until":null}],"channels":[{"id":"20","type":0}],"threads":[{"id":"30","type":12,"joined":false,"thread_metadata":{"archived":false,"locked":true}}]}}"#,
            10,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = create {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 10);
        }
        let SnapshotLookup::Found(guild) = store.get_guild("10", 10) else {
            panic!("expected guild");
        };
        assert_eq!(guild.owner_id.as_deref(), Some("99"));
        let SnapshotLookup::Found(channel) = store.get_channel("20", 10) else {
            panic!("expected guild channel");
        };
        assert_eq!(channel.guild_id, "10");
        let SnapshotLookup::Found(thread) = store.get_channel("30", 10) else {
            panic!("expected guild thread");
        };
        assert_eq!(thread.guild_id, "10");
        assert!(thread.locked);
        assert_eq!(thread.current_user_thread_joined, Some(false));
        let SnapshotLookup::Found(roles) = store.get_guild_roles("10", 10) else {
            panic!("expected guild roles");
        };
        assert!(roles.complete);
        assert_eq!(roles.roles.len(), 2);
        let SnapshotLookup::Found(member) = store.get_member("10", "99", 10) else {
            panic!("expected guild member");
        };
        assert_eq!(member.role_ids, vec!["40"]);
        assert!(member.complete);
    }

    #[test]
    fn guild_create_cannot_keep_old_access_when_identified_children_are_unreadable() {
        for child in [
            r#"{"id":"20","type":"bad"}"#,
            r#"{"id":"20","type":0,"parent_id":false}"#,
            r#"{"id":"20","type":0,"permission_overwrites":false}"#,
            r#"{"id":"20","guild_id":"11","type":0}"#,
        ] {
            let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 10, 10);
            ingestor
                .snapshot_mut()
                .upsert_channel(ChannelSnapshot::new("10", "20", 0, 0));
            ingestor
                .snapshot_mut()
                .upsert_channel(ChannelSnapshot::new("10", "21", 0, 0));
            ingestor.ingest_json(&format!(r#"{{"op":0,"s":1,"t":"GUILD_CREATE","d":{{"id":"10","channels":[{child},{{"id":"21","type":0}}]}}}}"#), 1).unwrap();
            let SnapshotLookup::Found(channel) = ingestor.snapshot_mut().get_channel("20", 1)
            else {
                panic!("expected preserved metadata")
            };
            assert!(!channel.complete, "{child}");
            assert_eq!(channel.guild_id, "10", "{child}");
            let SnapshotLookup::Found(good) = ingestor.snapshot_mut().get_channel("21", 1) else {
                panic!("expected valid channel")
            };
            assert!(good.complete);
        }
    }

    #[test]
    fn malformed_member_collections_revalidate_old_proofs_without_losing_fresh_members() {
        for members in [
            "false",
            r#"[{"user":{"id":"99"},"roles":[],"communication_disabled_until":null},{"roles":[]}]"#,
            r#"[{"user":{"id":"40"},"guild_id":"11","roles":[],"communication_disabled_until":null}]"#,
            r#"[{"user":{"id":"40"},"roles":["50"]},{"user":{"id":"40"},"roles":[]}]"#,
        ] {
            let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 10, 10);
            for (guild, user) in [("10", "40"), ("10", "99"), ("11", "40")] {
                ingestor
                    .snapshot_mut()
                    .upsert_member(MemberSnapshot::new(guild, user, vec![], 0));
            }
            ingestor.ingest_json(&format!(r#"{{"op":0,"s":1,"t":"GUILD_CREATE","d":{{"id":"10","members":{members}}}}}"#), 1).unwrap();
            let SnapshotLookup::Found(member) = ingestor.snapshot_mut().get_member("10", "40", 1)
            else {
                panic!("missing member metadata")
            };
            assert!(!member.complete, "{members}");
            let SnapshotLookup::Found(other) = ingestor.snapshot_mut().get_member("11", "40", 1)
            else {
                panic!("missing unrelated member")
            };
            assert!(other.complete);
            if members.contains(r#""99""#) {
                let SnapshotLookup::Found(fresh) =
                    ingestor.snapshot_mut().get_member("10", "99", 1)
                else {
                    panic!("missing fresh member")
                };
                assert!(fresh.complete);
            }
        }
    }

    #[test]
    fn guild_create_keeps_valid_snapshot_parts_when_children_are_malformed() {
        let create = parse_gateway_frame(
            r#"{"op":0,"s":5,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"99","roles":[{"id":"10","permissions":"1024","position":0},{"id":40,"permissions":"2048","position":1}],"members":[{"user":{"id":"99"},"roles":["10"],"communication_disabled_until":null},{"roles":["10"]}],"channels":[{"id":"20","type":0,"parent_id":"30","permission_overwrites":[{"id":"10","type":0,"allow":"2048","deny":"0"},{"id":"99","type":"bad","allow":"0","deny":"0"}]},{"id":25,"type":0}],"threads":[{"id":"40","parent_id":"20","type":12,"joined":true,"thread_metadata":{"archived":false,"locked":false}},{"id":"bad-thread"}]}}"#,
            10,
        )
        .unwrap();
        let GatewayFrame::Dispatch(dispatch) = create else {
            panic!("expected dispatch");
        };
        let GatewayDispatch::GuildCreate {
            sequence,
            guild,
            channels,
            roles,
            members,
        } = &dispatch
        else {
            panic!("expected guild create");
        };
        assert_eq!(*sequence, 5);
        assert_eq!(guild.guild_id, "10");
        assert_eq!(channels.len(), 2);
        let channel = channels
            .iter()
            .find(|channel| channel.channel_id == "20")
            .unwrap();
        assert_eq!(channel.parent_channel_id.as_deref(), Some("30"));
        assert!(!channel.complete);
        assert_eq!(channel.permission_overwrites.len(), 1);
        assert!(channel.permission_overwrites[0]
            .allow
            .has(crate::permission::BIT_SEND_MESSAGES));
        let thread = channels
            .iter()
            .find(|channel| channel.channel_id == "40")
            .unwrap();
        assert_eq!(thread.current_user_thread_joined, Some(true));
        assert!(thread.complete);
        let roles = roles.as_ref().expect("expected partial roles");
        assert!(!roles.complete);
        assert_eq!(roles.roles.len(), 1);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].user_id, "99");
        assert!(members[0].complete);

        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        apply_dispatch_to_snapshot(&dispatch, &mut store, 10);
        let SnapshotLookup::Found(stored_channel) = store.get_channel("20", 10) else {
            panic!("expected partial channel");
        };
        assert!(!stored_channel.complete);
        let SnapshotLookup::Found(stored_roles) = store.get_guild_roles("10", 10) else {
            panic!("expected partial roles");
        };
        assert!(!stored_roles.complete);
        let SnapshotLookup::Found(stored_member) = store.get_member("10", "99", 10) else {
            panic!("expected valid member");
        };
        assert_eq!(stored_member.role_ids, vec!["10"]);
    }

    #[test]
    fn guild_update_refreshes_owner() {
        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        let create = parse_gateway_frame(
            r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"99"}}"#,
            1,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = create {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 1);
        }
        let update = parse_gateway_frame(
            r#"{"op":0,"s":2,"t":"GUILD_UPDATE","d":{"id":"10","owner_id":"100"}}"#,
            2,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = update {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 2);
        }
        let SnapshotLookup::Found(guild) = store.get_guild("10", 2) else {
            panic!("expected guild");
        };
        assert_eq!(guild.owner_id.as_deref(), Some("100"));
    }

    #[test]
    fn role_events_update_guild_roles_snapshot() {
        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        store.upsert_channel(ChannelSnapshot::new("10", "30", 12, 0));
        assert!(store.set_current_user_thread_joined("30", true, 1));
        let create = parse_gateway_frame(
            r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","roles":[{"id":"10","permissions":"1024","position":0},{"id":"40","permissions":"2048","position":1}]}}"#,
            1,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = create {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 1);
        }
        let update = parse_gateway_frame(
            r#"{"op":0,"s":2,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"40","permissions":"3072","position":3}}}"#,
            2,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = update {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 2);
        }
        let SnapshotLookup::Found(thread) = store.get_channel("30", 2) else {
            panic!("expected thread");
        };
        assert_eq!(thread.current_user_thread_joined, None);
        let SnapshotLookup::Found(roles) = store.get_guild_roles("10", 2) else {
            panic!("expected updated roles");
        };
        let role = roles
            .roles
            .iter()
            .find(|role| role.role_id == "40")
            .unwrap();
        assert_eq!(role.position, 3);
        assert!(role.permissions.has(crate::permission::BIT_SEND_MESSAGES));

        let delete = parse_gateway_frame(
            r#"{"op":0,"s":3,"t":"GUILD_ROLE_DELETE","d":{"guild_id":"10","role_id":"40"}}"#,
            3,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = delete {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 3);
        }
        let SnapshotLookup::Found(roles) = store.get_guild_roles("10", 3) else {
            panic!("expected roles after delete");
        };
        assert!(roles.complete);
        assert!(roles.roles.iter().all(|role| role.role_id != "40"));
    }

    #[test]
    fn message_revisions_prevent_delayed_gateway_and_rest_responses_from_rolling_back_edits() {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 16, 16);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","author":{"id":"40"},"type":0,"content":"created","attachments":[],"edited_timestamp":null}}"#, 0).unwrap();
        let edited = parse_json(r#"{"channel_id":"20","id":"30","author":{"id":"40"},"type":0,"content":"REST edit","attachments":[],"edited_timestamp":"1970-01-01T00:00:01.000000002Z"}"#).unwrap();
        assert!(ingestor
            .snapshot_mut()
            .apply_rest_message_response(&edited, "20", "30", 1));
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"MESSAGE_UPDATE","d":{"channel_id":"20","id":"30","content":"delayed gateway edit","attachments":[{"filename":"old.png"}],"edited_timestamp":"1970-01-01T00:00:01.000000001Z"}}"#, 2).unwrap();
        let SnapshotLookup::Found(message) = ingestor.snapshot_mut().get_message("20", "30", 2)
        else {
            panic!("expected the current message");
        };
        assert_eq!(message.content, "REST edit");
        assert_eq!(message.image_attachment_count, 0);
        ingestor.ingest_json(r#"{"op":0,"s":3,"t":"MESSAGE_UPDATE","d":{"channel_id":"20","id":"30","content":"new gateway edit","attachments":[{"filename":"new.png"}],"edited_timestamp":"1970-01-01T00:00:01.000000003Z"}}"#, 3).unwrap();
        assert!(ingestor
            .snapshot_mut()
            .apply_rest_message_response(&edited, "20", "30", 4));
        let SnapshotLookup::Found(message) = ingestor.snapshot_mut().get_message("20", "30", 4)
        else {
            panic!("expected the newest message");
        };
        assert_eq!(message.content, "new gateway edit");
        assert_eq!(message.image_attachment_count, 1);
    }

    #[test]
    fn member_chunks_refresh_members_without_deleting_members_outside_the_chunk() {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 16, 16);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"GUILD_MEMBER_ADD","d":{"guild_id":"10","user":{"id":"41"},"roles":["51"],"communication_disabled_until":null}}"#, 1).unwrap();
        ingestor.pop_event().unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","chunk_index":0,"chunk_count":2,"nonce":"request-1","not_found":["invalid-id"],"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}]}}"#, 2).unwrap();
        let SnapshotLookup::Found(member) = ingestor.snapshot_mut().get_member("10", "40", 2)
        else {
            panic!("expected the member returned by the chunk");
        };
        assert_eq!(member.role_ids, ["50"]);
        assert!(member.complete);
        assert!(matches!(
            ingestor.snapshot_mut().get_member("10", "41", 2),
            SnapshotLookup::Found(_)
        ));
        let event = ingestor.pop_event().unwrap();
        assert_eq!(event.event_type, D7GEV);
        assert_eq!(event.description, "GUILD_MEMBERS_CHUNK");
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","chunk_index":1,"chunk_count":2,"members":[{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}]}}"#, 3).unwrap();
        assert!(ingestor.pop_event().is_none());
        let SnapshotLookup::Found(member) = ingestor.snapshot_mut().get_member("10", "40", 3)
        else {
            panic!("expected the member to survive a replay");
        };
        assert_eq!(member.role_ids, ["50"]);
    }

    #[test]
    fn member_events_update_and_tombstone_members() {
        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        let add = parse_gateway_frame(
            r#"{"op":0,"s":1,"t":"GUILD_MEMBER_ADD","d":{"guild_id":"10","user":{"id":"99"},"roles":["40"],"communication_disabled_until":"2026-06-03T00:00:00.000Z"}}"#,
            1,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = add {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 1);
        }
        let SnapshotLookup::Found(member) = store.get_member("10", "99", 1) else {
            panic!("expected member");
        };
        assert_eq!(member.role_ids, vec!["40"]);
        assert!(member.timed_out);

        let update = parse_gateway_frame(
            r#"{"op":0,"s":2,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"99"},"roles":[],"communication_disabled_until":null}}"#,
            2,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = update {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 2);
        }
        let SnapshotLookup::Found(member) = store.get_member("10", "99", 2) else {
            panic!("expected updated member");
        };
        assert!(member.role_ids.is_empty());
        assert!(!member.timed_out);

        let remove = parse_gateway_frame(
            r#"{"op":0,"s":3,"t":"GUILD_MEMBER_REMOVE","d":{"guild_id":"10","user":{"id":"99"}}}"#,
            3,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = remove {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 3);
        }
        assert!(matches!(
            store.get_member("10", "99", 3),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn partial_member_events_preserve_roles_timeouts_and_event_notifications() {
        let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 10, 10);
        ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
        ingestor.ingest_json(r#"{"op":0,"s":2,"t":"GUILD_MEMBER_ADD","d":{"guild_id":"10","user":{"id":"99"},"roles":["40"],"communication_disabled_until":"1970-01-01T00:00:00.100Z"}}"#, 1).unwrap();
        while ingestor.pop_event().is_some() {}
        for (sequence, fields) in [
            (3, r#""nick":"new nickname""#),
            (4, r#""roles":["50"]"#),
            (5, r#""communication_disabled_until":null"#),
        ] {
            let body = format!(
                r#"{{"op":0,"s":{sequence},"t":"GUILD_MEMBER_UPDATE","d":{{"guild_id":"10","user":{{"id":"99"}},{fields}}}}}"#
            );
            ingestor.ingest_json(&body, sequence).unwrap();
            let SnapshotLookup::Found(member) =
                ingestor.snapshot_mut().get_member("10", "99", sequence)
            else {
                panic!("expected complete cached member");
            };
            assert!(member.complete);
            assert_eq!(
                member.role_ids,
                vec![if sequence == 3 { "40" } else { "50" }]
            );
            assert_eq!(member.timed_out, sequence < 5);
            assert_eq!(member.captured_at_ms, if sequence == 3 { 1 } else { 4 });
            let event = ingestor.pop_event().unwrap();
            assert_eq!(event.event_type, D7MBU);
            assert_eq!(event.description, "GUILD_MEMBER_UPDATE");
            assert_eq!(
                event.data,
                GatewayEventData::Member {
                    guild_id: "10".into(),
                    user_id: "99".into(),
                }
            );
            assert!(ingestor.pop_event().is_none());
        }
        ingestor.machine.resume_seq_used = Some(6);
        ingestor.ingest_json(r#"{"op":0,"s":6,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"99"},"roles":[]}}"#, 6).unwrap();
        let SnapshotLookup::Found(member) = ingestor.snapshot_mut().get_member("10", "99", 6)
        else {
            panic!("expected cached member after ignored replay");
        };
        assert_eq!(member.role_ids, vec!["50"]);
        assert_eq!(ingestor.machine().gap_epoch(), 0);
    }

    #[test]
    fn invalid_member_updates_discard_old_permissions_instead_of_clearing_timeouts() {
        for fields in [
            r#""communication_disabled_until":"2025-02-29T00:00:00Z""#,
            r#""communication_disabled_until":false"#,
            r#""roles":["40",false]"#,
        ] {
            let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 10, 10);
            ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
            ingestor.ingest_json(r#"{"op":0,"s":2,"t":"GUILD_MEMBER_ADD","d":{"guild_id":"10","user":{"id":"99"},"roles":["40"],"communication_disabled_until":"1970-01-01T00:00:01Z"}}"#, 1).unwrap();
            while ingestor.pop_event().is_some() {}
            let body = format!(
                r#"{{"op":0,"s":3,"t":"GUILD_MEMBER_UPDATE","d":{{"guild_id":"10","user":{{"id":"99"}},{fields}}}}}"#
            );
            ingestor.ingest_json_tolerant(&body, 2).unwrap();
            assert_eq!(
                ingestor.snapshot_mut().get_member("10", "99", 2),
                SnapshotLookup::Missing
            );
            assert_eq!(ingestor.machine().gap_epoch(), 1);
            let gap = ingestor.pop_event().unwrap();
            assert_eq!(gap.event_type, D7GAP);
            assert_eq!(gap.description, "INVALID_PERMISSION_DISPATCH");
            assert_eq!(
                ingestor.pop_event().unwrap().description,
                "INVALID_GUILD_MEMBER_UPDATE"
            );
        }
    }

    #[test]
    fn invalid_full_member_timeouts_keep_roles_but_never_complete_permission_state() {
        let member = parse_member_snapshot(
            &parse_json(
                r#"{"user":{"id":"99"},"roles":["40"],"communication_disabled_until":"invalid"}"#,
            )
            .unwrap(),
            "10",
            0,
        )
        .unwrap();
        assert_eq!(member.role_ids, vec!["40"]);
        assert!(member.roles_complete);
        assert!(!member.timeout_known);
        assert!(!member.complete);
        assert!(member.timed_out);
    }

    #[test]
    fn malformed_thread_members_cannot_authorise_the_bot_or_another_thread() {
        for data in [
            r#"{"id":"30","added_members":[{"user_id":null}]}"#,
            r#"{"id":"30","added_members":[{"user_id":false}]}"#,
            r#"{"id":"30","added_members":[{"user_id":99}]}"#,
            r#"{"id":"30","added_members":[{}]}"#,
            r#"{"id":"30","added_members":[{"id":"31","user_id":"99"}]}"#,
            r#"{"id":"30","added_members":[{"id":false,"user_id":"99"}]}"#,
            r#"{"id":"30","added_members":[{"user_id":"bad"}]}"#,
            r#"{"id":"30","added_members":[{"user_id":"99","member":{"user":{"id":"40"}}}]}"#,
        ] {
            let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 10, 10);
            ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"fixture","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
            for channel_id in ["30", "31"] {
                let mut channel = ChannelSnapshot::new("10", channel_id, 12, 0);
                channel.current_user_thread_joined = Some(false);
                channel.current_user_thread_joined_at_ms = Some(0);
                ingestor.snapshot_mut().upsert_channel(channel);
            }
            let frame = format!(r#"{{"op":0,"s":2,"t":"THREAD_MEMBERS_UPDATE","d":{data}}}"#);
            assert!(ingestor.ingest_json(&frame, 1).is_err(), "{data}");
            for id in ["30", "31"] {
                assert!(
                    !matches!(ingestor.snapshot_mut().get_channel(id, 1), SnapshotLookup::Found(channel) if channel.current_user_thread_joined == Some(true)),
                    "{data}"
                );
            }
        }
    }

    #[test]
    fn thread_member_events_update_current_user_thread_access() {
        let mut ingestor = GatewayIngestor::new(8, 1000, 1000, 10, 10);
        ingestor
            .ingest_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#,
                1,
            )
            .unwrap();
        ingestor
            .ingest_json(
                r#"{"op":0,"s":2,"t":"THREAD_CREATE","d":{"guild_id":"10","id":"30","type":12,"joined":false}}"#,
                2,
            )
            .unwrap();
        let SnapshotLookup::Found(snapshot) = ingestor.snapshot_mut().get_channel("30", 2) else {
            panic!("expected thread snapshot");
        };
        assert_eq!(snapshot.current_user_thread_joined, Some(false));

        ingestor
            .ingest_json(
                r#"{"op":0,"s":3,"t":"THREAD_MEMBERS_UPDATE","d":{"id":"30","guild_id":"10","member_count":1,"added_members":[{"id":"30","user_id":"99"}],"removed_member_ids":["42"]}}"#,
                3,
            )
            .unwrap();
        let SnapshotLookup::Found(snapshot) = ingestor.snapshot_mut().get_channel("30", 3) else {
            panic!("expected joined thread snapshot");
        };
        assert_eq!(snapshot.current_user_thread_joined, Some(true));

        ingestor
            .ingest_json(
                r#"{"op":0,"s":4,"t":"THREAD_MEMBERS_UPDATE","d":{"id":"30","guild_id":"10","member_count":0,"removed_member_ids":["99"]}}"#,
                4,
            )
            .unwrap();
        let SnapshotLookup::Found(snapshot) = ingestor.snapshot_mut().get_channel("30", 4) else {
            panic!("expected left thread snapshot");
        };
        assert_eq!(snapshot.current_user_thread_joined, Some(false));

        ingestor
            .ingest_json(
                r#"{"op":0,"s":5,"t":"THREAD_MEMBER_UPDATE","d":{"id":"30"}}"#,
                5,
            )
            .unwrap();
        let SnapshotLookup::Found(snapshot) = ingestor.snapshot_mut().get_channel("30", 5) else {
            panic!("expected current user thread snapshot");
        };
        assert_eq!(snapshot.current_user_thread_joined, Some(true));
    }

    #[test]
    fn bot_member_update_stales_private_thread_access() {
        let mut ingestor = GatewayIngestor::new(8, 1000, 1000, 10, 10);
        ingestor
            .ingest_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#,
                1,
            )
            .unwrap();
        ingestor
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("10", "30", 12, 1));
        assert!(ingestor
            .snapshot_mut()
            .set_current_user_thread_joined("30", true, 1));
        ingestor
            .ingest_json(
                r#"{"op":0,"s":2,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"99"},"roles":[],"communication_disabled_until":null}}"#,
                2,
            )
            .unwrap();
        let SnapshotLookup::Found(snapshot) = ingestor.snapshot_mut().get_channel("30", 2) else {
            panic!("expected private thread");
        };
        assert_eq!(snapshot.current_user_thread_joined, None);
    }

    #[test]
    fn thread_list_sync_members_mark_joined_threads() {
        let mut ingestor = GatewayIngestor::new(8, 1000, 1000, 10, 10);
        ingestor
            .ingest_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#,
                1,
            )
            .unwrap();
        ingestor
            .ingest_json(
                r#"{"op":0,"s":2,"t":"THREAD_LIST_SYNC","d":{"guild_id":"10","threads":[{"id":"30","type":12},{"id":"31","type":12}],"members":[{"id":"30","user_id":"99"},{"id":"31","user_id":"42"}]}}"#,
                2,
            )
            .unwrap();
        let SnapshotLookup::Found(joined) = ingestor.snapshot_mut().get_channel("30", 2) else {
            panic!("expected joined thread");
        };
        assert_eq!(joined.current_user_thread_joined, Some(true));
        assert_eq!(joined.current_user_thread_joined_at_ms, Some(2));
        let SnapshotLookup::Found(other) = ingestor.snapshot_mut().get_channel("31", 2) else {
            panic!("expected other thread");
        };
        assert_eq!(other.current_user_thread_joined, None);
    }

    #[test]
    fn parses_delete_and_bulk_delete_into_tombstones() {
        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        let delete = parse_gateway_frame(
            r#"{"op":0,"s":3,"t":"MESSAGE_DELETE","d":{"guild_id":"10","channel_id":"20","id":"30"}}"#,
            0,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = delete {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 10);
        }
        assert!(matches!(
            store.get_message("20", "30", 10),
            SnapshotLookup::Tombstone
        ));

        let bulk = parse_gateway_frame(
            r#"{"op":0,"s":4,"t":"MESSAGE_DELETE_BULK","d":{"guild_id":"10","channel_id":"20","ids":["31","32"]}}"#,
            0,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = bulk {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 10);
        }
        assert!(matches!(
            store.get_message("20", "31", 10),
            SnapshotLookup::Tombstone
        ));
        assert!(matches!(
            store.get_message("20", "32", 10),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn parses_guild_create_and_delete_into_guild_state() {
        let mut store = SnapshotStore::new(1000, 1000, 10, 10);
        let create = parse_gateway_frame(
            r#"{"op":0,"s":5,"t":"GUILD_CREATE","d":{"id":"10","unavailable":true}}"#,
            10,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = create {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 10);
        }
        let SnapshotLookup::Found(snapshot) = store.get_guild("10", 10) else {
            panic!("expected guild unavailable snapshot");
        };
        assert!(snapshot.unavailable);

        let delete = parse_gateway_frame(
            r#"{"op":0,"s":6,"t":"GUILD_DELETE","d":{"id":"10","unavailable":false}}"#,
            20,
        )
        .unwrap();
        if let GatewayFrame::Dispatch(dispatch) = delete {
            apply_dispatch_to_snapshot(&dispatch, &mut store, 20);
        }
        assert!(matches!(
            store.get_guild("10", 20),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn parses_gateway_control_frames() {
        assert_eq!(
            parse_gateway_frame(r#"{"op":10,"d":{"heartbeat_interval":45000}}"#, 0).unwrap(),
            GatewayFrame::Hello {
                heartbeat_interval_ms: 45000
            }
        );
        assert_eq!(
            parse_gateway_frame(r#"{"op":11,"d":null}"#, 0).unwrap(),
            GatewayFrame::HeartbeatAck
        );
        assert_eq!(
            parse_gateway_frame(r#"{"op":1,"d":null}"#, 0).unwrap(),
            GatewayFrame::HeartbeatRequest
        );
        assert_eq!(
            parse_gateway_frame(r#"{"op":7,"d":null}"#, 0).unwrap(),
            GatewayFrame::Reconnect
        );
        assert_eq!(
            parse_gateway_frame(r#"{"op":9,"d":false}"#, 0).unwrap(),
            GatewayFrame::InvalidSession { resumable: false }
        );
    }

    #[test]
    fn discord_rate_limits_keep_their_sequence_and_existing_legacy_events_work() {
        for notification in [
            r#"{"op":0,"s":2,"t":"RATE_LIMITED","d":{"opcode":8,"retry_after":1.5001,"meta":{"guild_id":"10","nonce":"request"}}}"#,
            r#"{"op":0,"s":2,"t":"GATEWAY_RATE_LIMITED","d":{"opcode":8,"retry_ms":1501}}"#,
        ] {
            let mut ingestor = GatewayIngestor::new(16, 1000, 1000, 16, 16);
            ingestor.ingest_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"session","resume_gateway_url":"wss://resume"}}"#, 0).unwrap();
            ingestor.pop_event().unwrap();
            ingestor.ingest_json(notification, 0).unwrap();
            assert_eq!(ingestor.machine().last_seen_seq(), Some(2));
            let event = ingestor.pop_event().unwrap();
            assert_eq!(event.event_type, D7GRL);
            assert_eq!(event.retry_ms, Some(1501));
            assert_eq!(event.opcode, Some(8));
            assert_eq!(event.sequence, Some(2));
            ingestor.ingest_json(notification, 10).unwrap();
            assert!(ingestor.pop_event().is_none());
        }
    }

    #[test]
    fn parses_gateway_rate_limited_dispatch() {
        let frame = parse_gateway_frame(
            r#"{"op":0,"s":null,"t":"GATEWAY_RATE_LIMITED","d":{"opcode":2,"retry_ms":1500}}"#,
            0,
        )
        .unwrap();
        assert_eq!(
            frame,
            GatewayFrame::Dispatch(GatewayDispatch::GatewayRateLimited {
                opcode: 2,
                retry_ms: 1500
            })
        );
    }

    #[test]
    fn parsed_gateway_frames_drive_state_machine() {
        let mut machine = GatewayStateMachine::new(8);
        let ready = parse_gateway_frame(
            r#"{"op":0,"s":5,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
            0,
        )
        .unwrap();
        assert_eq!(machine.handle_frame(ready), GatewayAction::None);
        assert_eq!(machine.state(), GatewayState::Ready);
        assert_eq!(machine.pop_event().unwrap().event_type, D7RDY);

        let message = parse_gateway_frame(
            r#"{"op":0,"s":6,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"30","content":"hello","author":{"id":"40"}}}"#,
            1234,
        )
        .unwrap();
        assert_eq!(machine.handle_frame(message), GatewayAction::None);
        let event = machine.pop_event().unwrap();
        assert_eq!(event.event_type, D7MSG);
        assert_eq!(event.sequence, Some(6));
        assert!(matches!(event.data, GatewayEventData::Message(_)));

        let reconnect = parse_gateway_frame(r#"{"op":7,"d":null}"#, 0).unwrap();
        assert_eq!(
            machine.handle_frame(reconnect),
            GatewayAction::SendResume { seq: 6 }
        );
    }

    #[test]
    fn ingestor_delivers_public_events_and_updates_snapshot() {
        let mut ingestor = GatewayIngestor::new(8, 1000, 1000, 10, 10);
        ingestor
            .ingest_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
                0,
            )
            .unwrap();
        assert_eq!(ingestor.pop_event().unwrap().event_type, D7RDY);

        ingestor
            .ingest_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"30","content":"","author":{"id":"40","bot":true},"member":{"roles":["50"]}}}"#,
                100,
            )
            .unwrap();
        let message = ingestor.pop_event().unwrap();
        assert_eq!(message.event_type, D7MSG);
        let GatewayEventData::Message(snapshot) = message.data else {
            panic!("expected message data");
        };
        assert!(snapshot.content_available);
        assert!(snapshot.author_is_bot);
        assert_eq!(snapshot.role_ids, vec!["50"]);
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "30", 100),
            SnapshotLookup::Found(_)
        ));

        ingestor
            .ingest_json(
                r#"{"op":0,"s":3,"t":"MESSAGE_DELETE","d":{"guild_id":"10","channel_id":"20","id":"30"}}"#,
                110,
            )
            .unwrap();
        let delete = ingestor.pop_event().unwrap();
        assert_eq!(delete.event_type, D7MDE);
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "30", 110),
            SnapshotLookup::Tombstone
        ));

        ingestor
            .ingest_json(
                r#"{"op":0,"s":4,"t":"MESSAGE_DELETE_BULK","d":{"guild_id":"10","channel_id":"20","ids":["31","32"]}}"#,
                120,
            )
            .unwrap();
        let bulk = ingestor.pop_event().unwrap();
        assert_eq!(bulk.event_type, D7EMB);
        assert_eq!(bulk.description, "MESSAGE_DELETE_BULK");
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "31", 120),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn ingestor_queue_full_gap_invalidates_snapshot() {
        let mut ingestor = GatewayIngestor::new(1, 1000, 1000, 10, 10);
        ingestor
            .ingest_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
                0,
            )
            .unwrap();
        let _ = ingestor.pop_event();

        ingestor
            .ingest_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"30","content":"one","author":{"id":"40"}}}"#,
                10,
            )
            .unwrap();
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "30", 10),
            SnapshotLookup::Found(_)
        ));

        let action = ingestor
            .ingest_json(
                r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"31","content":"two","author":{"id":"40"}}}"#,
                20,
            )
            .unwrap();
        assert_eq!(action, GatewayAction::SendResume { seq: 3 });
        assert_eq!(ingestor.pop_event().unwrap().event_type, D7GAP);
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "30", 20),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "31", 20),
            SnapshotLookup::Missing
        ));
    }

    #[test]
    fn ingestor_does_not_snapshot_resume_duplicates() {
        let mut ingestor = GatewayIngestor::new(8, 1000, 1000, 10, 10);
        ingestor
            .ingest_json(
                r#"{"op":0,"s":10,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
                0,
            )
            .unwrap();
        let _ = ingestor.pop_event();
        assert_eq!(
            ingestor.machine_mut().close(Some(4008), false),
            GatewayAction::Backoff { delay_ms: 1_000 }
        );
        assert_eq!(
            ingestor.machine_mut().backoff_elapsed(),
            GatewayAction::SendResume { seq: 10 }
        );
        ingestor.machine_mut().resume_succeeded();

        ingestor
            .ingest_json(
                r#"{"op":0,"s":10,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"30","content":"old","author":{"id":"40"}}}"#,
                100,
            )
            .unwrap();
        assert!(ingestor.pop_event().is_none());
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "30", 100),
            SnapshotLookup::Missing
        ));

        ingestor
            .ingest_json(
                r#"{"op":0,"s":11,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"31","content":"new","author":{"id":"40"}}}"#,
                110,
            )
            .unwrap();
        assert_eq!(ingestor.pop_event().unwrap().event_type, D7MSG);
        assert!(matches!(
            ingestor.snapshot_mut().get_message("20", "31", 110),
            SnapshotLookup::Found(_)
        ));
    }

    #[test]
    fn ingestor_applies_message_update_and_publishes_event() {
        let mut ingestor = GatewayIngestor::new(8, 1000, 1000, 10, 10);
        ingestor
            .ingest_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
                0,
            )
            .unwrap();
        let _ = ingestor.pop_event();
        ingestor
            .ingest_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","content":"old","type":21,"author":{"id":"40"}}}"#,
                100,
            )
            .unwrap();
        let _ = ingestor.pop_event();
        ingestor
            .ingest_json(
                r#"{"op":0,"s":3,"t":"MESSAGE_UPDATE","d":{"channel_id":"20","id":"30","content":"new"}}"#,
                110,
            )
            .unwrap();
        let event = ingestor.pop_event().unwrap();
        assert_eq!(event.event_type, D7EMU);
        assert_eq!(event.description, "MESSAGE_UPDATE");
        let GatewayEventData::Resource {
            channel_id,
            message_id,
            text,
            ..
        } = event.data
        else {
            panic!("expected resource event data");
        };
        assert_eq!(channel_id.as_deref(), Some("20"));
        assert_eq!(message_id.as_deref(), Some("30"));
        assert_eq!(text.as_deref(), Some("new"));
        let SnapshotLookup::Found(snapshot) = ingestor.snapshot_mut().get_message("20", "30", 110)
        else {
            panic!("expected patched snapshot");
        };
        assert_eq!(snapshot.content, "new");
        assert_eq!(snapshot.message_type, 21);
    }

    fn serve_websocket(
        server_frames: Vec<&'static str>,
    ) -> Option<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("cannot bind local TCP listener; skipping websocket transport test");
                return None;
            }
            Err(error) => panic!("failed to bind websocket test listener: {error}"),
        };
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_ws_http_request(&mut stream);
            let accept = websocket_accept_key(&request);
            tx.send(request).unwrap();
            let response = format!(
                concat!(
                    "HTTP/1.1 101 Switching Protocols\r\n",
                    "Upgrade: websocket\r\n",
                    "Connection: Upgrade\r\n",
                    "Sec-WebSocket-Accept: {}\r\n\r\n"
                ),
                accept
            );
            stream.write_all(response.as_bytes()).unwrap();
            let client_frame = read_websocket_frame(&mut stream).unwrap();
            tx.send(String::from_utf8(client_frame.payload).unwrap())
                .unwrap();
            for frame in server_frames {
                let encoded = websocket_control_frame(0x1, frame.as_bytes()).unwrap();
                stream.write_all(&encoded).unwrap();
            }
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
            if let Ok(frame) = read_websocket_frame(&mut stream) {
                if frame.opcode == 0x8 {
                    let code = websocket_close_code_from_payload(&frame.payload).unwrap_or(0);
                    tx.send(format!("close:{code}")).unwrap();
                }
            }
        });
        Some((format!("ws://{addr}/gateway"), rx, handle))
    }

    fn websocket_accept_key(request: &str) -> String {
        let key = request
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("sec-websocket-key"))
            .map(|(_, value)| value.trim())
            .unwrap_or("");
        tungstenite::handshake::derive_accept_key(key.as_bytes())
    }

    fn read_ws_http_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut byte = [0_u8; 1];
        while bytes.len() < 16 * 1024 {
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            if bytes.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&bytes).to_string()
    }
}
