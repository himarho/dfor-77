use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::audit::{
    diagnostic_field, json_log_escape, write_diagnostic, AuditContentPolicy, AuditLogger,
    AuditRecord, RollingLog,
};
use crate::config::Config;
use crate::constants::*;
use crate::event_queue::{EventQueue, QueueEvent, DEFAULT_EVENT_QUEUE_MAX_BYTES};
use crate::gateway::{
    DailyIdentifyBudget, GatewayAction, GatewayBackoffInfo, GatewayEvent, GatewayEventData,
    GatewayIngestor, GatewayManager, GatewayNormalizeError, GatewayOutboundLimiter,
    GatewayPayloads, GatewayResumeSession, GatewaySendKind, GatewayState, GatewayTransport,
    GatewayTransportError, MAX_GATEWAY_PAYLOAD_BYTES,
};
use crate::ipc::{Field, Request, Response, MAX_VALUE_ENCODED_BYTES};
use crate::json::{parse_json, JsonValue};
use crate::memory::HeapSize;
use crate::permission::{
    MemberPermissions, PermissionResolver, PermissionSet, BIT_MANAGE_THREADS,
    GUILD_ONLY_PERMISSION_BITS,
};
use crate::rest::{
    validate_attachment_file_name, ArchivedThreadKind, AttachmentUpload, HttpMethod,
    HttpRestBackend, InvalidRequestBudget, MockRestBackend, ReactionOperation, RestAction,
    RestBackend, RestExecutor, RestOutcome, RestPlanner, RestRequest, MAX_REST_ATTACHMENTS,
    MAX_REST_ATTACHMENT_BYTES, MAX_REST_POLL_ANSWERS, MAX_REST_STICKERS,
};
use crate::snapshot::archive::{thread_member_matches, ArchivedThreadPage};
use crate::snapshot::{
    ChannelPermissionOverwrite, ChannelSnapshot, GuildRolesSnapshot, GuildSnapshot, MemberSnapshot,
    MessageSnapshot, SnapshotLookup, SnapshotSettings,
};
use crate::validation::{is_unreserved, utf8_prefix, validate_message_content, validate_snowflake};

mod gateway_send;
mod mutations;
mod permissions;
mod reads;

use gateway_send::QueuedGatewayPayload;
use permissions::*;

const MAX_GATEWAY_RESUME_OUTBOUND_WAIT_MS: u64 = 1_000;
const MAX_GATEWAY_USER_OUTBOUND_QUEUE: usize = 64;
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
const MAX_MESSAGE_SEARCH_RESULTS: usize = 1_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSettings {
    pub gateway_intents: u64,
    pub gateway_shard_id: u32,
    pub gateway_shard_count: u32,
    pub gateway_identify_max_wait_ms: u64,
    pub gateway_daily_identify_warn: u32,
    pub gateway_daily_identify_stop: u32,
    pub gateway_next_event_timeout_ms: u64,
    pub event_queue_capacity: usize,
    pub event_queue_max_bytes: usize,
    pub snapshot_max_bytes: usize,
    pub message_ttl_ms: u64,
    pub message_tombstone_ttl_ms: u64,
    pub message_max_entries: usize,
    pub message_channel_allowlist: Vec<String>,
    pub message_tombstone_max_entries: usize,
    pub member_ttl_ms: u64,
    pub member_max_entries: usize,
    pub member_tombstone_ttl_ms: u64,
    pub member_tombstone_max_entries: usize,
    pub bot_member_ttl_ms: u64,
    pub thread_member_ttl_ms: u64,
    pub resource_max_entries: usize,
    pub resource_tombstone_ttl_ms: u64,
    pub resource_tombstone_max_entries: usize,
    pub rest_http_timeout_ms: u64,
    pub rest_global_max_wait_ms: u64,
    pub rest_invalid_budget_warn: usize,
    pub rest_invalid_budget_recover: usize,
    pub rest_invalid_budget_stop: usize,
    pub runtime_log_rest_success: bool,
    pub runtime_log_gateway_events: bool,
}

impl RuntimeSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            gateway_intents: config.intent_bits,
            gateway_shard_id: config.gateway_shard_id,
            gateway_shard_count: config.gateway_shard_count,
            gateway_identify_max_wait_ms: config.gateway_identify_max_wait_ms,
            gateway_daily_identify_warn: config.gateway_daily_identify_warn,
            gateway_daily_identify_stop: config.gateway_daily_identify_stop,
            gateway_next_event_timeout_ms: config.next_event_timeout_ms,
            event_queue_capacity: config.snapshot_event_queue_capacity,
            event_queue_max_bytes: config.snapshot_event_queue_max_bytes,
            snapshot_max_bytes: config.snapshot_cache_max_bytes,
            message_ttl_ms: config.snapshot_message_ttl_ms,
            message_tombstone_ttl_ms: config.snapshot_message_tombstone_ttl_ms,
            message_max_entries: config.snapshot_message_max_entries,
            message_channel_allowlist: config.snapshot_message_channel_allowlist.clone(),
            message_tombstone_max_entries: config.snapshot_message_tombstone_max_entries,
            member_ttl_ms: config.snapshot_member_ttl_ms,
            member_max_entries: config.snapshot_member_max_entries,
            member_tombstone_ttl_ms: config.snapshot_member_tombstone_ttl_ms,
            member_tombstone_max_entries: config.snapshot_member_tombstone_max_entries,
            bot_member_ttl_ms: config.snapshot_bot_member_ttl_ms,
            thread_member_ttl_ms: config.snapshot_thread_member_ttl_ms,
            resource_max_entries: config.snapshot_resource_max_entries,
            resource_tombstone_ttl_ms: config.snapshot_resource_tombstone_ttl_ms,
            resource_tombstone_max_entries: config.snapshot_resource_tombstone_max_entries,
            rest_http_timeout_ms: config.rest_http_timeout_ms,
            rest_global_max_wait_ms: config.rest_global_max_wait_ms,
            rest_invalid_budget_warn: config.rest_invalid_budget_warn,
            rest_invalid_budget_recover: config.rest_invalid_budget_recover,
            rest_invalid_budget_stop: config.rest_invalid_budget_stop,
            runtime_log_rest_success: config.runtime_log_rest_success,
            runtime_log_gateway_events: config.runtime_log_gateway_events,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.gateway_shard_count == 0 || self.gateway_shard_id >= self.gateway_shard_count {
            return Err("gateway shard settings invalid");
        }
        DailyIdentifyBudget::new(
            self.gateway_daily_identify_warn,
            self.gateway_daily_identify_stop,
        )?;
        InvalidRequestBudget::new(
            self.rest_invalid_budget_warn,
            self.rest_invalid_budget_recover,
            self.rest_invalid_budget_stop,
        )?;
        if self.rest_http_timeout_ms == 0 {
            return Err("rest http timeout must be positive");
        }
        if self.event_queue_capacity == 0 {
            return Err("event queue capacity must be positive");
        }
        if self.event_queue_max_bytes == 0 || self.snapshot_max_bytes == 0 {
            return Err("memory budgets must be positive");
        }
        if self.message_ttl_ms == 0
            || self.member_ttl_ms == 0
            || self.bot_member_ttl_ms == 0
            || self.thread_member_ttl_ms == 0
        {
            return Err("snapshot ttl must be positive");
        }
        if self.message_max_entries == 0
            || self.message_tombstone_max_entries == 0
            || self.member_max_entries == 0
            || self.member_tombstone_max_entries == 0
            || self.resource_max_entries == 0
            || self.resource_tombstone_max_entries == 0
        {
            return Err("snapshot max entries must be positive");
        }
        Ok(())
    }

    fn sanitized(&self) -> Self {
        let defaults = Self::default();
        let mut settings = self.clone();
        if settings.gateway_shard_count == 0
            || settings.gateway_shard_id >= settings.gateway_shard_count
        {
            settings.gateway_shard_id = defaults.gateway_shard_id;
            settings.gateway_shard_count = defaults.gateway_shard_count;
        }
        if DailyIdentifyBudget::new(
            settings.gateway_daily_identify_warn,
            settings.gateway_daily_identify_stop,
        )
        .is_err()
        {
            settings.gateway_daily_identify_warn = defaults.gateway_daily_identify_warn;
            settings.gateway_daily_identify_stop = defaults.gateway_daily_identify_stop;
        }
        if InvalidRequestBudget::new(
            settings.rest_invalid_budget_warn,
            settings.rest_invalid_budget_recover,
            settings.rest_invalid_budget_stop,
        )
        .is_err()
        {
            settings.rest_invalid_budget_warn = defaults.rest_invalid_budget_warn;
            settings.rest_invalid_budget_recover = defaults.rest_invalid_budget_recover;
            settings.rest_invalid_budget_stop = defaults.rest_invalid_budget_stop;
        }
        if settings.rest_http_timeout_ms == 0 {
            settings.rest_http_timeout_ms = defaults.rest_http_timeout_ms;
        }
        if settings.event_queue_capacity == 0 {
            settings.event_queue_capacity = defaults.event_queue_capacity;
        }
        if settings.event_queue_max_bytes == 0 {
            settings.event_queue_max_bytes = defaults.event_queue_max_bytes;
        }
        if settings.snapshot_max_bytes == 0 {
            settings.snapshot_max_bytes = defaults.snapshot_max_bytes;
        }
        if settings.message_ttl_ms == 0 {
            settings.message_ttl_ms = defaults.message_ttl_ms;
        }
        if settings.member_ttl_ms == 0 {
            settings.member_ttl_ms = defaults.member_ttl_ms;
        }
        if settings.bot_member_ttl_ms == 0 {
            settings.bot_member_ttl_ms = defaults.bot_member_ttl_ms;
        }
        if settings.thread_member_ttl_ms == 0 {
            settings.thread_member_ttl_ms = defaults.thread_member_ttl_ms;
        }
        if settings.message_max_entries == 0 {
            settings.message_max_entries = defaults.message_max_entries;
        }
        if settings.message_tombstone_max_entries == 0 {
            settings.message_tombstone_max_entries = defaults.message_tombstone_max_entries;
        }
        if settings.member_max_entries == 0 {
            settings.member_max_entries = defaults.member_max_entries;
        }
        if settings.member_tombstone_max_entries == 0 {
            settings.member_tombstone_max_entries = defaults.member_tombstone_max_entries;
        }
        if settings.resource_max_entries == 0 {
            settings.resource_max_entries = defaults.resource_max_entries;
        }
        if settings.resource_tombstone_max_entries == 0 {
            settings.resource_tombstone_max_entries = defaults.resource_tombstone_max_entries;
        }
        settings
    }
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            gateway_intents: 513,
            gateway_shard_id: 0,
            gateway_shard_count: 1,
            gateway_identify_max_wait_ms: 60_000,
            gateway_daily_identify_warn: 900,
            gateway_daily_identify_stop: 999,
            gateway_next_event_timeout_ms: 100,
            event_queue_capacity: 1024,
            event_queue_max_bytes: DEFAULT_EVENT_QUEUE_MAX_BYTES,
            snapshot_max_bytes: crate::snapshot::DEFAULT_SNAPSHOT_MAX_BYTES,
            message_ttl_ms: 60_000,
            message_tombstone_ttl_ms: 60_000,
            message_max_entries: 4096,
            message_channel_allowlist: Vec::new(),
            message_tombstone_max_entries: 4096,
            member_ttl_ms: 60_000,
            member_max_entries: 4096,
            member_tombstone_ttl_ms: 60_000,
            member_tombstone_max_entries: 4096,
            bot_member_ttl_ms: 60_000,
            thread_member_ttl_ms: 60_000,
            resource_max_entries: 4096,
            resource_tombstone_ttl_ms: 60_000,
            resource_tombstone_max_entries: 4096,
            rest_http_timeout_ms: 10_000,
            rest_global_max_wait_ms: 10_000,
            rest_invalid_budget_warn: 6000,
            rest_invalid_budget_recover: 7000,
            rest_invalid_budget_stop: 8000,
            runtime_log_rest_success: true,
            runtime_log_gateway_events: true,
        }
    }
}

fn snapshot_settings(settings: &RuntimeSettings) -> SnapshotSettings {
    SnapshotSettings {
        message_ttl_ms: settings.message_ttl_ms,
        message_tombstone_ttl_ms: settings.message_tombstone_ttl_ms,
        message_max_entries: settings.message_max_entries,
        message_tombstone_max_entries: settings.message_tombstone_max_entries,
        resource_ttl_ms: settings.message_ttl_ms,
        resource_tombstone_ttl_ms: settings.resource_tombstone_ttl_ms,
        resource_max_entries: settings.resource_max_entries,
        resource_tombstone_max_entries: settings.resource_tombstone_max_entries,
        member_ttl_ms: settings.member_ttl_ms,
        bot_member_ttl_ms: settings.bot_member_ttl_ms,
        thread_member_ttl_ms: settings.thread_member_ttl_ms,
        member_tombstone_ttl_ms: settings.member_tombstone_ttl_ms,
        member_max_entries: settings.member_max_entries,
        member_tombstone_max_entries: settings.member_tombstone_max_entries,
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct GatewayStartup {
    pub status: i32,
    pub why: String,
    pub action: GatewayAction,
    pub gateway_url: Option<String>,
    pub identify_payload: Option<String>,
    pub identify_bucket: Option<u32>,
    pub identify_remaining: Option<u32>,
    pub identify_reset_after_ms: Option<u64>,
    pub daily_identify_count: u32,
    pub daily_identify_remaining: u32,
    pub daily_identify_warning: bool,
}

impl std::fmt::Debug for GatewayStartup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayStartup")
            .field("status", &self.status)
            .field("action", &self.action)
            .field(
                "gateway_url_bytes",
                &self.gateway_url.as_ref().map(String::len),
            )
            .field(
                "identify_payload_bytes",
                &self.identify_payload.as_ref().map(String::len),
            )
            .field("identify_bucket", &self.identify_bucket)
            .field("identify_remaining", &self.identify_remaining)
            .field("identify_reset_after_ms", &self.identify_reset_after_ms)
            .field("daily_identify_count", &self.daily_identify_count)
            .field("daily_identify_remaining", &self.daily_identify_remaining)
            .field("daily_identify_warning", &self.daily_identify_warning)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayConnectResult {
    pub status: i32,
    pub why: String,
    pub startup: GatewayStartup,
    pub connected: bool,
    pub identify_sent: bool,
    pub sent_payload_bytes: usize,
    pub gateway_outbound_remaining: Option<usize>,
    pub received_frames: usize,
    pub last_action: GatewayAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayPumpResult {
    pub status: i32,
    pub why: String,
    pub heartbeat_sent: bool,
    pub heartbeat_payload_bytes: usize,
    pub gateway_outbound_remaining: Option<usize>,
    pub received_frame: bool,
    pub action: GatewayAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeLogValue<'a> {
    String(&'a str),
    I32(i32),
    I64(i64),
    U32(u32),
    U64(u64),
    Usize(usize),
    Bool(bool),
    Null,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeLogField<'a> {
    key: &'a str,
    value: RuntimeLogValue<'a>,
}

impl<'a> RuntimeLogField<'a> {
    pub fn string(key: &'a str, value: &'a str) -> Self {
        Self {
            key,
            value: RuntimeLogValue::String(value),
        }
    }

    pub fn i32(key: &'a str, value: i32) -> Self {
        Self {
            key,
            value: RuntimeLogValue::I32(value),
        }
    }

    pub fn i64(key: &'a str, value: i64) -> Self {
        Self {
            key,
            value: RuntimeLogValue::I64(value),
        }
    }

    pub fn u32(key: &'a str, value: u32) -> Self {
        Self {
            key,
            value: RuntimeLogValue::U32(value),
        }
    }

    pub fn u64(key: &'a str, value: u64) -> Self {
        Self {
            key,
            value: RuntimeLogValue::U64(value),
        }
    }

    pub fn usize(key: &'a str, value: usize) -> Self {
        Self {
            key,
            value: RuntimeLogValue::Usize(value),
        }
    }

    pub fn bool(key: &'a str, value: bool) -> Self {
        Self {
            key,
            value: RuntimeLogValue::Bool(value),
        }
    }

    pub fn null(key: &'a str) -> Self {
        Self {
            key,
            value: RuntimeLogValue::Null,
        }
    }
}

#[derive(Debug)]
struct RuntimeEventLogger {
    file: RollingLog,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MessageSearchResult {
    channel_id: String,
    message_id: String,
    image_attachment_count: usize,
    delta_seconds: i64,
}

impl RuntimeEventLogger {
    fn open(path: &str) -> io::Result<Self> {
        Ok(Self {
            file: RollingLog::open(std::path::Path::new(path), 64 * 1024 * 1024)?,
        })
    }

    fn append(&mut self, event: &str, fields: &[RuntimeLogField<'_>]) -> io::Result<()> {
        let mut line = String::new();
        line.push_str("{\"schema\":\"dfor77.runtime.v1\"");
        runtime_log_u64(&mut line, "time_unix_ms", now_ms_u64());
        runtime_log_string(&mut line, "event", event);
        for field in fields {
            runtime_log_field(&mut line, field);
        }
        line.push_str("}\n");
        self.file.append(line.as_bytes())?;
        self.file.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLogValidation {
    pub records: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLogValidationError {
    message: String,
}

impl RuntimeLogValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RuntimeLogValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeLogValidationError {}

pub fn validate_runtime_log_text(
    text: &str,
) -> Result<RuntimeLogValidation, RuntimeLogValidationError> {
    let mut records = 0;
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            return Err(RuntimeLogValidationError::new(format!(
                "line {line_number}: empty runtime record"
            )));
        }
        let value = parse_json(line).map_err(|error| {
            RuntimeLogValidationError::new(format!("line {line_number}: invalid JSON: {error}"))
        })?;
        validate_runtime_log_record(line_number, &value)?;
        records += 1;
    }
    if records == 0 {
        return Err(RuntimeLogValidationError::new("runtime log has no records"));
    }
    Ok(RuntimeLogValidation { records })
}

fn validate_runtime_log_record(
    line_number: usize,
    value: &JsonValue,
) -> Result<(), RuntimeLogValidationError> {
    let JsonValue::Object(fields) = value else {
        return Err(RuntimeLogValidationError::new(format!(
            "line {line_number}: runtime record is not an object"
        )));
    };
    if value.get("schema").and_then(JsonValue::as_str) != Some("dfor77.runtime.v1") {
        return Err(RuntimeLogValidationError::new(format!(
            "line {line_number}: schema is not dfor77.runtime.v1"
        )));
    }
    if value
        .get("time_unix_ms")
        .and_then(JsonValue::as_u64)
        .is_none()
    {
        return Err(RuntimeLogValidationError::new(format!(
            "line {line_number}: time_unix_ms is not a non-negative integer"
        )));
    }
    if value.get("event").and_then(JsonValue::as_str) != Some("runtime") {
        return Err(RuntimeLogValidationError::new(format!(
            "line {line_number}: event is not runtime"
        )));
    }
    let kind = value
        .get("kind")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| {
            RuntimeLogValidationError::new(format!("line {line_number}: kind missing"))
        })?;
    match kind {
        "gateway_transition" | "process_lifecycle" => {
            if value.get("phase").and_then(JsonValue::as_str).is_none() {
                return Err(RuntimeLogValidationError::new(format!(
                    "line {line_number}: phase missing for {kind}"
                )));
            }
        }
        "rest_action" => {
            if value.get("action").and_then(JsonValue::as_str).is_none() {
                return Err(RuntimeLogValidationError::new(format!(
                    "line {line_number}: action missing for rest_action"
                )));
            }
            if value.get("status").and_then(JsonValue::as_i64).is_none() {
                return Err(RuntimeLogValidationError::new(format!(
                    "line {line_number}: status missing for rest_action"
                )));
            }
            if value
                .get("commit_state")
                .and_then(JsonValue::as_str)
                .is_none()
            {
                return Err(RuntimeLogValidationError::new(format!(
                    "line {line_number}: commit_state missing for rest_action"
                )));
            }
        }
        _ => {
            return Err(RuntimeLogValidationError::new(format!(
                "line {line_number}: unknown runtime log kind {kind}"
            )));
        }
    }
    for (key, value) in fields {
        validate_runtime_log_value(line_number, key, value)?;
    }
    Ok(())
}

fn validate_runtime_log_value(
    line_number: usize,
    key: &str,
    value: &JsonValue,
) -> Result<(), RuntimeLogValidationError> {
    match value {
        JsonValue::String(text) if runtime_log_contains_sensitive_text(text) => {
            Err(RuntimeLogValidationError::new(format!(
                "line {line_number}: field {key} contains sensitive text"
            )))
        }
        JsonValue::Array(values) => {
            for value in values {
                validate_runtime_log_value(line_number, key, value)?;
            }
            Ok(())
        }
        JsonValue::Object(fields) => {
            for (child_key, value) in fields {
                validate_runtime_log_value(line_number, child_key, value)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn runtime_log_contains_sensitive_text(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("authorization:")
        || (value.matches('.').count() >= 2
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_' || byte == b'-'
            })
            && value.len() >= 50)
}

pub struct DforRuntime {
    lifecycle: Lifecycle,
    events: EventQueue<Event>,
    current_event: Option<Event>,
    errors: ErrorScopes,
    dry_run: bool,
    log_only: bool,
    settings: RuntimeSettings,
    rest: RuntimeRest,
    gateway_manager: GatewayManager,
    gateway_outbound: GatewayOutboundLimiter,
    gateway_user_outbound: VecDeque<QueuedGatewayPayload>,
    gateway: GatewayIngestor,
    bot_user_id_cache: Option<String>,
    audit: Option<AuditLogger>,
    runtime_log: Option<RuntimeEventLogger>,
    bot_process_pid: Option<u32>,
    rest_auth_fatal: bool,
    last_raw_rest_body: Option<String>,
    last_message_search: Vec<MessageSearchResult>,
}

impl std::fmt::Debug for DforRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DforRuntime")
            .field("lifecycle", &self.lifecycle)
            .field("dry_run", &self.dry_run)
            .field("log_only", &self.log_only)
            .field("has_current_event", &self.current_event.is_some())
            .field(
                "pending_gateway_outbound",
                &self.gateway_user_outbound.len(),
            )
            .field("rest_auth_fatal", &self.rest_auth_fatal)
            .field(
                "raw_rest_response_bytes",
                &self.last_raw_rest_body.as_ref().map(String::len),
            )
            .finish()
    }
}

const INTERACTION_INITIAL_RESPONSE_DEADLINE_MS: u64 = 3_000;

impl DforRuntime {
    pub fn new(dry_run: bool, log_only: bool) -> Self {
        Self::with_settings(dry_run, log_only, RuntimeSettings::default())
    }

    pub fn with_settings(dry_run: bool, log_only: bool, settings: RuntimeSettings) -> Self {
        Self::build_with_settings(dry_run, log_only, settings.sanitized())
    }

    pub fn try_with_settings(
        dry_run: bool,
        log_only: bool,
        settings: RuntimeSettings,
    ) -> Result<Self, &'static str> {
        settings.validate()?;
        Ok(Self::build_with_settings(dry_run, log_only, settings))
    }

    fn build_with_settings(dry_run: bool, log_only: bool, settings: RuntimeSettings) -> Self {
        let mut gateway = GatewayIngestor::with_snapshot_settings(
            settings.event_queue_capacity,
            snapshot_settings(&settings),
        );
        gateway.set_message_snapshot_channel_allowlist(settings.message_channel_allowlist.clone());
        gateway
            .machine_mut()
            .set_event_queue_max_bytes(settings.event_queue_max_bytes);
        gateway
            .snapshot_mut()
            .set_max_bytes(settings.snapshot_max_bytes);
        let mut gateway_manager = GatewayManager::new(
            settings.gateway_intents,
            settings.gateway_shard_id,
            settings.gateway_shard_count,
            settings.gateway_daily_identify_warn,
            settings.gateway_daily_identify_stop,
            settings.gateway_identify_max_wait_ms,
            settings.event_queue_capacity,
        )
        .unwrap_or_default();
        gateway_manager
            .machine_mut()
            .set_event_queue_max_bytes(settings.event_queue_max_bytes);
        Self {
            lifecycle: Lifecycle::Uninitialized,
            events: EventQueue::new(
                settings.event_queue_capacity,
                settings.event_queue_max_bytes,
                false,
            ),
            current_event: None,
            errors: ErrorScopes::default(),
            dry_run,
            log_only,
            settings: settings.clone(),
            rest: RuntimeRest::PlanOnly(RestPlanner::new()),
            gateway_manager,
            gateway_outbound: GatewayOutboundLimiter::new(),
            gateway_user_outbound: VecDeque::new(),
            gateway,
            bot_user_id_cache: None,
            audit: None,
            runtime_log: None,
            bot_process_pid: None,
            rest_auth_fatal: false,
            last_raw_rest_body: None,
            last_message_search: Vec::new(),
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self::with_settings(
            config.dry_run,
            config.log_only,
            RuntimeSettings::from_config(config),
        )
    }

    pub fn with_demo_events(dry_run: bool, log_only: bool) -> Self {
        let mut runtime = Self::new(dry_run, log_only);
        runtime.push_event(Event::ready());
        runtime.push_event(Event::demo_message());
        runtime
    }

    pub fn with_demo_events_and_settings(
        dry_run: bool,
        log_only: bool,
        settings: RuntimeSettings,
    ) -> Self {
        let mut runtime = Self::with_settings(dry_run, log_only, settings);
        runtime.push_event(Event::ready());
        runtime.push_event(Event::demo_message());
        runtime
    }

    pub fn with_mock_rest_backend(dry_run: bool, log_only: bool, backend: MockRestBackend) -> Self {
        let mut runtime = Self::new(dry_run, log_only);
        runtime.rest = RuntimeRest::Mock(rest_executor(backend, &runtime.settings));
        runtime
    }

    pub fn use_mock_rest_backend(&mut self, backend: MockRestBackend) {
        self.rest = RuntimeRest::Mock(rest_executor(backend, &self.settings));
    }

    pub fn use_http_rest_backend(&mut self, backend: HttpRestBackend) {
        self.rest = RuntimeRest::Http(rest_executor(backend, &self.settings));
    }

    pub fn enable_audit(&mut self, path: &str) -> io::Result<()> {
        self.audit = Some(AuditLogger::open(path)?);
        Ok(())
    }

    pub fn enable_audit_with_policy(
        &mut self,
        path: &str,
        content_policy: AuditContentPolicy,
    ) -> io::Result<()> {
        self.audit = Some(AuditLogger::open_with_policy(path, content_policy)?);
        Ok(())
    }

    pub fn enable_runtime_log(&mut self, path: &str) -> io::Result<()> {
        self.runtime_log = Some(RuntimeEventLogger::open(path)?);
        Ok(())
    }

    pub fn write_runtime_event(
        &mut self,
        event: &str,
        fields: &[RuntimeLogField<'_>],
    ) -> io::Result<()> {
        match &mut self.runtime_log {
            Some(logger) => logger.append(event, fields),
            None => Ok(()),
        }
    }

    pub fn set_bot_process_pid(&mut self, pid: u32) {
        self.bot_process_pid = Some(pid);
    }

    pub fn rest_auth_fatal(&self) -> bool {
        self.rest_auth_fatal
    }

    pub fn shutdown_gateway(&mut self) {
        self.gateway.machine_mut().close(Some(1000), true);
        self.gateway_user_outbound.clear();
        self.errors.sev.clear();
    }

    pub fn ingest_gateway_json(
        &mut self,
        input: &str,
        now_ms: u64,
    ) -> Result<GatewayAction, GatewayNormalizeError> {
        let result = self.gateway.ingest_json(input, now_ms);
        self.finish_gateway_ingest(result, now_ms)
    }

    pub fn ingest_gateway_live_json(
        &mut self,
        input: &str,
        now_ms: u64,
    ) -> Result<GatewayAction, GatewayNormalizeError> {
        let result = self.gateway.ingest_json_tolerant(input, now_ms);
        self.finish_gateway_ingest(result, now_ms)
    }

    fn finish_gateway_ingest(
        &mut self,
        result: Result<GatewayAction, GatewayNormalizeError>,
        now_ms: u64,
    ) -> Result<GatewayAction, GatewayNormalizeError> {
        if let Some(heartbeat) = self.gateway.heartbeat() {
            self.gateway_outbound
                .configure_heartbeat_interval_ms(heartbeat.interval_ms());
        }
        let queue_action = self.drain_gateway_events(now_ms);
        result.map(|action| prefer_gateway_action(action, queue_action))
    }

    pub fn gateway_resume_session(&self) -> Option<GatewayResumeSession> {
        self.gateway.machine().resume_session()
    }

    pub fn gateway_resume_session_invalidated(&self) -> bool {
        self.gateway.machine().resume_session_invalidated()
    }

    fn sync_gateway_gap(&mut self, now_ms: u64) -> bool {
        let advanced = self.gateway.sync_snapshot_gap();
        if advanced {
            self.drain_gateway_events(now_ms);
        }
        advanced
    }

    pub fn gateway_backoff_info(&self) -> Option<GatewayBackoffInfo> {
        self.gateway.machine().backoff_info()
    }

    pub fn restore_gateway_resume_session(
        &mut self,
        session: GatewayResumeSession,
    ) -> Result<GatewayAction, &'static str> {
        let action = self.gateway.machine_mut().restore_resume_session(session)?;
        if self.sync_gateway_gap(now_ms_u64()) {
            self.bot_user_id_cache = None;
        }
        let queue_action = self.drain_gateway_events(now_ms_u64());
        Ok(prefer_gateway_action(action, queue_action))
    }

    pub fn prepare_gateway_identify(&mut self, token: &str, now_ms: u64) -> GatewayStartup {
        if !self.rest.can_execute() {
            self.errors.sev.set(
                D7STA,
                0,
                0,
                "gateway REST backend is not configured",
                "GATEWAY_REST_BACKEND_MISSING",
            );
            return self.gateway_startup(
                D7STA,
                "GATEWAY_REST_BACKEND_MISSING",
                GatewayAction::Fatal("GATEWAY_REST_BACKEND_MISSING"),
                None,
            );
        }
        let outcome = self.rest.execute_get_gateway_bot(now_ms);
        self.observe_rest_outcome(&outcome);
        if outcome.status != D7OK {
            self.errors.sev.set(
                outcome.status,
                outcome.http_status,
                outcome.discord_error_code,
                &outcome.why,
                &outcome.why,
            );
            return self.gateway_startup(
                outcome.status,
                &outcome.why,
                GatewayAction::Fatal("GATEWAY_BOT_FETCH_FAILED"),
                None,
            );
        }
        let Some(body) = outcome.response_body.as_deref() else {
            self.errors.sev.set(
                D7STA,
                outcome.http_status,
                outcome.discord_error_code,
                "gateway bot response body missing",
                "GATEWAY_BOT_RESPONSE_MISSING",
            );
            return self.gateway_startup(
                D7STA,
                "GATEWAY_BOT_RESPONSE_MISSING",
                GatewayAction::Fatal("GATEWAY_BOT_RESPONSE_MISSING"),
                None,
            );
        };
        let action = match self
            .gateway_manager
            .observe_gateway_bot_body_at(body, now_ms)
        {
            Ok(action) => action,
            Err(error) => {
                self.errors.sev.set(
                    D7BAD,
                    outcome.http_status,
                    outcome.discord_error_code,
                    &error.to_string(),
                    "GATEWAY_BOT_RESPONSE_INVALID",
                );
                return self.gateway_startup(
                    D7BAD,
                    "GATEWAY_BOT_RESPONSE_INVALID",
                    GatewayAction::Fatal("GATEWAY_BOT_RESPONSE_INVALID"),
                    None,
                );
            }
        };
        let identify_payload = if action == GatewayAction::SendIdentify {
            match self.gateway_manager.identify_payload(token) {
                Ok(payload) => Some(payload),
                Err(reason) => {
                    self.errors.sev.set(D7GWL, 0, 0, reason, reason);
                    return self.gateway_startup(D7GWL, reason, GatewayAction::Fatal(reason), None);
                }
            }
        } else {
            None
        };
        let status = if matches!(&action, GatewayAction::Fatal(_)) {
            D7GWL
        } else {
            D7OK
        };
        let why = match &action {
            GatewayAction::Fatal(reason) => *reason,
            _ => WHY_OK,
        };
        if status == D7OK {
            self.errors.sev.clear();
        } else {
            self.errors.sev.set(status, 0, 0, why, why);
        }
        self.gateway_startup(status, why, action, identify_payload)
    }

    pub fn connect_gateway_once<T: GatewayTransport>(
        &mut self,
        transport: &mut T,
        token: &str,
        now_ms: u64,
    ) -> GatewayConnectResult {
        let startup = self.prepare_gateway_identify(token, now_ms);
        let mut result = gateway_connect_result(startup.clone());
        if startup.status != D7OK || startup.action != GatewayAction::SendIdentify {
            return result;
        }
        let Some(url) = startup.gateway_url.as_deref() else {
            self.errors
                .sev
                .set(D7STA, 0, 0, "gateway url missing", "GATEWAY_URL_MISSING");
            result.status = D7STA;
            result.why = "GATEWAY_URL_MISSING".to_string();
            return result;
        };
        if let Err(error) = transport.connect(url) {
            self.errors.sev.set(D7NET, 0, 0, error.why(), error.why());
            result.status = D7NET;
            result.why = error.why().to_string();
            return result;
        }
        self.gateway.machine_mut().gateway_url_fetched();
        self.sync_gateway_gap(now_ms);
        self.gateway_outbound.reset_connection();
        result.connected = true;
        if self.receive_gateway_frame(transport, now_ms, &mut result, false) != D7OK {
            return result;
        }
        let Some(payload) = startup.identify_payload.as_deref() else {
            self.errors.sev.set(
                D7STA,
                0,
                0,
                "identify payload missing",
                "IDENTIFY_PAYLOAD_MISSING",
            );
            result.status = D7STA;
            result.why = "IDENTIFY_PAYLOAD_MISSING".to_string();
            return result;
        };
        let decision = self.gateway_outbound.check_and_record(
            GatewaySendKind::Identify,
            payload.len(),
            now_ms,
        );
        result.gateway_outbound_remaining = Some(decision.remaining);
        if !decision.allowed {
            if decision.why == "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED" {
                result.last_action = self.gateway_manager.identify_limiter_backoff_at(now_ms);
                self.errors.sev.clear();
                return result;
            }
            let status = gateway_send_denied_status(GatewaySendKind::Identify, decision.why);
            self.errors
                .sev
                .set(status, 0, 0, decision.why, decision.why);
            result.status = status;
            result.why = decision.why.to_string();
            result.last_action =
                gateway_send_denied_action(GatewaySendKind::Identify, decision.why);
            return result;
        }
        if let Err(error) = transport.send_text(payload) {
            self.errors.sev.set(D7NET, 0, 0, error.why(), error.why());
            result.status = D7NET;
            result.why = error.why().to_string();
            return result;
        }
        if let Err(reason) = self.gateway_manager.record_identify_sent_at(now_ms) {
            self.errors.sev.set(D7GWL, 0, 0, reason, reason);
            result.status = D7GWL;
            result.why = reason.to_string();
            return result;
        }
        result.identify_sent = true;
        result.sent_payload_bytes = payload.len();
        self.refresh_gateway_connect_budget_fields(&mut result);
        if result.startup.daily_identify_warning
            && self.write_gateway_identify_warning_audit(&result).is_err()
        {
            self.errors.sev.set(
                D7INT,
                0,
                0,
                "audit log write failed",
                "AUDIT_LOG_WRITE_FAILED",
            );
            result.status = D7INT;
            result.why = "AUDIT_LOG_WRITE_FAILED".to_string();
            return result;
        }
        if self.receive_gateway_frame(transport, now_ms, &mut result, true) != D7OK {
            return result;
        }
        self.errors.sev.clear();
        result
    }

    pub fn pump_gateway_once<T: GatewayTransport>(
        &mut self,
        transport: &mut T,
        now_ms: u64,
    ) -> GatewayPumpResult {
        let mut result = gateway_pump_result();
        if matches!(
            self.gateway.machine().state(),
            GatewayState::Closed | GatewayState::Fatal
        ) {
            return result;
        }
        if self
            .gateway
            .heartbeat()
            .is_some_and(|heartbeat| heartbeat.missed_ack(now_ms))
        {
            result.action = self.gateway.machine_mut().close_at(None, false, now_ms);
            self.sync_gateway_gap(now_ms);
            self.errors.sev.clear();
            return result;
        }
        let heartbeat_due = self
            .gateway
            .heartbeat()
            .is_some_and(|heartbeat| heartbeat.due(now_ms));
        if heartbeat_due && !self.send_gateway_heartbeat(transport, now_ms, &mut result) {
            return result;
        }
        if !self.gateway_user_outbound.is_empty()
            && !self.send_gateway_user_payload(transport, now_ms, &mut result)
        {
            return result;
        }
        match transport.receive_text() {
            Ok(Some(frame)) => match self.ingest_gateway_live_json(&frame, now_ms) {
                Ok(action) => {
                    result.received_frame = true;
                    if action == GatewayAction::SendHeartbeat {
                        if !self.send_gateway_heartbeat(transport, now_ms, &mut result) {
                            return result;
                        }
                        result.action = GatewayAction::None;
                    } else {
                        result.action = action;
                    }
                    self.errors.sev.clear();
                    result
                }
                Err(error) => {
                    self.errors
                        .sev
                        .set(D7GWL, 0, 0, &error.to_string(), "GATEWAY_FRAME_INVALID");
                    result.status = D7GWL;
                    result.why = "GATEWAY_FRAME_INVALID".to_string();
                    result
                }
            },
            Ok(None) => {
                result.action =
                    self.gateway
                        .machine_mut()
                        .close_at(transport.last_close_code(), false, now_ms);
                self.sync_gateway_gap(now_ms);
                self.errors.sev.clear();
                result
            }
            Err(GatewayTransportError::NoFrame) => {
                self.errors.sev.clear();
                result
            }
            Err(error) => {
                self.errors.sev.set(D7NET, 0, 0, error.why(), error.why());
                result.status = D7NET;
                result.why = error.why().to_string();
                result
            }
        }
    }

    fn send_gateway_heartbeat<T: GatewayTransport>(
        &mut self,
        transport: &mut T,
        now_ms: u64,
        result: &mut GatewayPumpResult,
    ) -> bool {
        let payload = GatewayPayloads::heartbeat(self.gateway.machine().last_seen_seq());
        let decision = self.gateway_outbound.check_and_record(
            GatewaySendKind::Heartbeat,
            payload.len(),
            now_ms,
        );
        result.gateway_outbound_remaining = Some(decision.remaining);
        if !decision.allowed {
            if decision.why == "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED" {
                result.action = self.gateway.machine_mut().close_at(None, false, now_ms);
                self.sync_gateway_gap(now_ms);
                self.errors.sev.clear();
                return false;
            }
            let status = gateway_send_denied_status(GatewaySendKind::Heartbeat, decision.why);
            self.errors
                .sev
                .set(status, 0, 0, decision.why, decision.why);
            result.status = status;
            result.why = decision.why.to_string();
            result.action = gateway_send_denied_action(GatewaySendKind::Heartbeat, decision.why);
            return false;
        }
        if let Err(error) = transport.send_text(&payload) {
            self.errors.sev.set(D7NET, 0, 0, error.why(), error.why());
            result.status = D7NET;
            result.why = error.why().to_string();
            return false;
        }
        if let Some(heartbeat) = self.gateway.heartbeat_mut() {
            heartbeat.mark_sent(now_ms);
        }
        result.heartbeat_sent = true;
        result.heartbeat_payload_bytes = payload.len();
        true
    }

    pub fn gateway_backoff_elapsed(&mut self) -> GatewayAction {
        if self.gateway_manager.machine().state() == GatewayState::Backoff {
            self.gateway_manager.backoff_elapsed()
        } else {
            let action = self.gateway.machine_mut().backoff_elapsed();
            self.sync_gateway_gap(now_ms_u64());
            action
        }
    }

    fn gateway_resume_failed(
        &mut self,
        result: &mut GatewayPumpResult,
        reason: &'static str,
        now_ms: u64,
    ) {
        let action = self.gateway.machine_mut().resume_failed_at(reason, now_ms);
        self.sync_gateway_gap(now_ms);
        let queue_action = self.drain_gateway_events(now_ms);
        result.status = D7OK;
        result.why = WHY_OK.to_string();
        result.action = prefer_gateway_action(action, queue_action);
        self.errors.sev.clear();
    }

    fn gateway_resume_outbound_budget_exhausted(
        &mut self,
        result: &mut GatewayPumpResult,
        retry_after_ms: Option<u64>,
    ) {
        if let Some(delay_ms) = retry_after_ms {
            if delay_ms <= MAX_GATEWAY_RESUME_OUTBOUND_WAIT_MS {
                result.status = D7OK;
                result.why = WHY_OK.to_string();
                result.action = self.gateway.machine_mut().resume_send_wait(delay_ms);
                self.errors.sev.clear();
                return;
            }
        }
        self.gateway_resume_failed(result, "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED", 0);
    }

    pub fn resume_gateway_once<T: GatewayTransport>(
        &mut self,
        transport: &mut T,
        token: &str,
        seq: i64,
        now_ms: u64,
    ) -> GatewayPumpResult {
        let mut result = gateway_pump_result();
        let Some(url) = self
            .gateway
            .machine()
            .resume_gateway_url()
            .map(ToOwned::to_owned)
        else {
            self.errors.sev.set(
                D7STA,
                0,
                0,
                "resume gateway url missing",
                "RESUME_GATEWAY_URL_MISSING",
            );
            result.status = D7STA;
            result.why = "RESUME_GATEWAY_URL_MISSING".to_string();
            return result;
        };
        let Some(session_id) = self.gateway.machine().session_id().map(ToOwned::to_owned) else {
            self.errors
                .sev
                .set(D7STA, 0, 0, "session id missing", "SESSION_ID_MISSING");
            result.status = D7STA;
            result.why = "SESSION_ID_MISSING".to_string();
            return result;
        };
        if let Err(error) = transport.disconnect_for_resume() {
            self.gateway_resume_failed(&mut result, error.why(), now_ms);
            return result;
        }
        if let Err(error) = transport.connect(&url) {
            self.gateway_resume_failed(&mut result, error.why(), now_ms);
            return result;
        }
        self.gateway_outbound.reset_connection();
        match transport.receive_text() {
            Ok(Some(frame)) => match self.ingest_gateway_json(&frame, now_ms) {
                Ok(action) => {
                    result.received_frame = true;
                    result.action = action;
                }
                Err(error) => {
                    let _ = error;
                    self.gateway_resume_failed(&mut result, "GATEWAY_FRAME_INVALID", now_ms);
                    return result;
                }
            },
            Ok(None) | Err(GatewayTransportError::NoFrame) => {}
            Err(error) => {
                self.gateway_resume_failed(&mut result, error.why(), now_ms);
                return result;
            }
        }
        let payload = self.gateway_manager.resume_payload(token, &session_id, seq);
        let decision =
            self.gateway_outbound
                .check_and_record(GatewaySendKind::Resume, payload.len(), now_ms);
        result.gateway_outbound_remaining = Some(decision.remaining);
        if !decision.allowed {
            if decision.why == "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED" {
                self.gateway_resume_outbound_budget_exhausted(&mut result, decision.retry_after_ms);
                return result;
            }
            let status = gateway_send_denied_status(GatewaySendKind::Resume, decision.why);
            self.errors
                .sev
                .set(status, 0, 0, decision.why, decision.why);
            result.status = status;
            result.why = decision.why.to_string();
            result.action = gateway_send_denied_action(GatewaySendKind::Resume, decision.why);
            return result;
        }
        if let Err(error) = transport.send_text(&payload) {
            self.gateway_resume_failed(&mut result, error.why(), now_ms);
            return result;
        }
        self.errors.sev.clear();
        result
    }

    pub fn handle(&mut self, request: Request) -> Response {
        match request.verb.as_str() {
            "INI" => self.ini(request.id),
            "END" => self.end(request.id),
            "NXT" => self.nxt(request.id),
            "TYP" => self.typ(request.id),
            "GET" => self.get(request),
            "RLC" => self.role_count(request.id),
            "RLI" => self.role_at(request),
            "MSF" => self.message_search(request),
            "MSI" => self.message_search_item(request),
            "SFD" => self.snowflake_delta(request),
            "ADM" => self.admin(request),
            "CDL" => self.delete_preflight(request),
            "CTM" => self.timeout_preflight(request),
            "CSN" => self.send_preflight(request),
            "CAD" => self.current_admin(request.id),
            "CDP" => self.current_delete_preflight(request.id),
            "CTP" => self.current_timeout_preflight(request.id),
            "CSP" => self.current_send_preflight(request.id),
            "DEL" => self.delete_message(request),
            "TMO" => self.timeout_member(request),
            "UNT" => self.untimeout_member(request),
            "SND" => self.send_message(request),
            "SJP" => self.send_message_payload(request),
            "STK" => self.send_message_stickers(request),
            "POL" => self.send_message_poll(request),
            "UPL" => self.send_message_attachment(request),
            "UPM" => self.send_message_attachments(request),
            "ICB" => self.interaction_callback(request),
            "ACR" => self.application_command(request),
            "TPI" => self.typing_indicator(request),
            "RAC" => self.message_reaction(request),
            "RST" => self.raw_rest(request),
            "RBD" => self.raw_rest_body(request),
            "GWS" => self.gateway_send(request),
            "PRS" => self.gateway_presence(request),
            "VST" => self.gateway_voice_state(request),
            "ERR" => self.err(request),
            "WHY" => self.why(request),
            "LOG" => self.log(request),
            "GOP" => self.gateway_opcode(request.id),
            "GRT" => self.gateway_retry_ms(request.id),
            _ => self.protocol_error(request.id, "unknown verb"),
        }
    }

    fn ini(&mut self, id: u64) -> Response {
        match self.lifecycle {
            Lifecycle::Uninitialized => {
                self.lifecycle = Lifecycle::Initialized;
                Response::empty(id, D7OK)
            }
            Lifecycle::Initialized | Lifecycle::Ended => Response::empty(id, D7STA),
        }
    }

    fn end(&mut self, id: u64) -> Response {
        match self.lifecycle {
            Lifecycle::Initialized => {
                if self.flush_audit().is_err() {
                    self.errors.act.set(
                        D7INT,
                        0,
                        0,
                        "audit log flush failed",
                        "AUDIT_LOG_FLUSH_FAILED",
                    );
                    return Response::empty(id, D7INT);
                }
                if self.flush_runtime_log().is_err() {
                    self.errors.act.set(
                        D7INT,
                        0,
                        0,
                        "runtime log flush failed",
                        "RUNTIME_LOG_FLUSH_FAILED",
                    );
                    return Response::empty(id, D7INT);
                }
                self.shutdown_gateway();
                self.lifecycle = Lifecycle::Ended;
                self.current_event = None;
                Response::empty(id, D7OK)
            }
            Lifecycle::Uninitialized | Lifecycle::Ended => Response::empty(id, D7STA),
        }
    }

    fn nxt(&mut self, id: u64) -> Response {
        match self.lifecycle {
            Lifecycle::Uninitialized => return Response::empty(id, D7STA),
            Lifecycle::Ended => {
                self.current_event = None;
                self.errors
                    .sev
                    .set(D7EOF, 0, 0, "event stream ended", WHY_OK);
                return Response::empty(id, D7EOF);
            }
            Lifecycle::Initialized => {}
        }

        if let Some(event) = self.events.pop_front() {
            let event_type = event.event_type;
            self.current_event = Some(event);
            self.errors.sev.clear();
            Response {
                id,
                status: D7OK,
                fields: vec![Field::raw("event_type", event_type.to_string())],
            }
        } else {
            self.current_event = None;
            self.errors.sev.set(D7TIM, 0, 0, "event timeout", WHY_OK);
            Response::empty(id, D7TIM)
        }
    }

    fn typ(&self, id: u64) -> Response {
        if let Some(event) = &self.current_event {
            Response {
                id,
                status: D7OK,
                fields: vec![Field::raw("event_type", event.event_type.to_string())],
            }
        } else {
            Response::empty(id, self.getter_status())
        }
    }

    fn get(&mut self, request: Request) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, self.getter_status());
        }
        let Some(name) = request.field("name") else {
            return Response::empty(request.id, D7ARG);
        };
        if name == "CAT" {
            return self.current_parent_category(request.id);
        }
        let Some(event) = &self.current_event else {
            return Response::empty(request.id, D7STA);
        };
        let value = match name {
            "ENM" => &event.event_name,
            "GID" if !event.guild_id.is_empty() => &event.guild_id,
            "CHN" if !event.channel_id.is_empty() => &event.channel_id,
            "MID" if !event.message_id.is_empty() => &event.message_id,
            "USR" if !event.user_id.is_empty() => &event.user_id,
            "EMJ" if !event.emoji.is_empty() => &event.emoji,
            "TXT"
                if event.event_type == D7MSG
                    || (event.text_available && event.event_type != D7GAP) =>
            {
                &event.content
            }
            "IID" if event.event_type == D7ITR => &event.interaction_id,
            "APP" if event.event_type == D7ITR => &event.application_id,
            "ITK" if event.event_type == D7ITR => &event.interaction_token,
            "INM" if event.event_type == D7ITR => &event.interaction_name,
            "ITP" if event.event_type == D7ITR => {
                return Response {
                    id: request.id,
                    status: D7OK,
                    fields: vec![Field::raw("value", event.interaction_type.to_string())],
                };
            }
            "TAV" if event.event_type == D7MSG => {
                return boolish(request.id, event.text_available);
            }
            "BOT" if event.event_type == D7MSG => {
                return boolish(request.id, event.author_is_bot);
            }
            "IMC" if event.event_type == D7MSG => {
                return Response {
                    id: request.id,
                    status: D7OK,
                    fields: vec![Field::raw(
                        "value",
                        event.image_attachment_count.to_string(),
                    )],
                };
            }
            _ => return Response::empty(request.id, D7STA),
        };
        Response {
            id: request.id,
            status: D7OK,
            fields: vec![Field::percent("value", value)],
        }
    }

    fn current_parent_category(&mut self, id: u64) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(id, self.getter_status());
        }
        let Some(event) = &self.current_event else {
            return Response::empty(id, D7STA);
        };
        if !event.has_ids() || event.channel_id.is_empty() {
            return Response::empty(id, D7STA);
        }
        let guild_id = event.guild_id.clone();
        let channel_id = event.channel_id.clone();
        let now_ms = now_ms_u64();
        match self.gateway.snapshot_mut().get_channel(&channel_id, now_ms) {
            SnapshotLookup::Found(snapshot) if snapshot.complete => {
                parent_category_response(id, snapshot.parent_channel_id)
            }
            SnapshotLookup::Stale(snapshot) if snapshot.complete && !self.rest.can_execute() => {
                parent_category_response(id, snapshot.parent_channel_id)
            }
            SnapshotLookup::Found(_) | SnapshotLookup::Missing | SnapshotLookup::Stale(_) => {
                self.fetch_parent_category(id, &guild_id, &channel_id, now_ms)
            }
            SnapshotLookup::Tombstone => {
                self.errors
                    .act
                    .set(D7NFD, 0, 0, WHY_CHANNEL_NOT_FOUND, WHY_CHANNEL_NOT_FOUND);
                Response::empty(id, D7NFD)
            }
        }
    }

    fn fetch_parent_category(
        &mut self,
        id: u64,
        guild_id: &str,
        channel_id: &str,
        now_ms: u64,
    ) -> Response {
        if !self.rest.can_execute() {
            self.errors.act.set(
                D7STA,
                0,
                0,
                WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
                WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
            );
            return Response::empty(id, D7STA);
        }
        let outcome = self.rest.execute_get_channel(channel_id, now_ms);
        self.observe_rest_outcome(&outcome);
        match outcome.status {
            D7OK => {
                let Some(body) = outcome.response_body.as_deref() else {
                    self.errors.act.set(
                        D7STA,
                        outcome.http_status,
                        outcome.discord_error_code,
                        WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
                        WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
                    );
                    return Response::empty(id, D7STA);
                };
                let Some(snapshot) =
                    self.validated_rest_channel(body, guild_id, channel_id, now_ms)
                else {
                    self.errors.act.set(
                        D7STA,
                        outcome.http_status,
                        outcome.discord_error_code,
                        WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
                        WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
                    );
                    return Response::empty(id, D7STA);
                };
                self.cache_rest_channel(snapshot.clone());
                if !snapshot.complete {
                    self.errors.act.set(
                        D7STA,
                        outcome.http_status,
                        outcome.discord_error_code,
                        WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
                        WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
                    );
                    return Response::empty(id, D7STA);
                }
                parent_category_response(id, snapshot.parent_channel_id)
            }
            D7NFD => {
                self.gateway
                    .snapshot_mut()
                    .tombstone_channel(channel_id, now_ms);
                self.errors.act.set(
                    D7NFD,
                    outcome.http_status,
                    outcome.discord_error_code,
                    WHY_CHANNEL_NOT_FOUND,
                    WHY_CHANNEL_NOT_FOUND,
                );
                Response::empty(id, D7NFD)
            }
            _ => {
                self.errors.act.set(
                    outcome.status,
                    outcome.http_status,
                    outcome.discord_error_code,
                    &outcome.why,
                    &outcome.why,
                );
                Response::empty(id, outcome.status)
            }
        }
    }

    fn role_count(&self, id: u64) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(id, self.getter_status());
        }
        let Some(event) = &self.current_event else {
            return Response::empty(id, D7STA);
        };
        if event.event_type != D7MSG && event.role_ids.is_empty() {
            return Response::empty(id, D7STA);
        }
        Response {
            id,
            status: D7OK,
            fields: vec![Field::raw("count", event.role_ids.len().to_string())],
        }
    }

    fn role_at(&self, request: Request) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, self.getter_status());
        }
        let Some(event) = &self.current_event else {
            return Response::empty(request.id, D7STA);
        };
        if event.event_type != D7MSG && event.role_ids.is_empty() {
            return Response::empty(request.id, D7STA);
        }
        let Some(index) = request
            .field("index")
            .and_then(|value| value.parse::<usize>().ok())
        else {
            return Response::empty(request.id, D7ARG);
        };
        if index == 0 || index > event.role_ids.len() {
            return Response::empty(request.id, D7ARG);
        }
        Response {
            id: request.id,
            status: D7OK,
            fields: vec![Field::raw("role_id", event.role_ids[index - 1].clone())],
        }
    }

    fn message_search(&mut self, request: Request) -> Response {
        self.last_message_search.clear();
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, self.state_status());
        }
        if !self.validate_request_ids(&request, &["guild_id", "user_id", "base_message_id"]) {
            return Response::empty(request.id, D7ARG);
        }
        let Some(before_seconds) = parse_u64_field(&request, "before_seconds") else {
            return Response::empty(request.id, D7ARG);
        };
        let Some(after_seconds) = parse_u64_field(&request, "after_seconds") else {
            return Response::empty(request.id, D7ARG);
        };
        let Some(min_image_count) = parse_usize_field(&request, "min_image_count") else {
            return Response::empty(request.id, D7ARG);
        };
        let Some(limit) = parse_usize_field(&request, "limit") else {
            return Response::empty(request.id, D7ARG);
        };
        if limit == 0 {
            return Response::empty(request.id, D7ARG);
        }
        let guild_id = request.field("guild_id").unwrap_or_default().to_string();
        let user_id = request.field("user_id").unwrap_or_default().to_string();
        let base_message_id = request
            .field("base_message_id")
            .unwrap_or_default()
            .to_string();
        let Some(base_timestamp_ms) = snowflake_timestamp_ms(&base_message_id) else {
            return Response::empty(request.id, D7ARG);
        };
        let before_ms = i128::from(before_seconds.saturating_mul(1_000));
        let after_ms = i128::from(after_seconds.saturating_mul(1_000));
        let now_ms = now_ms_u64();
        let effective_limit = limit.min(MAX_MESSAGE_SEARCH_RESULTS);
        let mut results = self
            .gateway
            .snapshot_mut()
            .message_refs_for_user(&guild_id, &user_id, now_ms)
            .filter_map(|snapshot| {
                if snapshot.message_id == base_message_id
                    || !snapshot.complete
                    || snapshot.author_is_bot
                    || snapshot.image_attachment_count < min_image_count
                    || !matches!(
                        message_delete_policy(snapshot.message_type),
                        MessageDeletePolicy::Deletable
                    )
                {
                    return None;
                }
                let candidate_timestamp_ms = snowflake_timestamp_ms(&snapshot.message_id)?;
                let delta_ms = i128::from(candidate_timestamp_ms) - i128::from(base_timestamp_ms);
                if delta_ms < -before_ms || delta_ms > after_ms {
                    return None;
                }
                Some(MessageSearchResult {
                    channel_id: snapshot.channel_id.clone(),
                    message_id: snapshot.message_id.clone(),
                    image_attachment_count: snapshot.image_attachment_count,
                    delta_seconds: signed_ms_to_seconds(delta_ms),
                })
            })
            .collect::<Vec<_>>();
        results.sort_by(|left, right| {
            left.delta_seconds
                .cmp(&right.delta_seconds)
                .then_with(|| left.channel_id.cmp(&right.channel_id))
                .then_with(|| left.message_id.cmp(&right.message_id))
        });
        let total = results.len();
        let truncated = total > effective_limit;
        self.last_message_search = results.into_iter().take(effective_limit).collect();
        Response {
            id: request.id,
            status: D7OK,
            fields: vec![
                Field::raw("count", self.last_message_search.len().to_string()),
                Field::raw("truncated", if truncated { "1" } else { "0" }),
            ],
        }
    }

    fn message_search_item(&self, request: Request) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, self.state_status());
        }
        let Some(index) = parse_usize_field(&request, "index") else {
            return Response::empty(request.id, D7ARG);
        };
        if index == 0 || index > self.last_message_search.len() {
            return Response::empty(request.id, D7ARG);
        }
        let item = &self.last_message_search[index - 1];
        Response {
            id: request.id,
            status: D7OK,
            fields: vec![
                Field::raw("channel_id", item.channel_id.clone()),
                Field::raw("message_id", item.message_id.clone()),
                Field::raw("image_count", item.image_attachment_count.to_string()),
                Field::raw("delta_seconds", item.delta_seconds.to_string()),
            ],
        }
    }

    fn snowflake_delta(&mut self, request: Request) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, self.state_status());
        }
        if !self.validate_request_ids(&request, &["id1", "id2"]) {
            return Response::empty(request.id, D7ARG);
        }
        let id1 = request.field("id1").unwrap_or_default();
        let id2 = request.field("id2").unwrap_or_default();
        let Some(left) = snowflake_timestamp_ms(id1) else {
            return Response::empty(request.id, D7ARG);
        };
        let Some(right) = snowflake_timestamp_ms(id2) else {
            return Response::empty(request.id, D7ARG);
        };
        Response {
            id: request.id,
            status: D7OK,
            fields: vec![Field::raw(
                "delta_seconds",
                signed_ms_to_seconds(i128::from(left) - i128::from(right)).to_string(),
            )],
        }
    }

    fn send_message(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = match (request.field("channel_id"), request.field("content")) {
            (Some(channel_id), Some(content)) => {
                let planned = self.rest.plan_send_message(channel_id, content);
                if planned.status != D7OK {
                    planned
                } else {
                    match self.send_action_precheck(channel_id, now_ms_u64()) {
                        Some(decision) => rest_outcome_from_preflight(decision),
                        None => planned,
                    }
                }
            }
            _ => RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
        };
        self.finish_action(id, RuntimeAction::SendMessage, outcome)
    }

    fn send_message_payload(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = match (request.field("channel_id"), request.field("payload")) {
            (Some(channel_id), Some(payload)) => {
                let planned = self.rest.plan_send_message_payload(channel_id, payload);
                if planned.status != D7OK {
                    planned
                } else {
                    match self.send_action_precheck(channel_id, now_ms_u64()) {
                        Some(decision) => rest_outcome_from_preflight(decision),
                        None => planned,
                    }
                }
            }
            _ => RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
        };
        self.finish_action(id, RuntimeAction::SendMessagePayload, outcome)
    }

    fn send_message_stickers(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = 'build: {
            let Some(channel_id) = request.field("channel_id") else {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            };
            let Some(count) = request
                .field("count")
                .and_then(|value| value.parse::<usize>().ok())
            else {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            };
            if count == 0 || count > MAX_REST_STICKERS {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            }
            let content = request.field("content").unwrap_or("");
            let mut sticker_ids = Vec::with_capacity(count);
            for index in 1..=count {
                let key = format!("sticker_id{}", index);
                let Some(sticker_id) = request.field(&key) else {
                    break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
                };
                sticker_ids.push(sticker_id.to_string());
            }
            let planned = self
                .rest
                .plan_send_message_stickers(channel_id, content, sticker_ids);
            if planned.status != D7OK {
                planned
            } else {
                match self.send_action_precheck(channel_id, now_ms_u64()) {
                    Some(decision) => rest_outcome_from_preflight(decision),
                    None => planned,
                }
            }
        };
        self.finish_action(id, RuntimeAction::SendMessageStickers, outcome)
    }

    fn send_message_poll(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = 'build: {
            let Some(channel_id) = request.field("channel_id") else {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            };
            let Some(question) = request.field("question") else {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            };
            let Some(count) = request
                .field("count")
                .and_then(|value| value.parse::<usize>().ok())
            else {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            };
            if !(2..=MAX_REST_POLL_ANSWERS).contains(&count) {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            }
            let Some(duration_hours) = request
                .field("duration_hours")
                .and_then(|value| value.parse::<u32>().ok())
            else {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            };
            let Some(allow_multiselect) =
                request
                    .field("allow_multiselect")
                    .and_then(|value| match value {
                        "0" => Some(false),
                        "1" => Some(true),
                        _ => None,
                    })
            else {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            };
            let content = request.field("content").unwrap_or("");
            let mut answers = Vec::with_capacity(count);
            for index in 1..=count {
                let key = format!("answer{}", index);
                let Some(answer) = request.field(&key) else {
                    break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
                };
                answers.push(answer.to_string());
            }
            let planned = self.rest.plan_send_message_poll(
                channel_id,
                content,
                question,
                answers,
                duration_hours,
                allow_multiselect,
            );
            if planned.status != D7OK {
                planned
            } else {
                match self.send_action_precheck(channel_id, now_ms_u64()) {
                    Some(decision) => rest_outcome_from_preflight(decision),
                    None => planned,
                }
            }
        };
        self.finish_action(id, RuntimeAction::SendMessagePoll, outcome)
    }

    fn send_message_attachment(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = match (
            request.field("channel_id"),
            request.field("file_path"),
            request.field("file_name"),
        ) {
            (Some(channel_id), Some(file_path), Some(file_name)) => {
                let content = request.field("content").unwrap_or("");
                if !validate_snowflake(channel_id)
                    || (!content.is_empty() && validate_message_content(content) != D7OK)
                    || !validate_attachment_file_name(file_name)
                {
                    RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY)
                } else if let Some(decision) = self.send_action_precheck(channel_id, now_ms_u64()) {
                    rest_outcome_from_preflight(decision)
                } else {
                    match read_attachment_file(file_path, MAX_REST_ATTACHMENT_BYTES) {
                        Ok(file_bytes) => self.rest.plan_send_message_attachment(
                            channel_id, content, file_name, file_bytes,
                        ),
                        Err(_) => RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
                    }
                }
            }
            _ => RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
        };
        self.finish_action(id, RuntimeAction::SendMessageAttachment, outcome)
    }

    fn send_message_attachments(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = 'build: {
            let Some(channel_id) = request.field("channel_id") else {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            };
            let Some(count) = request
                .field("count")
                .and_then(|value| value.parse::<usize>().ok())
            else {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            };
            if count == 0 || count > MAX_REST_ATTACHMENTS {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            }
            let content = request.field("content").unwrap_or("");
            if !validate_snowflake(channel_id)
                || (!content.is_empty() && validate_message_content(content) != D7OK)
            {
                break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            }
            let mut paths = Vec::with_capacity(count);
            for index in 1..=count {
                let path_key = format!("file_path{}", index);
                let name_key = format!("file_name{}", index);
                let (Some(file_path), Some(file_name)) =
                    (request.field(&path_key), request.field(&name_key))
                else {
                    break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
                };
                if !validate_attachment_file_name(file_name) {
                    break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
                }
                paths.push((file_path.to_string(), file_name.to_string()));
            }
            if let Some(decision) = self.send_action_precheck(channel_id, now_ms_u64()) {
                break 'build rest_outcome_from_preflight(decision);
            }
            let mut remaining_limit = MAX_REST_ATTACHMENT_BYTES;
            let mut files = Vec::with_capacity(count);
            for (file_path, file_name) in paths {
                let file_bytes = match read_attachment_file(&file_path, remaining_limit) {
                    Ok(file_bytes) => file_bytes,
                    Err(_) => break 'build RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
                };
                remaining_limit = remaining_limit.saturating_sub(file_bytes.len());
                files.push(AttachmentUpload {
                    file_name,
                    file_bytes,
                });
            }
            self.rest
                .plan_send_message_attachments(channel_id, content, files)
        };
        self.finish_action(id, RuntimeAction::SendMessageAttachments, outcome)
    }

    fn interaction_callback(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = match (
            request.field("interaction_id"),
            request.field("interaction_token"),
            request.field("payload"),
        ) {
            (Some(interaction_id), Some(interaction_token), Some(payload)) => self
                .rest
                .plan_interaction_callback(interaction_id, interaction_token, payload),
            _ => RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
        };
        self.finish_action(id, RuntimeAction::InteractionCallback, outcome)
    }

    fn application_command(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let Some(application_id) = request.field("application_id") else {
            return self.action_arg_error(id);
        };
        let method = match parse_http_method(request.field("method")) {
            Some(method) => method,
            None => return self.action_arg_error(id),
        };
        let guild_id = non_empty(request.field("guild_id").unwrap_or(""));
        let command_id = non_empty(request.field("command_id").unwrap_or(""));
        let payload = non_empty(request.field("payload").unwrap_or(""));
        let outcome = self.rest.plan_application_command(
            application_id,
            guild_id,
            command_id,
            method,
            payload,
        );
        self.finish_action(id, RuntimeAction::ApplicationCommand, outcome)
    }

    fn typing_indicator(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = match request.field("channel_id") {
            Some(channel_id) => {
                let planned = self.rest.plan_typing_indicator(channel_id);
                if planned.status != D7OK {
                    planned
                } else {
                    match self.send_action_precheck(channel_id, now_ms_u64()) {
                        Some(decision) => rest_outcome_from_preflight(decision),
                        None => planned,
                    }
                }
            }
            None => RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
        };
        self.finish_action(id, RuntimeAction::TypingIndicator, outcome)
    }

    fn message_reaction(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let Some(operation) = parse_reaction_operation(request.field("mode")) else {
            return self.action_arg_error(id);
        };
        let outcome = match (request.field("channel_id"), request.field("message_id")) {
            (Some(channel_id), Some(message_id)) => {
                let emoji = request.field("emoji").unwrap_or("");
                let user_id = non_empty(request.field("user_id").unwrap_or(""));
                self.rest.plan_message_reaction(
                    channel_id,
                    message_id,
                    emoji,
                    user_id.as_deref(),
                    operation,
                )
            }
            _ => RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
        };
        self.finish_action(id, RuntimeAction::MessageReaction, outcome)
    }

    fn raw_rest(&mut self, request: Request) -> Response {
        self.last_raw_rest_body = None;
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let Some(method) = parse_http_method(request.field("method")) else {
            return self.action_arg_error(id);
        };
        let Some(path) = request.field("path") else {
            return self.action_arg_error(id);
        };
        let payload = non_empty(request.field("payload").unwrap_or(""));
        let reason = request.field("reason").unwrap_or("");
        let outcome = self
            .rest
            .plan_raw_json_request(method, path, payload, reason);
        self.finish_action(id, RuntimeAction::RawRest, outcome)
    }

    fn raw_rest_body(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let Some(buf_len) = request
            .field("buf_len")
            .and_then(|value| value.parse::<usize>().ok())
        else {
            return self.action_arg_error(request.id);
        };
        let Some(body) = self.last_raw_rest_body.as_deref() else {
            return Response::empty(request.id, D7STA);
        };
        let (prefix, truncated) = raw_rest_body_prefix(body, buf_len);
        Response {
            id: request.id,
            status: if truncated { D7TRN } else { D7OK },
            fields: vec![
                Field::percent("body", prefix),
                Field::raw("out_len", body.len().to_string()),
                Field::raw("truncated", if truncated { "1" } else { "0" }),
            ],
        }
    }

    fn delete_message(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = match (request.field("channel_id"), request.field("message_id")) {
            (Some(channel_id), Some(message_id)) => {
                let reason = request.field("reason").unwrap_or("");
                let planned = self
                    .rest
                    .plan_delete_message(channel_id, message_id, reason);
                if planned.status != D7OK {
                    planned
                } else {
                    match self.delete_action_precheck(channel_id, message_id, now_ms_u64()) {
                        Some(decision) => rest_outcome_from_preflight(decision),
                        None => planned,
                    }
                }
            }
            _ => RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
        };
        self.finish_action(id, RuntimeAction::DeleteMessage, outcome)
    }

    fn timeout_member(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = match (
            request.field("guild_id"),
            request.field("user_id"),
            request
                .field("seconds")
                .and_then(|value| value.parse::<u64>().ok()),
        ) {
            (Some(guild_id), Some(user_id), Some(seconds)) => {
                let reason = request.field("reason").unwrap_or("");
                let now = now_ms_u64();
                let planned = self.rest.plan_timeout_member(
                    guild_id,
                    user_id,
                    seconds,
                    reason,
                    u128::from(now),
                );
                if planned.status != D7OK {
                    planned
                } else {
                    match moderation_action_precheck(
                        self.timeout_preflight_decision(guild_id, user_id, now),
                    ) {
                        Some(decision) => rest_outcome_from_preflight(decision),
                        None => planned,
                    }
                }
            }
            _ => RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
        };
        self.finish_action(id, RuntimeAction::TimeoutMember, outcome)
    }

    fn untimeout_member(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let id = request.id;
        let outcome = match (request.field("guild_id"), request.field("user_id")) {
            (Some(guild_id), Some(user_id)) => {
                let reason = request.field("reason").unwrap_or("");
                let planned = self.rest.plan_untimeout_member(guild_id, user_id, reason);
                if planned.status != D7OK {
                    planned
                } else {
                    match self.untimeout_action_precheck(guild_id, user_id, now_ms_u64()) {
                        Some(decision) => rest_outcome_from_preflight(decision),
                        None => planned,
                    }
                }
            }
            _ => RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY),
        };
        self.finish_action(id, RuntimeAction::UntimeoutMember, outcome)
    }

    fn err(&self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let Some(scope) = parse_scope(request.field("scope")) else {
            return Response::empty(request.id, D7ARG);
        };
        let error = self.errors.get(scope);
        Response {
            id: request.id,
            status: D7OK,
            fields: vec![
                Field::raw("errcode", error.errcode.to_string()),
                Field::raw("http", error.http.to_string()),
                Field::raw("discord_code", error.discord_code.to_string()),
                Field::percent("message", error.message.clone()),
            ],
        }
    }

    fn why(&self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let Some(scope) = parse_scope(request.field("scope")) else {
            return Response::empty(request.id, D7ARG);
        };
        let error = self.errors.get(scope);
        Response {
            id: request.id,
            status: D7OK,
            fields: vec![Field::raw("message", error.why.clone())],
        }
    }

    fn log(&self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let Some(level) = request
            .field("level")
            .and_then(|value| value.parse::<i32>().ok())
        else {
            return Response::empty(request.id, D7ARG);
        };
        if !(0..=3).contains(&level) || request.field("message").is_none() {
            return Response::empty(request.id, D7ARG);
        }
        let message = request.field("message").unwrap_or_default();
        write_diagnostic(format_args!("dfor77 {} {}", log_level_name(level), message));
        Response::empty(request.id, D7OK)
    }

    fn gateway_opcode(&self, id: u64) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(id, self.getter_status());
        }
        let Some(event) = &self.current_event else {
            return Response::empty(id, D7STA);
        };
        if event.event_type != D7GRL {
            return Response::empty(id, D7STA);
        }
        Response {
            id,
            status: D7OK,
            fields: vec![Field::raw("opcode", event.gateway_opcode.to_string())],
        }
    }

    fn gateway_retry_ms(&self, id: u64) -> Response {
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(id, self.getter_status());
        }
        let Some(event) = &self.current_event else {
            return Response::empty(id, D7STA);
        };
        if event.event_type != D7GRL {
            return Response::empty(id, D7STA);
        }
        Response {
            id,
            status: D7OK,
            fields: vec![Field::raw("retry_ms", event.gateway_retry_ms.to_string())],
        }
    }

    fn protocol_error(&mut self, id: u64, message: &str) -> Response {
        self.errors
            .sip
            .set(D7PRT, 0, 0, message, "IPC_PROTOCOL_VIOLATION");
        Response::empty(id, D7PRT)
    }

    fn action_arg_error(&mut self, id: u64) -> Response {
        self.errors
            .act
            .set(D7ARG, 0, 0, WHY_INVALID_FORM_BODY, WHY_INVALID_FORM_BODY);
        Response::empty(id, D7ARG)
    }

    fn state_status(&self) -> i32 {
        match self.lifecycle {
            Lifecycle::Ended => D7EOF,
            Lifecycle::Uninitialized | Lifecycle::Initialized => D7STA,
        }
    }

    fn getter_status(&self) -> i32 {
        D7STA
    }

    fn drain_gateway_events(&mut self, now_ms: u64) -> GatewayAction {
        let mut action = GatewayAction::None;
        while let Some(event) = self.gateway.pop_event() {
            if event.event_type == D7GRL {
                if let GatewayEventData::RateLimit(limit) = &event.data {
                    self.gateway_outbound.observe_rate_limit(limit, now_ms);
                } else if let (Some(opcode), Some(retry_ms)) = (event.opcode, event.retry_ms) {
                    self.gateway_outbound.observe_rate_limit(
                        &crate::gateway::GatewayRateLimit {
                            opcode,
                            retry_ms,
                            guild_id: None,
                            nonce: None,
                        },
                        now_ms,
                    );
                }
            }
            if let GatewayEventData::MembersChunk {
                guild_id, user_ids, ..
            } = &event.data
            {
                self.invalidate_event_permissions_for_members(guild_id, user_ids);
            }
            let event = Event::from_gateway_event(event, now_ms);
            if self.settings.runtime_log_gateway_events {
                write_diagnostic(format_args!(
                    "dfor77 info gateway event name={} type={} guild={} channel={} message={} user={} images={} bot={}",
                    diagnostic_field(&event.event_name),
                    event.event_type,
                    log_value_or_dash(&event.guild_id),
                    log_value_or_dash(&event.channel_id),
                    log_value_or_dash(&event.message_id),
                    log_value_or_dash(&event.user_id),
                    event.image_attachment_count,
                    event.author_is_bot
                ));
            }
            if self.push_event(event) && action == GatewayAction::None {
                action = self.gateway.machine_mut().event_queue_overflow();
            }
        }
        action
    }

    fn classify_interaction_deadline_error(&self, outcome: &mut RestOutcome, now_ms: u64) {
        if outcome.http_status != 404 || outcome.discord_error_code != 10062 {
            return;
        }
        let Some(request) = &outcome.request else {
            return;
        };
        if request.action != RestAction::InteractionCallback {
            return;
        }
        if !self.interaction_callback_request_matches_current_event(request) {
            return;
        }
        let Some(event) = &self.current_event else {
            return;
        };
        if now_ms.saturating_sub(event.received_at_ms) > INTERACTION_INITIAL_RESPONSE_DEADLINE_MS {
            outcome.status = D7TIM;
            outcome.why = WHY_INTERACTION_DEADLINE_EXCEEDED.to_string();
        }
    }

    fn interaction_callback_request_matches_current_event(&self, request: &RestRequest) -> bool {
        let Some(event) = &self.current_event else {
            return false;
        };
        if event.event_type != D7ITR {
            return false;
        }
        request.path
            == format!(
                "/api/v10/interactions/{}/{}/callback",
                event.interaction_id, event.interaction_token
            )
    }

    fn push_event(&mut self, event: Event) -> bool {
        match event.event_type {
            D7RDY | D7GAP => {
                if event.event_type == D7RDY || self.gateway_resume_session_invalidated() {
                    self.bot_user_id_cache = None;
                }
                self.last_message_search.clear();
                self.invalidate_event_permissions(None, None);
            }
            D7EGC | D7EGU | D7EGX | D7ERC | D7ERU | D7ERD => {
                self.invalidate_event_permissions(Some(&event.guild_id), None)
            }
            D7MBA | D7MBU | D7MBR => {
                self.invalidate_event_permissions(Some(&event.guild_id), Some(&event.user_id))
            }
            _ => {}
        }
        let epoch = self.events.gap_epoch();
        let overflowed = self.events.push(event);
        if self.events.gap_epoch() != epoch {
            self.gateway.snapshot_mut().mark_gap();
            self.last_message_search.clear();
            if let Some(event) = &mut self.current_event {
                event.member_permissions = None;
            }
        }
        overflowed
    }

    fn gateway_startup(
        &self,
        status: i32,
        why: &str,
        action: GatewayAction,
        identify_payload: Option<String>,
    ) -> GatewayStartup {
        let info = self.gateway_manager.gateway_bot_info();
        let limit = info.map(|info| &info.session_start_limit);
        GatewayStartup {
            status,
            why: why.to_string(),
            action,
            gateway_url: info.map(|info| info.url.clone()),
            identify_payload,
            identify_bucket: self.gateway_manager.identify_bucket(),
            identify_remaining: limit.map(|limit| limit.remaining),
            identify_reset_after_ms: limit.map(|limit| limit.reset_after_ms),
            daily_identify_count: self.gateway_manager.daily_budget().count(),
            daily_identify_remaining: self.gateway_manager.daily_budget().remaining(),
            daily_identify_warning: self.gateway_manager.daily_budget().should_warn(),
        }
    }

    fn refresh_gateway_connect_budget_fields(&self, result: &mut GatewayConnectResult) {
        result.startup.identify_remaining = self
            .gateway_manager
            .gateway_bot_info()
            .map(|info| info.session_start_limit.remaining);
        result.startup.daily_identify_count = self.gateway_manager.daily_budget().count();
        result.startup.daily_identify_remaining = self.gateway_manager.daily_budget().remaining();
        result.startup.daily_identify_warning = self.gateway_manager.daily_budget().should_warn();
    }

    fn receive_gateway_frame<T: GatewayTransport>(
        &mut self,
        transport: &mut T,
        now_ms: u64,
        result: &mut GatewayConnectResult,
        allow_no_frame: bool,
    ) -> i32 {
        match transport.receive_text() {
            Ok(Some(frame)) => match self.ingest_gateway_json(&frame, now_ms) {
                Ok(action) => {
                    result.received_frames += 1;
                    result.last_action = action;
                    D7OK
                }
                Err(error) => {
                    self.errors
                        .sev
                        .set(D7GWL, 0, 0, &error.to_string(), "GATEWAY_FRAME_INVALID");
                    result.status = D7GWL;
                    result.why = "GATEWAY_FRAME_INVALID".to_string();
                    D7GWL
                }
            },
            Ok(None) => D7OK,
            Err(GatewayTransportError::NoFrame) if allow_no_frame => D7OK,
            Err(error) => {
                self.errors.sev.set(D7NET, 0, 0, error.why(), error.why());
                result.status = D7NET;
                result.why = error.why().to_string();
                D7NET
            }
        }
    }

    fn finish_action(&mut self, id: u64, action: RuntimeAction, planned: RestOutcome) -> Response {
        let should_execute = planned.status == D7OK
            && !self.dry_run
            && (!self.log_only || action.executes_in_log_only())
            && self.rest.can_execute();
        let now_ms = now_ms_u64();
        let mut outcome = if should_execute {
            self.rest.execute_planned(planned, now_ms)
        } else {
            planned
        };
        if action == RuntimeAction::InteractionCallback {
            self.classify_interaction_deadline_error(&mut outcome, now_ms);
        }
        self.observe_rest_outcome(&outcome);

        let (status, why) = if outcome.status == D7SKP {
            self.errors.act.clear();
            (D7SKP, outcome.why.clone())
        } else if outcome.status != D7OK {
            self.errors.act.set(
                outcome.status,
                outcome.http_status,
                outcome.discord_error_code,
                &outcome.why,
                &outcome.why,
            );
            (outcome.status, outcome.why.clone())
        } else if self.dry_run {
            self.errors.act.clear();
            (D7DRY, WHY_OK.to_string())
        } else if self.log_only && !action.executes_in_log_only() {
            self.errors.act.clear();
            (D7SKP, WHY_OK.to_string())
        } else if self.rest.can_execute() {
            self.errors.act.clear();
            (D7OK, WHY_OK.to_string())
        } else {
            self.errors
                .act
                .set(D7STA, 0, 0, action.snapshot_why(), action.snapshot_why());
            (D7STA, action.snapshot_why().to_string())
        };

        if action == RuntimeAction::RawRest {
            self.last_raw_rest_body = if status == D7OK {
                outcome.response_body.clone()
            } else {
                None
            };
        }

        if self
            .write_action_audit(id, action.audit_name(), &outcome, status, &why)
            .is_err()
        {
            self.errors.act.set(
                D7INT,
                0,
                0,
                "audit log write failed",
                "AUDIT_LOG_WRITE_FAILED",
            );
            return Response::empty(id, D7INT);
        }
        if self.should_write_runtime_rest_action_log(&outcome, status) {
            let _ =
                self.write_runtime_rest_action_log(id, action.audit_name(), &outcome, status, &why);
        }
        Response::empty(id, status)
    }

    fn should_write_runtime_rest_action_log(&self, outcome: &RestOutcome, status: i32) -> bool {
        self.settings.runtime_log_rest_success || status != D7OK || outcome.status != D7OK
    }

    fn write_runtime_rest_action_log(
        &mut self,
        id: u64,
        action: &str,
        outcome: &RestOutcome,
        status: i32,
        why: &str,
    ) -> io::Result<()> {
        let mut fields = vec![
            RuntimeLogField::string("kind", "rest_action"),
            RuntimeLogField::u64("ipc_request_id", id),
            RuntimeLogField::string("action", action),
            RuntimeLogField::i32("status", status),
            RuntimeLogField::string("why", why),
            RuntimeLogField::i32("outcome_status", outcome.status),
            RuntimeLogField::string("outcome_why", &outcome.why),
            RuntimeLogField::string("commit_state", outcome.commit_state.as_str()),
            RuntimeLogField::i32("http_status", outcome.http_status),
            RuntimeLogField::i32("discord_error_code", outcome.discord_error_code),
            RuntimeLogField::bool("dry_run", self.dry_run),
            RuntimeLogField::bool("log_only", self.log_only),
        ];
        match self.bot_process_pid {
            Some(pid) => fields.push(RuntimeLogField::u32("bot_process_pid", pid)),
            None => fields.push(RuntimeLogField::null("bot_process_pid")),
        }
        if let Some(request) = &outcome.request {
            fields.push(RuntimeLogField::string(
                "http_method",
                http_method_name(request.method),
            ));
            fields.push(RuntimeLogField::string("route", request.route_template));
            fields.push(RuntimeLogField::string(
                "major_parameter",
                &request.major_parameter,
            ));
            fields.push(RuntimeLogField::string(
                "rest_action",
                rest_action_name(request.action),
            ));
        } else {
            fields.push(RuntimeLogField::null("http_method"));
            fields.push(RuntimeLogField::null("route"));
            fields.push(RuntimeLogField::null("major_parameter"));
            fields.push(RuntimeLogField::null("rest_action"));
        }
        let event_type = self.current_event.as_ref().map(|event| event.event_type);
        let guild_id = self
            .current_event
            .as_ref()
            .and_then(|event| non_empty(&event.guild_id));
        let channel_id = self
            .current_event
            .as_ref()
            .and_then(|event| non_empty(&event.channel_id));
        let message_id = self
            .current_event
            .as_ref()
            .and_then(|event| non_empty(&event.message_id));
        let user_id = self
            .current_event
            .as_ref()
            .and_then(|event| non_empty(&event.user_id));
        let gateway_sequence = self
            .current_event
            .as_ref()
            .and_then(|event| event.gateway_sequence);
        if let Some(event_type) = event_type {
            fields.push(RuntimeLogField::i32("gateway_event_type", event_type));
            push_runtime_owned_string(&mut fields, "guild_id", guild_id.as_deref());
            push_runtime_owned_string(&mut fields, "channel_id", channel_id.as_deref());
            push_runtime_owned_string(&mut fields, "message_id", message_id.as_deref());
            push_runtime_owned_string(&mut fields, "user_id", user_id.as_deref());
            match gateway_sequence {
                Some(seq) => fields.push(RuntimeLogField::i64("gateway_sequence", seq)),
                None => fields.push(RuntimeLogField::null("gateway_sequence")),
            }
        } else {
            fields.push(RuntimeLogField::null("gateway_event_type"));
            fields.push(RuntimeLogField::null("guild_id"));
            fields.push(RuntimeLogField::null("channel_id"));
            fields.push(RuntimeLogField::null("message_id"));
            fields.push(RuntimeLogField::null("user_id"));
            fields.push(RuntimeLogField::null("gateway_sequence"));
        }
        self.write_runtime_event("runtime", &fields)
    }

    fn write_action_audit(
        &mut self,
        id: u64,
        action: &str,
        outcome: &RestOutcome,
        status: i32,
        why: &str,
    ) -> io::Result<()> {
        let Some(logger) = &mut self.audit else {
            return Ok(());
        };
        let content_policy = logger.content_policy().clone();
        let mut record = AuditRecord::action_with_content_policy(
            action,
            outcome,
            self.dry_run,
            self.log_only,
            &content_policy,
        );
        if outcome.why == WHY_AUTH_INVALID {
            record.fatal_reason = Some(WHY_REST_AUTH_FATAL.to_string());
        }
        record.ipc_request_id = Some(id);
        record.bot_process_pid = self.bot_process_pid;
        record.status = status;
        record.why = why.to_string();
        if let Some(event) = &self.current_event {
            record.event_type = Some(event.event_type);
            record.guild_id = non_empty(&event.guild_id);
            record.channel_id = non_empty(&event.channel_id);
            record.message_id = non_empty(&event.message_id);
            record.user_id = non_empty(&event.user_id);
            record.gateway_sequence = event.gateway_sequence;
            record.runtime_event_seq = event.gateway_sequence;
        }
        logger.append(&record)
    }

    fn write_gateway_queue_audit(
        &mut self,
        id: u64,
        action: &str,
        status: i32,
        why: &str,
        payload_bytes: Option<usize>,
        remaining: Option<usize>,
    ) -> io::Result<()> {
        let Some(logger) = &mut self.audit else {
            return Ok(());
        };
        let mut record = AuditRecord::gateway_queue(
            action,
            status,
            why,
            self.dry_run,
            self.log_only,
            payload_bytes,
            remaining,
        );
        record.ipc_request_id = Some(id);
        record.bot_process_pid = self.bot_process_pid;
        if let Some(event) = &self.current_event {
            record.event_type = Some(event.event_type);
            record.guild_id = non_empty(&event.guild_id);
            record.channel_id = non_empty(&event.channel_id);
            record.message_id = non_empty(&event.message_id);
            record.user_id = non_empty(&event.user_id);
            record.gateway_sequence = event.gateway_sequence;
            record.runtime_event_seq = event.gateway_sequence;
        }
        logger.append(&record)
    }

    fn write_gateway_identify_warning_audit(
        &mut self,
        result: &GatewayConnectResult,
    ) -> io::Result<()> {
        let Some(logger) = &mut self.audit else {
            return Ok(());
        };
        let mut record = AuditRecord::gateway_identify_warning(
            result.startup.daily_identify_count,
            result.startup.daily_identify_remaining,
            result.startup.identify_bucket,
            result.startup.identify_remaining,
            result.startup.identify_reset_after_ms,
            result.gateway_outbound_remaining,
            result.sent_payload_bytes,
        );
        record.bot_process_pid = self.bot_process_pid;
        logger.append(&record)
    }

    fn flush_audit(&mut self) -> io::Result<()> {
        match &mut self.audit {
            Some(logger) => logger.flush(),
            None => Ok(()),
        }
    }

    fn flush_runtime_log(&mut self) -> io::Result<()> {
        match &mut self.runtime_log {
            Some(logger) => logger.flush(),
            None => Ok(()),
        }
    }

    fn observe_rest_outcome(&mut self, outcome: &RestOutcome) {
        if outcome.why == WHY_AUTH_INVALID {
            self.rest_auth_fatal = true;
        }
        self.observe_rest_snapshots(outcome);
    }
}

#[derive(Debug)]
enum RuntimeRest {
    PlanOnly(RestPlanner),
    Mock(RestExecutor<MockRestBackend>),
    Http(RestExecutor<HttpRestBackend>),
}

impl RuntimeRest {
    fn can_execute(&self) -> bool {
        !matches!(self, Self::PlanOnly(_))
    }

    fn plan_send_message(&mut self, channel_id: &str, content: &str) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.send_message(channel_id, content),
            Self::Mock(executor) => executor.planner.send_message(channel_id, content),
            Self::Http(executor) => executor.planner.send_message(channel_id, content),
        }
    }

    fn plan_send_message_payload(&mut self, channel_id: &str, payload_json: &str) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.send_message_payload(channel_id, payload_json),
            Self::Mock(executor) => executor
                .planner
                .send_message_payload(channel_id, payload_json),
            Self::Http(executor) => executor
                .planner
                .send_message_payload(channel_id, payload_json),
        }
    }

    fn plan_send_message_stickers(
        &mut self,
        channel_id: &str,
        content: &str,
        sticker_ids: Vec<String>,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => {
                planner.send_message_stickers(channel_id, content, sticker_ids)
            }
            Self::Mock(executor) => {
                executor
                    .planner
                    .send_message_stickers(channel_id, content, sticker_ids)
            }
            Self::Http(executor) => {
                executor
                    .planner
                    .send_message_stickers(channel_id, content, sticker_ids)
            }
        }
    }

    fn plan_send_message_poll(
        &mut self,
        channel_id: &str,
        content: &str,
        question: &str,
        answers: Vec<String>,
        duration_hours: u32,
        allow_multiselect: bool,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.send_message_poll(
                channel_id,
                content,
                question,
                answers,
                duration_hours,
                allow_multiselect,
            ),
            Self::Mock(executor) => executor.planner.send_message_poll(
                channel_id,
                content,
                question,
                answers,
                duration_hours,
                allow_multiselect,
            ),
            Self::Http(executor) => executor.planner.send_message_poll(
                channel_id,
                content,
                question,
                answers,
                duration_hours,
                allow_multiselect,
            ),
        }
    }

    fn plan_send_message_attachment(
        &mut self,
        channel_id: &str,
        content: &str,
        file_name: &str,
        file_bytes: Vec<u8>,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => {
                planner.send_message_attachment(channel_id, content, file_name, file_bytes)
            }
            Self::Mock(executor) => executor
                .planner
                .send_message_attachment(channel_id, content, file_name, file_bytes),
            Self::Http(executor) => executor
                .planner
                .send_message_attachment(channel_id, content, file_name, file_bytes),
        }
    }

    fn plan_send_message_attachments(
        &mut self,
        channel_id: &str,
        content: &str,
        files: Vec<AttachmentUpload>,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.send_message_attachments(channel_id, content, files),
            Self::Mock(executor) => executor
                .planner
                .send_message_attachments(channel_id, content, files),
            Self::Http(executor) => executor
                .planner
                .send_message_attachments(channel_id, content, files),
        }
    }

    fn plan_interaction_callback(
        &mut self,
        interaction_id: &str,
        interaction_token: &str,
        payload_json: &str,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => {
                planner.interaction_callback(interaction_id, interaction_token, payload_json)
            }
            Self::Mock(executor) => executor.planner.interaction_callback(
                interaction_id,
                interaction_token,
                payload_json,
            ),
            Self::Http(executor) => executor.planner.interaction_callback(
                interaction_id,
                interaction_token,
                payload_json,
            ),
        }
    }

    fn plan_application_command(
        &mut self,
        application_id: &str,
        guild_id: Option<String>,
        command_id: Option<String>,
        method: HttpMethod,
        payload_json: Option<String>,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.application_command(
                application_id,
                guild_id.as_deref(),
                command_id.as_deref(),
                method,
                payload_json.as_deref(),
            ),
            Self::Mock(executor) => executor.planner.application_command(
                application_id,
                guild_id.as_deref(),
                command_id.as_deref(),
                method,
                payload_json.as_deref(),
            ),
            Self::Http(executor) => executor.planner.application_command(
                application_id,
                guild_id.as_deref(),
                command_id.as_deref(),
                method,
                payload_json.as_deref(),
            ),
        }
    }

    fn plan_typing_indicator(&mut self, channel_id: &str) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.typing_indicator(channel_id),
            Self::Mock(executor) => executor.planner.typing_indicator(channel_id),
            Self::Http(executor) => executor.planner.typing_indicator(channel_id),
        }
    }

    fn plan_message_reaction(
        &mut self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
        user_id: Option<&str>,
        operation: ReactionOperation,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => {
                planner.message_reaction(channel_id, message_id, emoji, user_id, operation)
            }
            Self::Mock(executor) => executor
                .planner
                .message_reaction(channel_id, message_id, emoji, user_id, operation),
            Self::Http(executor) => executor
                .planner
                .message_reaction(channel_id, message_id, emoji, user_id, operation),
        }
    }

    fn plan_raw_json_request(
        &mut self,
        method: HttpMethod,
        path: &str,
        payload_json: Option<String>,
        reason: &str,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => {
                planner.raw_json_request(method, path, payload_json.as_deref(), reason)
            }
            Self::Mock(executor) => {
                executor
                    .planner
                    .raw_json_request(method, path, payload_json.as_deref(), reason)
            }
            Self::Http(executor) => {
                executor
                    .planner
                    .raw_json_request(method, path, payload_json.as_deref(), reason)
            }
        }
    }

    fn execute_get_gateway_bot(&mut self, now_ms: u64) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.get_gateway_bot(),
            Self::Mock(executor) => executor.execute_get_gateway_bot(now_ms),
            Self::Http(executor) => executor.execute_get_gateway_bot(now_ms),
        }
    }

    fn execute_get_current_user(&mut self, now_ms: u64) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.get_current_user(),
            Self::Mock(executor) => executor.execute_get_current_user(now_ms),
            Self::Http(executor) => executor.execute_get_current_user(now_ms),
        }
    }

    fn execute_get_message(
        &mut self,
        channel_id: &str,
        message_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.get_message(channel_id, message_id),
            Self::Mock(executor) => executor.execute_get_message(channel_id, message_id, now_ms),
            Self::Http(executor) => executor.execute_get_message(channel_id, message_id, now_ms),
        }
    }

    fn execute_get_channel(&mut self, channel_id: &str, now_ms: u64) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.get_channel(channel_id),
            Self::Mock(executor) => executor.execute_get_channel(channel_id, now_ms),
            Self::Http(executor) => executor.execute_get_channel(channel_id, now_ms),
        }
    }

    fn execute_get_thread_member(
        &mut self,
        channel_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.get_thread_member(channel_id, user_id),
            Self::Mock(executor) => executor.execute_get_thread_member(channel_id, user_id, now_ms),
            Self::Http(executor) => executor.execute_get_thread_member(channel_id, user_id, now_ms),
        }
    }

    fn execute_get_joined_private_archived_threads(
        &mut self,
        channel_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.get_joined_private_archived_threads(channel_id),
            Self::Mock(executor) => {
                executor.execute_get_joined_private_archived_threads(channel_id, now_ms)
            }
            Self::Http(executor) => {
                executor.execute_get_joined_private_archived_threads(channel_id, now_ms)
            }
        }
    }

    fn execute_get_archived_private_threads(
        &mut self,
        channel_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.get_archived_private_threads(channel_id),
            Self::Mock(executor) => {
                executor.execute_get_archived_private_threads(channel_id, now_ms)
            }
            Self::Http(executor) => {
                executor.execute_get_archived_private_threads(channel_id, now_ms)
            }
        }
    }

    fn execute_get_archived_threads_page(
        &mut self,
        channel_id: &str,
        kind: ArchivedThreadKind,
        before: Option<&str>,
        limit: u16,
        now_ms: u64,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => {
                planner.get_archived_threads_page(channel_id, kind, before, limit)
            }
            Self::Mock(executor) => {
                executor.execute_get_archived_threads_page(channel_id, kind, before, limit, now_ms)
            }
            Self::Http(executor) => {
                executor.execute_get_archived_threads_page(channel_id, kind, before, limit, now_ms)
            }
        }
    }

    fn execute_get_guild(&mut self, guild_id: &str, now_ms: u64) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.get_guild(guild_id),
            Self::Mock(executor) => executor.execute_get_guild(guild_id, now_ms),
            Self::Http(executor) => executor.execute_get_guild(guild_id, now_ms),
        }
    }

    fn execute_get_guild_member(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.get_guild_member(guild_id, user_id),
            Self::Mock(executor) => executor.execute_get_guild_member(guild_id, user_id, now_ms),
            Self::Http(executor) => executor.execute_get_guild_member(guild_id, user_id, now_ms),
        }
    }

    fn execute_get_guild_roles(&mut self, guild_id: &str, now_ms: u64) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.get_guild_roles(guild_id),
            Self::Mock(executor) => executor.execute_get_guild_roles(guild_id, now_ms),
            Self::Http(executor) => executor.execute_get_guild_roles(guild_id, now_ms),
        }
    }

    fn plan_delete_message(
        &mut self,
        channel_id: &str,
        message_id: &str,
        reason: &str,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.delete_message(channel_id, message_id, reason),
            Self::Mock(executor) => executor
                .planner
                .delete_message(channel_id, message_id, reason),
            Self::Http(executor) => executor
                .planner
                .delete_message(channel_id, message_id, reason),
        }
    }

    fn plan_timeout_member(
        &mut self,
        guild_id: &str,
        user_id: &str,
        seconds: u64,
        reason: &str,
        now_ms: u128,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => {
                planner.timeout_member(guild_id, user_id, seconds, reason, now_ms)
            }
            Self::Mock(executor) => executor
                .planner
                .timeout_member(guild_id, user_id, seconds, reason, now_ms),
            Self::Http(executor) => executor
                .planner
                .timeout_member(guild_id, user_id, seconds, reason, now_ms),
        }
    }

    fn plan_untimeout_member(
        &mut self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
    ) -> RestOutcome {
        match self {
            Self::PlanOnly(planner) => planner.untimeout_member(guild_id, user_id, reason),
            Self::Mock(executor) => executor.planner.untimeout_member(guild_id, user_id, reason),
            Self::Http(executor) => executor.planner.untimeout_member(guild_id, user_id, reason),
        }
    }

    fn execute_planned(&mut self, planned: RestOutcome, now_ms: u64) -> RestOutcome {
        match self {
            Self::PlanOnly(_) => planned,
            Self::Mock(executor) => executor.execute_planned(planned, now_ms),
            Self::Http(executor) => executor.execute_planned(planned, now_ms),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeAction {
    SendMessage,
    SendMessagePayload,
    SendMessageStickers,
    SendMessagePoll,
    SendMessageAttachment,
    SendMessageAttachments,
    InteractionCallback,
    ApplicationCommand,
    TypingIndicator,
    MessageReaction,
    RawRest,
    DeleteMessage,
    TimeoutMember,
    UntimeoutMember,
}

impl RuntimeAction {
    fn audit_name(self) -> &'static str {
        match self {
            Self::SendMessage => "D7SND",
            Self::SendMessagePayload => "D7SJP",
            Self::SendMessageStickers => "D7STK",
            Self::SendMessagePoll => "D7POL",
            Self::SendMessageAttachment => "D7UPL",
            Self::SendMessageAttachments => "D7UPM",
            Self::InteractionCallback => "D7ICB",
            Self::ApplicationCommand => "D7ACR",
            Self::TypingIndicator => "D7TPI",
            Self::MessageReaction => "D7RAC",
            Self::RawRest => "D7RST",
            Self::DeleteMessage => "D7DEL",
            Self::TimeoutMember => "D7TMO",
            Self::UntimeoutMember => "D7UNT",
        }
    }

    fn snapshot_why(self) -> &'static str {
        match self {
            Self::SendMessage
            | Self::SendMessagePayload
            | Self::SendMessageStickers
            | Self::SendMessagePoll
            | Self::SendMessageAttachment
            | Self::SendMessageAttachments
            | Self::TypingIndicator => WHY_CHANNEL_SNAPSHOT_INCOMPLETE,
            Self::InteractionCallback
            | Self::ApplicationCommand
            | Self::MessageReaction
            | Self::RawRest => "REST_BACKEND_MISSING",
            Self::DeleteMessage => WHY_MESSAGE_SNAPSHOT_INCOMPLETE,
            Self::TimeoutMember | Self::UntimeoutMember => WHY_MEMBER_SNAPSHOT_INCOMPLETE,
        }
    }

    fn executes_in_log_only(self) -> bool {
        matches!(
            self,
            Self::SendMessage
                | Self::SendMessagePayload
                | Self::SendMessageStickers
                | Self::SendMessagePoll
                | Self::SendMessageAttachment
                | Self::SendMessageAttachments
        )
    }
}

fn json_str<'a>(value: &'a JsonValue, key: &str) -> Option<&'a str> {
    value.get(key).and_then(JsonValue::as_str)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Uninitialized,
    Initialized,
    Ended,
}

#[derive(Clone, Debug)]
struct Event {
    event_type: i32,
    event_name: String,
    guild_id: String,
    channel_id: String,
    message_id: String,
    interaction_id: String,
    application_id: String,
    interaction_token: String,
    interaction_name: String,
    interaction_type: i32,
    user_id: String,
    emoji: String,
    content: String,
    text_available: bool,
    author_is_bot: bool,
    role_ids: Vec<String>,
    image_attachment_count: usize,
    member_permissions: Option<PermissionSet>,
    gateway_opcode: i32,
    gateway_retry_ms: i64,
    gateway_sequence: Option<i64>,
    received_at_ms: u64,
}

impl Event {
    fn ready() -> Self {
        Self {
            event_type: D7RDY,
            event_name: "READY".to_string(),
            guild_id: String::new(),
            channel_id: String::new(),
            message_id: String::new(),
            interaction_id: String::new(),
            application_id: String::new(),
            interaction_token: String::new(),
            interaction_name: String::new(),
            interaction_type: 0,
            user_id: String::new(),
            emoji: String::new(),
            content: String::new(),
            text_available: false,
            author_is_bot: false,
            role_ids: Vec::new(),
            image_attachment_count: 0,
            member_permissions: None,
            gateway_opcode: 0,
            gateway_retry_ms: 0,
            gateway_sequence: None,
            received_at_ms: 0,
        }
    }

    fn demo_message() -> Self {
        Self {
            event_type: D7MSG,
            event_name: "MESSAGE_CREATE".to_string(),
            guild_id: "100000000000000001".to_string(),
            channel_id: "100000000000000002".to_string(),
            message_id: "100000000000000003".to_string(),
            interaction_id: String::new(),
            application_id: String::new(),
            interaction_token: String::new(),
            interaction_name: String::new(),
            interaction_type: 0,
            user_id: "100000000000000004".to_string(),
            emoji: String::new(),
            content: "dfor77 demo message".to_string(),
            text_available: true,
            author_is_bot: false,
            role_ids: vec!["100000000000000005".to_string()],
            image_attachment_count: 0,
            member_permissions: None,
            gateway_opcode: 0,
            gateway_retry_ms: 0,
            gateway_sequence: Some(2),
            received_at_ms: 0,
        }
    }

    fn gap(reason: impl Into<String>) -> Self {
        Self {
            event_type: D7GAP,
            event_name: "GAP".to_string(),
            guild_id: String::new(),
            channel_id: String::new(),
            message_id: String::new(),
            interaction_id: String::new(),
            application_id: String::new(),
            interaction_token: String::new(),
            interaction_name: String::new(),
            interaction_type: 0,
            user_id: String::new(),
            emoji: String::new(),
            content: reason.into(),
            text_available: false,
            author_is_bot: false,
            role_ids: Vec::new(),
            image_attachment_count: 0,
            member_permissions: None,
            gateway_opcode: 0,
            gateway_retry_ms: 0,
            gateway_sequence: None,
            received_at_ms: 0,
        }
    }

    fn from_gateway_event(event: GatewayEvent, received_at_ms: u64) -> Self {
        let description = event.description;
        let event_type = event.event_type;
        let mut converted = Self {
            event_type,
            event_name: description.clone(),
            guild_id: String::new(),
            channel_id: String::new(),
            message_id: String::new(),
            interaction_id: String::new(),
            application_id: String::new(),
            interaction_token: String::new(),
            interaction_name: String::new(),
            interaction_type: 0,
            user_id: String::new(),
            emoji: String::new(),
            content: if event_type == D7GAP {
                description
            } else {
                String::new()
            },
            text_available: false,
            author_is_bot: false,
            role_ids: Vec::new(),
            image_attachment_count: 0,
            member_permissions: None,
            gateway_opcode: event.opcode.unwrap_or(0),
            gateway_retry_ms: event.retry_ms.map(u64_to_i64_saturating).unwrap_or(0),
            gateway_sequence: event.sequence,
            received_at_ms,
        };
        match event.data {
            GatewayEventData::Message(message) => {
                converted.guild_id = message.guild_id;
                converted.channel_id = message.channel_id;
                converted.message_id = message.message_id;
                converted.user_id = message.user_id;
                converted.content = message.content;
                converted.text_available = message.content_available;
                converted.author_is_bot = message.author_is_bot;
                converted.role_ids = message.role_ids;
                converted.image_attachment_count = message.image_attachment_count;
                converted.member_permissions = message.member_permissions;
            }
            GatewayEventData::Interaction(interaction) => {
                converted.guild_id = interaction.guild_id;
                converted.channel_id = interaction.channel_id;
                converted.interaction_id = interaction.interaction_id;
                converted.application_id = interaction.application_id;
                converted.interaction_token = interaction.token;
                converted.interaction_name = interaction.name;
                converted.interaction_type = interaction.interaction_type;
                converted.user_id = interaction.user_id;
            }
            GatewayEventData::MessageDelete {
                guild_id,
                channel_id,
                message_id,
            } => {
                converted.guild_id = guild_id.unwrap_or_default();
                converted.channel_id = channel_id;
                converted.message_id = message_id;
            }
            GatewayEventData::Reaction {
                guild_id,
                channel_id,
                message_id,
                user_id,
                emoji,
            } => {
                converted.guild_id = guild_id;
                converted.channel_id = channel_id;
                converted.message_id = message_id;
                converted.user_id = user_id;
                converted.emoji = emoji;
            }
            GatewayEventData::Member { guild_id, user_id } => {
                converted.guild_id = guild_id;
                converted.user_id = user_id;
            }
            GatewayEventData::RateLimit(limit) => {
                converted.content = limit.metadata_json();
                converted.guild_id = limit.guild_id.unwrap_or_default();
                converted.text_available = true;
            }
            GatewayEventData::MembersChunk {
                guild_id, metadata, ..
            } => {
                converted.guild_id = guild_id;
                converted.content = metadata;
                converted.text_available = true;
            }
            GatewayEventData::Resource {
                guild_id,
                channel_id,
                message_id,
                user_id,
                role_id,
                text,
            } => {
                converted.guild_id = guild_id.unwrap_or_default();
                converted.channel_id = channel_id.unwrap_or_default();
                converted.message_id = message_id.unwrap_or_default();
                converted.user_id = user_id.unwrap_or_default();
                if let Some(role_id) = role_id {
                    converted.role_ids.push(role_id);
                }
                if let Some(text) = text {
                    converted.content = text;
                    converted.text_available = true;
                }
            }
            GatewayEventData::None => {}
        }
        converted
    }

    fn has_ids(&self) -> bool {
        !self.guild_id.is_empty()
            || !self.channel_id.is_empty()
            || !self.message_id.is_empty()
            || !self.interaction_id.is_empty()
            || !self.user_id.is_empty()
            || !self.role_ids.is_empty()
    }

    fn is_gap(&self) -> bool {
        self.event_type == D7GAP
    }
}

impl HeapSize for Event {
    fn heap_size(&self) -> usize {
        self.event_name.heap_size()
            + self.guild_id.heap_size()
            + self.channel_id.heap_size()
            + self.message_id.heap_size()
            + self.interaction_id.heap_size()
            + self.application_id.heap_size()
            + self.interaction_token.heap_size()
            + self.interaction_name.heap_size()
            + self.user_id.heap_size()
            + self.emoji.heap_size()
            + self.content.heap_size()
            + self.role_ids.heap_size()
            + self.member_permissions.heap_size()
    }
}

impl QueueEvent for Event {
    fn rate_limit_scope(&self) -> Option<&str> {
        (self.event_type == D7GRL && self.gateway_opcode == 8 && !self.guild_id.is_empty())
            .then_some(self.guild_id.as_str())
    }

    fn kind(&self) -> i32 {
        self.event_type
    }
    fn description(&self) -> &str {
        if self.is_gap() {
            &self.content
        } else {
            &self.event_name
        }
    }
    fn description_mut(&mut self) -> &mut String {
        if self.event_type == D7GAP {
            &mut self.content
        } else {
            &mut self.event_name
        }
    }
    fn opcode(&self) -> Option<i32> {
        (self.event_type == D7GRL).then_some(self.gateway_opcode)
    }
    fn gap(reason: String) -> Self {
        Self::gap(reason)
    }
}

fn u64_to_i64_saturating(value: u64) -> i64 {
    if value > i64::MAX as u64 {
        i64::MAX
    } else {
        value as i64
    }
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn read_attachment_file(file_path: &str, remaining_limit: usize) -> Result<Vec<u8>, ()> {
    if remaining_limit == 0 {
        return Err(());
    }
    let file = File::open(file_path).map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > remaining_limit as u64 {
        return Err(());
    }
    let mut limited = file.take(remaining_limit as u64 + 1);
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    limited.read_to_end(&mut bytes).map_err(|_| ())?;
    if bytes.is_empty() || bytes.len() > remaining_limit {
        return Err(());
    }
    Ok(bytes)
}

#[derive(Clone, Debug)]
struct ErrorScopes {
    act: ErrorInfo,
    sip: ErrorInfo,
    sev: ErrorInfo,
}

impl ErrorScopes {
    fn get(&self, scope: i32) -> &ErrorInfo {
        match scope {
            D7ACT => &self.act,
            D7SIP => &self.sip,
            D7SEV => &self.sev,
            _ => &self.act,
        }
    }
}

impl Default for ErrorScopes {
    fn default() -> Self {
        Self {
            act: ErrorInfo::ok(),
            sip: ErrorInfo::ok(),
            sev: ErrorInfo::ok(),
        }
    }
}

#[derive(Clone, Debug)]
struct ErrorInfo {
    errcode: i32,
    http: i32,
    discord_code: i32,
    message: String,
    why: String,
}

impl ErrorInfo {
    fn ok() -> Self {
        Self {
            errcode: D7OK,
            http: 0,
            discord_code: 0,
            message: String::new(),
            why: WHY_OK.to_string(),
        }
    }

    fn clear(&mut self) {
        *self = Self::ok();
    }

    fn set(&mut self, errcode: i32, http: i32, discord_code: i32, message: &str, why: &str) {
        self.errcode = errcode;
        self.http = http;
        self.discord_code = discord_code;
        self.message = ipc_percent_encoded_prefix(message, MAX_VALUE_ENCODED_BYTES).to_string();
        self.why = ipc_percent_encoded_prefix(why, MAX_VALUE_ENCODED_BYTES).to_string();
    }
}

fn ipc_percent_encoded_prefix(value: &str, max_encoded_bytes: usize) -> &str {
    let mut used = 0;
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let mut buffer = [0; 4];
        let encoded_len: usize = character
            .encode_utf8(&mut buffer)
            .bytes()
            .map(|byte| if is_unreserved(byte) { 1 } else { 3 })
            .sum();
        if used + encoded_len > max_encoded_bytes {
            break;
        }
        used += encoded_len;
        end = index + character.len_utf8();
    }
    &value[..end]
}

fn raw_rest_body_prefix(body: &str, buf_len: usize) -> (&str, bool) {
    let buffer_prefix = utf8_prefix(body, buf_len);
    let prefix = ipc_percent_encoded_prefix(buffer_prefix, MAX_VALUE_ENCODED_BYTES);
    (prefix, prefix.len() < body.len())
}

fn parse_scope(value: Option<&str>) -> Option<i32> {
    match value.and_then(|value| value.parse::<i32>().ok()) {
        Some(scope @ (D7ACT | D7SIP | D7SEV)) => Some(scope),
        _ => None,
    }
}

fn parse_u64_field(request: &Request, key: &str) -> Option<u64> {
    request.field(key)?.parse::<u64>().ok()
}

fn parse_usize_field(request: &Request, key: &str) -> Option<usize> {
    request.field(key)?.parse::<usize>().ok()
}

fn parse_http_method(value: Option<&str>) -> Option<HttpMethod> {
    match value {
        Some("1" | "GET") => Some(HttpMethod::Get),
        Some("2" | "POST") => Some(HttpMethod::Post),
        Some("3" | "PUT") => Some(HttpMethod::Put),
        Some("4" | "PATCH") => Some(HttpMethod::Patch),
        Some("5" | "DELETE") => Some(HttpMethod::Delete),
        _ => None,
    }
}

fn parse_reaction_operation(value: Option<&str>) -> Option<ReactionOperation> {
    match value {
        Some("1" | "ADD_OWN") => Some(ReactionOperation::AddOwn),
        Some("2" | "DELETE_OWN") => Some(ReactionOperation::DeleteOwn),
        Some("3" | "DELETE_USER") => Some(ReactionOperation::DeleteUser),
        Some("4" | "DELETE_ALL") => Some(ReactionOperation::DeleteAll),
        Some("5" | "DELETE_EMOJI") => Some(ReactionOperation::DeleteEmoji),
        _ => None,
    }
}

fn parse_boolish(value: Option<&str>) -> Option<bool> {
    match value {
        Some("0") => Some(false),
        Some("1") => Some(true),
        _ => None,
    }
}

fn snowflake_timestamp_ms(value: &str) -> Option<u64> {
    if !validate_snowflake(value) {
        return None;
    }
    let raw = value.parse::<u64>().ok()?;
    (raw >> 22).checked_add(DISCORD_EPOCH_MS)
}

fn signed_ms_to_seconds(value: i128) -> i64 {
    let seconds = if value >= 0 {
        value / 1_000
    } else {
        -((-value + 999) / 1_000)
    };
    seconds.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn json_bool(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn runtime_log_field(line: &mut String, field: &RuntimeLogField<'_>) {
    match field.value {
        RuntimeLogValue::String(value) => runtime_log_string(line, field.key, value),
        RuntimeLogValue::I32(value) => runtime_log_i32(line, field.key, value),
        RuntimeLogValue::I64(value) => runtime_log_i64(line, field.key, value),
        RuntimeLogValue::U32(value) => runtime_log_u32(line, field.key, value),
        RuntimeLogValue::U64(value) => runtime_log_u64(line, field.key, value),
        RuntimeLogValue::Usize(value) => runtime_log_usize(line, field.key, value),
        RuntimeLogValue::Bool(value) => runtime_log_bool(line, field.key, value),
        RuntimeLogValue::Null => runtime_log_null(line, field.key),
    }
}

fn runtime_log_key(line: &mut String, key: &str) {
    line.push_str(",\"");
    line.push_str(&json_log_escape(key));
    line.push_str("\":");
}

fn runtime_log_string(line: &mut String, key: &str, value: &str) {
    runtime_log_key(line, key);
    line.push('"');
    line.push_str(&json_log_escape(value));
    line.push('"');
}

fn runtime_log_i32(line: &mut String, key: &str, value: i32) {
    runtime_log_key(line, key);
    line.push_str(&value.to_string());
}

fn runtime_log_i64(line: &mut String, key: &str, value: i64) {
    runtime_log_key(line, key);
    line.push_str(&value.to_string());
}

fn runtime_log_u32(line: &mut String, key: &str, value: u32) {
    runtime_log_key(line, key);
    line.push_str(&value.to_string());
}

fn runtime_log_u64(line: &mut String, key: &str, value: u64) {
    runtime_log_key(line, key);
    line.push_str(&value.to_string());
}

fn runtime_log_usize(line: &mut String, key: &str, value: usize) {
    runtime_log_key(line, key);
    line.push_str(&value.to_string());
}

fn runtime_log_bool(line: &mut String, key: &str, value: bool) {
    runtime_log_key(line, key);
    line.push_str(json_bool(value));
}

fn runtime_log_null(line: &mut String, key: &str) {
    runtime_log_key(line, key);
    line.push_str("null");
}

fn push_runtime_owned_string<'a>(
    fields: &mut Vec<RuntimeLogField<'a>>,
    key: &'a str,
    value: Option<&'a str>,
) {
    match value {
        Some(value) => fields.push(RuntimeLogField::string(key, value)),
        None => fields.push(RuntimeLogField::null(key)),
    }
}

fn http_method_name(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
    }
}

fn rest_action_name(action: RestAction) -> &'static str {
    match action {
        RestAction::SendMessage => "send_message",
        RestAction::DeleteMessage => "delete_message",
        RestAction::TimeoutMember => "timeout_member",
        RestAction::UntimeoutMember => "untimeout_member",
        RestAction::MessageReaction => "message_reaction",
        RestAction::InteractionCallback => "interaction_callback",
        RestAction::ApplicationCommand => "application_command",
        RestAction::TypingIndicator => "typing_indicator",
        RestAction::GuildManagement => "guild_management",
        RestAction::ChannelManagement => "channel_management",
        RestAction::RoleManagement => "role_management",
        RestAction::MemberManagement => "member_management",
        RestAction::GatewayVoice => "gateway_voice",
        RestAction::LowLevel => "low_level",
        RestAction::ReadOnly => "read_only",
    }
}

fn log_level_name(level: i32) -> &'static str {
    match level {
        0 => "debug",
        1 => "info",
        2 => "warn",
        3 => "error",
        _ => "unknown",
    }
}

fn log_value_or_dash(value: &str) -> String {
    if value.is_empty() {
        "-".to_string()
    } else {
        diagnostic_field(value)
    }
}

fn parent_category_response(id: u64, parent_channel_id: Option<String>) -> Response {
    Response {
        id,
        status: D7OK,
        fields: vec![Field::percent(
            "value",
            parent_channel_id.as_deref().unwrap_or_default(),
        )],
    }
}

fn boolish(id: u64, value: bool) -> Response {
    Response {
        id,
        status: D7OK,
        fields: vec![Field::raw("value", if value { "1" } else { "0" })],
    }
}

fn gateway_connect_result(startup: GatewayStartup) -> GatewayConnectResult {
    GatewayConnectResult {
        status: startup.status,
        why: startup.why.clone(),
        startup,
        connected: false,
        identify_sent: false,
        sent_payload_bytes: 0,
        gateway_outbound_remaining: None,
        received_frames: 0,
        last_action: GatewayAction::None,
    }
}

fn gateway_pump_result() -> GatewayPumpResult {
    GatewayPumpResult {
        status: D7OK,
        why: WHY_OK.to_string(),
        heartbeat_sent: false,
        heartbeat_payload_bytes: 0,
        gateway_outbound_remaining: None,
        received_frame: false,
        action: GatewayAction::None,
    }
}

fn gateway_send_denied_status(kind: GatewaySendKind, why: &str) -> i32 {
    if why == "GATEWAY_PAYLOAD_TOO_LARGE"
        && matches!(
            kind,
            GatewaySendKind::Heartbeat | GatewaySendKind::Resume | GatewaySendKind::Identify
        )
    {
        D7INT
    } else {
        D7RTL
    }
}

fn gateway_send_denied_action(kind: GatewaySendKind, why: &'static str) -> GatewayAction {
    if gateway_send_denied_status(kind, why) == D7INT {
        GatewayAction::Fatal(why)
    } else {
        GatewayAction::None
    }
}

fn prefer_gateway_action(primary: GatewayAction, fallback: GatewayAction) -> GatewayAction {
    if primary == GatewayAction::None {
        fallback
    } else {
        primary
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn now_ms_u64() -> u64 {
    let millis = now_ms();
    if millis > u128::from(u64::MAX) {
        u64::MAX
    } else {
        millis as u64
    }
}

fn rest_executor<B: RestBackend>(backend: B, settings: &RuntimeSettings) -> RestExecutor<B> {
    RestExecutor::new(backend, invalid_budget(settings))
        .with_global_max_wait_ms(settings.rest_global_max_wait_ms)
}

fn invalid_budget(settings: &RuntimeSettings) -> InvalidRequestBudget {
    let settings = settings.sanitized();
    InvalidRequestBudget::new(
        settings.rest_invalid_budget_warn,
        settings.rest_invalid_budget_recover,
        settings.rest_invalid_budget_stop,
    )
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::RoleSnapshot;

    fn owner_runtime() -> DforRuntime {
        let mut runtime = DforRuntime::with_settings(
            false,
            false,
            RuntimeSettings {
                message_ttl_ms: 100,
                member_ttl_ms: 100,
                bot_member_ttl_ms: 100,
                ..Default::default()
            },
        );
        runtime.bot_user_id_cache = Some("99".into());
        let store = runtime.gateway.snapshot_mut();
        store.upsert_channel(ChannelSnapshot::new("10", "30", 0, 100));
        for user in ["99", "98"] {
            store.upsert_member(MemberSnapshot::new("10", user, Vec::new(), 100));
        }
        store.upsert_guild_roles(GuildRolesSnapshot::new(
            "10",
            vec![RoleSnapshot {
                guild_id: "10".into(),
                role_id: "10".into(),
                permissions: PermissionSet::zero(),
                position: 0,
            }],
            100,
        ));
        let mut guild = crate::snapshot::GuildSnapshot::new("10", false, 0);
        guild.owner_id = Some("99".into());
        store.upsert_guild(guild);
        runtime
    }

    #[test]
    fn current_administrator_roles_remain_usable_when_ownership_is_expired() {
        let mut runtime = owner_runtime();
        runtime
            .gateway
            .snapshot_mut()
            .upsert_guild_roles(GuildRolesSnapshot::new(
                "10",
                vec![RoleSnapshot {
                    guild_id: "10".into(),
                    role_id: "10".into(),
                    permissions: PermissionSet::from_bits(&[3]),
                    position: 0,
                }],
                100,
            ));
        assert!(runtime.send_preflight_decision("10", "30", 101).can);
        assert_eq!(runtime.admin_decision("10", "99", 101), Ok(true));
    }

    #[test]
    fn ownership_fetch_errors_preserve_rest_details_and_cannot_grant_permissions() {
        for (http, code, status, reason) in [
            (403, 50001, D7FOR, WHY_MISSING_ACCESS),
            (404, 10004, D7NFD, WHY_GUILD_NOT_FOUND),
        ] {
            let mut runtime = owner_runtime();
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::discord_error(http, code));
            runtime.use_mock_rest_backend(backend);
            let result = runtime.send_preflight_decision("10", "30", 101);
            assert!(!result.can);
            assert_eq!(result.status, status);
            assert_eq!(result.why, reason);
            assert_eq!(result.http_status, i32::from(http));
            assert_eq!(result.discord_code, code);
            if status == D7NFD {
                assert_eq!(
                    runtime.gateway.snapshot_mut().get_guild("10", 101),
                    SnapshotLookup::Tombstone
                );
            }
        }
    }

    #[test]
    fn unavailable_or_incomplete_guild_state_cannot_restore_owner_privileges() {
        let mut runtime = owner_runtime();
        let SnapshotLookup::Found(mut guild) = runtime.gateway.snapshot_mut().get_guild("10", 0)
        else {
            unreachable!()
        };
        guild.complete = false;
        runtime.gateway.snapshot_mut().upsert_guild(guild);
        let result = runtime.send_preflight_decision("10", "30", 100);
        assert!(!result.can);
        assert_eq!(result.status, D7STA);
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(
            r#"{"id":"10","owner_id":"99","unavailable":true}"#,
        ));
        runtime.use_mock_rest_backend(backend);
        let result = runtime.send_preflight_decision("10", "30", 101);
        assert!(!result.can);
        assert_eq!(result.status, D7STA);
        assert_eq!(result.why, WHY_GUILD_UNAVAILABLE);
        assert_eq!(
            runtime.guild_owner_id_decision("10", 101).unwrap_err().why,
            WHY_GUILD_UNAVAILABLE
        );
    }

    #[test]
    fn expired_ownership_cannot_grant_permissions_without_confirmation() {
        let mut runtime = owner_runtime();
        assert!(runtime.send_preflight_decision("10", "30", 100).can);
        assert_eq!(runtime.admin_decision("10", "99", 100), Ok(true));
        runtime
            .gateway
            .snapshot_mut()
            .mark_guild_available("10", 100);
        let send = runtime.send_preflight_decision("10", "30", 101);
        assert_eq!(send.status, D7STA);
        assert!(!send.can);
        assert_eq!(send.why, "GUILD_SNAPSHOT_INCOMPLETE");
        let admin = runtime.admin_decision("10", "99", 101).unwrap_err();
        assert_eq!(admin.status, D7STA);
        assert_eq!(admin.why, "GUILD_SNAPSHOT_INCOMPLETE");
    }

    #[test]
    fn ownership_refresh_preserves_current_owners_and_revokes_former_owners() {
        for owner in ["98", "99"] {
            let mut runtime = owner_runtime();
            let mut backend = MockRestBackend::default();
            backend.push_response(rest_body(&format!(r#"{{"id":"10","owner_id":"{owner}"}}"#)));
            runtime.use_mock_rest_backend(backend);
            let send = runtime.send_preflight_decision("10", "30", 101);
            assert_eq!(send.status, D7OK);
            assert_eq!(send.can, owner == "99");
            assert_eq!(runtime.admin_decision("10", "99", 101), Ok(owner == "99"));
            assert_eq!(runtime.admin_decision("10", "98", 101), Ok(owner == "98"));
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                unreachable!()
            };
            assert_eq!(executor.backend.requests().len(), 1);
            assert_eq!(executor.backend.requests()[0].path, "/api/v10/guilds/10");
        }
    }

    #[test]
    fn malformed_ownership_refreshes_never_restore_owner_privileges() {
        for body in [
            r#"{"id":"11","owner_id":"99"}"#,
            r#"{"id":"10"}"#,
            r#"{"id":"10","owner_id":true}"#,
            r#"{"id":"10","owner_id":null}"#,
            r#"{"id":"10","owner_id":""}"#,
            r#"{"id":"10","owner_id":"invalid"}"#,
            r#"{"id":"10","owner_id":"99","unavailable":"false"}"#,
            "[]",
        ] {
            let mut runtime = owner_runtime();
            let mut backend = MockRestBackend::default();
            backend.push_response(rest_body(body));
            runtime.use_mock_rest_backend(backend);
            let result = runtime.send_preflight_decision("10", "30", 101);
            assert_eq!(result.status, D7STA);
            assert!(!result.can);
            assert_eq!(result.why, "GUILD_SNAPSHOT_INCOMPLETE");
        }
    }

    #[test]
    fn unrelated_role_updates_cannot_extend_cached_send_permissions() {
        for remove in [false, true] {
            let settings = RuntimeSettings {
                message_ttl_ms: 100,
                bot_member_ttl_ms: 100,
                ..Default::default()
            };
            let mut runtime = DforRuntime::with_settings(false, false, settings);
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
            runtime
                .gateway
                .snapshot_mut()
                .upsert_channel(ChannelSnapshot::new("10", "30", 0, 50));
            runtime
                .gateway
                .snapshot_mut()
                .upsert_member(MemberSnapshot::new("10", "99", Vec::new(), 50));
            runtime
                .gateway
                .snapshot_mut()
                .upsert_guild_roles(GuildRolesSnapshot::new(
                    "10",
                    vec![
                        RoleSnapshot {
                            guild_id: "10".into(),
                            role_id: "10".into(),
                            permissions: PermissionSet::from_bits(&[10, 11]),
                            position: 0,
                        },
                        RoleSnapshot {
                            guild_id: "10".into(),
                            role_id: "20".into(),
                            permissions: PermissionSet::zero(),
                            position: 1,
                        },
                    ],
                    0,
                ));
            let event = if remove {
                r#"{"op":0,"s":2,"t":"GUILD_ROLE_DELETE","d":{"guild_id":"10","role_id":"20"}}"#
            } else {
                r#"{"op":0,"s":2,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"20","permissions":"0","position":1}}}"#
            };
            runtime.ingest_gateway_json(event, 99).unwrap();
            assert!(runtime.send_preflight_decision("10", "30", 100).can);
            let expired = runtime.send_preflight_decision("10", "30", 101);
            assert_eq!(expired.status, D7STA);
            assert!(!expired.can);
            assert_eq!(expired.why, WHY_ROLE_SNAPSHOT_INCOMPLETE);
            let mut backend = MockRestBackend::default();
            backend.push_response(rest_body(
                r#"[{"id":"10","permissions":"1024","position":0}]"#,
            ));
            backend.push_response(rest_body(r#"{"id":"10","owner_id":"100000000000000099"}"#));
            runtime.use_mock_rest_backend(backend);
            let refreshed = runtime.send_preflight_decision("10", "30", 101);
            assert_eq!(refreshed.status, D7OK);
            assert!(!refreshed.can);
            assert_eq!(refreshed.why, "MISSING_SEND_MESSAGES");
        }
    }

    #[test]
    fn deleted_roles_do_not_break_send_checks_using_remaining_permissions() {
        for (everyone_bits, removed_bits, expected_can) in
            [(vec![10, 11], vec![3], true), (vec![10], vec![11], false)]
        {
            let mut runtime = DforRuntime::new(false, false);
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
            runtime
                .gateway
                .snapshot_mut()
                .upsert_channel(ChannelSnapshot::new("10", "30", 0, 0));
            runtime
                .gateway
                .snapshot_mut()
                .upsert_member(MemberSnapshot::new("10", "99", vec!["20".into()], 0));
            runtime
                .gateway
                .snapshot_mut()
                .upsert_guild_roles(GuildRolesSnapshot::new(
                    "10",
                    vec![
                        RoleSnapshot {
                            guild_id: "10".into(),
                            role_id: "10".into(),
                            permissions: PermissionSet::from_bits(&everyone_bits),
                            position: 0,
                        },
                        RoleSnapshot {
                            guild_id: "10".into(),
                            role_id: "20".into(),
                            permissions: PermissionSet::from_bits(&removed_bits),
                            position: 1,
                        },
                    ],
                    0,
                ));
            runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"GUILD_ROLE_DELETE","d":{"guild_id":"10","role_id":"20"}}"#, 1).unwrap();
            let decision = runtime.send_preflight_decision("10", "30", 1);
            assert_eq!(decision.status, D7OK);
            assert_eq!(decision.can, expected_can);
            assert_eq!(
                decision.why,
                if expected_can {
                    WHY_OK
                } else {
                    "MISSING_SEND_MESSAGES"
                }
            );
        }
    }

    #[test]
    fn channel_overwrites_cannot_grant_or_revoke_guild_only_permissions() {
        for (kind, id) in [(0, "10"), (0, "20"), (1, "99")] {
            let mut channel = ChannelSnapshot::new("10", "30", 0, 0);
            channel
                .permission_overwrites
                .push(ChannelPermissionOverwrite {
                    overwrite_id: id.into(),
                    kind,
                    allow: PermissionSet::from_bits(&[2, 3, 5, 11, 81]),
                    deny: PermissionSet::from_bits(&[1, 40, 80]),
                });
            let base = MemberPermissions::new(PermissionSet::from_bits(&[1, 40, 80]));
            let result = member_permissions_for_channel(base, "10", "99", &["20".into()], &channel);
            assert!(!result.administrator());
            assert!(result.permissions.has(1));
            assert!(result.permissions.has(40));
            assert!(!result.permissions.has(2));
            assert!(!result.permissions.has(5));
            assert!(result.permissions.has(11));
            assert!(!result.permissions.has(80));
            assert!(result.permissions.has(81));
        }
    }

    #[test]
    fn overwrite_administrator_flags_cannot_bypass_send_delete_or_private_thread_checks() {
        for (kind, id) in [(0, "10"), (0, "20"), (1, "99")] {
            let mut runtime = DforRuntime::new(false, false);
            runtime.bot_user_id_cache = Some("99".into());
            let mut channel = ChannelSnapshot::new("10", "30", 0, 0);
            channel
                .permission_overwrites
                .push(ChannelPermissionOverwrite {
                    overwrite_id: id.into(),
                    kind,
                    allow: PermissionSet::from_bits(&[3]),
                    deny: PermissionSet::zero(),
                });
            let mut thread = ChannelSnapshot::new("10", "31", 12, 0);
            thread.parent_channel_id = Some("30".into());
            thread.current_user_thread_joined = Some(false);
            thread.current_user_thread_joined_at_ms = Some(0);
            let store = runtime.gateway.snapshot_mut();
            store.upsert_channel(channel);
            store.upsert_channel(thread);
            store.upsert_member(MemberSnapshot::new("10", "99", vec!["20".into()], 0));
            store.upsert_message(MessageSnapshot::new("10", "30", "40", "42", 0));
            store.upsert_guild_roles(GuildRolesSnapshot::new(
                "10",
                vec![
                    RoleSnapshot {
                        guild_id: "10".into(),
                        role_id: "10".into(),
                        permissions: PermissionSet::from_bits(&[10, 16, 38]),
                        position: 0,
                    },
                    RoleSnapshot {
                        guild_id: "10".into(),
                        role_id: "20".into(),
                        permissions: PermissionSet::zero(),
                        position: 1,
                    },
                ],
                0,
            ));
            let send = runtime.send_preflight_decision("10", "30", 0);
            assert!(!send.can);
            assert_eq!(send.why, "MISSING_SEND_MESSAGES");
            let delete = runtime.delete_preflight_decision("10", "30", "40", 0);
            assert!(!delete.can);
            assert_eq!(delete.why, "MISSING_MANAGE_MESSAGES");
            let private = runtime.send_preflight_decision("10", "31", 0);
            assert!(!private.can);
            assert_eq!(private.why, WHY_THREAD_NOT_JOINED);
        }
    }

    #[test]
    fn guild_owners_and_role_administrators_still_bypass_channel_overwrites() {
        let mut channel = ChannelSnapshot::new("10", "30", 0, 0);
        channel
            .permission_overwrites
            .push(ChannelPermissionOverwrite {
                overwrite_id: "99".into(),
                kind: 1,
                allow: PermissionSet::zero(),
                deny: PermissionSet::from_bits(&[3, 10, 11, 16, 38]),
            });
        for owner in [false, true] {
            let mut base = MemberPermissions::new(if owner {
                PermissionSet::zero()
            } else {
                PermissionSet::from_bits(&[3])
            });
            base.is_owner = owner;
            let result = member_permissions_for_channel(base.clone(), "10", "99", &[], &channel);
            assert_eq!(result, base);
            assert!(
                PermissionResolver::new(result)
                    .can_send(false, true, false, false)
                    .can
            );
        }
    }

    #[test]
    fn everyone_overwrites_are_not_reapplied_as_assigned_role_overwrites() {
        let mut channel = ChannelSnapshot::new("10", "30", 0, 0);
        channel.permission_overwrites = vec![
            ChannelPermissionOverwrite {
                overwrite_id: "10".into(),
                kind: 0,
                allow: PermissionSet::from_bits(&[10, 11]),
                deny: PermissionSet::zero(),
            },
            ChannelPermissionOverwrite {
                overwrite_id: "20".into(),
                kind: 0,
                allow: PermissionSet::zero(),
                deny: PermissionSet::from_bits(&[10, 11]),
            },
        ];
        for roles in [
            vec!["20".into()],
            vec!["10".into(), "20".into()],
            vec!["20".into(), "10".into()],
        ] {
            let result = member_permissions_for_channel(
                MemberPermissions::new(PermissionSet::zero()),
                "10",
                "99",
                &roles,
                &channel,
            );
            assert!(!result.permissions.has(10));
            assert!(!result.permissions.has(11));
        }
    }

    #[test]
    fn large_role_sets_and_overwrites_preserve_permission_precedence() {
        let count = 20_000;
        let mut roles = vec![RoleSnapshot {
            guild_id: "1".into(),
            role_id: "1".into(),
            permissions: PermissionSet::from_bits(&[
                BIT_VIEW_CHANNEL,
                crate::permission::BIT_SEND_MESSAGES,
            ]),
            position: 0,
        }];
        let mut assigned = Vec::new();
        let mut overwrites = Vec::new();
        for index in 0..count {
            let id = (index + 100).to_string();
            assigned.push(id.clone());
            roles.push(RoleSnapshot {
                guild_id: "1".into(),
                role_id: id.clone(),
                permissions: PermissionSet::zero(),
                position: i64::from(index + 1),
            });
            overwrites.push(ChannelPermissionOverwrite {
                overwrite_id: id,
                kind: 0,
                allow: PermissionSet::zero(),
                deny: PermissionSet::from_bits(&[BIT_VIEW_CHANNEL]),
            });
        }
        assigned.reverse();
        overwrites.push(ChannelPermissionOverwrite {
            overwrite_id: "2".into(),
            kind: 1,
            allow: PermissionSet::from_bits(&[BIT_VIEW_CHANNEL]),
            deny: PermissionSet::zero(),
        });
        let member = MemberSnapshot::new("1", "2", assigned, 0);
        let roles = GuildRolesSnapshot::new("1", roles, 0);
        let mut channel = ChannelSnapshot::new("1", "3", 0, 0);
        channel.has_permission_overwrites = true;
        channel.permission_overwrites = overwrites;
        let resolved = member_permissions_for_channel(
            member_permissions_from_roles("1", &member, &roles, None).unwrap(),
            "1",
            "2",
            &member.role_ids,
            &channel,
        );
        assert_eq!(resolved.top_role_position, i64::from(count));
        assert!(resolved.permissions.has(BIT_VIEW_CHANNEL));
        assert!(resolved
            .permissions
            .has(crate::permission::BIT_SEND_MESSAGES));
        assert!(!resolved.administrator());
        channel.permission_overwrites.pop();
        let resolved = member_permissions_for_channel(
            member_permissions_from_roles("1", &member, &roles, None).unwrap(),
            "1",
            "2",
            &member.role_ids,
            &channel,
        );
        assert!(!resolved.permissions.has(BIT_VIEW_CHANNEL));
        assert!(resolved
            .permissions
            .has(crate::permission::BIT_SEND_MESSAGES));
    }

    #[test]
    fn duplicate_or_cross_guild_roles_cannot_authorise_a_member() {
        let member = MemberSnapshot::new("1", "2", vec![], 0);
        let everyone = RoleSnapshot {
            guild_id: "1".into(),
            role_id: "1".into(),
            permissions: PermissionSet::from_bits(&[3]),
            position: 0,
        };
        let duplicate = GuildRolesSnapshot::new("1", vec![everyone.clone(), everyone.clone()], 0);
        assert!(member_permissions_from_roles("1", &member, &duplicate, None).is_none());
        let mut wrong_guild = everyone;
        wrong_guild.guild_id = "3".into();
        let roles = GuildRolesSnapshot::new("1", vec![wrong_guild], 0);
        assert!(member_permissions_from_roles("1", &member, &roles, None).is_none());
    }

    #[test]
    fn rest_snapshots_reject_wrong_identity_and_malformed_permissions() {
        for body in [
            r#"{"id":"11","guild_id":"1","type":0}"#,
            r#"{"id":10,"guild_id":"1","type":0}"#,
            r#"{"id":"10","guild_id":"2","type":0}"#,
            r#"{"id":"10","guild_id":false,"type":0}"#,
        ] {
            assert!(
                channel_snapshot_from_rest_body(body, "1", "10", 0).is_none(),
                "{body}"
            );
        }
        for overwrites in [
            "{}",
            "null",
            r#"[{"id":"1","type":2,"allow":"0","deny":"1024"}]"#,
        ] {
            let body = format!(
                r#"{{"id":"10","guild_id":"1","type":0,"permission_overwrites":{overwrites}}}"#
            );
            assert!(
                !channel_snapshot_from_rest_body(&body, "1", "10", 0)
                    .unwrap()
                    .complete
            );
        }
        for body in [
            r#"{"user":{"id":"3"},"roles":[]}"#,
            r#"{"user":5,"roles":[]}"#,
            r#"{"user":{"id":"2"},"roles":["10",false]}"#,
            r#"{"user":{"id":"2"}}"#,
        ] {
            assert!(
                member_snapshot_from_rest_body(body, "1", "2", 0).is_none(),
                "{body}"
            );
        }
        let duplicates = r#"[{"id":"1","permissions":"0","position":0},{"id":"1","permissions":"8","position":0}]"#;
        assert!(guild_roles_snapshot_from_rest_body(duplicates, "1", 0).is_none());
    }

    #[test]
    fn mismatched_rest_identity_blocks_a_send_before_any_mutation() {
        for body in [
            r#"{"id":"11","guild_id":"1","type":0}"#,
            r#"{"id":10,"guild_id":"1","type":0}"#,
        ] {
            let mut backend = MockRestBackend::default();
            backend.push_response(rest_body(body));
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            let response = runtime.handle(request(
                2,
                "SND",
                vec![
                    Field::raw("channel_id", "10"),
                    Field::percent("content", "must not be sent"),
                ],
            ));
            assert_eq!(response.status, D7STA);
            assert_eq!(mock_rest_paths(&runtime), vec!["/api/v10/channels/10"]);
        }
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(r#"{"id":"10","guild_id":"1","type":0}"#));
        backend.push_response(rest_body(r#"{"id":"9"}"#));
        backend.push_response(rest_body(r#"{"user":{"id":"8"},"roles":[]}"#));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(
            runtime
                .handle(request(
                    2,
                    "SND",
                    vec![
                        Field::raw("channel_id", "10"),
                        Field::percent("content", "must not be sent")
                    ]
                ))
                .status,
            D7STA
        );
        assert_eq!(
            mock_rest_paths(&runtime),
            vec![
                "/api/v10/channels/10",
                "/api/v10/users/@me",
                "/api/v10/guilds/1/members/9"
            ]
        );
    }

    #[test]
    fn runtime_and_startup_diagnostics_exclude_authentication_payloads_and_stored_bodies() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .gateway_user_outbound
            .push_back(QueuedGatewayPayload {
                payload: "private-outbound-message".repeat(100_000),
                session_id: Some("private-session-id".into()),
                kind: gateway_send::GatewayPayloadKind::Other,
                request_key: crate::gateway::request_limit::GatewayRequestKey::parse(
                    &parse_json(r#"{"op":255,"d":null}"#).unwrap(),
                )
                .unwrap(),
            });
        runtime.last_raw_rest_body = Some(r#"{"token":"private-webhook-token"}"#.into());
        let startup = GatewayStartup {
            status: D7OK,
            why: "OK".into(),
            action: GatewayAction::SendIdentify,
            gateway_url: Some("wss://gateway.discord.gg/?private-url-secret".into()),
            identify_payload: Some(r#"{"op":2,"d":{"token":"private-bot-token"}}"#.into()),
            identify_bucket: Some(0),
            identify_remaining: Some(1),
            identify_reset_after_ms: Some(60_000),
            daily_identify_count: 1,
            daily_identify_remaining: 998,
            daily_identify_warning: false,
        };
        let result = GatewayConnectResult {
            status: D7OK,
            why: "OK".into(),
            startup,
            connected: true,
            identify_sent: true,
            sent_payload_bytes: 48,
            gateway_outbound_remaining: Some(119),
            received_frames: 1,
            last_action: GatewayAction::None,
        };
        let output = format!("{runtime:#?} {result:#?}");
        for secret in [
            "private-outbound-message",
            "private-webhook-token",
            "private-url-secret",
            "private-bot-token",
        ] {
            assert!(!output.contains(secret), "{output}");
        }
        assert!(output.len() < 4096);
        assert!(result
            .startup
            .identify_payload
            .as_deref()
            .unwrap()
            .contains("private-bot-token"));
    }
    use crate::permission::{
        BIT_MANAGE_MESSAGES, BIT_MODERATE_MEMBERS, BIT_READ_MESSAGE_HISTORY, BIT_VIEW_CHANNEL,
    };

    fn request(id: u64, verb: &str, fields: Vec<Field>) -> Request {
        Request {
            id,
            verb: verb.to_string(),
            fields,
        }
    }

    fn snowflake_at(timestamp_ms: u64) -> String {
        ((timestamp_ms - DISCORD_EPOCH_MS) << 22).to_string()
    }

    #[allow(clippy::too_many_arguments)]
    fn message_json(
        sequence: i64,
        guild_id: &str,
        channel_id: &str,
        message_id: &str,
        user_id: &str,
        image_count: usize,
        bot: bool,
        message_type: i32,
    ) -> String {
        let attachments = (0..image_count)
            .map(|index| format!(r#"{{"filename":"image{index}.png","content_type":"image/png"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"op":0,"s":{sequence},"t":"MESSAGE_CREATE","d":{{"guild_id":"{guild_id}","channel_id":"{channel_id}","id":"{message_id}","content":"image spam","type":{message_type},"attachments":[{attachments}],"author":{{"id":"{user_id}","bot":{bot}}},"member":{{"roles":[]}}}}}}"#
        )
    }

    fn rest_body(body: &str) -> crate::rest::RestResponse {
        let mut response = crate::rest::RestResponse::ok();
        response.body = body.to_string();
        response
    }

    fn rest_server_error() -> crate::rest::RestResponse {
        let mut response = crate::rest::RestResponse::ok();
        response.http_status = 500;
        response
    }

    fn mock_rest_paths(runtime: &DforRuntime) -> Vec<String> {
        match &runtime.rest {
            RuntimeRest::Mock(executor) => executor
                .backend
                .requests()
                .iter()
                .map(|request| request.path.clone())
                .collect(),
            _ => panic!("expected mock REST runtime"),
        }
    }

    #[test]
    fn runtime_queue_bytes_and_repeated_gaps_clear_permission_snapshots() {
        let settings = RuntimeSettings {
            event_queue_capacity: 1,
            event_queue_max_bytes: 4096,
            ..RuntimeSettings::default()
        };
        let mut runtime = DforRuntime::with_settings(false, false, settings);
        for sequence in 1..=4 {
            let message = message_json(
                sequence,
                "10",
                "20",
                &sequence.to_string(),
                "40",
                2,
                false,
                0,
            );
            runtime.ingest_gateway_json(&message, 100).unwrap();
            assert_eq!(
                runtime.gateway.snapshot().message_count(),
                if sequence % 2 == 1 { 1 } else { 0 }
            );
        }
        assert_eq!(runtime.events.gap_epoch(), 2);
        assert_eq!(runtime.events.pop_front().unwrap().event_type, D7GAP);
        assert!(runtime.events.is_empty());
        let mut large = Event::demo_message();
        large.content = "x".repeat(4096);
        assert!(runtime.push_event(large));
        assert_eq!(runtime.gateway.snapshot().retained_bytes(), 0);
        assert_eq!(
            runtime.events.pop_front().unwrap().content,
            "QUEUE_BYTES_FULL"
        );
    }

    #[test]
    fn runtime_control_flood_is_bounded_and_cache_eviction_cannot_grant_permissions() {
        let mut runtime = DforRuntime::new(false, false);
        for _ in 0..10_000 {
            runtime.push_event(Event::ready());
        }
        assert!(runtime.events.len() <= 8);
        let now = now_ms_u64();
        runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, now).unwrap();
        runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"99","channels":[{"id":"20","type":0}],"roles":[{"id":"10","position":0,"permissions":"0"}],"members":[{"user":{"id":"99"},"roles":[]}]}}"#, now).unwrap();
        assert!(runtime.send_preflight_decision("10", "20", now).can);
        runtime.gateway.snapshot_mut().set_max_bytes(1);
        let decision = runtime.send_preflight_decision("10", "20", now);
        assert_eq!(decision.status, D7STA);
        assert!(!decision.can);
    }

    #[test]
    fn runtime_settings_validate_public_input_errors() {
        let invalid_shard = RuntimeSettings {
            gateway_shard_id: 1,
            gateway_shard_count: 1,
            ..RuntimeSettings::default()
        };
        assert_eq!(
            invalid_shard.validate().unwrap_err(),
            "gateway shard settings invalid"
        );

        let invalid_daily = RuntimeSettings {
            gateway_daily_identify_warn: 0,
            ..RuntimeSettings::default()
        };
        assert!(DforRuntime::try_with_settings(false, false, invalid_daily)
            .unwrap_err()
            .contains("daily identify budget"));

        let invalid_rest = RuntimeSettings {
            rest_invalid_budget_warn: 0,
            rest_invalid_budget_recover: 0,
            rest_invalid_budget_stop: 0,
            ..RuntimeSettings::default()
        };
        assert_eq!(
            invalid_rest.validate().unwrap_err(),
            "invalid budget thresholds"
        );

        let invalid_snapshot_ttl = RuntimeSettings {
            member_ttl_ms: 0,
            ..RuntimeSettings::default()
        };
        assert_eq!(
            invalid_snapshot_ttl.validate().unwrap_err(),
            "snapshot ttl must be positive"
        );

        let invalid_snapshot_limit = RuntimeSettings {
            resource_max_entries: 0,
            ..RuntimeSettings::default()
        };
        assert_eq!(
            invalid_snapshot_limit.validate().unwrap_err(),
            "snapshot max entries must be positive"
        );
    }

    #[test]
    fn runtime_with_settings_sanitizes_invalid_public_settings() {
        let settings = RuntimeSettings {
            gateway_shard_id: 99,
            gateway_shard_count: 0,
            gateway_daily_identify_warn: 0,
            gateway_daily_identify_stop: 1000,
            rest_http_timeout_ms: 0,
            rest_invalid_budget_warn: 0,
            rest_invalid_budget_recover: 0,
            rest_invalid_budget_stop: 0,
            event_queue_capacity: 0,
            member_ttl_ms: 0,
            member_max_entries: 0,
            resource_max_entries: 0,
            ..RuntimeSettings::default()
        };
        let mut runtime = DforRuntime::with_settings(false, false, settings);
        assert_eq!(runtime.settings.gateway_shard_id, 0);
        assert_eq!(runtime.settings.gateway_shard_count, 1);
        assert_eq!(runtime.settings.gateway_daily_identify_warn, 900);
        assert_eq!(runtime.settings.gateway_daily_identify_stop, 999);
        assert_eq!(runtime.settings.rest_http_timeout_ms, 10_000);
        assert_eq!(runtime.settings.rest_invalid_budget_warn, 6000);
        assert_eq!(runtime.settings.event_queue_capacity, 1024);
        assert_eq!(runtime.settings.member_ttl_ms, 60_000);
        assert_eq!(runtime.settings.member_max_entries, 4096);
        assert_eq!(runtime.settings.resource_max_entries, 4096);
        runtime.use_mock_rest_backend(MockRestBackend::default());
        assert!(matches!(runtime.rest, RuntimeRest::Mock(_)));
    }

    #[test]
    fn runtime_settings_apply_separate_snapshot_max_entries() {
        let settings = RuntimeSettings {
            message_max_entries: 3,
            member_max_entries: 1,
            resource_max_entries: 1,
            ..RuntimeSettings::default()
        };
        let mut runtime = DforRuntime::with_settings(false, false, settings);
        runtime
            .gateway
            .snapshot_mut()
            .upsert_message(MessageSnapshot::new("g", "c", "m1", "u", 1));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_message(MessageSnapshot::new("g", "c", "m2", "u", 2));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("g", "c1", 0, 3));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("g", "c2", 0, 4));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new("g", "u1", vec![], 5));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new("g", "u2", vec![], 6));

        assert!(matches!(
            runtime.gateway.snapshot_mut().get_message("c", "m1", 6),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            runtime.gateway.snapshot_mut().get_channel("c1", 6),
            SnapshotLookup::Missing
        ));
        assert!(matches!(
            runtime.gateway.snapshot_mut().get_member("g", "u1", 6),
            SnapshotLookup::Missing
        ));
    }

    #[test]
    fn error_detail_fields_stay_within_ipc_response_limits() {
        let mut runtime = DforRuntime::new(false, false);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);

        let message = "あ".repeat(MAX_VALUE_ENCODED_BYTES);
        let why = "理由".repeat(MAX_VALUE_ENCODED_BYTES);
        runtime.errors.act.set(D7NET, 429, 12_345, &message, &why);

        let err = runtime.handle(request(
            2,
            "ERR",
            vec![Field::raw("scope", D7ACT.to_string())],
        ));
        assert_eq!(err.status, D7OK);
        assert_eq!(err.field("errcode").unwrap().parse::<i32>().unwrap(), D7NET);
        assert!(crate::ipc::serialize_response(&err).is_ok());
        let err_message = err.field("message").unwrap();
        assert!(crate::validation::percent_encoded_len(err_message) <= MAX_VALUE_ENCODED_BYTES);
        assert!(err_message.len() < message.len());

        let why_response = runtime.handle(request(
            3,
            "WHY",
            vec![Field::raw("scope", D7ACT.to_string())],
        ));
        assert_eq!(why_response.status, D7OK);
        assert!(crate::ipc::serialize_response(&why_response).is_ok());
        let why_message = why_response.field("message").unwrap();
        assert!(crate::validation::percent_encoded_len(why_message) <= MAX_VALUE_ENCODED_BYTES);
        assert!(why_message.len() < why.len());
    }

    fn push_private_thread_permissions(backend: &mut MockRestBackend, permissions: &str) {
        backend.push_response(rest_body(r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#));
        backend.push_response(rest_body(&format!(
            r#"[{{"id":"100000000000000001","permissions":"{permissions}","position":0}}]"#
        )));
        backend.push_response(rest_body(
            r#"{"id":"100000000000000001","owner_id":"100000000000000099"}"#,
        ));
        backend.push_response(rest_body(r#"{"id":"100000000000000010","guild_id":"100000000000000001","type":0,"permission_overwrites":[]}"#));
    }

    fn seed_thread_snapshots(
        runtime: &mut DforRuntime,
        kind: i32,
        parent_kind: i32,
        joined: Option<bool>,
        permission_bits: &[u32],
        now_ms: u64,
    ) {
        seed_send_snapshots(runtime, now_ms);
        let mut thread =
            ChannelSnapshot::new("100000000000000010", "100000000000000001", kind, now_ms);
        thread.parent_channel_id = Some("100000000000000020".into());
        thread.current_user_thread_joined = joined;
        thread.current_user_thread_joined_at_ms = joined.map(|_| now_ms);
        runtime.gateway.snapshot_mut().upsert_channel(thread);
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new(
                "100000000000000010",
                "100000000000000020",
                parent_kind,
                now_ms,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_guild_roles(GuildRolesSnapshot::new(
                "100000000000000010",
                vec![RoleSnapshot {
                    guild_id: "100000000000000010".into(),
                    role_id: "100000000000000010".into(),
                    permissions: PermissionSet::from_bits(permission_bits),
                    position: 0,
                }],
                now_ms,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_message(MessageSnapshot::new(
                "100000000000000010",
                "100000000000000001",
                "100000000000000003",
                "100000000000000004",
                now_ms,
            ));
    }

    #[test]
    fn thread_permissions_follow_parent_overwrites_for_send_and_delete() {
        for (kind, parent_kind) in [(10, 5), (11, 0), (11, 15), (11, 16), (12, 0)] {
            let now = now_ms_u64();
            let mut runtime = DforRuntime::new(false, false);
            seed_thread_snapshots(&mut runtime, kind, parent_kind, Some(true), &[], now);
            let mut parent =
                ChannelSnapshot::new("100000000000000010", "100000000000000020", parent_kind, now);
            parent
                .permission_overwrites
                .push(ChannelPermissionOverwrite {
                    overwrite_id: "100000000000000010".into(),
                    kind: 0,
                    allow: PermissionSet::from_bits(&[10, 13, 16, 38]),
                    deny: PermissionSet::zero(),
                });
            parent.has_permission_overwrites = true;
            runtime.gateway.snapshot_mut().upsert_channel(parent);
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            for verb in ["CSN", "CDL"] {
                let response = runtime.handle(request(
                    2,
                    verb,
                    vec![
                        Field::raw("guild_id", "100000000000000010"),
                        Field::raw("channel_id", "100000000000000001"),
                        Field::raw("message_id", "100000000000000003"),
                    ],
                ));
                assert_eq!(response.status, D7OK, "{kind} {parent_kind} {verb}");
                assert_eq!(
                    response.field("can"),
                    Some("1"),
                    "{kind} {parent_kind} {verb}"
                );
            }
            runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"CHANNEL_UPDATE","d":{"id":"100000000000000020","permission_overwrites":[{"id":"100000000000000010","type":0,"allow":"0","deny":"1024"}]}}"#, now).unwrap();
            if kind == 12 {
                runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"THREAD_MEMBER_UPDATE","d":{"id":"100000000000000001","guild_id":"100000000000000010","user_id":"100000000000000009"}}"#, now).unwrap();
            }
            for verb in ["CSN", "CDL"] {
                let response = runtime.handle(request(
                    3,
                    verb,
                    vec![
                        Field::raw("guild_id", "100000000000000010"),
                        Field::raw("channel_id", "100000000000000001"),
                        Field::raw("message_id", "100000000000000003"),
                    ],
                ));
                assert_eq!(
                    response.field("can"),
                    Some("0"),
                    "{kind} {parent_kind} {verb}"
                );
            }
            assert_eq!(
                runtime
                    .handle(request(
                        4,
                        "SND",
                        vec![
                            Field::raw("channel_id", "100000000000000001"),
                            Field::percent("content", "blocked"),
                        ]
                    ))
                    .status,
                D7FOR
            );
        }
    }

    #[test]
    fn private_thread_moderators_need_parent_permissions_but_not_membership() {
        for joined in [None, Some(false)] {
            for manage_threads in [false, true] {
                let mut bits = vec![10, 13, 16, 38];
                if manage_threads {
                    bits.push(34);
                }
                let mut runtime = DforRuntime::new(false, false);
                seed_thread_snapshots(&mut runtime, 12, 0, joined, &bits, 0);
                let send =
                    runtime.send_preflight_decision("100000000000000010", "100000000000000001", 0);
                assert_eq!(send.can, manage_threads);
                let delete = runtime.delete_preflight_decision(
                    "100000000000000010",
                    "100000000000000001",
                    "100000000000000003",
                    0,
                );
                assert_eq!(delete.can, manage_threads);
                if manage_threads {
                    let mut parent =
                        ChannelSnapshot::new("100000000000000010", "100000000000000020", 0, 0);
                    parent
                        .permission_overwrites
                        .push(ChannelPermissionOverwrite {
                            overwrite_id: "100000000000000010".into(),
                            kind: 0,
                            allow: PermissionSet::zero(),
                            deny: PermissionSet::from_bits(&[10]),
                        });
                    parent.has_permission_overwrites = true;
                    runtime.gateway.snapshot_mut().upsert_channel(parent);
                    assert!(
                        !runtime
                            .send_preflight_decision("100000000000000010", "100000000000000001", 0)
                            .can
                    );
                    assert!(
                        !runtime
                            .delete_preflight_decision(
                                "100000000000000010",
                                "100000000000000001",
                                "100000000000000003",
                                0
                            )
                            .can
                    );
                }
            }
        }
    }

    #[test]
    fn deleting_in_locked_archived_threads_requires_manage_threads() {
        for kind in [11, 12] {
            for archived in [false, true] {
                for locked in [false, true] {
                    for manage in [false, true] {
                        let mut runtime = DforRuntime::new(false, false);
                        let mut bits = vec![10, 13, 16];
                        if manage {
                            bits.push(34);
                        }
                        seed_thread_snapshots(&mut runtime, kind, 0, Some(true), &bits, 0);
                        let SnapshotLookup::Found(mut channel) = runtime
                            .gateway
                            .snapshot_mut()
                            .get_channel("100000000000000001", 0)
                        else {
                            panic!("expected cached thread");
                        };
                        channel.archived = archived;
                        channel.locked = locked;
                        runtime.gateway.snapshot_mut().upsert_channel(channel);
                        let decision = runtime.delete_preflight_decision(
                            "100000000000000010",
                            "100000000000000001",
                            "100000000000000003",
                            0,
                        );
                        assert_eq!(
                            decision.can,
                            !archived || !locked || manage,
                            "{kind} {archived} {locked} {manage}"
                        );
                        if !decision.can {
                            assert_eq!(decision.why, WHY_THREAD_LOCKED);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_missing_and_inaccessible_thread_parents_never_grant_permissions() {
        for variant in 0..10 {
            let mut runtime = DforRuntime::new(false, false);
            seed_thread_snapshots(&mut runtime, 11, 0, Some(true), &[3], 0);
            let mut parent = ChannelSnapshot::new("100000000000000010", "100000000000000020", 0, 0);
            match variant {
                0 => parent.guild_id = "100000000000000099".into(),
                1 => parent.complete = false,
                2 => parent.obfuscated = true,
                3 => parent.channel_type = 1,
                4 => parent.channel_type = 4,
                5 => {
                    parent.channel_type = 11;
                    parent.parent_channel_id = Some("100000000000000001".into());
                }
                6 => {
                    let mut thread =
                        ChannelSnapshot::new("100000000000000010", "100000000000000001", 11, 0);
                    thread.parent_channel_id = Some(thread.channel_id.clone());
                    runtime.gateway.snapshot_mut().upsert_channel(thread);
                }
                7 => runtime
                    .gateway
                    .snapshot_mut()
                    .upsert_channel(ChannelSnapshot::new(
                        "100000000000000010",
                        "100000000000000001",
                        11,
                        0,
                    )),
                8 => parent.captured_at_ms = 0,
                9 => runtime
                    .gateway
                    .snapshot_mut()
                    .tombstone_channel("100000000000000020", 0),
                _ => unreachable!(),
            }
            if variant != 9 {
                runtime.gateway.snapshot_mut().upsert_channel(parent);
            }
            let now = if variant == 8 {
                runtime.settings.message_ttl_ms + 1
            } else {
                0
            };
            if variant == 8 {
                runtime
                    .gateway
                    .snapshot_mut()
                    .upsert_member(MemberSnapshot::new(
                        "100000000000000010",
                        "100000000000000009",
                        vec![],
                        now,
                    ));
                runtime
                    .gateway
                    .snapshot_mut()
                    .upsert_guild_roles(GuildRolesSnapshot::new(
                        "100000000000000010",
                        vec![RoleSnapshot {
                            guild_id: "100000000000000010".into(),
                            role_id: "100000000000000010".into(),
                            permissions: PermissionSet::from_bits(&[3]),
                            position: 0,
                        }],
                        now,
                    ));
                let mut thread =
                    ChannelSnapshot::new("100000000000000010", "100000000000000001", 11, now);
                thread.parent_channel_id = Some("100000000000000020".into());
                runtime.gateway.snapshot_mut().upsert_channel(thread);
            }
            assert!(
                !runtime
                    .send_preflight_decision("100000000000000010", "100000000000000001", now)
                    .can,
                "{variant}"
            );
        }
    }

    #[test]
    fn stale_thread_parents_are_refreshed_and_transport_failures_never_reuse_old_grants() {
        for failure in [false, true] {
            let mut backend = MockRestBackend::default();
            backend.push_response(if failure { rest_server_error() } else {
                rest_body(r#"{"id":"100000000000000020","guild_id":"100000000000000010","type":0,"permission_overwrites":[{"id":"100000000000000010","type":0,"allow":"0","deny":"1024"}]}"#)
            });
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            let now = runtime.settings.message_ttl_ms + 1;
            seed_thread_snapshots(&mut runtime, 11, 0, Some(true), &[10, 38], now);
            runtime
                .gateway
                .snapshot_mut()
                .upsert_channel(ChannelSnapshot::new(
                    "100000000000000010",
                    "100000000000000020",
                    0,
                    0,
                ));
            let send =
                runtime.send_preflight_decision("100000000000000010", "100000000000000001", now);
            assert!(!send.can);
            assert_eq!(send.status, if failure { D7STA } else { D7OK });
            assert_eq!(
                mock_rest_paths(&runtime),
                vec!["/api/v10/channels/100000000000000020"]
            );
        }
    }

    #[test]
    fn malformed_thread_member_confirmation_is_unknown_and_never_changes_cached_membership() {
        for body in ["{}", "not json", r#"{"id":"100000000000000099","type":12}"#] {
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::discord_error(404, 10007));
            backend.push_response(rest_body(body));
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            seed_thread_snapshots(&mut runtime, 12, 0, None, &[10, 38], 0);
            let send =
                runtime.send_preflight_decision("100000000000000010", "100000000000000001", 0);
            assert!(!send.can);
            assert_eq!(send.status, D7STA);
            let SnapshotLookup::Found(thread) = runtime
                .gateway
                .snapshot_mut()
                .get_channel("100000000000000001", 0)
            else {
                panic!("expected cached thread");
            };
            assert_eq!(thread.current_user_thread_joined, None);
        }
    }

    #[test]
    fn thread_member_probes_recheck_refreshed_channel_restrictions_and_rejoining() {
        for (flags, archived, locked, member, why, allowed) in [
            (131072, false, false, "", "MISSING_VIEW_CHANNEL", false),
            (0, true, false, "", WHY_THREAD_ARCHIVED, false),
            (0, false, true, "", WHY_THREAD_LOCKED, false),
            (0, false, false, r#", "member":{}"#, WHY_OK, true),
        ] {
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::discord_error(404, 10007));
            backend.push_response(rest_body(&format!(r#"{{"id":"100000000000000001","guild_id":"100000000000000010","type":12,"parent_id":"100000000000000020","flags":{flags},"thread_metadata":{{"archived":{archived},"locked":{locked}}}{member}}}"#)));
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            seed_thread_snapshots(
                &mut runtime,
                12,
                0,
                None,
                if allowed { &[10, 38] } else { &[3] },
                0,
            );
            let send =
                runtime.send_preflight_decision("100000000000000010", "100000000000000001", 0);
            assert_eq!(send.can, allowed);
            assert_eq!(send.why, why);
            assert_eq!(
                mock_rest_paths(&runtime),
                vec![
                    "/api/v10/channels/100000000000000001/thread-members/100000000000000009",
                    "/api/v10/channels/100000000000000001",
                ]
            );
        }
    }

    fn archive_thread(id: &str, member: bool) -> String {
        let member = if member {
            r#", "member":{"user_id":"100000000000000009"}"#
        } else {
            ""
        };
        format!(
            r#"{{"id":"{id}","guild_id":"100000000000000010","parent_id":"100000000000000020","type":12,"thread_metadata":{{"archived":true,"locked":false}}{member}}}"#
        )
    }

    fn archive_page(threads: &str, has_more: bool) -> String {
        format!(r#"{{"threads":[{threads}],"members":[],"has_more":{has_more}}}"#)
    }

    fn seed_archived_thread(runtime: &mut DforRuntime, manager: bool) {
        let mut permissions = vec![10, 13, 16];
        if manager {
            permissions.push(34);
        }
        seed_thread_snapshots(runtime, 12, 0, None, &permissions, 0);
        let SnapshotLookup::Found(mut thread) = runtime
            .gateway
            .snapshot_mut()
            .get_channel("100000000000000001", 0)
        else {
            panic!("expected cached thread");
        };
        thread.archived = true;
        runtime.gateway.snapshot_mut().upsert_channel(thread);
    }

    #[test]
    fn archived_joined_threads_beyond_the_first_page_use_an_exact_id_cursor() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(&archive_page(
            &archive_thread("100000000000000090", false),
            true,
        )));
        backend.push_response(rest_body(&archive_page(
            &archive_thread("100000000000000001", false),
            true,
        )));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_archived_thread(&mut runtime, false);
        let delete = runtime.delete_preflight_decision(
            "100000000000000010",
            "100000000000000001",
            "100000000000000003",
            0,
        );
        assert!(delete.can);
        assert_eq!(mock_rest_paths(&runtime), vec![
            "/api/v10/channels/100000000000000020/users/@me/threads/archived/private",
            "/api/v10/channels/100000000000000020/users/@me/threads/archived/private?limit=1&before=100000000000000002",
        ]);
        let SnapshotLookup::Found(thread) = runtime
            .gateway
            .snapshot_mut()
            .get_channel("100000000000000001", 0)
        else {
            panic!("expected refreshed thread");
        };
        assert_eq!(thread.current_user_thread_joined, Some(true));
    }

    #[test]
    fn archive_cursor_responses_must_prove_presence_or_absence_without_loops() {
        for found in [false, true] {
            let mut backend = MockRestBackend::default();
            backend.push_response(rest_body(&archive_page(
                &archive_thread("100000000000000090", false),
                true,
            )));
            backend.push_response(rest_body(&archive_page(
                &archive_thread(
                    if found {
                        "100000000000000001"
                    } else {
                        "100000000000000003"
                    },
                    false,
                ),
                true,
            )));
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            seed_archived_thread(&mut runtime, false);
            let delete = runtime.delete_preflight_decision(
                "100000000000000010",
                "100000000000000001",
                "100000000000000003",
                0,
            );
            assert_eq!(delete.can, found);
            assert_eq!(mock_rest_paths(&runtime).len(), 2);
            if !found {
                assert_eq!(delete.status, D7STA);
                let SnapshotLookup::Found(thread) = runtime
                    .gateway
                    .snapshot_mut()
                    .get_channel("100000000000000001", 0)
                else {
                    panic!("expected unchanged cached thread");
                };
                assert_eq!(thread.current_user_thread_joined, None);
            }
        }
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(&archive_page(
            &archive_thread("100000000000000090", false),
            true,
        )));
        backend.push_response(rest_body(&archive_page("", false)));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_archived_thread(&mut runtime, false);
        let delete = runtime.delete_preflight_decision(
            "100000000000000010",
            "100000000000000001",
            "100000000000000003",
            0,
        );
        assert!(!delete.can);
        assert_eq!(delete.status, D7FOR);
        assert_eq!(mock_rest_paths(&runtime).len(), 2);
    }

    #[test]
    fn malformed_archive_members_and_parents_never_set_cached_membership() {
        let valid = archive_thread("100000000000000001", false);
        for body in [
            archive_page(
                &valid.replace("100000000000000020", "100000000000000021"),
                false,
            ),
            archive_page(
                &valid.replace("100000000000000010", "100000000000000011"),
                false,
            ),
            archive_page(&valid, false).replace(
                r#""members":[]"#,
                r#""members":[{"id":"100000000000000001","user_id":"100000000000000099"}]"#,
            ),
            archive_page("", false).replace(
                r#""members":[]"#,
                r#""members":[{"id":"100000000000000001","user_id":"100000000000000009"}]"#,
            ),
            archive_page(&valid, false).replace(r#""has_more":false"#, r#""has_more":null"#),
        ] {
            let mut backend = MockRestBackend::default();
            backend.push_response(rest_body(&body));
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            seed_archived_thread(&mut runtime, true);
            let delete = runtime.delete_preflight_decision(
                "100000000000000010",
                "100000000000000001",
                "100000000000000003",
                0,
            );
            assert!(!delete.can);
            assert_eq!(delete.status, D7STA);
            assert_eq!(mock_rest_paths(&runtime).len(), 1);
            let SnapshotLookup::Found(thread) = runtime
                .gateway
                .snapshot_mut()
                .get_channel("100000000000000001", 0)
            else {
                panic!("expected cached thread");
            };
            assert_eq!(thread.current_user_thread_joined, None);
        }
    }

    #[test]
    fn moderators_refresh_the_exact_thread_when_it_is_beyond_the_private_archive_page() {
        for joined in [false, true] {
            let mut backend = MockRestBackend::default();
            backend.push_response(rest_body(&archive_page("", false)));
            backend.push_response(rest_body(&archive_page(
                &archive_thread("100000000000000090", false),
                true,
            )));
            backend.push_response(rest_body(&archive_thread("100000000000000001", joined)));
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            seed_archived_thread(&mut runtime, true);
            let delete = runtime.delete_preflight_decision(
                "100000000000000010",
                "100000000000000001",
                "100000000000000003",
                0,
            );
            assert!(delete.can);
            assert_eq!(
                mock_rest_paths(&runtime),
                vec![
                    "/api/v10/channels/100000000000000020/users/@me/threads/archived/private",
                    "/api/v10/channels/100000000000000020/threads/archived/private",
                    "/api/v10/channels/100000000000000001",
                ]
            );
            let SnapshotLookup::Found(thread) = runtime
                .gateway
                .snapshot_mut()
                .get_channel("100000000000000001", 0)
            else {
                panic!("expected refreshed thread");
            };
            assert_eq!(thread.current_user_thread_joined, Some(joined));
        }
    }

    #[test]
    fn failed_exact_thread_refreshes_never_reuse_old_archive_access() {
        for variant in 0..5 {
            let mut backend = MockRestBackend::default();
            backend.push_response(rest_body(&archive_page("", false)));
            backend.push_response(rest_body(&archive_page(
                &archive_thread("100000000000000090", false),
                true,
            )));
            let response = match variant {
                0 => rest_body(
                    &archive_thread("100000000000000001", false)
                        .replace("100000000000000020", "100000000000000021"),
                ),
                1 => rest_body(&archive_thread("100000000000000001", true).replace(
                    r#""user_id":"100000000000000009""#,
                    r#""user_id":"100000000000000099""#,
                )),
                2 => rest_body("{}"),
                3 => crate::rest::RestResponse::discord_error(404, 10003),
                4 => rest_server_error(),
                _ => unreachable!(),
            };
            backend.push_response(response);
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            seed_archived_thread(&mut runtime, true);
            let delete = runtime.delete_preflight_decision(
                "100000000000000010",
                "100000000000000001",
                "100000000000000003",
                0,
            );
            assert!(!delete.can);
            assert_eq!(delete.status, if variant == 3 { D7NFD } else { D7STA });
            assert_eq!(mock_rest_paths(&runtime).len(), 3);
            let lookup = runtime
                .gateway
                .snapshot_mut()
                .get_channel("100000000000000001", 0);
            if variant == 3 {
                assert_eq!(lookup, SnapshotLookup::Tombstone);
            } else {
                let SnapshotLookup::Found(thread) = lookup else {
                    panic!("expected unchanged cached thread");
                };
                assert_eq!(thread.current_user_thread_joined, None);
            }
        }
    }

    #[test]
    fn successful_thread_member_responses_need_matching_ids() {
        for body in [
            "",
            "{}",
            "not json",
            "null",
            r#"{"id":"100000000000000099","user_id":"100000000000000009"}"#,
            r#"{"id":"100000000000000001","user_id":"100000000000000099"}"#,
        ] {
            let mut backend = MockRestBackend::default();
            backend.push_response(rest_body(body));
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            seed_thread_snapshots(&mut runtime, 12, 0, None, &[10, 38], 0);
            let send =
                runtime.send_preflight_decision("100000000000000010", "100000000000000001", 0);
            assert!(!send.can);
            assert_eq!(send.status, D7STA);
            let SnapshotLookup::Found(thread) = runtime
                .gateway
                .snapshot_mut()
                .get_channel("100000000000000001", 0)
            else {
                panic!("expected cached thread");
            };
            assert_eq!(thread.current_user_thread_joined, None);
        }
    }

    fn push_send_preflight_allow(backend: &mut MockRestBackend) {
        push_send_preflight_allow_for_channel(backend, "100000000000000001");
    }

    fn push_send_preflight_allow_for_channel(backend: &mut MockRestBackend, channel_id: &str) {
        backend.push_response(rest_body(&format!(
            r#"{{"id":"{channel_id}","guild_id":"100000000000000010","type":0}}"#,
        )));
        backend.push_response(rest_body(r#"{"id":"100000000000000009"}"#));
        backend.push_response(rest_body(
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#,
        ));
        backend.push_response(rest_body(
            r#"[{"id":"100000000000000010","permissions":"76800","position":0}]"#,
        ));
        backend.push_response(rest_body(
            r#"{"id":"100000000000000010","owner_id":"100000000000000099"}"#,
        ));
    }

    fn seed_known_guild(runtime: &mut DforRuntime, guild_id: &str, now_ms: u64) {
        let mut guild = GuildSnapshot::new(guild_id, false, now_ms);
        guild.owner_id = Some("100000000000000099".into());
        runtime.gateway.snapshot_mut().upsert_guild(guild);
    }

    fn seed_send_snapshots(runtime: &mut DforRuntime, now_ms: u64) {
        seed_known_guild(runtime, "100000000000000010", now_ms);
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new(
                "100000000000000010",
                "100000000000000001",
                0,
                now_ms,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new(
                "100000000000000010",
                "100000000000000009",
                Vec::new(),
                now_ms,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_guild_roles(GuildRolesSnapshot::new(
                "100000000000000010",
                vec![RoleSnapshot {
                    guild_id: "100000000000000010".to_string(),
                    role_id: "100000000000000010".to_string(),
                    permissions: PermissionSet::parse_decimal("3072").unwrap(),
                    position: 0,
                }],
                now_ms,
            ));
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms,
            )
            .unwrap();
    }

    fn mark_bot_timed_out(runtime: &mut DforRuntime, guild_id: &str, now_ms: u64) {
        let mut bot_member =
            MemberSnapshot::new(guild_id, "100000000000000009", Vec::new(), now_ms);
        bot_member.timed_out = true;
        runtime.gateway.snapshot_mut().upsert_member(bot_member);
    }

    fn seed_delete_snapshots(
        runtime: &mut DforRuntime,
        message_type: i32,
        message_user_id: &str,
        permission_bits: &[u32],
        now_ms: u64,
    ) {
        seed_known_guild(runtime, "100000000000000001", now_ms);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms,
            )
            .unwrap();
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new(
                "100000000000000001",
                "100000000000000002",
                0,
                now_ms,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new(
                "100000000000000001",
                "100000000000000009",
                Vec::new(),
                now_ms,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_guild_roles(GuildRolesSnapshot::new(
                "100000000000000001",
                vec![RoleSnapshot {
                    guild_id: "100000000000000001".to_string(),
                    role_id: "100000000000000001".to_string(),
                    permissions: PermissionSet::from_bits(permission_bits),
                    position: 0,
                }],
                now_ms,
            ));
        let mut message = MessageSnapshot::new(
            "100000000000000001",
            "100000000000000002",
            "100000000000000003",
            message_user_id,
            now_ms,
        );
        message.message_type = message_type;
        runtime.gateway.snapshot_mut().upsert_message(message);
    }

    fn seed_moderation_snapshots(runtime: &mut DforRuntime, now_ms: u64) {
        seed_known_guild(runtime, "100000000000000001", now_ms);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms,
            )
            .unwrap();
        mark_bot_timed_out(runtime, "100000000000000001", now_ms);
        runtime
            .gateway
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new(
                "100000000000000001",
                "100000000000000004",
                Vec::new(),
                now_ms,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_guild_roles(GuildRolesSnapshot::new(
                "100000000000000001",
                vec![RoleSnapshot {
                    guild_id: "100000000000000001".to_string(),
                    role_id: "100000000000000001".to_string(),
                    permissions: PermissionSet::from_bits(&[BIT_MODERATE_MEMBERS]),
                    position: 0,
                }],
                now_ms,
            ));
    }

    #[test]
    fn gateway_timeout_expiry_restores_send_permission_without_another_member_event() {
        let mut runtime = DforRuntime::new(false, false);
        seed_send_snapshots(&mut runtime, 0);
        runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"100000000000000010","user":{"id":"100000000000000009"},"communication_disabled_until":"1970-01-01T00:00:00.100001Z"}}"#, 1).unwrap();
        for now in [99, 100, 101, 102] {
            let decision =
                runtime.send_preflight_decision("100000000000000010", "100000000000000001", now);
            assert_eq!(decision.can, now >= 101);
            assert_eq!(
                decision.why,
                if now < 101 { WHY_BOT_TIMED_OUT } else { WHY_OK }
            );
        }
    }

    #[test]
    fn channel_updates_cannot_remove_cached_denials_or_bypass_obfuscation() {
        for obfuscated in [false, true] {
            let mut runtime =
                DforRuntime::with_mock_rest_backend(false, false, MockRestBackend::default());
            let now = now_ms_u64();
            seed_send_snapshots(&mut runtime, now);
            if obfuscated {
                runtime
                    .gateway
                    .snapshot_mut()
                    .upsert_guild_roles(GuildRolesSnapshot::new(
                        "100000000000000010",
                        vec![RoleSnapshot {
                            guild_id: "100000000000000010".into(),
                            role_id: "100000000000000010".into(),
                            permissions: PermissionSet::from_bits(&[3]),
                            position: 0,
                        }],
                        now,
                    ));
            } else {
                let mut channel =
                    ChannelSnapshot::new("100000000000000010", "100000000000000001", 0, now);
                channel.has_permission_overwrites = true;
                channel
                    .permission_overwrites
                    .push(ChannelPermissionOverwrite {
                        overwrite_id: "100000000000000010".into(),
                        kind: 0,
                        allow: PermissionSet::zero(),
                        deny: PermissionSet::from_bits(&[11]),
                    });
                runtime.gateway.snapshot_mut().upsert_channel(channel);
            }
            let flags = if obfuscated {
                r#", "flags":131072"#
            } else {
                ""
            };
            runtime.ingest_gateway_json(&format!(r#"{{"op":0,"s":2,"t":"CHANNEL_UPDATE","d":{{"id":"100000000000000001","guild_id":"100000000000000010","type":0,"name":"new name"{flags}}}}}"#), now).unwrap();
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            let response = runtime.handle(request(
                2,
                "CSN",
                vec![
                    Field::raw("guild_id", "100000000000000010"),
                    Field::raw("channel_id", "100000000000000001"),
                ],
            ));
            assert_eq!(response.status, D7OK);
            assert_eq!(response.field("can"), Some("0"));
            assert_eq!(
                response.field("why"),
                Some(if obfuscated {
                    "MISSING_VIEW_CHANNEL"
                } else {
                    "MISSING_SEND_MESSAGES"
                })
            );
            let response = runtime.handle(request(
                3,
                "SND",
                vec![
                    Field::raw("channel_id", "100000000000000001"),
                    Field::percent("content", "blocked"),
                ],
            ));
            assert_eq!(response.status, D7FOR);
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock backend");
            };
            assert!(executor.backend.requests().is_empty());
            if obfuscated {
                runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"CHANNEL_UPDATE","d":{"id":"100000000000000001","flags":0}}"#, now).unwrap();
                let decision = runtime.send_preflight_decision(
                    "100000000000000010",
                    "100000000000000001",
                    now,
                );
                assert!(!decision.can);
                runtime.ingest_gateway_json(r#"{"op":0,"s":4,"t":"CHANNEL_UPDATE","d":{"id":"100000000000000001","guild_id":"100000000000000010","type":0,"parent_id":null,"flags":0,"permission_overwrites":[]}}"#, now).unwrap();
                let mut confirmed = crate::rest::RestResponse::ok();
                confirmed.body = r#"{"id":"100000000000000001","guild_id":"100000000000000010","type":0,"flags":0,"permission_overwrites":[]}"#.into();
                let RuntimeRest::Mock(executor) = &mut runtime.rest else {
                    panic!("expected mock backend")
                };
                executor.backend.push_response(confirmed);
            } else {
                runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"CHANNEL_UPDATE","d":{"id":"100000000000000001","permission_overwrites":[]}}"#, now).unwrap();
            }
            assert!(
                runtime
                    .send_preflight_decision("100000000000000010", "100000000000000001", now)
                    .can
            );
        }
    }

    #[test]
    fn bot_timeout_blocks_send_delete_and_timeout_preflights() {
        let now = now_ms_u64();

        let mut send_runtime = DforRuntime::new(false, false);
        seed_send_snapshots(&mut send_runtime, now);
        mark_bot_timed_out(&mut send_runtime, "100000000000000010", now);
        assert_eq!(send_runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let send = send_runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000010"),
                Field::raw("channel_id", "100000000000000001"),
            ],
        ));
        assert_eq!(send.status, D7FOR);
        assert_eq!(send.field("can"), Some("0"));
        assert_eq!(send.field("why"), Some(WHY_BOT_TIMED_OUT));
        let send_err = send_runtime.handle(request(3, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(send_err.field("errcode"), Some("40"));
        assert_eq!(send_err.field("message"), Some(WHY_BOT_TIMED_OUT));

        let mut delete_runtime = DforRuntime::new(false, false);
        seed_delete_snapshots(
            &mut delete_runtime,
            0,
            "100000000000000004",
            &[
                BIT_VIEW_CHANNEL,
                BIT_READ_MESSAGE_HISTORY,
                BIT_MANAGE_MESSAGES,
            ],
            now,
        );
        mark_bot_timed_out(&mut delete_runtime, "100000000000000001", now);
        assert_eq!(
            delete_runtime.handle(request(4, "INI", vec![])).status,
            D7OK
        );
        let delete = delete_runtime.handle(request(
            5,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(delete.status, D7FOR);
        assert_eq!(delete.field("can"), Some("0"));
        assert_eq!(delete.field("why"), Some(WHY_BOT_TIMED_OUT));

        let mut timeout_runtime = DforRuntime::new(false, false);
        seed_moderation_snapshots(&mut timeout_runtime, now);
        assert_eq!(
            timeout_runtime.handle(request(6, "INI", vec![])).status,
            D7OK
        );
        let timeout = timeout_runtime.handle(request(
            7,
            "CTM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(timeout.status, D7FOR);
        assert_eq!(timeout.field("can"), Some("0"));
        assert_eq!(timeout.field("why"), Some(WHY_BOT_TIMED_OUT));
    }

    #[test]
    fn bot_timeout_blocks_actions_before_rest_send() {
        let now = now_ms_u64();

        let mut send_runtime =
            DforRuntime::with_mock_rest_backend(false, false, MockRestBackend::default());
        seed_send_snapshots(&mut send_runtime, now);
        mark_bot_timed_out(&mut send_runtime, "100000000000000010", now);
        assert_eq!(send_runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let send = send_runtime.handle(request(
            2,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::percent("content", "blocked"),
            ],
        ));
        assert_eq!(send.status, D7FOR);
        let send_why = send_runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(send_why.field("message"), Some(WHY_BOT_TIMED_OUT));
        assert!(mock_rest_paths(&send_runtime).is_empty());

        let mut timeout_runtime =
            DforRuntime::with_mock_rest_backend(false, false, MockRestBackend::default());
        seed_moderation_snapshots(&mut timeout_runtime, now);
        assert_eq!(
            timeout_runtime.handle(request(4, "INI", vec![])).status,
            D7OK
        );
        let timeout = timeout_runtime.handle(request(
            5,
            "TMO",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
                Field::raw("seconds", "60"),
                Field::percent("reason", "blocked"),
            ],
        ));
        assert_eq!(timeout.status, D7FOR);
        let untimeout = timeout_runtime.handle(request(
            6,
            "UNT",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
                Field::percent("reason", "blocked"),
            ],
        ));
        assert_eq!(untimeout.status, D7FOR);
        let timeout_why = timeout_runtime.handle(request(7, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(timeout_why.field("message"), Some(WHY_BOT_TIMED_OUT));
        assert!(mock_rest_paths(&timeout_runtime).is_empty());
    }

    #[test]
    fn lifecycle_matches_spec() {
        let mut runtime = DforRuntime::new(false, false);
        assert_eq!(runtime.handle(request(1, "END", vec![])).status, D7STA);
        assert_eq!(runtime.handle(request(2, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(3, "INI", vec![])).status, D7STA);
        assert_eq!(runtime.handle(request(4, "END", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(5, "NXT", vec![])).status, D7EOF);
    }

    #[test]
    fn ended_lifecycle_returns_spec_status_by_api_category() {
        let mut runtime = DforRuntime::new(false, false);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "END", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(3, "INI", vec![])).status, D7STA);
        assert_eq!(runtime.handle(request(4, "END", vec![])).status, D7STA);
        assert_eq!(runtime.handle(request(5, "NXT", vec![])).status, D7EOF);
        for (id, verb, fields, expected) in [
            (10, "TYP", vec![], D7STA),
            (11, "GET", vec![Field::raw("name", "GID")], D7STA),
            (12, "RLC", vec![], D7STA),
            (13, "RLI", vec![Field::raw("index", "1")], D7STA),
            (14, "GOP", vec![], D7STA),
            (15, "GRT", vec![], D7STA),
            (
                20,
                "ADM",
                vec![
                    Field::raw("guild_id", "100000000000000001"),
                    Field::raw("user_id", "100000000000000002"),
                ],
                D7EOF,
            ),
            (
                21,
                "CDL",
                vec![
                    Field::raw("guild_id", "100000000000000001"),
                    Field::raw("channel_id", "100000000000000002"),
                    Field::raw("message_id", "100000000000000003"),
                ],
                D7EOF,
            ),
            (
                22,
                "CTM",
                vec![
                    Field::raw("guild_id", "100000000000000001"),
                    Field::raw("user_id", "100000000000000002"),
                ],
                D7EOF,
            ),
            (
                23,
                "CSN",
                vec![
                    Field::raw("guild_id", "100000000000000001"),
                    Field::raw("channel_id", "100000000000000002"),
                ],
                D7EOF,
            ),
            (24, "CAD", vec![], D7EOF),
            (25, "CDP", vec![], D7EOF),
            (26, "CTP", vec![], D7EOF),
            (27, "CSP", vec![], D7EOF),
            (
                30,
                "SND",
                vec![
                    Field::raw("channel_id", "100000000000000002"),
                    Field::percent("content", "hello"),
                ],
                D7EOF,
            ),
            (
                31,
                "DEL",
                vec![
                    Field::raw("channel_id", "100000000000000002"),
                    Field::raw("message_id", "100000000000000003"),
                ],
                D7EOF,
            ),
            (
                32,
                "TMO",
                vec![
                    Field::raw("guild_id", "100000000000000001"),
                    Field::raw("user_id", "100000000000000002"),
                    Field::raw("seconds", "60"),
                ],
                D7EOF,
            ),
            (
                33,
                "UNT",
                vec![
                    Field::raw("guild_id", "100000000000000001"),
                    Field::raw("user_id", "100000000000000002"),
                ],
                D7EOF,
            ),
            (40, "ERR", vec![Field::raw("scope", "1")], D7EOF),
            (41, "WHY", vec![Field::raw("scope", "1")], D7EOF),
            (
                42,
                "LOG",
                vec![Field::raw("level", "1"), Field::percent("message", "done")],
                D7EOF,
            ),
        ] {
            assert_eq!(
                runtime.handle(request(id, verb, fields)).status,
                expected,
                "{verb}"
            );
        }
    }

    #[test]
    fn error_readers_follow_lifecycle_before_scope_validation() {
        let mut runtime = DforRuntime::new(false, false);
        assert_eq!(
            runtime
                .handle(request(1, "ERR", vec![Field::raw("scope", "1")]))
                .status,
            D7STA
        );
        assert_eq!(
            runtime
                .handle(request(2, "WHY", vec![Field::raw("scope", "1")]))
                .status,
            D7STA
        );
        assert_eq!(runtime.handle(request(3, "INI", vec![])).status, D7OK);
        let err = runtime.handle(request(4, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(err.status, D7OK);
        assert_eq!(err.field("errcode"), Some("0"));
        assert_eq!(
            runtime
                .handle(request(5, "WHY", vec![Field::raw("scope", "9")]))
                .status,
            D7ARG
        );
        assert_eq!(runtime.handle(request(6, "END", vec![])).status, D7OK);
        assert_eq!(
            runtime
                .handle(request(7, "ERR", vec![Field::raw("scope", "9")]))
                .status,
            D7EOF
        );
        assert_eq!(
            runtime
                .handle(request(8, "WHY", vec![Field::raw("scope", "9")]))
                .status,
            D7EOF
        );
    }

    #[test]
    fn invalid_error_scope_does_not_overwrite_existing_action_error() {
        let mut runtime = DforRuntime::new(false, false);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let failed = runtime.handle(request(
            2,
            "SND",
            vec![
                Field::raw("channel_id", "not-an-id"),
                Field::percent("content", "hello"),
            ],
        ));
        assert_eq!(failed.status, D7ARG);
        assert_eq!(
            runtime
                .handle(request(3, "WHY", vec![Field::raw("scope", "9")]))
                .status,
            D7ARG
        );
        let err = runtime.handle(request(4, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(err.status, D7OK);
        assert_eq!(err.field("errcode"), Some("30"));
        assert_eq!(err.field("message"), Some(WHY_INVALID_FORM_BODY));
        let why = runtime.handle(request(5, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_INVALID_FORM_BODY));
    }

    #[test]
    fn direct_action_argument_errors_replace_stale_action_error() {
        let cases = vec![
            (10, "ACR", vec![Field::raw("method", "POST")]),
            (
                20,
                "ACR",
                vec![Field::raw("application_id", "100000000000000001")],
            ),
            (
                30,
                "RAC",
                vec![
                    Field::raw("channel_id", "100000000000000001"),
                    Field::raw("message_id", "100000000000000002"),
                    Field::raw("mode", "99"),
                ],
            ),
            (40, "RST", vec![Field::raw("path", "/api/v10/users/@me")]),
            (50, "RST", vec![Field::raw("method", "GET")]),
            (60, "RBD", vec![Field::raw("buf_len", "bad")]),
            (70, "GWS", vec![]),
            (80, "PRS", vec![Field::percent("payload", "[]")]),
            (
                90,
                "VST",
                vec![
                    Field::raw("guild_id", "not-an-id"),
                    Field::raw("self_mute", "0"),
                    Field::raw("self_deaf", "0"),
                ],
            ),
        ];

        for (id, verb, fields) in cases {
            let mut runtime = DforRuntime::new(false, false);
            assert_eq!(runtime.handle(request(id - 2, "INI", vec![])).status, D7OK);
            runtime
                .errors
                .act
                .set(D7FOR, 403, 50013, "old", "OLD_ERROR");

            let response = runtime.handle(request(id, verb, fields));
            assert_eq!(response.status, D7ARG, "{verb}");

            let err = runtime.handle(request(id + 1, "ERR", vec![Field::raw("scope", "1")]));
            assert_eq!(err.field("errcode"), Some("30"), "{verb}");
            assert_eq!(err.field("http"), Some("0"), "{verb}");
            assert_eq!(err.field("discord_code"), Some("0"), "{verb}");
            assert_eq!(err.field("message"), Some(WHY_INVALID_FORM_BODY), "{verb}");

            let why = runtime.handle(request(id + 2, "WHY", vec![Field::raw("scope", "1")]));
            assert_eq!(why.field("message"), Some(WHY_INVALID_FORM_BODY), "{verb}");
        }
    }

    #[test]
    fn timeout_action_rejects_invalid_seconds_in_action_scope() {
        let mut runtime = DforRuntime::new(false, false);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        for (id, seconds) in [(2, "0"), (3, "2419201"), (4, "18446744073709551616")] {
            let response = runtime.handle(request(
                id,
                "TMO",
                vec![
                    Field::raw("guild_id", "100000000000000001"),
                    Field::raw("user_id", "100000000000000004"),
                    Field::raw("seconds", seconds),
                    Field::percent("reason", "test"),
                ],
            ));
            assert_eq!(response.status, D7ARG);
            let err = runtime.handle(request(id + 10, "ERR", vec![Field::raw("scope", "1")]));
            assert_eq!(err.field("errcode"), Some("30"));
            assert_eq!(err.field("message"), Some(WHY_INVALID_FORM_BODY));
        }
    }

    #[test]
    fn successful_preflight_clears_existing_action_error() {
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow(&mut backend);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(
            runtime
                .handle(request(
                    2,
                    "CSN",
                    vec![
                        Field::raw("guild_id", "100000000000000010"),
                        Field::raw("channel_id", "not-an-id"),
                    ],
                ))
                .status,
            D7ARG
        );
        assert_eq!(
            runtime
                .handle(request(
                    3,
                    "CSN",
                    vec![
                        Field::raw("guild_id", "100000000000000010"),
                        Field::raw("channel_id", "100000000000000001"),
                    ],
                ))
                .status,
            D7OK
        );
        let err = runtime.handle(request(4, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(err.field("errcode"), Some("0"));
        assert_eq!(err.field("message"), Some(""));
    }

    #[test]
    fn next_event_success_clears_existing_event_error() {
        let mut runtime = DforRuntime::new(false, false);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7TIM);
        let err = runtime.handle(request(3, "ERR", vec![Field::raw("scope", "3")]));
        assert_eq!(err.field("errcode"), Some("51"));
        runtime.push_event(Event::ready());
        assert_eq!(runtime.handle(request(4, "NXT", vec![])).status, D7OK);
        let cleared = runtime.handle(request(5, "ERR", vec![Field::raw("scope", "3")]));
        assert_eq!(cleared.field("errcode"), Some("0"));
        assert_eq!(cleared.field("message"), Some(""));
    }

    #[test]
    fn next_event_eof_records_event_error_scope() {
        let mut runtime = DforRuntime::new(false, false);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        runtime.current_event = Some(Event::ready());
        assert_eq!(runtime.handle(request(2, "END", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(3, "NXT", vec![])).status, D7EOF);
        assert_eq!(runtime.errors.sev.errcode, D7EOF);
        assert_eq!(runtime.errors.sev.why, WHY_OK);
        assert!(runtime.current_event.is_none());
    }

    #[test]
    fn dry_run_send_validates_arguments() {
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow(&mut backend);
        let mut runtime = DforRuntime::with_mock_rest_backend(true, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(
            runtime
                .handle(request(
                    2,
                    "SND",
                    vec![
                        Field::raw("channel_id", "100000000000000001"),
                        Field::percent("content", "hello"),
                    ],
                ))
                .status,
            D7DRY
        );
        assert_eq!(
            runtime
                .handle(request(
                    3,
                    "SND",
                    vec![
                        Field::raw("channel_id", "not-an-id"),
                        Field::percent("content", "hello"),
                    ],
                ))
                .status,
            D7ARG
        );
    }

    #[test]
    fn dry_run_send_runs_preflight_and_blocks_missing_permission() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(
            r#"{"id":"100000000000000001","guild_id":"100000000000000010","type":0}"#,
        ));
        backend.push_response(rest_body(r#"{"id":"100000000000000009"}"#));
        backend.push_response(rest_body(
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#,
        ));
        backend.push_response(rest_body(
            r#"[{"id":"100000000000000010","permissions":"1024","position":0}]"#,
        ));
        let mut runtime = DforRuntime::with_mock_rest_backend(true, false, backend);
        seed_known_guild(&mut runtime, "100000000000000010", now_ms_u64());
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::percent("content", "hello"),
            ],
        ));
        assert_eq!(response.status, D7FOR);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some("MISSING_SEND_MESSAGES"));
    }

    #[test]
    fn direct_message_receive_send_and_own_delete_use_no_guild_permissions() {
        let now = now_ms_u64();
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(r#"{"id":"20","type":1}"#));
        backend.push_response(rest_body(
            r#"{"id":"31","channel_id":"20","author":{"id":"99"},"type":0}"#,
        ));
        backend.push_response(crate::rest::RestResponse::ok());
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, now).unwrap();
        runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"channel_id":"20","id":"30","content":"ping","type":0,"author":{"id":"40"}}}"#, now).unwrap();
        runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"channel_id":"20","id":"31","content":"own","type":0,"author":{"id":"99","bot":true}}}"#, now).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(
            runtime
                .handle(request(2, "NXT", vec![]))
                .field("event_type"),
            Some("100")
        );
        assert_eq!(
            runtime
                .handle(request(3, "NXT", vec![]))
                .field("event_type"),
            Some("101")
        );
        assert_eq!(
            runtime
                .handle(request(4, "GET", vec![Field::raw("name", "GID")]))
                .status,
            D7STA
        );
        assert_eq!(
            runtime
                .handle(request(5, "GET", vec![Field::raw("name", "TXT")]))
                .field("value"),
            Some("ping")
        );
        assert_eq!(
            runtime.handle(request(6, "CSP", vec![])).field("can"),
            Some("1")
        );
        let forbidden = runtime.handle(request(7, "CDP", vec![]));
        assert_eq!(forbidden.field("can"), Some("0"));
        assert_eq!(forbidden.field("why"), Some("DM_MESSAGE_NOT_OWNED"));
        assert_eq!(
            runtime
                .handle(request(
                    8,
                    "SND",
                    vec![
                        Field::raw("channel_id", "20"),
                        Field::percent("content", "pong")
                    ]
                ))
                .status,
            D7OK
        );
        assert_eq!(
            runtime
                .handle(request(
                    9,
                    "DEL",
                    vec![
                        Field::raw("channel_id", "20"),
                        Field::raw("message_id", "31"),
                        Field::percent("reason", "")
                    ]
                ))
                .status,
            D7OK
        );
        assert_eq!(
            mock_rest_paths(&runtime),
            vec![
                "/api/v10/channels/20",
                "/api/v10/channels/20/messages",
                "/api/v10/channels/20/messages/31"
            ]
        );
    }

    #[test]
    fn a_dm_channel_cannot_bypass_a_guild_context_or_group_dm_restrictions() {
        let mut runtime = DforRuntime::new(false, false);
        let now = now_ms_u64();
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("", "20", 1, now));
        assert_eq!(
            runtime.send_preflight_decision("10", "20", now).status,
            D7STA
        );
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("10", "21", 1, now));
        assert_eq!(
            runtime.send_preflight_decision("10", "21", now).status,
            D7STA
        );
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new("", "22", 3, now));
        assert!(!runtime.send_preflight_decision("", "22", now).can);
    }

    #[test]
    fn gateway_json_becomes_fortran_visible_message_event() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"CHANNEL_CREATE","d":{"guild_id":"10","id":"20","type":0,"parent_id":"90"}}"#,
                99,
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"30","content":"hello \uD83D\uDE00","attachments":[{"filename":"a.png","content_type":"image/png"},{"filename":"b.jpg"},{"filename":"c.txt","content_type":"text/plain"}],"author":{"id":"40","bot":false},"member":{"roles":["50","60"]}}}"#,
                100,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let channel = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(channel.status, D7OK);
        assert_eq!(channel.field("event_type"), Some("114"));
        let next = runtime.handle(request(12, "NXT", vec![]));
        assert_eq!(next.status, D7OK);
        assert_eq!(next.field("event_type"), Some("101"));
        let text = runtime.handle(request(3, "GET", vec![Field::raw("name", "TXT")]));
        assert_eq!(text.status, D7OK);
        let value = text.field("value").unwrap();
        assert!(value.starts_with("hello "));
        assert_eq!(value.chars().last(), char::from_u32(0x1F600));
        let roles = runtime.handle(request(4, "RLC", vec![]));
        assert_eq!(roles.status, D7OK);
        assert_eq!(roles.field("count"), Some("2"));
        let images = runtime.handle(request(5, "GET", vec![Field::raw("name", "IMC")]));
        assert_eq!(images.status, D7OK);
        assert_eq!(images.field("value"), Some("2"));
        let category = runtime.handle(request(6, "GET", vec![Field::raw("name", "CAT")]));
        assert_eq!(category.status, D7OK);
        assert_eq!(category.field("value"), Some("90"));
    }

    #[test]
    fn message_search_finds_snapshot_messages_in_snowflake_window() {
        let guild_id = "100000000000000001";
        let user_id = "100000000000000004";
        let base_ms = 1_700_000_000_000;
        let base_id = snowflake_at(base_ms);
        let before_id = snowflake_at(base_ms - 30_000);
        let after_id = snowflake_at(base_ms + 30_000);
        let old_id = snowflake_at(base_ms - 61_000);
        let wrong_user_id = snowflake_at(base_ms - 10_000);
        let low_image_id = snowflake_at(base_ms + 10_000);
        let bot_id = snowflake_at(base_ms + 20_000);
        let undeletable_id = snowflake_at(base_ms + 40_000);
        let mut runtime = DforRuntime::new(false, false);
        for (sequence, channel_id, message_id, user, image_count, bot, message_type) in [
            (
                1,
                "100000000000000010",
                before_id.as_str(),
                user_id,
                2,
                false,
                0,
            ),
            (
                2,
                "100000000000000011",
                base_id.as_str(),
                user_id,
                3,
                false,
                0,
            ),
            (
                3,
                "100000000000000012",
                after_id.as_str(),
                user_id,
                2,
                false,
                0,
            ),
            (
                4,
                "100000000000000013",
                old_id.as_str(),
                user_id,
                2,
                false,
                0,
            ),
            (
                5,
                "100000000000000014",
                wrong_user_id.as_str(),
                "100000000000000005",
                2,
                false,
                0,
            ),
            (
                6,
                "100000000000000015",
                low_image_id.as_str(),
                user_id,
                1,
                false,
                0,
            ),
            (
                7,
                "100000000000000016",
                bot_id.as_str(),
                user_id,
                2,
                true,
                0,
            ),
            (
                8,
                "100000000000000017",
                undeletable_id.as_str(),
                user_id,
                2,
                false,
                21,
            ),
        ] {
            runtime
                .ingest_gateway_json(
                    &message_json(
                        sequence,
                        guild_id,
                        channel_id,
                        message_id,
                        user,
                        image_count,
                        bot,
                        message_type,
                    ),
                    now_ms_u64(),
                )
                .unwrap();
        }

        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let found = runtime.handle(request(
            2,
            "MSF",
            vec![
                Field::raw("guild_id", guild_id),
                Field::raw("user_id", user_id),
                Field::raw("base_message_id", base_id.as_str()),
                Field::raw("before_seconds", "60"),
                Field::raw("after_seconds", "60"),
                Field::raw("min_image_count", "2"),
                Field::raw("limit", "10"),
            ],
        ));
        assert_eq!(found.status, D7OK);
        assert_eq!(found.field("count"), Some("2"));
        assert_eq!(found.field("truncated"), Some("0"));

        let first = runtime.handle(request(3, "MSI", vec![Field::raw("index", "1")]));
        assert_eq!(first.status, D7OK);
        assert_eq!(first.field("channel_id"), Some("100000000000000010"));
        assert_eq!(first.field("message_id"), Some(before_id.as_str()));
        assert_eq!(first.field("image_count"), Some("2"));
        assert_eq!(first.field("delta_seconds"), Some("-30"));

        let second = runtime.handle(request(4, "MSI", vec![Field::raw("index", "2")]));
        assert_eq!(second.status, D7OK);
        assert_eq!(second.field("channel_id"), Some("100000000000000012"));
        assert_eq!(second.field("message_id"), Some(after_id.as_str()));
        assert_eq!(second.field("delta_seconds"), Some("30"));
    }

    #[test]
    fn message_search_reflects_attachment_edits_and_removals() {
        let guild = "100000000000000001";
        let user = "100000000000000004";
        let channel = "100000000000000010";
        let now = now_ms_u64();
        let base = snowflake_at(now);
        let message = snowflake_at(now - 1_000);
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                &message_json(1, guild, channel, &message, user, 2, false, 0),
                now,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let search = || {
            request(
                2,
                "MSF",
                vec![
                    Field::raw("guild_id", guild),
                    Field::raw("user_id", user),
                    Field::raw("base_message_id", base.clone()),
                    Field::raw("before_seconds", "60"),
                    Field::raw("after_seconds", "0"),
                    Field::raw("min_image_count", "2"),
                    Field::raw("limit", "10"),
                ],
            )
        };
        assert_eq!(runtime.handle(search()).field("count"), Some("1"));
        for (sequence, fields, expected) in [
            (2, r#""content":"edited""#, "1"),
            (3, r#""attachments":[]"#, "0"),
            (
                4,
                r#""attachments":[{"filename":"one.PNG"},{"filename":"two.webp"}]"#,
                "1",
            ),
        ] {
            runtime.ingest_gateway_json(
                &format!(r#"{{"op":0,"s":{sequence},"t":"MESSAGE_UPDATE","d":{{"channel_id":"{channel}","id":"{message}",{fields}}}}}"#),
                now,
            ).unwrap();
            assert_eq!(runtime.handle(search()).field("count"), Some(expected));
        }
    }

    #[test]
    fn message_search_excludes_expired_and_deleted_records_after_partial_updates() {
        let now = now_ms_u64();
        let guild = "100000000000000001";
        let user = "100000000000000004";
        let channel = "100000000000000002";
        let base = snowflake_at(now);
        let message = snowflake_at(now - 1000);
        let mut runtime = DforRuntime::new(false, false);
        let old = now.saturating_sub(runtime.settings.message_ttl_ms + 1000);
        runtime
            .ingest_gateway_json(
                &message_json(1, guild, channel, &message, user, 2, false, 0),
                old,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let search = || {
            request(
                2,
                "MSF",
                vec![
                    Field::raw("guild_id", guild),
                    Field::raw("user_id", user),
                    Field::raw("base_message_id", &base),
                    Field::raw("before_seconds", "60"),
                    Field::raw("after_seconds", "0"),
                    Field::raw("min_image_count", "2"),
                    Field::raw("limit", "10"),
                ],
            )
        };
        assert_eq!(runtime.handle(search()).field("count"), Some("0"));
        runtime.ingest_gateway_json(&format!(r#"{{"op":0,"s":2,"t":"MESSAGE_UPDATE","d":{{"channel_id":"{channel}","id":"{message}","content":"partial edit"}}}}"#), now).unwrap();
        assert_eq!(runtime.handle(search()).field("count"), Some("0"));
        let full_update = format!(
            r#"{{"guild_id":"{guild}","channel_id":"{channel}","id":"{message}","author":{{"id":"{user}"}},"type":0,"content":"full edit","attachments":[{{"filename":"one.png"}},{{"filename":"two.png"}}]}}"#
        );
        runtime
            .ingest_gateway_json(
                &format!(r#"{{"op":0,"s":3,"t":"MESSAGE_UPDATE","d":{full_update}}}"#),
                now,
            )
            .unwrap();
        assert_eq!(runtime.handle(search()).field("count"), Some("1"));
        runtime.ingest_gateway_json(&format!(r#"{{"op":0,"s":4,"t":"MESSAGE_DELETE","d":{{"channel_id":"{channel}","id":"{message}"}}}}"#), now).unwrap();
        runtime
            .ingest_gateway_json(
                &format!(r#"{{"op":0,"s":5,"t":"MESSAGE_UPDATE","d":{full_update}}}"#),
                now,
            )
            .unwrap();
        assert_eq!(runtime.handle(search()).field("count"), Some("0"));
        assert!(matches!(
            runtime
                .gateway
                .snapshot_mut()
                .get_message(channel, &message, now),
            SnapshotLookup::Tombstone
        ));
    }

    #[test]
    fn message_search_honours_limit_and_snowflake_delta() {
        let guild_id = "100000000000000001";
        let user_id = "100000000000000004";
        let base_ms = 1_700_000_000_000;
        let base_id = snowflake_at(base_ms);
        let before_id = snowflake_at(base_ms - 30_000);
        let after_id = snowflake_at(base_ms + 30_000);
        let mut runtime = DforRuntime::new(false, false);
        for (sequence, channel_id, message_id) in [
            (1, "100000000000000010", before_id.as_str()),
            (2, "100000000000000011", base_id.as_str()),
            (3, "100000000000000012", after_id.as_str()),
        ] {
            runtime
                .ingest_gateway_json(
                    &message_json(
                        sequence, guild_id, channel_id, message_id, user_id, 2, false, 0,
                    ),
                    now_ms_u64(),
                )
                .unwrap();
        }
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);

        let found = runtime.handle(request(
            2,
            "MSF",
            vec![
                Field::raw("guild_id", guild_id),
                Field::raw("user_id", user_id),
                Field::raw("base_message_id", base_id.as_str()),
                Field::raw("before_seconds", "60"),
                Field::raw("after_seconds", "60"),
                Field::raw("min_image_count", "2"),
                Field::raw("limit", "1"),
            ],
        ));
        assert_eq!(found.status, D7OK);
        assert_eq!(found.field("count"), Some("1"));
        assert_eq!(found.field("truncated"), Some("1"));
        assert_eq!(
            runtime
                .handle(request(3, "MSI", vec![Field::raw("index", "2")]))
                .status,
            D7ARG
        );

        let delta = runtime.handle(request(
            4,
            "SFD",
            vec![
                Field::raw("id1", after_id.as_str()),
                Field::raw("id2", base_id.as_str()),
            ],
        ));
        assert_eq!(delta.status, D7OK);
        assert_eq!(delta.field("delta_seconds"), Some("30"));

        let invalid = runtime.handle(request(
            5,
            "SFD",
            vec![
                Field::raw("id1", "not-a-snowflake"),
                Field::raw("id2", base_id.as_str()),
            ],
        ));
        assert_eq!(invalid.status, D7ARG);
    }

    #[test]
    fn unknown_gateway_dispatch_is_visible_by_event_name() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(r#"{"op":0,"s":1,"t":"SOME_FUTURE_EVENT","d":{}}"#, 100)
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let next = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(next.status, D7OK);
        assert_eq!(next.field("event_type"), Some("106"));
        let name = runtime.handle(request(3, "GET", vec![Field::raw("name", "ENM")]));
        assert_eq!(name.status, D7OK);
        assert_eq!(name.field("value"), Some("SOME_FUTURE_EVENT"));
    }

    #[test]
    fn gateway_resource_dispatches_are_fortran_visible() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"CHANNEL_UPDATE","d":{"guild_id":"10","id":"20","type":0}}"#,
                100,
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"30","permissions":"2048","position":2}}}"#,
                101,
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":3,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"40"},"roles":["30"]}}"#,
                102,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);

        let channel = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(channel.status, D7OK);
        assert_eq!(channel.field("event_type"), Some("115"));
        for (id, name, expected) in [
            (3, "ENM", "CHANNEL_UPDATE"),
            (4, "GID", "10"),
            (5, "CHN", "20"),
        ] {
            let response = runtime.handle(request(id, "GET", vec![Field::raw("name", name)]));
            assert_eq!(response.status, D7OK);
            assert_eq!(response.field("value"), Some(expected));
        }

        let role = runtime.handle(request(6, "NXT", vec![]));
        assert_eq!(role.status, D7OK);
        assert_eq!(role.field("event_type"), Some("126"));
        let roles = runtime.handle(request(7, "RLC", vec![]));
        assert_eq!(roles.status, D7OK);
        assert_eq!(roles.field("count"), Some("1"));
        let role_id = runtime.handle(request(8, "RLI", vec![Field::raw("index", "1")]));
        assert_eq!(role_id.status, D7OK);
        assert_eq!(role_id.field("role_id"), Some("30"));

        let member = runtime.handle(request(9, "NXT", vec![]));
        assert_eq!(member.status, D7OK);
        assert_eq!(member.field("event_type"), Some("128"));
        let user = runtime.handle(request(10, "GET", vec![Field::raw("name", "USR")]));
        assert_eq!(user.status, D7OK);
        assert_eq!(user.field("value"), Some("40"));
    }

    #[test]
    fn reaction_gateway_dispatches_are_fortran_visible() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_REACTION_ADD","d":{"guild_id":"10","channel_id":"20","message_id":"30","user_id":"40","emoji":{"id":"50","name":"party"}}}"#,
                100,
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_REACTION_REMOVE","d":{"guild_id":"10","channel_id":"20","message_id":"30","user_id":"40","emoji":{"name":"🔥"}}}"#,
                101,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let add = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(add.status, D7OK);
        assert_eq!(add.field("event_type"), Some("107"));
        for (id, name, expected) in [
            (3, "ENM", "MESSAGE_REACTION_ADD"),
            (4, "GID", "10"),
            (5, "CHN", "20"),
            (6, "MID", "30"),
            (7, "USR", "40"),
            (8, "EMJ", "party:50"),
        ] {
            let response = runtime.handle(request(id, "GET", vec![Field::raw("name", name)]));
            assert_eq!(response.status, D7OK);
            assert_eq!(response.field("value"), Some(expected));
        }
        let remove = runtime.handle(request(9, "NXT", vec![]));
        assert_eq!(remove.status, D7OK);
        assert_eq!(remove.field("event_type"), Some("108"));
        let emoji = runtime.handle(request(10, "GET", vec![Field::raw("name", "EMJ")]));
        assert_eq!(emoji.status, D7OK);
        assert_eq!(emoji.field("value"), Some("🔥"));
    }

    #[test]
    fn member_gateway_dispatches_are_fortran_visible() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"GUILD_MEMBER_ADD","d":{"guild_id":"10","user":{"id":"40"},"roles":[]}}"#,
                100,
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"GUILD_MEMBER_REMOVE","d":{"guild_id":"10","user":{"id":"40"}}}"#,
                101,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let add = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(add.status, D7OK);
        assert_eq!(add.field("event_type"), Some("109"));
        for (id, name, expected) in [
            (3, "ENM", "GUILD_MEMBER_ADD"),
            (4, "GID", "10"),
            (5, "USR", "40"),
        ] {
            let response = runtime.handle(request(id, "GET", vec![Field::raw("name", name)]));
            assert_eq!(response.status, D7OK);
            assert_eq!(response.field("value"), Some(expected));
        }
        let remove = runtime.handle(request(6, "NXT", vec![]));
        assert_eq!(remove.status, D7OK);
        assert_eq!(remove.field("event_type"), Some("110"));
    }

    #[test]
    fn category_getter_fetches_missing_channel_snapshot() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(
            r#"{"id":"20","guild_id":"10","type":0,"parent_id":"90"}"#,
        ));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"30","content":"hello","author":{"id":"40","bot":false},"member":{"roles":[]}}}"#,
                100,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let next = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(next.status, D7OK);
        let category = runtime.handle(request(3, "GET", vec![Field::raw("name", "CAT")]));
        assert_eq!(category.status, D7OK);
        assert_eq!(category.field("value"), Some("90"));
        assert_eq!(mock_rest_paths(&runtime), vec!["/api/v10/channels/20"]);
    }

    #[test]
    fn gateway_json_becomes_fortran_visible_interaction_event() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"INTERACTION_CREATE","d":{"id":"100000000000000001","application_id":"100000000000000002","type":2,"token":"token_abc","guild_id":"100000000000000003","channel_id":"100000000000000004","member":{"user":{"id":"100000000000000005"}},"data":{"name":"ping"}}}"#,
                100,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let next = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(next.status, D7OK);
        assert_eq!(next.field("event_type"), Some("105"));
        for (id, name, expected) in [
            (3, "IID", "100000000000000001"),
            (4, "APP", "100000000000000002"),
            (5, "ITK", "token_abc"),
            (6, "USR", "100000000000000005"),
            (7, "INM", "ping"),
        ] {
            let response = runtime.handle(request(id, "GET", vec![Field::raw("name", name)]));
            assert_eq!(response.status, D7OK);
            assert_eq!(response.field("value"), Some(expected));
        }
        let itype = runtime.handle(request(8, "GET", vec![Field::raw("name", "ITP")]));
        assert_eq!(itype.status, D7OK);
        assert_eq!(itype.field("value"), Some("2"));
    }

    #[test]
    fn interaction_callback_classifies_returned_unknown_interaction_after_deadline() {
        let stale_event_ms =
            now_ms_u64().saturating_sub(INTERACTION_INITIAL_RESPONSE_DEADLINE_MS.saturating_add(1));
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10062));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"INTERACTION_CREATE","d":{"id":"100000000000000001","application_id":"100000000000000002","type":2,"token":"token_abc","guild_id":"100000000000000003","channel_id":"100000000000000004","member":{"user":{"id":"100000000000000005"}},"data":{"name":"ping"}}}"#,
                stale_event_ms,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);

        let response = runtime.handle(request(
            3,
            "ICB",
            vec![
                Field::raw("interaction_id", "100000000000000001"),
                Field::percent("interaction_token", "token_abc"),
                Field::percent("payload", r#"{"type":4,"data":{"content":"late"}}"#),
            ],
        ));
        assert_eq!(response.status, D7TIM);
        assert_eq!(
            mock_rest_paths(&runtime),
            vec!["/api/v10/interactions/100000000000000001/token_abc/callback"]
        );
        let why = runtime.handle(request(4, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(
            why.field("message"),
            Some(WHY_INTERACTION_DEADLINE_EXCEEDED)
        );
        let err = runtime.handle(request(5, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(err.field("errcode"), Some("51"));
        assert_eq!(err.field("http"), Some("404"));
        assert_eq!(err.field("discord_code"), Some("10062"));
    }

    #[test]
    fn interaction_callback_keeps_unknown_interaction_when_within_deadline() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10062));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"INTERACTION_CREATE","d":{"id":"100000000000000001","application_id":"100000000000000002","type":2,"token":"token_abc","guild_id":"100000000000000003","channel_id":"100000000000000004","member":{"user":{"id":"100000000000000005"}},"data":{"name":"ping"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);

        let response = runtime.handle(request(
            3,
            "ICB",
            vec![
                Field::raw("interaction_id", "100000000000000001"),
                Field::percent("interaction_token", "token_abc"),
                Field::percent("payload", r#"{"type":4,"data":{"content":"ok"}}"#),
            ],
        ));
        assert_eq!(response.status, D7NFD);
        assert_eq!(
            mock_rest_paths(&runtime),
            vec!["/api/v10/interactions/100000000000000001/token_abc/callback"]
        );
        let why = runtime.handle(request(4, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some("INTERACTION_NOT_FOUND"));
        let err = runtime.handle(request(5, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(err.field("errcode"), Some("41"));
        assert_eq!(err.field("http"), Some("404"));
        assert_eq!(err.field("discord_code"), Some("10062"));
    }

    #[test]
    fn role_index_getter_distinguishes_event_state_from_bad_index() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"30","content":"hello","author":{"id":"40","bot":false},"member":{"roles":["50","60"]}}}"#,
                100,
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_DELETE","d":{"guild_id":"10","channel_id":"20","id":"31"}}"#,
                101,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(
            runtime
                .handle(request(2, "RLI", vec![Field::raw("index", "1")]))
                .status,
            D7STA
        );
        assert_eq!(runtime.handle(request(3, "NXT", vec![])).status, D7OK);
        let role = runtime.handle(request(4, "RLI", vec![Field::raw("index", "2")]));
        assert_eq!(role.status, D7OK);
        assert_eq!(role.field("role_id"), Some("60"));
        assert_eq!(
            runtime
                .handle(request(5, "RLI", vec![Field::raw("index", "0")]))
                .status,
            D7ARG
        );
        assert_eq!(
            runtime
                .handle(request(6, "RLI", vec![Field::raw("index", "3")]))
                .status,
            D7ARG
        );
        assert_eq!(runtime.handle(request(7, "NXT", vec![])).status, D7OK);
        assert_eq!(
            runtime
                .handle(request(8, "RLI", vec![Field::raw("index", "1")]))
                .status,
            D7STA
        );
    }

    #[test]
    fn runtime_event_queue_full_emits_gap_and_requests_resume() {
        let settings = RuntimeSettings {
            event_queue_capacity: 1,
            ..RuntimeSettings::default()
        };
        let mut runtime = DforRuntime::with_settings(false, false, settings);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
                0,
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"30","content":"one","author":{"id":"40"}}}"#,
                0,
            )
            .unwrap();
        let action = runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"10","channel_id":"20","id":"31","content":"two","author":{"id":"40"}}}"#,
                0,
            )
            .unwrap();
        assert_eq!(action, GatewayAction::SendResume { seq: 3 });
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let ready = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(ready.status, D7OK);
        assert_eq!(ready.field("event_type"), Some("100"));
        let gap = runtime.handle(request(3, "NXT", vec![]));
        assert_eq!(gap.status, D7OK);
        assert_eq!(gap.field("event_type"), Some("103"));
        assert_eq!(
            runtime
                .handle(request(4, "GET", vec![Field::raw("name", "TXT")]))
                .status,
            D7STA
        );
        let preflight = runtime.handle(request(
            5,
            "CDL",
            vec![
                Field::raw("guild_id", "10"),
                Field::raw("channel_id", "20"),
                Field::raw("message_id", "30"),
            ],
        ));
        assert_eq!(preflight.status, D7STA);
        assert_eq!(preflight.field("can"), Some("0"));
        assert_eq!(
            preflight.field("why"),
            Some(WHY_MESSAGE_SNAPSHOT_INCOMPLETE)
        );
    }

    #[test]
    fn gateway_json_delete_becomes_mde_event() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_DELETE","d":{"guild_id":"10","channel_id":"20","id":"30"}}"#,
                100,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let next = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(next.status, D7OK);
        assert_eq!(next.field("event_type"), Some("102"));
        assert_eq!(
            runtime
                .handle(request(3, "GET", vec![Field::raw("name", "MID")]))
                .field("value"),
            Some("30")
        );
        assert_eq!(
            runtime
                .handle(request(4, "GET", vec![Field::raw("name", "TXT")]))
                .status,
            D7STA
        );
    }

    #[test]
    fn delete_preflight_uses_message_snapshot_type() {
        let mut runtime = DforRuntime::new(false, false);
        let now = now_ms_u64();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"starter","type":21,"author":{"id":"100000000000000004"}}}"#,
                now,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_MESSAGE_TYPE_NOT_DELETABLE));
    }

    #[test]
    fn delete_preflight_reports_tombstone_and_unknown_type() {
        let mut runtime = DforRuntime::new(false, false);
        let now = now_ms_u64();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"old","type":0,"author":{"id":"100000000000000004"}}}"#,
                now,
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_DELETE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003"}}"#,
                now.saturating_add(10),
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":3,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000005","content":"future","type":999,"author":{"id":"100000000000000004"}}}"#,
                now.saturating_add(20),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let tombstone = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(tombstone.status, D7NFD);
        assert_eq!(tombstone.field("can"), Some("0"));
        assert_eq!(tombstone.field("why"), Some(WHY_MESSAGE_NOT_FOUND));
        let unknown = runtime.handle(request(
            3,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000005"),
            ],
        ));
        assert_eq!(unknown.status, D7STA);
        assert_eq!(unknown.field("can"), Some("0"));
        assert_eq!(unknown.field("why"), Some(WHY_UNKNOWN_MESSAGE_TYPE));
    }

    #[test]
    fn current_delete_preflight_uses_current_message_snapshot() {
        let mut runtime = DforRuntime::new(false, false);
        let now = now_ms_u64();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"starter","type":21,"author":{"id":"100000000000000004"}}}"#,
                now,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
        let response = runtime.handle(request(3, "CDP", vec![]));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_MESSAGE_TYPE_NOT_DELETABLE));
    }

    #[test]
    fn gateway_session_boundaries_cannot_reuse_old_authority_or_pending_messages() {
        for replacement in 0..3 {
            for owner in [false, true] {
                for queued in [false, true] {
                    let mut runtime = DforRuntime::new(false, false);
                    let now = now_ms_u64();
                    runtime.handle(request(1, "INI", vec![]));
                    runtime.ingest_gateway_json(r#"{"op":0,"s":10,"t":"READY","d":{"session_id":"old","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, now).unwrap();
                    let guild = format!(
                        r#"{{"op":0,"s":11,"t":"GUILD_CREATE","d":{{"id":"10","owner_id":"{}","roles":[{{"id":"10","permissions":"0","position":0}},{{"id":"50","permissions":"{}","position":1}}],"members":[{{"user":{{"id":"40"}},"roles":["50"],"communication_disabled_until":null}}]}}}}"#,
                        if owner { "40" } else { "70" },
                        if owner { "0" } else { "8" }
                    );
                    runtime.ingest_gateway_json(&guild, now).unwrap();
                    let message = format!(
                        r#"{{"op":0,"s":12,"t":"MESSAGE_CREATE","d":{{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{{"id":"40"}},"member":{{"roles":["50"]{}}}}}}}"#,
                        if owner { "" } else { r#", "permissions":"8""# }
                    );
                    runtime.ingest_gateway_json(&message, now).unwrap();
                    if !queued {
                        for _ in 0..3 {
                            assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
                        }
                        assert_eq!(
                            runtime.handle(request(3, "CAD", vec![])).field("is_admin"),
                            Some("1")
                        );
                    }
                    runtime.bot_user_id_cache = Some("99".into());
                    let (session_id, sequence) = if replacement == 2 {
                        ("old", 14)
                    } else {
                        ("new", 1)
                    };
                    if replacement != 0 {
                        let session =
                            GatewayResumeSession::new(session_id, "wss://new", sequence, None)
                                .unwrap();
                        assert_eq!(
                            runtime.restore_gateway_resume_session(session),
                            Ok(GatewayAction::SendResume { seq: sequence })
                        );
                    } else {
                        runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"new","resume_gateway_url":"wss://new"}}"#, now).unwrap();
                    }
                    assert_eq!(runtime.bot_user_id_cache, None);
                    assert_eq!(runtime.gateway.snapshot().message_count(), 0);
                    assert_eq!(
                        runtime.gateway.snapshot_mut().get_guild_roles("10", now),
                        SnapshotLookup::Missing
                    );
                    if !queued {
                        assert_eq!(
                            runtime.current_event.as_ref().unwrap().member_permissions,
                            None
                        );
                        assert_eq!(runtime.handle(request(4, "CAD", vec![])).status, D7STA);
                    }
                    let mut gaps = 0;
                    while !runtime.events.is_empty() {
                        let event = runtime.handle(request(5, "NXT", vec![]));
                        assert_eq!(event.status, D7OK);
                        assert_ne!(event.field("event_type"), Some("101"));
                        gaps += usize::from(event.field("event_type") == Some("103"));
                    }
                    assert_eq!(gaps, 1);
                    let message = format!(
                        r#"{{"op":0,"s":{},"t":"MESSAGE_CREATE","d":{{"guild_id":"10","channel_id":"20","id":"31","type":0,"author":{{"id":"40"}},"member":{{"permissions":"0"}}}}}}"#,
                        sequence + 1
                    );
                    runtime.ingest_gateway_json(&message, now).unwrap();
                    assert_eq!(
                        runtime
                            .handle(request(6, "NXT", vec![]))
                            .field("event_type"),
                        Some("101")
                    );
                    assert_eq!(
                        runtime.handle(request(7, "CAD", vec![])).field("is_admin"),
                        Some("0")
                    );
                }
            }
        }
    }

    #[test]
    fn delayed_creations_cannot_reauthorise_an_old_administrator_after_a_known_edit() {
        for timestamp in [r#", "edited_timestamp":null"#, ""] {
            let mut runtime = DforRuntime::new(false, false);
            let now = now_ms_u64();
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0}],"members":[{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}]}}"#, now).unwrap();
            assert!(runtime.gateway.snapshot_mut().apply_rest_message_response(&parse_json(r#"{"guild_id":"10","channel_id":"20","id":"30","author":{"id":"40"},"type":0,"content":"current edit","attachments":[],"edited_timestamp":"1970-01-01T00:00:02Z"}"#).unwrap(), "20", "30", now));
            runtime.ingest_gateway_json(&format!(r#"{{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{{"guild_id":"10","channel_id":"20","id":"30","author":{{"id":"40"}},"type":0,"content":"old post","attachments":[],"member":{{"permissions":"8"}}{timestamp}}}}}"#), now).unwrap();
            runtime.handle(request(1, "INI", vec![]));
            runtime.handle(request(2, "NXT", vec![]));
            assert_eq!(
                runtime
                    .handle(request(3, "NXT", vec![]))
                    .field("event_type"),
                Some("101")
            );
            let admin = runtime.handle(request(4, "CAD", vec![]));
            assert_eq!(admin.status, D7OK);
            assert_eq!(admin.field("is_admin"), Some("0"));
            assert_eq!(
                runtime
                    .handle(request(5, "GET", vec![Field::raw("name", "TXT")]))
                    .field("value"),
                Some("old post")
            );
        }
    }

    #[test]
    fn current_admin_uses_gateway_member_permissions() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"admin","type":0,"author":{"id":"100000000000000004"},"member":{"permissions":"8"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
        let response = runtime.handle(request(3, "CAD", vec![]));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("is_admin"), Some("1"));
    }

    #[test]
    fn cached_message_permissions_do_not_survive_later_role_or_member_revocation() {
        for queued in [false, true] {
            for (update, expected) in [
                (
                    r#"{"op":0,"s":3,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}]}}"#,
                    D7OK,
                ),
                (
                    r#"{"op":0,"s":3,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":"0","position":1}}}"#,
                    D7OK,
                ),
                (
                    r#"{"op":0,"s":3,"t":"GUILD_ROLE_DELETE","d":{"guild_id":"10","role_id":"50"}}"#,
                    D7OK,
                ),
                (
                    r#"{"op":0,"s":3,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"40"},"roles":[]}}"#,
                    D7OK,
                ),
                (
                    r#"{"op":0,"s":3,"t":"GUILD_MEMBER_REMOVE","d":{"guild_id":"10","user":{"id":"40"}}}"#,
                    D7NFD,
                ),
                (
                    r#"{"op":0,"s":3,"t":"GUILD_DELETE","d":{"id":"10","unavailable":true}}"#,
                    D7STA,
                ),
                (
                    r#"{"op":0,"s":3,"t":"GUILD_DELETE","d":{"id":"10","unavailable":false}}"#,
                    D7NFD,
                ),
            ] {
                let mut runtime = DforRuntime::new(false, false);
                let now = now_ms_u64();
                assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
                runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}],"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}]}}"#, now).unwrap();
                runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"roles":["50"],"permissions":"8"}}}"#, now).unwrap();
                if !queued {
                    runtime.handle(request(2, "NXT", vec![]));
                    runtime.handle(request(3, "NXT", vec![]));
                    assert_eq!(
                        runtime.handle(request(4, "CAD", vec![])).field("is_admin"),
                        Some("1")
                    );
                }
                runtime.ingest_gateway_json(update, now).unwrap();
                if queued {
                    runtime.handle(request(5, "NXT", vec![]));
                    assert_eq!(
                        runtime
                            .handle(request(6, "NXT", vec![]))
                            .field("event_type"),
                        Some("101")
                    );
                }
                let response = runtime.handle(request(7, "CAD", vec![]));
                assert_eq!(response.status, expected, "queued={queued}: {update}");
                assert_eq!(
                    response.field("is_admin"),
                    (expected == D7OK).then_some("0")
                );
            }
        }
    }

    #[test]
    fn unrelated_guild_or_member_events_preserve_fresh_message_permissions() {
        for queued in [false, true] {
            for update in [
                r#"{"op":0,"s":2,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"11","chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"40"},"roles":[]}]}}"#,
                r#"{"op":0,"s":2,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"41"},"roles":[]}]}}"#,
                r#"{"op":0,"s":2,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[],"not_found":["41"]}}"#,
                r#"{"op":0,"s":2,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"11","role":{"id":"50","permissions":"0","position":1}}}"#,
                r#"{"op":0,"s":2,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"10","user":{"id":"41"},"roles":[]}}"#,
                r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"31","type":0,"author":{"id":"41"}}}"#,
            ] {
                let mut runtime = DforRuntime::new(false, false);
                assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
                let now = now_ms_u64();
                runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"8"}}}"#, now).unwrap();
                if !queued {
                    runtime.handle(request(2, "NXT", vec![]));
                }
                runtime.ingest_gateway_json(update, now).unwrap();
                if queued {
                    runtime.handle(request(3, "NXT", vec![]));
                }
                let response = runtime.handle(request(4, "CAD", vec![]));
                assert_eq!(response.status, D7OK, "queued={queued}: {update}");
                assert_eq!(response.field("is_admin"), Some("1"));
            }
        }
    }

    #[test]
    fn member_chunk_not_found_clears_current_and_queued_authority_including_ownership() {
        for queued in [false, true] {
            for owner in ["40", "70"] {
                let mut runtime = DforRuntime::new(false, false);
                runtime.handle(request(1, "INI", vec![]));
                let now = now_ms_u64();
                runtime.ingest_gateway_json(&format!(r#"{{"op":0,"s":1,"t":"GUILD_CREATE","d":{{"id":"10","owner_id":"{owner}","roles":[{{"id":"10","permissions":"0","position":0}},{{"id":"50","permissions":"8","position":1}}],"members":[{{"user":{{"id":"40"}},"roles":["50"],"communication_disabled_until":null}}]}}}}"#), now).unwrap();
                runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"8"}}}"#, now).unwrap();
                runtime.handle(request(2, "NXT", vec![]));
                if !queued {
                    runtime.handle(request(3, "NXT", vec![]));
                    assert_eq!(
                        runtime.handle(request(4, "CAD", vec![])).field("is_admin"),
                        Some("1")
                    );
                }
                runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","chunk_index":0,"chunk_count":1,"members":[],"not_found":["40"]}}"#, now).unwrap();
                if queued {
                    assert_eq!(
                        runtime
                            .handle(request(5, "NXT", vec![]))
                            .field("event_type"),
                        Some("101")
                    );
                }
                let response = runtime.handle(request(6, "CAD", vec![]));
                assert_eq!(response.status, D7STA, "queued={queued} owner={owner}");
                assert_eq!(response.field("is_admin"), None);
                assert!(runtime
                    .current_event
                    .as_ref()
                    .unwrap()
                    .member_permissions
                    .is_none());
                assert!(matches!(
                    runtime.gateway.snapshot_mut().get_member("10", "40", now),
                    SnapshotLookup::Missing
                ));
            }
        }
    }

    #[test]
    fn gaps_protocol_errors_and_expiry_cannot_reuse_current_message_permissions() {
        for cause in 0..4 {
            let mut runtime = DforRuntime::new(false, false);
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            let now = now_ms_u64();
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"8"}}}"#, now).unwrap();
            runtime.handle(request(2, "NXT", vec![]));
            assert!(runtime.current_event_target_is_admin("10", "40", now));
            match cause {
                0 => {
                    runtime.ingest_gateway_live_json(r#"{"op":0,"s":2,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":false,"position":1}}}"#, now).unwrap();
                }
                1 => {
                    assert!(runtime
                        .ingest_gateway_live_json(
                            r#"{"op":0,"s":"2","t":"GUILD_ROLE_UPDATE","d":{}}"#,
                            now
                        )
                        .is_err());
                }
                2 => {
                    runtime.events.set_max_bytes(1);
                    let mut event = Event::demo_message();
                    event.content = "x".repeat(2000);
                    assert!(runtime.push_event(event));
                }
                3 => {
                    runtime.current_event.as_mut().unwrap().received_at_ms =
                        now.saturating_sub(runtime.settings.member_ttl_ms + 1);
                }
                _ => unreachable!(),
            }
            assert!(!runtime.current_event_target_is_admin("10", "40", now));
            let response = runtime.handle(request(3, "CAD", vec![]));
            assert_eq!(response.status, D7STA, "cause={cause}");
            assert_eq!(response.field("is_admin"), None);
            let author = runtime.handle(request(4, "GET", vec![Field::raw("name", "USR")]));
            assert_eq!(author.status, D7OK);
            assert_eq!(author.field("value"), Some("40"));
        }
    }

    #[test]
    fn refreshed_rest_members_and_roles_replace_message_permission_hints() {
        for member in [false, true] {
            let mut backend = MockRestBackend::default();
            backend.push_response(rest_body(if member {
                r#"{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}"#
            } else {
                r#"[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"0","position":1}]"#
            }));
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            let now = now_ms_u64();
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}],"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}]}}"#, now).unwrap();
            runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"8"}}}"#, now).unwrap();
            runtime.handle(request(1, "INI", vec![]));
            runtime.handle(request(2, "NXT", vec![]));
            runtime.handle(request(3, "NXT", vec![]));
            assert_eq!(
                runtime.handle(request(4, "CAD", vec![])).field("is_admin"),
                Some("1")
            );
            if member {
                runtime
                    .fetch_member_snapshot_decision("10", "40", now, false)
                    .unwrap();
            } else {
                runtime.fetch_guild_roles_decision("10", now).unwrap();
            }
            let response = runtime.handle(request(5, "CAD", vec![]));
            assert_eq!(response.status, D7OK);
            assert_eq!(response.field("is_admin"), Some("0"));
            assert_eq!(mock_rest_paths(&runtime).len(), 1);
        }
    }

    #[test]
    fn current_message_admin_checks_observe_later_permission_grants() {
        let mut runtime = DforRuntime::new(false, false);
        let now = now_ms_u64();
        runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"0","position":1}],"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}]}}"#, now).unwrap();
        runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"0"}}}"#, now).unwrap();
        runtime.handle(request(1, "INI", vec![]));
        runtime.handle(request(2, "NXT", vec![]));
        runtime.handle(request(3, "NXT", vec![]));
        assert_eq!(
            runtime.handle(request(4, "CAD", vec![])).field("is_admin"),
            Some("0")
        );
        runtime.ingest_gateway_json(r#"{"op":0,"s":3,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":"8","position":1}}}"#, now).unwrap();
        let response = runtime.handle(request(5, "CAD", vec![]));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("is_admin"), Some("1"));
    }

    #[test]
    fn fresh_guild_ownership_survives_hint_invalidation_without_requiring_role_fetches() {
        let mut runtime = DforRuntime::new(false, false);
        let now = now_ms_u64();
        runtime.handle(request(1, "INI", vec![]));
        runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"0"}}}"#, now).unwrap();
        runtime.handle(request(2, "NXT", vec![]));
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"GUILD_UPDATE","d":{"id":"10","owner_id":"40"}}"#,
                now,
            )
            .unwrap();
        let response = runtime.handle(request(3, "CAD", vec![]));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("is_admin"), Some("1"));
        runtime
            .gateway
            .snapshot_mut()
            .tombstone_member("10", "40", now);
        assert_eq!(runtime.handle(request(4, "CAD", vec![])).status, D7NFD);
    }

    #[test]
    fn committed_or_uncertain_permission_writes_cannot_leave_message_admin_hints_active() {
        for uncertain in [false, true] {
            for (method, path, payload, refreshed, expected_read) in [
                (
                    "4",
                    "/api/v10/guilds/10/roles/50",
                    r#"{"permissions":"0"}"#,
                    r#"[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"0","position":1}]"#,
                    "/api/v10/guilds/10/roles",
                ),
                (
                    "5",
                    "/api/v10/guilds/10/members/40/roles/50",
                    "",
                    r#"{"user":{"id":"40"},"roles":[],"communication_disabled_until":null}"#,
                    "/api/v10/guilds/10/members/40",
                ),
            ] {
                let mut backend = MockRestBackend::default();
                if uncertain {
                    backend.push_error(crate::rest::RestTransportError::TimeoutAfterSend);
                } else {
                    backend.push_response(crate::rest::RestResponse::ok());
                }
                backend.push_response(rest_body(refreshed));
                let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
                let now = now_ms_u64();
                runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}],"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}]}}"#, now).unwrap();
                runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"permissions":"8"}}}"#, now).unwrap();
                runtime.handle(request(1, "INI", vec![]));
                runtime.handle(request(2, "NXT", vec![]));
                runtime.handle(request(3, "NXT", vec![]));
                assert_eq!(
                    runtime.handle(request(4, "CAD", vec![])).field("is_admin"),
                    Some("1")
                );
                let mutation = runtime.handle(request(
                    5,
                    "RST",
                    vec![
                        Field::raw("method", method),
                        Field::raw("path", path),
                        Field::percent("payload", payload),
                        Field::percent("reason", ""),
                    ],
                ));
                assert_eq!(mutation.status, if uncertain { D7UNC } else { D7OK });
                let admin = runtime.handle(request(6, "CAD", vec![]));
                assert_eq!(admin.status, D7OK);
                assert_eq!(admin.field("is_admin"), Some("0"));
                assert_eq!(mock_rest_paths(&runtime), [path, expected_read]);
            }
        }
    }

    #[test]
    fn current_admin_resolves_cached_members_without_event_permissions() {
        for (role_permissions, owner_id, expected) in
            [("8", "70", "1"), ("0", "70", "0"), ("0", "40", "1")]
        {
            let mut runtime = DforRuntime::new(false, false);
            runtime.ingest_gateway_json(&format!(r#"{{"op":0,"s":1,"t":"GUILD_CREATE","d":{{"id":"10","owner_id":"{owner_id}","roles":[{{"id":"10","permissions":"0","position":0}},{{"id":"50","permissions":"{role_permissions}","position":1}}],"members":[{{"user":{{"id":"40"}},"roles":["50"],"communication_disabled_until":null}}]}}}}"#), now_ms_u64()).unwrap();
            runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"roles":["50"]}}}"#, now_ms_u64()).unwrap();
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
            let message = runtime.handle(request(3, "NXT", vec![]));
            assert_eq!(message.field("event_type"), Some("101"));
            let response = runtime.handle(request(4, "CAD", vec![]));
            assert_eq!(response.status, D7OK);
            assert_eq!(response.field("is_admin"), Some(expected));
        }
    }

    #[test]
    fn replayed_gateway_roles_and_messages_cannot_restore_admin_or_duplicate_bot_events() {
        for live in [false, true] {
            let mut runtime = DforRuntime::new(false, false);
            let now = now_ms_u64();
            for frame in [
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#,
                r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}],"members":[{"user":{"id":"40"},"roles":["50"],"communication_disabled_until":null}]}}"#,
                r#"{"op":0,"s":3,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":"0","position":1}}}"#,
                r#"{"op":0,"s":2,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"10","role":{"id":"50","permissions":"8","position":1}}}"#,
                r#"{"op":0,"s":4,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"roles":["50"]}}}"#,
                r#"{"op":0,"s":4,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"roles":["50"]}}}"#,
            ] {
                if live {
                    runtime.ingest_gateway_live_json(frame, now).unwrap();
                } else {
                    runtime.ingest_gateway_json(frame, now).unwrap();
                }
            }
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            for id in 2..=5 {
                let event = runtime.handle(request(id, "NXT", vec![]));
                assert_eq!(event.status, D7OK);
                if id == 5 {
                    assert_eq!(event.field("event_type"), Some("101"));
                }
            }
            let response = runtime.handle(request(6, "CAD", vec![]));
            assert_eq!(response.status, D7OK);
            assert_eq!(response.field("is_admin"), Some("0"));
            let response = runtime.handle(request(
                7,
                "ADM",
                vec![Field::raw("guild_id", "10"), Field::raw("user_id", "40")],
            ));
            assert_eq!(response.status, D7OK);
            assert_eq!(response.field("is_admin"), Some("0"));
            assert_eq!(runtime.handle(request(8, "NXT", vec![])).status, D7TIM);
            assert_eq!(runtime.gateway_resume_session().unwrap().last_seen_seq, 4);
        }
    }

    #[test]
    fn current_admin_fetches_missing_members_and_roles_without_trusting_event_roles() {
        for (roles, expected) in [(r#"["50"]"#, "1"), ("[]", "0")] {
            let mut backend = MockRestBackend::default();
            let mut member = crate::rest::RestResponse::ok();
            member.body = format!(r#"{{"user":{{"id":"40"}},"roles":{roles}}}"#);
            backend.push_response(member);
            let mut roles = crate::rest::RestResponse::ok();
            roles.body = r#"[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}]"#.into();
            backend.push_response(roles);
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            seed_known_guild(&mut runtime, "10", now_ms_u64());
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"roles":["50"]}}}"#, now_ms_u64()).unwrap();
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
            for id in [3, 4] {
                let response = runtime.handle(request(id, "CAD", vec![]));
                assert_eq!(response.status, D7OK);
                assert_eq!(response.field("is_admin"), Some(expected));
            }
        }
    }

    #[test]
    fn current_admin_preserves_member_fetch_errors_without_granting_admin() {
        for (http, code, status, reason) in [
            (403, 50001, D7FOR, WHY_MISSING_ACCESS),
            (404, 10007, D7NFD, WHY_MEMBER_NOT_FOUND),
            (401, 0, D7FOR, WHY_MISSING_ACCESS),
            (429, 0, D7RTL, "RATE_LIMITED"),
            (200, 0, D7STA, WHY_MEMBER_SNAPSHOT_INCOMPLETE),
        ] {
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::discord_error(http, code));
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"}}}"#, now_ms_u64()).unwrap();
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
            let response = runtime.handle(request(3, "CAD", vec![]));
            assert_eq!(response.status, status, "{http}");
            assert_eq!(response.field("is_admin"), None);
            assert_eq!(runtime.errors.act.http, i32::from(http));
            assert_eq!(runtime.errors.act.discord_code, code);
            assert_eq!(runtime.errors.act.why, reason);
        }
    }

    #[test]
    fn current_admin_does_not_reuse_expired_member_roles() {
        for fetch in [false, true] {
            let mut runtime = DforRuntime::new(false, false);
            if fetch {
                let mut backend = MockRestBackend::default();
                let mut member = crate::rest::RestResponse::ok();
                member.body = r#"{"user":{"id":"40"},"roles":[]}"#.into();
                backend.push_response(member);
                runtime.use_mock_rest_backend(backend);
            }
            let now_ms = now_ms_u64();
            runtime
                .gateway
                .snapshot_mut()
                .upsert_member(MemberSnapshot::new("10", "40", vec!["50".into()], 0));
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","owner_id":"70","roles":[{"id":"10","permissions":"0","position":0},{"id":"50","permissions":"8","position":1}]}}"#, now_ms).unwrap();
            runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"},"member":{"roles":["50"]}}}"#, now_ms).unwrap();
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
            let message = runtime.handle(request(3, "NXT", vec![]));
            assert_eq!(message.field("event_type"), Some("101"));
            let response = runtime.handle(request(4, "CAD", vec![]));
            assert_eq!(response.status, if fetch { D7OK } else { D7STA });
            assert_eq!(
                response.field("is_admin"),
                if fetch { Some("0") } else { None }
            );
        }
    }

    #[test]
    fn current_admin_checks_guild_availability_before_resolving_missing_permissions() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"10","unavailable":true}}"#,
                now_ms_u64(),
            )
            .unwrap();
        runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"10","channel_id":"20","id":"30","type":0,"author":{"id":"40"}}}"#, now_ms_u64()).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
        let message = runtime.handle(request(3, "NXT", vec![]));
        assert_eq!(message.field("event_type"), Some("101"));
        let response = runtime.handle(request(4, "CAD", vec![]));
        assert_eq!(response.status, D7STA);
        assert_eq!(response.field("is_admin"), None);
        assert_eq!(runtime.errors.act.why, WHY_GUILD_UNAVAILABLE);
    }

    #[test]
    fn current_admin_requires_guild_message_context_even_with_permissions() {
        for member in ["{}", r#"{"permissions":"8"}"#] {
            let mut runtime = DforRuntime::new(false, false);
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            assert_eq!(runtime.handle(request(2, "CAD", vec![])).status, D7STA);
            runtime.ingest_gateway_json(&format!(r#"{{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{{"channel_id":"20","id":"30","type":0,"author":{{"id":"40"}},"member":{member}}}}}"#), now_ms_u64()).unwrap();
            assert_eq!(runtime.handle(request(3, "NXT", vec![])).status, D7OK);
            let response = runtime.handle(request(4, "CAD", vec![]));
            assert_eq!(response.status, D7STA);
            assert_eq!(response.field("is_admin"), None);
        }
    }

    #[test]
    fn current_admin_reports_incomplete_member_snapshot_without_permissions() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"member","type":0,"author":{"id":"100000000000000004"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
        let response = runtime.handle(request(3, "CAD", vec![]));
        assert_eq!(response.status, D7STA);
        let why = runtime.handle(request(4, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_MEMBER_SNAPSHOT_INCOMPLETE));
    }

    #[test]
    fn admin_fetches_member_and_roles_to_report_admin() {
        let mut backend = MockRestBackend::default();
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000008"]}"#.to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[
            {"id":"100000000000000001","permissions":"0","position":0},
            {"id":"100000000000000008","permissions":"8","position":2}
        ]"#
        .to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "ADM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("is_admin"), Some("1"));
    }

    #[test]
    fn admin_fetches_member_and_roles_to_report_non_admin() {
        let mut backend = MockRestBackend::default();
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000008"]}"#.to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[
            {"id":"100000000000000001","permissions":"0","position":0},
            {"id":"100000000000000008","permissions":"2048","position":2}
        ]"#
        .to_string();
        backend.push_response(roles);
        backend.push_response(rest_body(
            r#"{"id":"100000000000000001","owner_id":"100000000000000099"}"#,
        ));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "ADM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("is_admin"), Some("0"));
    }

    #[test]
    fn admin_reports_guild_owner_as_admin() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"100000000000000001","owner_id":"100000000000000004","roles":[{"id":"100000000000000001","permissions":"0","position":0}],"members":[{"user":{"id":"100000000000000004"},"roles":[],"communication_disabled_until":null}]}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "ADM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("is_admin"), Some("1"));
    }

    #[test]
    fn admin_reports_incomplete_role_snapshot_in_action_scope() {
        let mut backend = MockRestBackend::default();
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000008"]}"#.to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000001","permissions":"0","position":0}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "ADM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7STA);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_ROLE_SNAPSHOT_INCOMPLETE));
    }

    #[test]
    fn admin_reports_member_not_found_in_action_scope() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10007));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "ADM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7NFD);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_MEMBER_NOT_FOUND));
    }

    #[test]
    fn admin_reports_user_not_found_and_reuses_user_tombstone() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10013));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let first = runtime.handle(request(
            2,
            "ADM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(first.status, D7NFD);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_USER_NOT_FOUND));

        let second = runtime.handle(request(
            4,
            "ADM",
            vec![
                Field::raw("guild_id", "100000000000000002"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(second.status, D7NFD);
        let why = runtime.handle(request(5, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_USER_NOT_FOUND));
    }

    #[test]
    fn admin_reports_gateway_guild_unavailable_before_rest() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"100000000000000001","unavailable":true}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "ADM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7STA);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_GUILD_UNAVAILABLE));
    }

    #[test]
    fn timeout_preflight_reports_gateway_guild_tombstone_before_rest() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"GUILD_DELETE","d":{"id":"100000000000000001","unavailable":false}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CTM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7NFD);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_GUILD_NOT_FOUND));
    }

    #[test]
    fn send_preflight_reports_gateway_guild_tombstone_before_channel_lookup() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"GUILD_DELETE","d":{"id":"100000000000000001","unavailable":false}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7NFD);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_GUILD_NOT_FOUND));
    }

    #[test]
    fn send_preflight_requires_channel_snapshot() {
        let mut runtime = DforRuntime::new(false, false);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7STA);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_CHANNEL_SNAPSHOT_INCOMPLETE));
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_CHANNEL_SNAPSHOT_INCOMPLETE));
    }

    #[test]
    fn send_preflight_channel_fetch_5xx_records_read_only_snapshot_failure() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_server_error());
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7STA);
        assert_ne!(response.status, D7UNC);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_CHANNEL_SNAPSHOT_INCOMPLETE));
        let err = runtime.handle(request(3, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(err.field("errcode"), Some("31"));
        assert_eq!(err.field("http"), Some("500"));
        assert_eq!(err.field("discord_code"), Some("0"));
        let why = runtime.handle(request(4, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_CHANNEL_SNAPSHOT_INCOMPLETE));
    }

    #[test]
    fn send_preflight_private_thread_unknown_access_fetches_thread_member() {
        let mut backend = MockRestBackend::default();
        let mut channel = crate::rest::RestResponse::ok();
        channel.body = r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":false,"locked":false}}"#.to_string();
        backend.push_response(channel);
        backend.push_response(rest_body(
            r#"{"id":"100000000000000002","user_id":"100000000000000009"}"#,
        ));
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#
                .to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000001","permissions":"274877910016","position":0}]"#
            .to_string();
        backend.push_response(roles);
        backend.push_response(rest_body(r#"{"id":"100000000000000010","guild_id":"100000000000000001","type":0,"permission_overwrites":[]}"#));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn send_preflight_private_thread_member_404_is_not_joined() {
        let mut backend = MockRestBackend::default();
        let mut channel = crate::rest::RestResponse::ok();
        channel.body = r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":false,"locked":false}}"#.to_string();
        backend.push_response(channel);
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10007));
        let mut channel = crate::rest::RestResponse::ok();
        channel.body = r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":false,"locked":false}}"#.to_string();
        backend.push_response(channel);
        push_private_thread_permissions(&mut backend, "274877981696");
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_THREAD_NOT_JOINED));
    }

    #[test]
    fn send_preflight_private_thread_member_404_confirms_channel_missing() {
        let mut backend = MockRestBackend::default();
        let mut channel = crate::rest::RestResponse::ok();
        channel.body = r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":false,"locked":false}}"#.to_string();
        backend.push_response(channel);
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10007));
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10003));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7NFD);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_CHANNEL_NOT_FOUND));
    }

    #[test]
    fn send_preflight_private_thread_member_403_is_private_forbidden() {
        let mut backend = MockRestBackend::default();
        let mut channel = crate::rest::RestResponse::ok();
        channel.body = r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":false,"locked":false}}"#.to_string();
        backend.push_response(channel);
        backend.push_response(crate::rest::RestResponse::discord_error(403, 50001));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7FOR);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_PRIVATE_THREAD_FORBIDDEN));
    }

    #[test]
    fn send_preflight_private_thread_not_joined_is_denied() {
        let mut backend = MockRestBackend::default();
        let mut channel = crate::rest::RestResponse::ok();
        channel.body = r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"current_user_joined":false,"thread_metadata":{"archived":false,"locked":false}}"#.to_string();
        backend.push_response(channel);
        backend.push_response(rest_body(r#"{"id":"100000000000000009"}"#));
        push_private_thread_permissions(&mut backend, "274877981696");
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_THREAD_NOT_JOINED));
    }

    #[test]
    fn delete_preflight_private_thread_member_404_is_forbidden() {
        let mut backend = MockRestBackend::default();
        let mut message = crate::rest::RestResponse::ok();
        message.body = r#"{"id":"100000000000000003","guild_id":"100000000000000001","channel_id":"100000000000000002","type":0,"author":{"id":"100000000000000004"}}"#.to_string();
        backend.push_response(message);
        let mut channel = crate::rest::RestResponse::ok();
        channel.body = r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":false,"locked":false}}"#.to_string();
        backend.push_response(channel);
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10007));
        backend.push_response(rest_body(r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":false,"locked":false}}"#));
        push_private_thread_permissions(&mut backend, "274877981696");
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7FOR);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_PRIVATE_THREAD_FORBIDDEN));
    }

    #[test]
    fn delete_preflight_archived_private_thread_uses_joined_archive_route_first() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(
            r#"{"id":"100000000000000003","guild_id":"100000000000000001","channel_id":"100000000000000002","type":0,"author":{"id":"100000000000000004"}}"#,
        ));
        backend.push_response(rest_body(
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":true,"locked":false}}"#,
        ));
        backend.push_response(rest_body(
            r#"{"threads":[{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":true,"locked":false}}],"members":[{"id":"100000000000000002","user_id":"100000000000000009"}],"has_more":false}"#,
        ));
        backend.push_response(rest_body(
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#,
        ));
        backend.push_response(rest_body(
            r#"[{"id":"100000000000000001","permissions":"8","position":0}]"#,
        ));
        backend.push_response(rest_body(r#"{"id":"100000000000000010","guild_id":"100000000000000001","type":0,"permission_overwrites":[]}"#));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
        assert_eq!(
            mock_rest_paths(&runtime),
            vec![
                "/api/v10/channels/100000000000000002/messages/100000000000000003",
                "/api/v10/channels/100000000000000002",
                "/api/v10/channels/100000000000000010/users/@me/threads/archived/private",
                "/api/v10/guilds/100000000000000001/members/100000000000000009",
                "/api/v10/guilds/100000000000000001/roles",
                "/api/v10/channels/100000000000000010",
            ]
        );
    }

    #[test]
    fn delete_preflight_archived_private_thread_uses_private_archive_with_manage_threads() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(
            r#"{"id":"100000000000000003","guild_id":"100000000000000001","channel_id":"100000000000000002","type":0,"author":{"id":"100000000000000004"}}"#,
        ));
        backend.push_response(rest_body(
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":true,"locked":false}}"#,
        ));
        backend.push_response(rest_body(r#"{"threads":[],"members":[],"has_more":false}"#));
        backend.push_response(rest_body(
            r#"{"id":"100000000000000010","guild_id":"100000000000000001","type":0}"#,
        ));
        backend.push_response(rest_body(
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#,
        ));
        backend.push_response(rest_body(
            r#"[{"id":"100000000000000001","permissions":"17179943936","position":0}]"#,
        ));
        backend.push_response(rest_body(
            r#"{"threads":[{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":true,"locked":false}}],"members":[],"has_more":false}"#,
        ));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
        assert_eq!(
            mock_rest_paths(&runtime),
            vec![
                "/api/v10/channels/100000000000000002/messages/100000000000000003",
                "/api/v10/channels/100000000000000002",
                "/api/v10/channels/100000000000000010/users/@me/threads/archived/private",
                "/api/v10/channels/100000000000000010",
                "/api/v10/guilds/100000000000000001/members/100000000000000009",
                "/api/v10/guilds/100000000000000001/roles",
                "/api/v10/channels/100000000000000010/threads/archived/private",
            ]
        );
    }

    #[test]
    fn delete_preflight_archived_private_thread_skips_private_archive_when_permission_unknown() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(
            r#"{"id":"100000000000000003","guild_id":"100000000000000001","channel_id":"100000000000000002","type":0,"author":{"id":"100000000000000004"}}"#,
        ));
        backend.push_response(rest_body(
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"thread_metadata":{"archived":true,"locked":false}}"#,
        ));
        backend.push_response(rest_body(r#"{"threads":[],"members":[],"has_more":false}"#));
        backend.push_response(crate::rest::RestResponse::discord_error(500, 0));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7STA);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_THREAD_ACCESS_UNKNOWN));
        let err = runtime.handle(request(3, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(err.field("errcode"), Some("31"));
        assert_eq!(err.field("http"), Some("500"));
        assert_eq!(err.field("discord_code"), Some("0"));
        let why = runtime.handle(request(4, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_THREAD_ACCESS_UNKNOWN));
        assert_eq!(
            mock_rest_paths(&runtime),
            vec![
                "/api/v10/channels/100000000000000002/messages/100000000000000003",
                "/api/v10/channels/100000000000000002",
                "/api/v10/channels/100000000000000010/users/@me/threads/archived/private",
                "/api/v10/channels/100000000000000010",
            ]
        );
    }

    #[test]
    fn send_preflight_fetches_channel_and_rejects_unsupported_type() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":2}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_CHANNEL_TYPE_UNSUPPORTED));
    }

    #[test]
    fn send_preflight_reports_forum_parent_as_unsupported_channel() {
        let mut runtime = DforRuntime::new(false, false);
        let now = now_ms_u64();
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new(
                "100000000000000001",
                "100000000000000002",
                15,
                now,
            ));
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_CHANNEL_TYPE_UNSUPPORTED));
    }

    #[test]
    fn send_action_rejects_forum_and_media_parent_before_rest() {
        let mut runtime = DforRuntime::new(false, false);
        let now = now_ms_u64();
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new(
                "100000000000000001",
                "100000000000000002",
                15,
                now,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new(
                "100000000000000001",
                "100000000000000003",
                16,
                now,
            ));
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let forum = runtime.handle(request(
            2,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000002"),
                Field::percent("content", "hello"),
            ],
        ));
        assert_eq!(forum.status, D7FOR);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_CANNOT_SEND_TO_FORUM_PARENT));
        let media = runtime.handle(request(
            4,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000003"),
                Field::percent("content", "hello"),
            ],
        ));
        assert_eq!(media.status, D7FOR);
        let why = runtime.handle(request(5, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_CANNOT_SEND_TO_MEDIA_PARENT));
    }

    #[test]
    fn attachment_action_prechecks_before_reading_file() {
        let mut runtime = DforRuntime::new(false, false);
        let now = now_ms_u64();
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new(
                "100000000000000001",
                "100000000000000002",
                15,
                now,
            ));
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);

        let response = runtime.handle(request(
            2,
            "UPL",
            vec![
                Field::raw("channel_id", "100000000000000002"),
                Field::percent("content", "blocked upload"),
                Field::percent("file_path", "/path/that/does/not/exist"),
                Field::percent("file_name", "blocked.txt"),
            ],
        ));
        assert_eq!(response.status, D7FOR);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_CANNOT_SEND_TO_FORUM_PARENT));
    }

    #[test]
    fn attachment_file_reader_enforces_limit() {
        let temp_dir = std::env::temp_dir().join(format!("dfor77-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        let file_path = temp_dir.join("sample.txt");
        std::fs::write(&file_path, b"abcdef").unwrap();

        assert!(read_attachment_file(file_path.to_str().unwrap(), 3).is_err());
        assert_eq!(
            read_attachment_file(file_path.to_str().unwrap(), 6).unwrap(),
            b"abcdef"
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn multi_attachment_action_reads_files_and_executes_multipart() {
        let temp_dir = std::env::temp_dir().join(format!("dfor77-upm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        let first_path = temp_dir.join("first.txt");
        let second_path = temp_dir.join("second.bin");
        let audit_path = temp_dir.join("audit.jsonl");
        std::fs::write(&first_path, b"first-body").unwrap();
        std::fs::write(&second_path, b"second-body").unwrap();

        let mut backend = MockRestBackend::default();
        push_send_preflight_allow(&mut backend);
        backend.push_response(rest_body(r#"{"id":"100000000000000099"}"#));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime.enable_audit(audit_path.to_str().unwrap()).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);

        let response = runtime.handle(request(
            2,
            "UPM",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::raw("count", "2"),
                Field::percent("content", "multi files"),
                Field::percent("file_path1", first_path.to_str().unwrap()),
                Field::percent("file_name1", "first.txt"),
                Field::percent("file_path2", second_path.to_str().unwrap()),
                Field::percent("file_name2", "second.bin"),
            ],
        ));
        assert_eq!(response.status, D7OK);

        let requests = match &runtime.rest {
            RuntimeRest::Mock(executor) => executor.backend.requests(),
            _ => panic!("expected mock REST runtime"),
        };
        let upload = requests.last().unwrap();
        assert_eq!(
            upload.route_template,
            "POST /channels/{channel_id}/messages"
        );
        assert!(upload
            .content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("multipart/form-data; boundary=dfor77-")));
        assert!(upload.body.as_deref().unwrap().contains(
            r#""attachments":[{"id":"0","filename":"first.txt"},{"id":"1","filename":"second.bin"}]"#
        ));
        let body = upload.body_bytes();
        assert!(body
            .windows(b"name=\"files[0]\"; filename=\"first.txt\"".len())
            .any(|window| window == b"name=\"files[0]\"; filename=\"first.txt\""));
        assert!(body
            .windows(b"name=\"files[1]\"; filename=\"second.bin\"".len())
            .any(|window| window == b"name=\"files[1]\"; filename=\"second.bin\""));
        assert!(body
            .windows(b"first-body".len())
            .any(|window| window == b"first-body"));
        assert!(body
            .windows(b"second-body".len())
            .any(|window| window == b"second-body"));

        let audit = std::fs::read_to_string(&audit_path).unwrap();
        assert!(audit.contains("\"action\":\"D7UPM\""));
        assert!(audit.contains("\"created_message_id\":\"100000000000000099\""));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn sticker_action_executes_with_send_preflight_and_audits() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-runtime-sticker-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow(&mut backend);
        backend.push_response(rest_body(r#"{"id":"100000000000000099"}"#));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime.enable_audit(path.to_str().unwrap()).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);

        let response = runtime.handle(request(
            2,
            "STK",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::raw("count", "2"),
                Field::percent("content", "sticker action"),
                Field::raw("sticker_id1", "100000000000000002"),
                Field::raw("sticker_id2", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);

        let requests = match &runtime.rest {
            RuntimeRest::Mock(executor) => executor.backend.requests(),
            _ => panic!("expected mock REST runtime"),
        };
        let posted = requests.last().unwrap();
        assert_eq!(posted.path, "/api/v10/channels/100000000000000001/messages");
        let body = posted.body.as_deref().unwrap();
        assert!(body.contains("\"content\":\"sticker action\""));
        assert!(body.contains(r#""sticker_ids":["100000000000000002","100000000000000003"]"#));

        let audit = std::fs::read_to_string(&path).unwrap();
        assert!(audit.contains("\"action\":\"D7STK\""));
        assert!(audit.contains("\"created_message_id\":\"100000000000000099\""));
        assert!(!audit.contains("sticker action"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn poll_action_executes_with_send_preflight_and_audits() {
        let path =
            std::env::temp_dir().join(format!("dfor77-runtime-poll-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow(&mut backend);
        backend.push_response(rest_body(r#"{"id":"100000000000000099"}"#));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime.enable_audit(path.to_str().unwrap()).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);

        let response = runtime.handle(request(
            2,
            "POL",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::percent("question", "Deploy now?"),
                Field::raw("count", "3"),
                Field::raw("duration_hours", "24"),
                Field::raw("allow_multiselect", "1"),
                Field::percent("content", "poll action"),
                Field::percent("answer1", "yes"),
                Field::percent("answer2", "no"),
                Field::percent("answer3", "later"),
            ],
        ));
        assert_eq!(response.status, D7OK);

        let requests = match &runtime.rest {
            RuntimeRest::Mock(executor) => executor.backend.requests(),
            _ => panic!("expected mock REST runtime"),
        };
        let posted = requests.last().unwrap();
        assert_eq!(posted.path, "/api/v10/channels/100000000000000001/messages");
        let body = posted.body.as_deref().unwrap();
        assert!(body.contains("\"content\":\"poll action\""));
        assert!(body.contains("\"question\":{\"text\":\"Deploy now?\"}"));
        assert!(body.contains(
            r#""answers":[{"poll_media":{"text":"yes"}},{"poll_media":{"text":"no"}},{"poll_media":{"text":"later"}}]"#
        ));
        assert!(body.contains("\"duration\":24"));
        assert!(body.contains("\"allow_multiselect\":true"));

        let audit = std::fs::read_to_string(&path).unwrap();
        assert!(audit.contains("\"action\":\"D7POL\""));
        assert!(audit.contains("\"created_message_id\":\"100000000000000099\""));
        assert!(!audit.contains("poll action"));
        assert!(!audit.contains("Deploy now"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn raw_rest_body_getter_returns_last_successful_body() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(r#"{"id":"100000000000000001","name":"general"}"#));
        backend.push_response(crate::rest::RestResponse::ok());
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);

        let get = runtime.handle(request(
            2,
            "RST",
            vec![
                Field::raw("method", "1"),
                Field::raw("path", "/api/v10/channels/100000000000000001"),
            ],
        ));
        assert_eq!(get.status, D7OK);

        let full = runtime.handle(request(3, "RBD", vec![Field::raw("buf_len", "60000")]));
        assert_eq!(full.status, D7OK);
        assert_eq!(
            full.field("body"),
            Some(r#"{"id":"100000000000000001","name":"general"}"#)
        );
        assert_eq!(full.field("out_len"), Some("44"));
        assert_eq!(full.field("truncated"), Some("0"));

        let small = runtime.handle(request(4, "RBD", vec![Field::raw("buf_len", "12")]));
        assert_eq!(small.status, D7TRN);
        assert_eq!(small.field("body"), Some(r#"{"id":"10000"#));
        assert_eq!(small.field("out_len"), Some("44"));
        assert_eq!(small.field("truncated"), Some("1"));

        let post = runtime.handle(request(
            5,
            "RST",
            vec![
                Field::raw("method", "2"),
                Field::raw("path", "/api/v10/channels/100000000000000001/typing"),
            ],
        ));
        assert_eq!(post.status, D7OK);
        let stale = runtime.handle(request(6, "RBD", vec![Field::raw("buf_len", "60000")]));
        assert_eq!(stale.status, D7STA);
    }

    #[test]
    fn raw_rest_body_getter_respects_utf8_boundaries() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(r#"{"text":"あいう"}"#));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let get = runtime.handle(request(
            2,
            "RST",
            vec![
                Field::raw("method", "1"),
                Field::raw("path", "/api/v10/test"),
            ],
        ));
        assert_eq!(get.status, D7OK);

        let body = runtime.handle(request(3, "RBD", vec![Field::raw("buf_len", "11")]));
        assert_eq!(body.status, D7TRN);
        assert_eq!(body.field("body"), Some(r#"{"text":""#));
        assert_eq!(body.field("out_len"), Some("20"));
        assert_eq!(body.field("truncated"), Some("1"));
    }

    #[test]
    fn raw_rest_mutation_bodies_are_available_and_cleared_after_failed_or_invalid_actions() {
        let payload = r#"{"id":"20","token":"private-webhook-token","name":"あいう"}"#;
        let mut backend = MockRestBackend::default();
        for _ in 0..4 {
            backend.push_response(rest_body(payload));
        }
        backend.push_response(crate::rest::RestResponse::discord_error(403, 50013));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            assert_eq!(
                runtime
                    .handle(request(
                        2,
                        "RST",
                        vec![
                            Field::raw("method", method),
                            Field::raw("path", "/api/v10/custom/resource"),
                        ]
                    ))
                    .status,
                D7OK
            );
            let body = runtime.handle(request(3, "RBD", vec![Field::raw("buf_len", "60000")]));
            assert_eq!(body.status, D7OK);
            assert_eq!(body.field("body"), Some(payload));
            assert!(!format!("{runtime:?} {body:?}").contains("private-webhook-token"));
            assert_eq!(
                runtime
                    .handle(request(4, "RST", vec![Field::raw("method", "invalid")]))
                    .status,
                D7ARG
            );
            assert_eq!(
                runtime
                    .handle(request(5, "RBD", vec![Field::raw("buf_len", "60000")]))
                    .status,
                D7STA
            );
        }
        assert_eq!(
            runtime
                .handle(request(
                    6,
                    "RST",
                    vec![
                        Field::raw("method", "POST"),
                        Field::raw("path", "/api/v10/custom/resource"),
                    ]
                ))
                .status,
            D7FOR
        );
        assert_eq!(
            runtime
                .handle(request(7, "RBD", vec![Field::raw("buf_len", "60000")]))
                .status,
            D7STA
        );
    }

    #[test]
    fn rest_channel_snapshot_thread_join_state_has_capture_time() {
        let snapshot = channel_snapshot_from_rest_body(
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","parent_id":"100000000000000010","type":12,"current_user_joined":true,"thread_metadata":{"archived":false,"locked":false}}"#,
            "",
            "",
            77,
        )
        .unwrap();
        assert_eq!(
            snapshot.parent_channel_id.as_deref(),
            Some("100000000000000010")
        );
        assert_eq!(snapshot.current_user_thread_joined, Some(true));
        assert_eq!(snapshot.current_user_thread_joined_at_ms, Some(77));
    }

    #[test]
    fn send_preflight_fetches_channel_and_reports_bot_member_incomplete() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7STA);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(
            response.field("why"),
            Some(WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE)
        );
    }

    #[test]
    fn send_preflight_fetches_bot_member_and_roles_to_allow() {
        let mut backend = MockRestBackend::default();
        let mut channel = crate::rest::RestResponse::ok();
        channel.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#.to_string();
        backend.push_response(channel);
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#
                .to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body =
            r#"[{"id":"100000000000000001","permissions":"3072","position":0}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn send_preflight_refetches_incomplete_gateway_channel_snapshot() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#,
        ));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"100000000000000001","roles":[{"id":"100000000000000001","permissions":"3072","position":0}],"members":[{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}],"channels":[{"id":"100000000000000002","type":0,"permission_overwrites":[{"id":"100000000000000001","type":"bad","allow":"0","deny":"0"}]}]}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn send_preflight_allows_guild_owner_without_role_permissions() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"100000000000000001","owner_id":"100000000000000009","roles":[{"id":"100000000000000001","permissions":"0","position":0}],"members":[{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}],"channels":[{"id":"100000000000000002","type":0}]}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn send_preflight_refetches_incomplete_role_snapshot_from_gateway_role_update() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_body(
            r#"[{"id":"100000000000000001","permissions":"3072","position":0}]"#,
        ));
        let now = now_ms_u64();
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now,
            )
            .unwrap();
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new(
                "100000000000000001",
                "100000000000000002",
                0,
                now,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new(
                "100000000000000001",
                "100000000000000009",
                Vec::new(),
                now,
            ));
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"100000000000000001","role":{"id":"100000000000000001","permissions":"0","position":0}}}"#,
                now + 1,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn send_preflight_roles_fetch_404_records_rest_error_details() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10004));
        let now = now_ms_u64();
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now,
            )
            .unwrap();
        runtime
            .gateway
            .snapshot_mut()
            .upsert_channel(ChannelSnapshot::new(
                "100000000000000001",
                "100000000000000002",
                0,
                now,
            ));
        runtime
            .gateway
            .snapshot_mut()
            .upsert_member(MemberSnapshot::new(
                "100000000000000001",
                "100000000000000009",
                Vec::new(),
                now,
            ));
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"GUILD_ROLE_UPDATE","d":{"guild_id":"100000000000000001","role":{"id":"100000000000000001","permissions":"0","position":0}}}"#,
                now + 1,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7NFD);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_GUILD_NOT_FOUND));
        let err = runtime.handle(request(3, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(err.field("errcode"), Some("41"));
        assert_eq!(err.field("http"), Some("404"));
        assert_eq!(err.field("discord_code"), Some("10004"));
        let why = runtime.handle(request(4, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_GUILD_NOT_FOUND));
    }

    #[test]
    fn send_preflight_fetches_current_user_without_ready() {
        let mut backend = MockRestBackend::default();
        let mut channel = crate::rest::RestResponse::ok();
        channel.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#.to_string();
        backend.push_response(channel);
        let mut current_user = crate::rest::RestResponse::ok();
        current_user.body = r#"{"id":"100000000000000009"}"#.to_string();
        backend.push_response(current_user);
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#
                .to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body =
            r#"[{"id":"100000000000000001","permissions":"3072","position":0}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn send_preflight_reports_missing_send_messages_from_roles() {
        let mut backend = MockRestBackend::default();
        let mut channel = crate::rest::RestResponse::ok();
        channel.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#.to_string();
        backend.push_response(channel);
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#
                .to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body =
            r#"[{"id":"100000000000000001","permissions":"1024","position":0}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some("MISSING_SEND_MESSAGES"));
    }

    #[test]
    fn send_preflight_applies_role_channel_permission_overwrite() {
        let mut backend = MockRestBackend::default();
        let mut channel = crate::rest::RestResponse::ok();
        channel.body = r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0,"permission_overwrites":[{"id":"100000000000000001","type":0,"allow":"0","deny":"2048"},{"id":"100000000000000007","type":0,"allow":"2048","deny":"0"}]}"#.to_string();
        backend.push_response(channel);
        let mut member = crate::rest::RestResponse::ok();
        member.body = r#"{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000001","permissions":"3072","position":0},{"id":"100000000000000007","permissions":"0","position":1}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn send_preflight_applies_member_channel_permission_overwrite_last() {
        let mut backend = MockRestBackend::default();
        let mut channel = crate::rest::RestResponse::ok();
        channel.body = r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0,"permission_overwrites":[{"id":"100000000000000001","type":0,"allow":"0","deny":"2048"},{"id":"100000000000000007","type":0,"allow":"2048","deny":"0"},{"id":"100000000000000009","type":1,"allow":"0","deny":"2048"}]}"#.to_string();
        backend.push_response(channel);
        let mut member = crate::rest::RestResponse::ok();
        member.body = r#"{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000001","permissions":"3072","position":0},{"id":"100000000000000007","permissions":"0","position":1}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some("MISSING_SEND_MESSAGES"));
    }

    #[test]
    fn send_preflight_404_tombstones_channel() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10003));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(response.status, D7NFD);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_CHANNEL_NOT_FOUND));
        let cached = runtime.handle(request(
            3,
            "CSN",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
            ],
        ));
        assert_eq!(cached.status, D7NFD);
        assert_eq!(cached.field("why"), Some(WHY_CHANNEL_NOT_FOUND));
    }

    #[test]
    fn current_send_preflight_requires_channel_snapshot() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"send","type":0,"author":{"id":"100000000000000004"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
        let response = runtime.handle(request(3, "CSP", vec![]));
        assert_eq!(response.status, D7STA);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_CHANNEL_SNAPSHOT_INCOMPLETE));
    }

    #[test]
    fn timeout_preflight_uses_known_target_snapshot_without_allowing_unknown_bot() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"target","type":0,"author":{"id":"100000000000000004"},"member":{"permissions":"0"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
        let response = runtime.handle(request(
            3,
            "CTM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7STA);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(
            response.field("why"),
            Some(WHY_BOT_MEMBER_SNAPSHOT_INCOMPLETE)
        );
    }

    #[test]
    fn timeout_preflight_fetches_members_and_roles_to_allow() {
        let mut backend = MockRestBackend::default();
        let mut bot_member = crate::rest::RestResponse::ok();
        bot_member.body = r#"{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(bot_member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000001","permissions":"0","position":0},{"id":"100000000000000007","permissions":"1099511627776","position":10},{"id":"100000000000000008","permissions":"0","position":1}]"#.to_string();
        backend.push_response(roles);
        let mut target_member = crate::rest::RestResponse::ok();
        target_member.body = r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000008"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(target_member);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CTM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn timeout_preflight_bot_member_fetch_403_records_rest_error_details() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::discord_error(403, 50001));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CTM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7FOR);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_MISSING_ACCESS));
        let err = runtime.handle(request(3, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(err.field("errcode"), Some("40"));
        assert_eq!(err.field("http"), Some("403"));
        assert_eq!(err.field("discord_code"), Some("50001"));
        let why = runtime.handle(request(4, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_MISSING_ACCESS));
    }

    #[test]
    fn timeout_preflight_fetches_current_user_without_ready() {
        let mut backend = MockRestBackend::default();
        let mut current_user = crate::rest::RestResponse::ok();
        current_user.body = r#"{"id":"100000000000000009"}"#.to_string();
        backend.push_response(current_user);
        let mut bot_member = crate::rest::RestResponse::ok();
        bot_member.body = r#"{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(bot_member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000001","permissions":"0","position":0},{"id":"100000000000000007","permissions":"1099511627776","position":10},{"id":"100000000000000008","permissions":"0","position":1}]"#.to_string();
        backend.push_response(roles);
        let mut target_member = crate::rest::RestResponse::ok();
        target_member.body = r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000008"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(target_member);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CTM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn timeout_preflight_denies_target_admin_from_roles() {
        let mut backend = MockRestBackend::default();
        let mut bot_member = crate::rest::RestResponse::ok();
        bot_member.body = r#"{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(bot_member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000001","permissions":"0","position":0},{"id":"100000000000000007","permissions":"1099511627776","position":10},{"id":"100000000000000008","permissions":"8","position":1}]"#.to_string();
        backend.push_response(roles);
        let mut target_member = crate::rest::RestResponse::ok();
        target_member.body = r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000008"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(target_member);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CTM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_TARGET_IS_ADMIN));
    }

    #[test]
    fn timeout_preflight_denies_target_owner() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"100000000000000001","owner_id":"100000000000000004","roles":[{"id":"100000000000000001","permissions":"0","position":0},{"id":"100000000000000007","permissions":"1099511627776","position":10}],"members":[{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null},{"user":{"id":"100000000000000004"},"roles":[],"communication_disabled_until":null}]}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CTM",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_TARGET_IS_OWNER));
    }

    #[test]
    fn current_timeout_preflight_denies_known_owner_target_without_rest() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"GUILD_CREATE","d":{"id":"100000000000000001","owner_id":"100000000000000004"}}"#,
                now_ms_u64(),
            )
            .unwrap();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"owner","type":0,"author":{"id":"100000000000000004"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
        let message = runtime.handle(request(12, "NXT", vec![]));
        assert_eq!(message.status, D7OK);
        assert_eq!(message.field("event_type"), Some("101"));
        let response = runtime.handle(request(3, "CTP", vec![]));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_TARGET_IS_OWNER));
    }

    #[test]
    fn timeout_action_prechecks_then_executes_patch() {
        let mut backend = MockRestBackend::default();
        let mut bot_member = crate::rest::RestResponse::ok();
        bot_member.body = r#"{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(bot_member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000001","permissions":"0","position":0},{"id":"100000000000000007","permissions":"1099511627776","position":10},{"id":"100000000000000008","permissions":"0","position":1}]"#.to_string();
        backend.push_response(roles);
        let mut target_member = crate::rest::RestResponse::ok();
        target_member.body = r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000008"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(target_member);
        backend.push_response(crate::rest::RestResponse::ok());
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "TMO",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
                Field::raw("seconds", "60"),
                Field::percent("reason", "ok"),
            ],
        ));
        assert_eq!(response.status, D7OK);
    }

    #[test]
    fn timeout_action_precheck_blocks_admin_target() {
        let mut backend = MockRestBackend::default();
        let mut bot_member = crate::rest::RestResponse::ok();
        bot_member.body = r#"{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(bot_member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000001","permissions":"0","position":0},{"id":"100000000000000007","permissions":"1099511627776","position":10},{"id":"100000000000000008","permissions":"8","position":1}]"#.to_string();
        backend.push_response(roles);
        let mut target_member = crate::rest::RestResponse::ok();
        target_member.body = r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000008"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(target_member);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "TMO",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
                Field::raw("seconds", "60"),
                Field::percent("reason", "no"),
            ],
        ));
        assert_eq!(response.status, D7FOR);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.status, D7OK);
        assert_eq!(why.field("message"), Some(WHY_TARGET_IS_ADMIN));
    }

    #[test]
    fn dry_run_timeout_runs_preflight_and_blocks_admin_target() {
        let mut backend = MockRestBackend::default();
        let mut bot_member = crate::rest::RestResponse::ok();
        bot_member.body = r#"{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(bot_member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000010","permissions":"0","position":0},{"id":"100000000000000007","permissions":"1099511627776","position":10},{"id":"100000000000000008","permissions":"8","position":1}]"#.to_string();
        backend.push_response(roles);
        let mut target_member = crate::rest::RestResponse::ok();
        target_member.body = r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000008"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(target_member);
        let mut runtime = DforRuntime::with_mock_rest_backend(true, false, backend);
        seed_known_guild(&mut runtime, "100000000000000010", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "TMO",
            vec![
                Field::raw("guild_id", "100000000000000010"),
                Field::raw("user_id", "100000000000000004"),
                Field::raw("seconds", "60"),
                Field::percent("reason", "dry"),
            ],
        ));
        assert_eq!(response.status, D7FOR);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_TARGET_IS_ADMIN));
    }

    #[test]
    fn untimeout_action_skips_when_target_is_not_timed_out() {
        let mut backend = MockRestBackend::default();
        let mut bot_member = crate::rest::RestResponse::ok();
        bot_member.body = r#"{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(bot_member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body = r#"[{"id":"100000000000000001","permissions":"0","position":0},{"id":"100000000000000007","permissions":"1099511627776","position":10},{"id":"100000000000000008","permissions":"0","position":1}]"#.to_string();
        backend.push_response(roles);
        let mut target_member = crate::rest::RestResponse::ok();
        target_member.body = r#"{"user":{"id":"100000000000000004"},"roles":["100000000000000008"],"communication_disabled_until":null}"#.to_string();
        backend.push_response(target_member);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "UNT",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("user_id", "100000000000000004"),
                Field::percent("reason", "already clear"),
            ],
        ));
        assert_eq!(response.status, D7SKP);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.status, D7OK);
        assert_eq!(why.field("message"), Some(WHY_OK));
    }

    #[test]
    fn untimeout_action_only_patches_an_active_valid_timeout() {
        let future =
            chrono::DateTime::from_timestamp_millis(i64::try_from(now_ms_u64() + 60_000).unwrap())
                .unwrap()
                .to_rfc3339();
        for (until, expected, request_count) in [
            (future.as_str(), D7OK, 4),
            ("2000-01-01T00:00:00Z", D7SKP, 3),
            ("invalid", D7STA, 3),
        ] {
            let mut backend = MockRestBackend::default();
            let mut bot_member = crate::rest::RestResponse::ok();
            bot_member.body = r#"{"user":{"id":"100000000000000009"},"roles":["100000000000000007"],"communication_disabled_until":null}"#.to_string();
            backend.push_response(bot_member);
            let mut roles = crate::rest::RestResponse::ok();
            roles.body = r#"[{"id":"100000000000000001","permissions":"0","position":0},{"id":"100000000000000007","permissions":"1099511627776","position":10},{"id":"100000000000000008","permissions":"0","position":1}]"#.to_string();
            backend.push_response(roles);
            let mut target_member = crate::rest::RestResponse::ok();
            target_member.body = format!(
                r#"{{"user":{{"id":"100000000000000004"}},"roles":["100000000000000008"],"communication_disabled_until":"{until}"}}"#
            );
            backend.push_response(target_member);
            backend.push_response(crate::rest::RestResponse::ok());
            let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
            seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
            runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
            assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
            let response = runtime.handle(request(
                2,
                "UNT",
                vec![
                    Field::raw("guild_id", "100000000000000001"),
                    Field::raw("user_id", "100000000000000004"),
                    Field::percent("reason", "clear"),
                ],
            ));
            assert_eq!(response.status, expected, "{until}");
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock backend");
            };
            assert_eq!(executor.backend.requests().len(), request_count);
            assert_eq!(
                executor
                    .backend
                    .requests()
                    .iter()
                    .filter(|r| r.method == HttpMethod::Patch)
                    .count(),
                usize::from(expected == D7OK)
            );
        }
    }

    #[test]
    fn current_timeout_preflight_denies_known_admin_target() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"admin","type":0,"author":{"id":"100000000000000004"},"member":{"permissions":"8"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
        let response = runtime.handle(request(3, "CTP", vec![]));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_TARGET_IS_ADMIN));
    }

    #[test]
    fn delete_action_rejects_known_undeletable_snapshot_before_rest() {
        let mut runtime =
            DforRuntime::with_mock_rest_backend(false, false, MockRestBackend::default());
        let now = now_ms_u64();
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"starter","type":21,"author":{"id":"100000000000000004"}}}"#,
                now,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "DEL",
            vec![
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7FOR);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.status, D7OK);
        assert_eq!(why.field("message"), Some(WHY_MESSAGE_TYPE_NOT_DELETABLE));
    }

    #[test]
    fn delete_preflight_fetches_missing_message_snapshot() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"id":"100000000000000003","channel_id":"100000000000000002","type":21,"content":"starter","author":{"id":"100000000000000004"}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_MESSAGE_TYPE_NOT_DELETABLE));
    }

    #[test]
    fn delete_preflight_message_fetch_5xx_records_read_only_snapshot_failure() {
        let mut backend = MockRestBackend::default();
        backend.push_response(rest_server_error());
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7STA);
        assert_ne!(response.status, D7UNC);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_MESSAGE_SNAPSHOT_INCOMPLETE));
        let err = runtime.handle(request(3, "ERR", vec![Field::raw("scope", "1")]));
        assert_eq!(err.field("errcode"), Some("31"));
        assert_eq!(err.field("http"), Some("500"));
        assert_eq!(err.field("discord_code"), Some("0"));
        let why = runtime.handle(request(4, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_MESSAGE_SNAPSHOT_INCOMPLETE));
    }

    #[test]
    fn delete_preflight_fetches_bot_permissions_to_allow_other_message() {
        let mut backend = MockRestBackend::default();
        let mut message = crate::rest::RestResponse::ok();
        message.body = r#"{"id":"100000000000000003","guild_id":"100000000000000001","channel_id":"100000000000000002","type":0,"content":"normal","author":{"id":"100000000000000004"}}"#.to_string();
        backend.push_response(message);
        let mut channel = crate::rest::RestResponse::ok();
        channel.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#.to_string();
        backend.push_response(channel);
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#
                .to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body =
            r#"[{"id":"100000000000000001","permissions":"74752","position":0}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn delete_preflight_fetches_current_user_without_ready() {
        let mut backend = MockRestBackend::default();
        let mut message = crate::rest::RestResponse::ok();
        message.body = r#"{"id":"100000000000000003","guild_id":"100000000000000001","channel_id":"100000000000000002","type":0,"content":"normal","author":{"id":"100000000000000004"}}"#.to_string();
        backend.push_response(message);
        let mut channel = crate::rest::RestResponse::ok();
        channel.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#.to_string();
        backend.push_response(channel);
        let mut current_user = crate::rest::RestResponse::ok();
        current_user.body = r#"{"id":"100000000000000009"}"#.to_string();
        backend.push_response(current_user);
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#
                .to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body =
            r#"[{"id":"100000000000000001","permissions":"74752","position":0}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn delete_preflight_reports_missing_manage_messages_for_other_message() {
        let mut backend = MockRestBackend::default();
        let mut message = crate::rest::RestResponse::ok();
        message.body = r#"{"id":"100000000000000003","guild_id":"100000000000000001","channel_id":"100000000000000002","type":0,"content":"normal","author":{"id":"100000000000000004"}}"#.to_string();
        backend.push_response(message);
        let mut channel = crate::rest::RestResponse::ok();
        channel.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#.to_string();
        backend.push_response(channel);
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#
                .to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body =
            r#"[{"id":"100000000000000001","permissions":"66560","position":0}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some("MISSING_MANAGE_MESSAGES"));
    }

    #[test]
    fn delete_preflight_allows_own_message_without_manage_messages() {
        let mut backend = MockRestBackend::default();
        let mut message = crate::rest::RestResponse::ok();
        message.body = r#"{"id":"100000000000000003","guild_id":"100000000000000001","channel_id":"100000000000000002","type":0,"content":"normal","author":{"id":"100000000000000009"}}"#.to_string();
        backend.push_response(message);
        let mut channel = crate::rest::RestResponse::ok();
        channel.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#.to_string();
        backend.push_response(channel);
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#
                .to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body =
            r#"[{"id":"100000000000000001","permissions":"66560","position":0}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn delete_preflight_allows_complete_snapshot_without_read_message_history() {
        let mut runtime = DforRuntime::new(false, false);
        seed_delete_snapshots(
            &mut runtime,
            0,
            "100000000000000009",
            &[BIT_VIEW_CHANNEL],
            now_ms_u64(),
        );
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn delete_preflight_requires_read_message_history_when_fetching_snapshot() {
        let mut backend = MockRestBackend::default();
        let mut message = crate::rest::RestResponse::ok();
        message.body = r#"{"id":"100000000000000003","guild_id":"100000000000000001","channel_id":"100000000000000002","type":0,"content":"normal","author":{"id":"100000000000000009"}}"#.to_string();
        backend.push_response(message);
        let mut channel = crate::rest::RestResponse::ok();
        channel.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#.to_string();
        backend.push_response(channel);
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#
                .to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body =
            r#"[{"id":"100000000000000001","permissions":"1024","position":0}]"#.to_string();
        backend.push_response(roles);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(
            response.field("why"),
            Some("MISSING_READ_MESSAGE_HISTORY_FOR_SNAPSHOT")
        );
    }

    #[test]
    fn delete_preflight_requires_manage_messages_for_own_auto_moderation_action() {
        let mut runtime = DforRuntime::new(false, false);
        seed_delete_snapshots(
            &mut runtime,
            24,
            "100000000000000009",
            &[BIT_VIEW_CHANNEL, BIT_READ_MESSAGE_HISTORY],
            now_ms_u64(),
        );
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some("MISSING_MANAGE_MESSAGES"));

        let mut runtime = DforRuntime::new(false, false);
        seed_delete_snapshots(
            &mut runtime,
            24,
            "100000000000000009",
            &[
                BIT_VIEW_CHANNEL,
                BIT_READ_MESSAGE_HISTORY,
                BIT_MANAGE_MESSAGES,
            ],
            now_ms_u64(),
        );
        assert_eq!(runtime.handle(request(3, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            4,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(response.field("can"), Some("1"));
        assert_eq!(response.field("why"), Some(WHY_OK));
    }

    #[test]
    fn delete_preflight_fetch_404_creates_tombstone() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::discord_error(404, 10008));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7NFD);
        assert_eq!(response.field("can"), Some("0"));
        assert_eq!(response.field("why"), Some(WHY_MESSAGE_NOT_FOUND));
        let second = runtime.handle(request(
            3,
            "CDL",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(second.status, D7NFD);
        assert_eq!(second.field("why"), Some(WHY_MESSAGE_NOT_FOUND));
    }

    #[test]
    fn delete_action_fetches_snapshot_before_delete() {
        let mut backend = MockRestBackend::default();
        let mut fetch = crate::rest::RestResponse::ok();
        fetch.body = r#"{"id":"100000000000000003","guild_id":"100000000000000001","channel_id":"100000000000000002","type":0,"content":"normal","author":{"id":"100000000000000004"}}"#.to_string();
        backend.push_response(fetch);
        let mut channel = crate::rest::RestResponse::ok();
        channel.body =
            r#"{"id":"100000000000000002","guild_id":"100000000000000001","type":0}"#.to_string();
        backend.push_response(channel);
        let mut member = crate::rest::RestResponse::ok();
        member.body =
            r#"{"user":{"id":"100000000000000009"},"roles":[],"communication_disabled_until":null}"#
                .to_string();
        backend.push_response(member);
        let mut roles = crate::rest::RestResponse::ok();
        roles.body =
            r#"[{"id":"100000000000000001","permissions":"74752","position":0}]"#.to_string();
        backend.push_response(roles);
        backend.push_response(crate::rest::RestResponse::ok());
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_known_guild(&mut runtime, "100000000000000001", now_ms_u64());
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume","user":{"id":"100000000000000009"}}}"#,
                now_ms_u64(),
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "DEL",
            vec![
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
    }

    #[test]
    fn delete_action_fetch_blocks_undeletable_message() {
        let mut backend = MockRestBackend::default();
        let mut fetch = crate::rest::RestResponse::ok();
        fetch.body = r#"{"id":"100000000000000003","channel_id":"100000000000000002","type":21,"content":"starter","author":{"id":"100000000000000004"}}"#.to_string();
        backend.push_response(fetch);
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "DEL",
            vec![
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7FOR);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.field("message"), Some(WHY_MESSAGE_TYPE_NOT_DELETABLE));
    }

    #[test]
    fn delete_action_rejects_own_auto_moderation_action_before_rest() {
        let mut runtime =
            DforRuntime::with_mock_rest_backend(false, false, MockRestBackend::default());
        seed_delete_snapshots(
            &mut runtime,
            24,
            "100000000000000009",
            &[BIT_VIEW_CHANNEL, BIT_READ_MESSAGE_HISTORY],
            now_ms_u64(),
        );
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "DEL",
            vec![
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7FOR);
        let why = runtime.handle(request(3, "WHY", vec![Field::raw("scope", "1")]));
        assert_eq!(why.status, D7OK);
        assert_eq!(why.field("message"), Some("MISSING_MANAGE_MESSAGES"));
        assert!(mock_rest_paths(&runtime).is_empty());
    }

    #[test]
    fn delete_action_allows_complete_snapshot_without_read_message_history() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::ok());
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        seed_delete_snapshots(
            &mut runtime,
            0,
            "100000000000000009",
            &[BIT_VIEW_CHANNEL],
            now_ms_u64(),
        );
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "DEL",
            vec![
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(
            mock_rest_paths(&runtime),
            vec!["/api/v10/channels/100000000000000002/messages/100000000000000003"]
        );
    }

    #[test]
    fn action_audit_records_runtime_status_and_event_context() {
        let path =
            std::env::temp_dir().join(format!("dfor77-runtime-audit-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow_for_channel(&mut backend, "100000000000000002");
        let mut runtime = DforRuntime::with_mock_rest_backend(true, false, backend);
        runtime.enable_audit(path.to_str().unwrap()).unwrap();
        runtime.set_bot_process_pid(12345);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"secret text","author":{"id":"100000000000000004"}}}"#,
                100,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
        let response = runtime.handle(request(
            3,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000002"),
                Field::percent("content", "audit should not include this"),
            ],
        ));
        assert_eq!(response.status, D7DRY);

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert!(content.contains("\"ipc_request_id\":3"));
        assert!(content.contains("\"bot_process_pid\":12345"));
        assert!(content.contains("\"action\":\"D7SND\""));
        assert!(content.contains("\"status\":20"));
        assert!(content.contains("\"dry_run\":true"));
        assert!(content.contains("\"event_type\":101"));
        assert!(content.contains("\"guild_id\":\"100000000000000001\""));
        assert!(content.contains("\"nonce\":"));
        assert!(!content.contains("audit should not include this"));
        assert!(!content.contains("secret text"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn runtime_json_logs_escape_display_controls_without_changing_metadata_values() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-runtime-unicode-log-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let original = "metadata\n\u{0085}\u{2028}\u{202e}日本語\u{202c}\u{2066}👩‍💻\u{2069}";
        let mut runtime = DforRuntime::new(false, false);
        runtime.enable_runtime_log(path.to_str().unwrap()).unwrap();
        runtime
            .write_runtime_event(
                "runtime",
                &[
                    RuntimeLogField::string("kind", "gateway_transition"),
                    RuntimeLogField::string("phase", original),
                ],
            )
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        for ch in [
            '\u{0085}', '\u{2028}', '\u{202e}', '\u{202c}', '\u{2066}', '\u{2069}',
        ] {
            assert!(!text.contains(ch));
        }
        let parsed = parse_json(text.trim_end()).unwrap();
        assert_eq!(
            parsed.get("phase").and_then(JsonValue::as_str),
            Some(original)
        );
        assert_eq!(validate_runtime_log_text(&text).unwrap().records, 1);
        drop(runtime);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn runtime_log_records_rest_action_without_content() {
        let path =
            std::env::temp_dir().join(format!("dfor77-runtime-log-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow_for_channel(&mut backend, "100000000000000002");
        let mut runtime = DforRuntime::with_mock_rest_backend(true, false, backend);
        runtime.enable_runtime_log(path.to_str().unwrap()).unwrap();
        runtime.set_bot_process_pid(12345);
        runtime
            .ingest_gateway_json(
                r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"type":0,"guild_id":"100000000000000001","channel_id":"100000000000000002","id":"100000000000000003","content":"secret text","author":{"id":"100000000000000004"}}}"#,
                100,
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "NXT", vec![])).status, D7OK);
        let response = runtime.handle(request(
            3,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000002"),
                Field::percent("content", "runtime log should not include this"),
            ],
        ));
        assert_eq!(response.status, D7DRY);
        assert_eq!(runtime.handle(request(4, "END", vec![])).status, D7OK);

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert!(content.contains("\"schema\":\"dfor77.runtime.v1\""));
        assert!(content.contains("\"event\":\"runtime\""));
        assert!(content.contains("\"kind\":\"rest_action\""));
        assert!(content.contains("\"action\":\"D7SND\""));
        assert!(content.contains("\"status\":20"));
        assert!(content.contains("\"commit_state\":\"not_sent\""));
        assert!(content.contains("\"bot_process_pid\":12345"));
        assert!(content.contains("\"gateway_event_type\":101"));
        assert!(content.contains("\"guild_id\":\"100000000000000001\""));
        assert!(content.contains("\"route\":\"POST /channels/{channel_id}/messages\""));
        assert!(!content.contains("runtime log should not include this"));
        assert!(!content.contains("secret text"));
        let validation = validate_runtime_log_text(&content).unwrap();
        assert_eq!(validation.records, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn runtime_log_can_skip_successful_rest_actions() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-runtime-log-skip-success-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow(&mut backend);
        backend.push_response(crate::rest::RestResponse::ok());
        let settings = RuntimeSettings {
            runtime_log_rest_success: false,
            ..RuntimeSettings::default()
        };
        let mut runtime = DforRuntime::with_settings(false, false, settings);
        runtime.use_mock_rest_backend(backend);
        runtime.enable_runtime_log(path.to_str().unwrap()).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::percent("content", "normal success"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(runtime.handle(request(3, "END", vec![])).status, D7OK);
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.trim().is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn runtime_log_validation_rejects_sensitive_text() {
        let text = concat!(
            "{\"schema\":\"dfor77.runtime.v1\",\"time_unix_ms\":1,",
            "\"event\":\"runtime\",\"kind\":\"process_lifecycle\",",
            "\"phase\":\"start\",\"detail\":\"Authorization: Bot abc.def.ghi\"}\n"
        );
        let error = validate_runtime_log_text(text).unwrap_err();
        assert!(error.to_string().contains("contains sensitive text"));
    }

    #[test]
    fn action_audit_hash_policy_records_hmac_without_content() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-runtime-audit-hash-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow_for_channel(&mut backend, "100000000000000002");
        let mut runtime = DforRuntime::with_mock_rest_backend(true, false, backend);
        runtime
            .enable_audit_with_policy(
                path.to_str().unwrap(),
                AuditContentPolicy::Hash {
                    key: b"key".to_vec(),
                },
            )
            .unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000002"),
                Field::percent("content", "The quick brown fox jumps over the lazy dog"),
            ],
        ));
        assert_eq!(response.status, D7DRY);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"content_mode\":\"hash\""));
        assert!(content.contains(
            "\"content_hash\":\"f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8\""
        ));
        assert!(!content.contains("The quick brown"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn action_audit_records_argument_failure_as_not_sent() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-runtime-audit-arg-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut runtime = DforRuntime::new(false, false);
        runtime.enable_audit(path.to_str().unwrap()).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "DEL",
            vec![
                Field::raw("channel_id", "bad"),
                Field::raw("message_id", "100000000000000003"),
            ],
        ));
        assert_eq!(response.status, D7ARG);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"action\":\"D7DEL\""));
        assert!(content.contains("\"status\":30"));
        assert!(content.contains("\"commit_state\":\"not_sent\""));
        assert!(content.contains("\"why\":\"INVALID_FORM_BODY\""));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mock_rest_executor_commits_runtime_action() {
        let path =
            std::env::temp_dir().join(format!("dfor77-runtime-rest-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow(&mut backend);
        backend.push_response(crate::rest::RestResponse::ok());
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime.enable_audit(path.to_str().unwrap()).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::percent("content", "committed message"),
            ],
        ));
        assert_eq!(response.status, D7OK);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"action\":\"D7SND\""));
        assert!(content.contains("\"status\":0"));
        assert!(content.contains("\"committed\":true"));
        assert!(content.contains("\"commit_state\":\"committed\""));
        assert!(!content.contains("committed message"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn interaction_callback_rejects_encoded_path_changes_without_sending_http() {
        let mut runtime =
            DforRuntime::with_mock_rest_backend(false, false, MockRestBackend::default());
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        for (index, token) in ["%2f", "%252F", "%2E%2E", ".", ".."]
            .into_iter()
            .enumerate()
        {
            let response = runtime.handle(request(
                index as u64 + 2,
                "ICB",
                vec![
                    Field::raw("interaction_id", "20"),
                    Field::percent("interaction_token", token),
                    Field::percent("payload", r#"{"type":5}"#),
                ],
            ));
            assert_eq!(response.status, D7ARG);
        }
        assert!(mock_rest_paths(&runtime).is_empty());
    }

    #[test]
    fn reaction_action_executes_and_audits_without_payload_leak() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-runtime-reaction-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::ok());
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        runtime.enable_audit(path.to_str().unwrap()).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let response = runtime.handle(request(
            2,
            "RAC",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::raw("message_id", "100000000000000002"),
                Field::percent("emoji", "🔥"),
                Field::raw("user_id", ""),
                Field::raw("mode", "1"),
            ],
        ));
        assert_eq!(response.status, D7OK);
        assert_eq!(
            mock_rest_paths(&runtime),
            vec![
                "/api/v10/channels/100000000000000001/messages/100000000000000002/reactions/%F0%9F%94%A5/@me"
            ]
        );
        let audit = std::fs::read_to_string(&path).unwrap();
        assert!(audit.contains("\"action\":\"D7RAC\""));
        assert!(audit.contains("\"status\":0"));
        assert!(audit.contains("\"commit_state\":\"committed\""));
        assert!(!audit.contains("🔥"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn log_only_executes_send_but_skips_moderation_actions() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-runtime-log-only-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow(&mut backend);
        backend.push_response(crate::rest::RestResponse::ok());
        backend.push_response(rest_body(
            r#"{"id":"100000000000000002","guild_id":"100000000000000010","channel_id":"100000000000000001","type":0,"content":"normal","author":{"id":"100000000000000004"}}"#,
        ));
        let mut runtime = DforRuntime::with_mock_rest_backend(false, true, backend);
        runtime.enable_audit(path.to_str().unwrap()).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let send = runtime.handle(request(
            2,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::percent("content", "log line"),
            ],
        ));
        assert_eq!(send.status, D7OK);
        let delete = runtime.handle(request(
            3,
            "DEL",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::raw("message_id", "100000000000000002"),
            ],
        ));
        assert_eq!(delete.status, D7SKP);

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);
        let send_line = content
            .lines()
            .find(|line| line.contains("\"action\":\"D7SND\""))
            .unwrap();
        assert!(send_line.contains("\"status\":0"));
        assert!(send_line.contains("\"log_only\":true"));
        assert!(send_line.contains("\"commit_state\":\"committed\""));
        let delete_line = content
            .lines()
            .find(|line| line.contains("\"action\":\"D7DEL\""))
            .unwrap();
        assert!(delete_line.contains("\"status\":21"));
        assert!(delete_line.contains("\"log_only\":true"));
        assert!(delete_line.contains("\"commit_state\":\"not_sent\""));
        assert!(!content.contains("log line"));
        assert!(!content.contains("normal"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn runtime_settings_control_rest_global_wait_budget() {
        let settings = RuntimeSettings {
            rest_global_max_wait_ms: 0,
            ..RuntimeSettings::default()
        };
        let mut runtime = DforRuntime::with_settings(false, false, settings.clone());
        runtime.rest = RuntimeRest::Mock(rest_executor(MockRestBackend::default(), &settings));
        seed_send_snapshots(&mut runtime, now_ms_u64());
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        for id in 2..52 {
            let response = runtime.handle(request(
                id,
                "SND",
                vec![
                    Field::raw("channel_id", "100000000000000001"),
                    Field::percent("content", "hello"),
                ],
            ));
            assert_eq!(response.status, D7OK);
        }
        let mut rate_limited = false;
        for id in 52..200 {
            let response = runtime.handle(request(
                id,
                "SND",
                vec![
                    Field::raw("channel_id", "100000000000000001"),
                    Field::percent("content", "hello"),
                ],
            ));
            if response.status == D7RTL {
                rate_limited = true;
                break;
            }
        }
        assert!(rate_limited);
    }

    #[test]
    fn runtime_prepares_gateway_identify_through_mock_rest() {
        let settings = RuntimeSettings {
            gateway_shard_id: 5,
            gateway_shard_count: 8,
            ..RuntimeSettings::default()
        };
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":4}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::with_settings(false, false, settings);
        runtime.use_mock_rest_backend(backend);
        let startup = runtime.prepare_gateway_identify("secret-token", 0);
        assert_eq!(startup.status, D7OK);
        assert_eq!(startup.why, WHY_OK);
        assert_eq!(startup.action, GatewayAction::SendIdentify);
        assert_eq!(
            startup.gateway_url.as_deref(),
            Some("wss://gateway.discord.gg")
        );
        assert_eq!(startup.identify_bucket, Some(1));
        assert_eq!(startup.identify_remaining, Some(2));
        assert_eq!(startup.identify_reset_after_ms, Some(60_000));
        assert_eq!(startup.daily_identify_count, 0);
        assert_eq!(startup.daily_identify_remaining, 999);
        assert!(startup
            .identify_payload
            .as_deref()
            .is_some_and(|payload| payload.contains("\"token\":\"secret-token\"")));
    }

    #[test]
    fn runtime_gateway_prepare_backs_off_without_consuming_identify_budget() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":0,"reset_after":5000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let startup = runtime.prepare_gateway_identify("secret-token", 0);
        assert_eq!(startup.status, D7OK);
        assert_eq!(startup.action, GatewayAction::Backoff { delay_ms: 5000 });
        assert_eq!(startup.identify_payload, None);
        assert_eq!(startup.daily_identify_count, 0);
        assert_eq!(startup.identify_remaining, Some(0));
    }

    #[test]
    fn runtime_identify_limit_backoff_refetches_gateway_bot_even_when_resumable() {
        let mut backend = MockRestBackend::default();
        let mut first = crate::rest::RestResponse::ok();
        first.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(first);
        let mut second = crate::rest::RestResponse::ok();
        second.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":0,"reset_after":5000,"max_concurrency":1}}"#.to_string();
        backend.push_response(second);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let connected = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(connected.status, D7OK);
        assert_eq!(runtime.gateway.machine().state(), GatewayState::Ready);
        let startup = runtime.prepare_gateway_identify("secret-token", 10_000);
        assert_eq!(startup.status, D7OK);
        assert_eq!(startup.action, GatewayAction::Backoff { delay_ms: 5_000 });
        assert_eq!(
            runtime.gateway_backoff_elapsed(),
            GatewayAction::FetchGatewayBot
        );
    }

    #[test]
    fn runtime_gateway_prepare_requires_executable_rest_backend() {
        let mut runtime = DforRuntime::new(false, false);
        let startup = runtime.prepare_gateway_identify("secret-token", 0);
        assert_eq!(startup.status, D7STA);
        assert_eq!(startup.why, "GATEWAY_REST_BACKEND_MISSING");
        assert_eq!(
            startup.action,
            GatewayAction::Fatal("GATEWAY_REST_BACKEND_MISSING")
        );
        assert!(startup.identify_payload.is_none());
    }

    #[test]
    fn runtime_gateway_hello_updates_outbound_heartbeat_reserve() {
        let mut runtime = DforRuntime::new(false, false);
        assert_eq!(runtime.gateway_outbound.heartbeat_reserve(), 2);
        runtime
            .ingest_gateway_json(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#, 0)
            .unwrap();
        assert_eq!(runtime.gateway_outbound.heartbeat_reserve(), 61);
    }

    #[test]
    fn runtime_gateway_control_payload_too_large_is_internal_fatal() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        let token = "x".repeat(crate::gateway::MAX_GATEWAY_PAYLOAD_BYTES + 1);
        let result = runtime.connect_gateway_once(&mut transport, &token, 0);
        assert_eq!(result.status, D7INT);
        assert_eq!(result.why, "GATEWAY_PAYLOAD_TOO_LARGE");
        assert_eq!(
            result.last_action,
            GatewayAction::Fatal("GATEWAY_PAYLOAD_TOO_LARGE")
        );
        assert!(result.connected);
        assert!(!result.identify_sent);
        assert!(transport.sent_texts().is_empty());
    }

    #[test]
    fn runtime_identify_outbound_budget_exhaustion_backs_off_without_identify() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1}}"#);
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        assert_eq!(result.why, WHY_OK);
        assert!(result.connected);
        assert!(!result.identify_sent);
        assert_eq!(result.gateway_outbound_remaining, Some(120));
        assert_eq!(
            result.last_action,
            GatewayAction::Backoff { delay_ms: 5_000 }
        );
        assert_eq!(result.startup.identify_remaining, Some(2));
        assert_eq!(result.startup.daily_identify_count, 0);
        assert_eq!(result.startup.daily_identify_remaining, 999);
        assert!(transport.sent_texts().is_empty());
        assert_eq!(
            runtime.gateway_manager.machine().state(),
            crate::gateway::GatewayState::Backoff
        );
        assert_eq!(
            runtime.gateway_backoff_elapsed(),
            GatewayAction::FetchGatewayBot
        );
    }

    #[test]
    fn gateway_send_denial_maps_control_payload_overflow_to_internal() {
        for kind in [
            GatewaySendKind::Heartbeat,
            GatewaySendKind::Resume,
            GatewaySendKind::Identify,
        ] {
            assert_eq!(
                gateway_send_denied_status(kind, "GATEWAY_PAYLOAD_TOO_LARGE"),
                D7INT
            );
            assert_eq!(
                gateway_send_denied_action(kind, "GATEWAY_PAYLOAD_TOO_LARGE"),
                GatewayAction::Fatal("GATEWAY_PAYLOAD_TOO_LARGE")
            );
        }
        assert_eq!(
            gateway_send_denied_status(GatewaySendKind::Other, "GATEWAY_PAYLOAD_TOO_LARGE"),
            D7RTL
        );
        assert_eq!(
            gateway_send_denied_status(
                GatewaySendKind::Heartbeat,
                "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED"
            ),
            D7RTL
        );
    }

    #[test]
    fn runtime_connects_gateway_sends_identify_and_ingests_ready() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        for _ in 0..120 {
            assert!(
                runtime
                    .gateway_outbound
                    .check_and_record(GatewaySendKind::Heartbeat, 10, 0)
                    .allowed
            );
        }
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        assert!(result.connected);
        assert!(result.identify_sent);
        assert_eq!(result.received_frames, 2);
        assert_eq!(result.gateway_outbound_remaining, Some(119));
        assert_eq!(result.startup.identify_remaining, Some(1));
        assert_eq!(result.startup.daily_identify_count, 1);
        assert_eq!(result.startup.daily_identify_remaining, 998);
        assert_eq!(transport.connected_to(), Some("wss://gateway.discord.gg"));
        assert_eq!(transport.sent_texts().len(), 1);
        assert!(transport.sent_texts()[0].contains("\"op\":2"));
        assert!(transport.sent_texts()[0].contains("\"token\":\"secret-token\""));
        transport.push_incoming(r#"{"op":11,"d":null}"#);
        let pump = runtime.pump_gateway_once(&mut transport, 1000);
        assert_eq!(pump.status, D7OK);
        assert!(pump.heartbeat_sent);
        assert!(pump.received_frame);
        assert_eq!(pump.action, GatewayAction::None);
        assert_eq!(transport.sent_texts().len(), 2);
        assert_eq!(transport.sent_texts()[1], "{\"op\":1,\"d\":1}");
        assert_eq!(
            runtime.gateway.heartbeat().unwrap().last_ack_at_ms(),
            Some(1000)
        );
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let next = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(next.status, D7OK);
        assert_eq!(next.field("event_type"), Some("100"));
    }

    #[test]
    fn runtime_connect_gateway_allows_ready_after_identify_timeout() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_error(crate::gateway::GatewayTransportError::NoFrame);
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        assert_eq!(result.why, WHY_OK);
        assert!(result.connected);
        assert!(result.identify_sent);
        assert_eq!(result.received_frames, 1);
        assert_eq!(result.gateway_outbound_remaining, Some(119));
        assert_eq!(transport.sent_texts().len(), 1);

        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let pump = runtime.pump_gateway_once(&mut transport, 250);
        assert_eq!(pump.status, D7OK);
        assert!(pump.received_frame);
        assert_eq!(pump.action, GatewayAction::None);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let next = runtime.handle(request(2, "NXT", vec![]));
        assert_eq!(next.status, D7OK);
        assert_eq!(next.field("event_type"), Some("100"));
    }

    #[test]
    fn runtime_gateway_user_payload_queue_sends_presence_and_voice_state() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-gateway-queue-audit-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.enable_audit(path.to_str().unwrap()).unwrap();
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);

        let presence = runtime.handle(request(
            2,
            "PRS",
            vec![Field::percent(
                "payload",
                r#"{"since":null,"activities":[],"status":"online","afk":false}"#,
            )],
        ));
        assert_eq!(presence.status, D7OK);
        let voice = runtime.handle(request(
            3,
            "VST",
            vec![
                Field::raw("guild_id", "100000000000000001"),
                Field::raw("channel_id", "100000000000000002"),
                Field::raw("self_mute", "1"),
                Field::raw("self_deaf", "0"),
            ],
        ));
        assert_eq!(voice.status, D7OK);

        transport.push_error(crate::gateway::GatewayTransportError::NoFrame);
        let first = runtime.pump_gateway_once(&mut transport, 100);
        assert_eq!(first.status, D7OK);
        transport.push_error(crate::gateway::GatewayTransportError::NoFrame);
        let second = runtime.pump_gateway_once(&mut transport, 101);
        assert_eq!(second.status, D7OK);
        assert_eq!(transport.sent_texts().len(), 3);
        assert!(transport.sent_texts()[1].contains(r#""op":3"#));
        assert!(transport.sent_texts()[1].contains(r#""status":"online""#));
        assert!(transport.sent_texts()[2].contains(r#""op":4"#));
        assert!(transport.sent_texts()[2].contains(r#""self_mute":true"#));
        let audit = std::fs::read_to_string(&path).unwrap();
        assert_eq!(audit.lines().count(), 2);
        assert!(audit.contains("\"action\":\"D7PRS\""));
        assert!(audit.contains("\"action\":\"D7VST\""));
        assert!(audit.contains("\"commit_state_source\":\"gateway_queue\""));
        assert!(audit.contains("\"gateway_payload_bytes\":"));
        assert!(audit.contains("\"gateway_outbound_remaining\":63"));
        assert!(audit.contains("\"gateway_outbound_remaining\":62"));
        assert!(!audit.contains("online"));
        assert!(!audit.contains("secret-token"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn runtime_reports_daily_identify_warning_after_threshold() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-daily-identify-audit-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let settings = RuntimeSettings {
            gateway_daily_identify_warn: 1,
            gateway_daily_identify_stop: 3,
            ..RuntimeSettings::default()
        };
        let mut runtime = DforRuntime::with_settings(false, false, settings);
        runtime.enable_audit(path.to_str().unwrap()).unwrap();
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );

        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);

        assert_eq!(result.status, D7OK);
        assert!(result.identify_sent);
        assert_eq!(result.startup.daily_identify_count, 1);
        assert_eq!(result.startup.daily_identify_remaining, 2);
        assert!(result.startup.daily_identify_warning);
        let audit = std::fs::read_to_string(&path).unwrap();
        assert_eq!(audit.lines().count(), 1);
        assert!(audit.contains("\"action\":\"GATEWAY_IDENTIFY_WARNING\""));
        assert!(audit.contains("\"why\":\"DAILY_IDENTIFY_WARNING\""));
        assert!(audit.contains("\"daily_identify_count\":1"));
        assert!(audit.contains("\"daily_identify_remaining\":2"));
        assert!(audit.contains("\"identify_remaining\":1"));
        assert!(audit.contains("\"gateway_outbound_remaining\":119"));
        assert!(audit.contains("\"gateway_payload_bytes\":"));
        assert!(!audit.contains("secret-token"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn runtime_gateway_heartbeat_request_sends_immediate_heartbeat() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":60000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        let next_due = runtime.gateway.heartbeat().unwrap().next_due_ms().unwrap();
        assert!(next_due > 0);
        transport.push_incoming(r#"{"op":1,"d":null}"#);
        let pump = runtime.pump_gateway_once(&mut transport, next_due - 1);
        assert_eq!(pump.status, D7OK);
        assert!(pump.received_frame);
        assert!(pump.heartbeat_sent);
        assert_eq!(pump.action, GatewayAction::None);
        assert_eq!(transport.sent_texts().len(), 2);
        assert_eq!(transport.sent_texts()[1], "{\"op\":1,\"d\":1}");
    }

    #[test]
    fn runtime_gateway_pump_treats_no_frame_as_idle() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        transport.push_error(crate::gateway::GatewayTransportError::NoFrame);
        let pump = runtime.pump_gateway_once(&mut transport, 500);
        assert_eq!(pump.status, D7OK);
        assert_eq!(pump.why, WHY_OK);
        assert!(!pump.heartbeat_sent);
        assert!(!pump.received_frame);
    }

    #[test]
    fn fresh_identify_reopens_stopped_sessions_and_discards_the_old_resume_cursor() {
        for prior in 0..3 {
            let mut backend = MockRestBackend::default();
            let mut response = crate::rest::RestResponse::ok();
            response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
            backend.push_response(response);
            let mut runtime = DforRuntime::new(false, false);
            runtime.use_mock_rest_backend(backend);
            runtime.ingest_gateway_json(r#"{"op":0,"s":10,"t":"READY","d":{"session_id":"old","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
            if prior == 1 {
                runtime.shutdown_gateway();
            } else if prior == 2 {
                runtime.gateway.machine_mut().close(Some(4004), false);
            }
            let mut transport = crate::gateway::MockGatewayTransport::default();
            transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
            transport.push_error(crate::gateway::GatewayTransportError::NoFrame);
            let connected = runtime.connect_gateway_once(&mut transport, "test-token", 0);
            assert_eq!(connected.status, D7OK);
            assert!(connected.identify_sent);
            assert_eq!(runtime.gateway.machine().state(), GatewayState::Identifying);
            assert!(runtime.gateway_resume_session().is_none());
            transport.push_incoming(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"new","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#);
            let result = runtime.pump_gateway_once(&mut transport, 0);
            assert_eq!(result.status, D7OK);
            assert!(result.received_frame);
            assert_eq!(runtime.gateway.machine().state(), GatewayState::Ready);
            assert_eq!(runtime.gateway.machine().last_seen_seq(), Some(1));
            assert!(!runtime.gateway_resume_session_invalidated());
        }
    }

    #[test]
    fn runtime_shutdown_gateway_marks_gateway_closed() {
        let mut runtime = DforRuntime::new(false, false);
        runtime.shutdown_gateway();
        assert_eq!(
            runtime.gateway.machine().state(),
            crate::gateway::GatewayState::Closed
        );
        let err = runtime.handle(request(1, "ERR", vec![Field::raw("scope", "3")]));
        assert_eq!(err.status, D7STA);
    }

    #[test]
    fn closed_and_fatal_gateways_do_not_send_heartbeats_or_consume_frames() {
        for fatal in [false, true] {
            let mut runtime = DforRuntime::new(false, false);
            runtime
                .ingest_gateway_json(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#, 0)
                .unwrap();
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"saved","resume_gateway_url":"wss://resume"}}"#, 0).unwrap();
            if fatal {
                runtime.gateway.machine_mut().close(Some(4004), false);
            } else {
                runtime.shutdown_gateway();
            }
            let mut transport = crate::gateway::MockGatewayTransport::default();
            transport.push_incoming(r#"{"op":0,"s":2,"t":"RESUMED","d":{}}"#);
            let result = runtime.pump_gateway_once(&mut transport, 1000);
            assert_eq!(result.status, D7OK);
            assert!(!result.heartbeat_sent);
            assert!(!result.received_frame);
            assert!(transport.sent_texts().is_empty());
            assert!(transport.receive_text().unwrap().is_some());
            assert!(runtime.gateway_resume_session().is_none());
        }
    }

    #[test]
    fn runtime_end_marks_gateway_closed() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":1,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        assert_eq!(
            runtime.gateway.machine().state(),
            crate::gateway::GatewayState::Ready
        );
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        assert_eq!(runtime.handle(request(2, "END", vec![])).status, D7OK);
        assert_eq!(
            runtime.gateway.machine().state(),
            crate::gateway::GatewayState::Closed
        );
    }

    #[test]
    fn runtime_end_flushes_audit_log() {
        let path = std::env::temp_dir().join(format!(
            "dfor77-runtime-end-flush-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend = MockRestBackend::default();
        push_send_preflight_allow(&mut backend);
        let mut runtime = DforRuntime::with_mock_rest_backend(true, false, backend);
        runtime.enable_audit(path.to_str().unwrap()).unwrap();
        assert_eq!(runtime.handle(request(1, "INI", vec![])).status, D7OK);
        let send = runtime.handle(request(
            2,
            "SND",
            vec![
                Field::raw("channel_id", "100000000000000001"),
                Field::percent("content", "flush me"),
            ],
        ));
        assert_eq!(send.status, D7DRY);
        assert_eq!(runtime.handle(request(3, "END", vec![])).status, D7OK);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"action\":\"D7SND\""));
        assert!(content.contains("\"status\":20"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn runtime_gateway_missed_heartbeat_ack_requests_resume() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        transport.push_error(crate::gateway::GatewayTransportError::NoFrame);
        let first = runtime.pump_gateway_once(&mut transport, 1000);
        assert_eq!(first.status, D7OK);
        assert!(first.heartbeat_sent);
        assert_eq!(first.action, GatewayAction::None);
        let missed = runtime.pump_gateway_once(&mut transport, 2001);
        assert_eq!(missed.status, D7OK);
        assert!(!missed.heartbeat_sent);
        assert_eq!(missed.action, GatewayAction::SendResume { seq: 1 });
    }

    #[test]
    fn runtime_gateway_heartbeat_budget_exhaustion_requests_resume_without_grl() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        assert_eq!(runtime.events.pop_front().unwrap().event_type, D7RDY);
        for _ in 0..119 {
            assert!(
                runtime
                    .gateway_outbound
                    .check_and_record(GatewaySendKind::Heartbeat, 10, 0)
                    .allowed
            );
        }
        let exhausted = runtime.pump_gateway_once(&mut transport, 1000);
        assert_eq!(exhausted.status, D7OK);
        assert_eq!(exhausted.why, WHY_OK);
        assert!(!exhausted.heartbeat_sent);
        assert_eq!(exhausted.gateway_outbound_remaining, Some(0));
        assert_eq!(exhausted.action, GatewayAction::SendResume { seq: 1 });
        assert_eq!(transport.sent_texts().len(), 1);
        assert!(runtime.events.is_empty());
    }

    #[test]
    fn runtime_gateway_resume_reconnects_and_sends_resume_payload() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        transport.push_incoming(r#"{"op":7,"d":null}"#);
        let pump = runtime.pump_gateway_once(&mut transport, 100);
        assert_eq!(pump.status, D7OK);
        assert_eq!(pump.action, GatewayAction::SendResume { seq: 1 });
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1}}"#);
        let resume = runtime.resume_gateway_once(&mut transport, "secret-token", 1, 200);
        assert_eq!(resume.status, D7OK);
        assert!(resume.received_frame);
        assert_eq!(resume.gateway_outbound_remaining, Some(119));
        assert_eq!(transport.connected_to(), Some("wss://resume"));
        assert_eq!(transport.resume_disconnects(), 1);
        assert!(transport.graceful_close_codes().is_empty());
        let sent = transport.sent_texts().last().unwrap();
        assert!(sent.contains("\"op\":6"));
        assert!(sent.contains("\"session_id\":\"abc\""));
        assert!(sent.contains("\"seq\":1"));
    }

    #[test]
    fn runtime_gateway_resume_send_failure_gaps_and_backs_off_to_new_identify() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        assert_eq!(runtime.events.pop_front().unwrap().event_type, D7RDY);
        let mut resume_transport = crate::gateway::MockGatewayTransport::default();
        resume_transport.set_send_error(crate::gateway::GatewayTransportError::SendFailed);
        let resume = runtime.resume_gateway_once(&mut resume_transport, "secret-token", 1, 200);
        assert_eq!(resume.status, D7OK);
        assert_eq!(resume.why, WHY_OK);
        assert_eq!(resume.gateway_outbound_remaining, Some(119));
        assert_eq!(resume.action, GatewayAction::Backoff { delay_ms: 1_000 });
        assert!(resume_transport.sent_texts().is_empty());
        let gap = runtime.events.pop_front().unwrap();
        assert_eq!(gap.event_type, D7GAP);
        assert_eq!(gap.content, "GATEWAY_SEND_FAILED");
        assert_eq!(
            runtime.gateway_backoff_elapsed(),
            GatewayAction::FetchGatewayBot
        );
    }

    #[test]
    fn runtime_gateway_resume_connect_failure_gaps_and_backs_off_to_new_identify() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        assert_eq!(runtime.events.pop_front().unwrap().event_type, D7RDY);
        let mut resume_transport = crate::gateway::MockGatewayTransport::default();
        resume_transport.set_connect_error(crate::gateway::GatewayTransportError::ConnectFailed);
        let resume = runtime.resume_gateway_once(&mut resume_transport, "secret-token", 1, 200);
        assert_eq!(resume.status, D7OK);
        assert_eq!(resume.why, WHY_OK);
        assert_eq!(resume.gateway_outbound_remaining, None);
        assert_eq!(resume.action, GatewayAction::Backoff { delay_ms: 1_000 });
        let gap = runtime.events.pop_front().unwrap();
        assert_eq!(gap.event_type, D7GAP);
        assert_eq!(gap.content, "GATEWAY_CONNECT_FAILED");
        assert_eq!(
            runtime.gateway_backoff_elapsed(),
            GatewayAction::FetchGatewayBot
        );
    }

    #[test]
    fn runtime_gateway_resume_budget_short_wait_retries_resume_without_gap() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .gateway
            .machine_mut()
            .ready(1, "abc", "wss://resume");
        let _ = runtime.gateway.pop_event();
        let mut result = gateway_pump_result();
        runtime.gateway_resume_outbound_budget_exhausted(&mut result, Some(50));
        assert_eq!(result.status, D7OK);
        assert_eq!(result.why, WHY_OK);
        assert_eq!(result.action, GatewayAction::Backoff { delay_ms: 50 });
        assert_eq!(
            runtime.gateway.machine().backoff_info(),
            Some(crate::gateway::GatewayBackoffInfo {
                reason: "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED",
                next: crate::gateway::GatewayBackoffNext::Resume,
                delay_ms: 50
            })
        );
        assert!(runtime.events.is_empty());
        assert_eq!(
            runtime.gateway_backoff_elapsed(),
            GatewayAction::SendResume { seq: 1 }
        );
    }

    #[test]
    fn runtime_gateway_resume_budget_long_wait_gaps_to_new_identify() {
        let mut runtime = DforRuntime::new(false, false);
        runtime
            .gateway
            .machine_mut()
            .ready(1, "abc", "wss://resume");
        let _ = runtime.gateway.pop_event();
        let mut result = gateway_pump_result();
        runtime.gateway_resume_outbound_budget_exhausted(&mut result, Some(1_001));
        assert_eq!(result.status, D7OK);
        assert_eq!(result.why, WHY_OK);
        assert_eq!(result.action, GatewayAction::Backoff { delay_ms: 1_000 });
        assert_eq!(
            runtime.gateway.machine().backoff_info(),
            Some(crate::gateway::GatewayBackoffInfo {
                reason: "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED",
                next: crate::gateway::GatewayBackoffNext::NewIdentify,
                delay_ms: 1_000
            })
        );
        let gap = runtime.events.pop_front().unwrap();
        assert_eq!(gap.event_type, D7GAP);
        assert_eq!(gap.content, "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED");
        assert_eq!(
            runtime.gateway_backoff_elapsed(),
            GatewayAction::FetchGatewayBot
        );
    }

    #[test]
    fn runtime_gateway_close_code_drives_backoff_resume_action() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":2,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.push_incoming(
            r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume"}}"#,
        );
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7OK);
        transport.push_close_code(4008);
        let pump = runtime.pump_gateway_once(&mut transport, 100);
        assert_eq!(pump.status, D7OK);
        assert_eq!(pump.action, GatewayAction::Backoff { delay_ms: 1_000 });
        assert_eq!(
            runtime.gateway_backoff_elapsed(),
            GatewayAction::SendResume { seq: 1 }
        );
    }

    #[test]
    fn runtime_connect_gateway_reports_transport_connect_failure() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":1,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.set_connect_error(crate::gateway::GatewayTransportError::ConnectFailed);
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7NET);
        assert_eq!(result.why, "GATEWAY_CONNECT_FAILED");
        assert!(!result.connected);
        assert!(!result.identify_sent);
        assert_eq!(result.startup.identify_remaining, Some(1));
        assert_eq!(result.startup.daily_identify_count, 0);
        assert_eq!(result.startup.daily_identify_remaining, 999);
        assert!(transport.sent_texts().is_empty());
    }

    #[test]
    fn runtime_connect_gateway_send_failure_does_not_consume_identify_budget() {
        let mut backend = MockRestBackend::default();
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":1,"reset_after":60000,"max_concurrency":1}}"#.to_string();
        backend.push_response(response);
        let mut runtime = DforRuntime::new(false, false);
        runtime.use_mock_rest_backend(backend);
        let mut transport = crate::gateway::MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#);
        transport.set_send_error(crate::gateway::GatewayTransportError::SendFailed);
        let result = runtime.connect_gateway_once(&mut transport, "secret-token", 0);
        assert_eq!(result.status, D7NET);
        assert_eq!(result.why, "GATEWAY_SEND_FAILED");
        assert!(result.connected);
        assert!(!result.identify_sent);
        assert_eq!(result.startup.identify_remaining, Some(1));
        assert_eq!(result.startup.daily_identify_count, 0);
        assert_eq!(result.startup.daily_identify_remaining, 999);
        assert!(transport.sent_texts().is_empty());
    }
}
