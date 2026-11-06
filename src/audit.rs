use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod text;

use text::contains_definite_sensitive_text;
pub use text::{
    diagnostic_field, diagnostic_text, json_escape, json_log_escape, redact_sensitive_text,
    write_diagnostic,
};

use crate::ipc::percent_decode;
use crate::json::{parse_json, JsonValue};
use crate::rest::{CommitState, RestAction, RestOutcome, RestRequest};

#[derive(Clone, Default, Eq, PartialEq)]
pub enum AuditContentPolicy {
    #[default]
    None,
    Hash {
        key: Vec<u8>,
    },
    Truncated {
        preview_bytes: usize,
    },
}

impl std::fmt::Debug for AuditContentPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Hash { .. } => formatter.write_str("Hash { key: [REDACTED] }"),
            Self::Truncated { preview_bytes } => formatter
                .debug_struct("Truncated")
                .field("preview_bytes", preview_bytes)
                .finish(),
        }
    }
}

impl AuditContentPolicy {
    pub fn none() -> Self {
        Self::None
    }

    pub fn hash_from_env(name: &str) -> Result<Self, AuditContentError> {
        let value = std::env::var(name).map_err(|_| {
            AuditContentError::new(format!("environment variable {} is not set", name))
        })?;
        if value.is_empty() {
            return Err(AuditContentError::new(format!(
                "environment variable {} is empty",
                name
            )));
        }
        Ok(Self::Hash {
            key: value.into_bytes(),
        })
    }

    fn mode_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Hash { .. } => "hash",
            Self::Truncated { .. } => "truncated",
        }
    }

    fn render(&self, content: Option<&str>) -> AuditContentRender {
        match self {
            Self::None => AuditContentRender {
                mode: self.mode_name().to_string(),
                hash: None,
                preview: None,
            },
            Self::Hash { key } => AuditContentRender {
                mode: self.mode_name().to_string(),
                hash: content.map(|content| hmac_sha256_hex(key, content.as_bytes())),
                preview: None,
            },
            Self::Truncated { preview_bytes } => AuditContentRender {
                mode: self.mode_name().to_string(),
                hash: None,
                preview: content.map(|content| sanitized_content_preview(content, *preview_bytes)),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditContentError {
    message: String,
}

impl AuditContentError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AuditContentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AuditContentError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditLogValidation {
    pub records: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuditLogValidationOptions {
    pub required_actions: Vec<String>,
    pub forbidden_actions: Vec<String>,
    pub require_live_actions: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditLogValidationError {
    message: String,
}

impl AuditLogValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AuditLogValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AuditLogValidationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuditContentRender {
    mode: String,
    hash: Option<String>,
    preview: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditRecord {
    pub timestamp: String,
    pub bot_process_pid: Option<u32>,
    pub ipc_request_id: Option<u64>,
    pub request_id: Option<String>,
    pub event_seq: Option<i64>,
    pub event_type: Option<i32>,
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub user_id: Option<String>,
    pub target_guild_id: Option<String>,
    pub target_channel_id: Option<String>,
    pub target_message_id: Option<String>,
    pub target_user_id: Option<String>,
    pub action: String,
    pub content_mode: String,
    pub content_hash: Option<String>,
    pub content_preview: Option<String>,
    pub route: Option<String>,
    pub route_major_parameter: Option<String>,
    pub bucket: Option<String>,
    pub status: i32,
    pub http_status: i32,
    pub discord_error_code: i32,
    pub dry_run: bool,
    pub log_only: bool,
    pub committed: Option<bool>,
    pub commit_state: String,
    pub commit_state_source: Option<String>,
    pub request_started_at: Option<String>,
    pub request_finished_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub why: String,
    pub reason: Option<String>,
    pub fatal_reason: Option<String>,
    pub gateway_session_id: Option<String>,
    pub gateway_sequence: Option<i64>,
    pub runtime_event_seq: Option<i64>,
    pub resume_attempt: Option<u32>,
    pub snapshot_age_ms: Option<u64>,
    pub nonce: Option<String>,
    pub created_message_id: Option<String>,
    pub computed_until: Option<String>,
    pub identify_bucket: Option<String>,
    pub identify_remaining: Option<i64>,
    pub identify_reset_after_ms: Option<u64>,
    pub daily_identify_count: Option<u64>,
    pub daily_identify_remaining: Option<i64>,
    pub gateway_outbound_remaining: Option<i64>,
    pub gateway_payload_bytes: Option<usize>,
}

impl AuditRecord {
    fn base(action: &str, status: i32, why: &str) -> Self {
        Self {
            timestamp: current_timestamp_millis(),
            bot_process_pid: None,
            ipc_request_id: None,
            request_id: None,
            event_seq: None,
            event_type: None,
            guild_id: None,
            channel_id: None,
            message_id: None,
            user_id: None,
            target_guild_id: None,
            target_channel_id: None,
            target_message_id: None,
            target_user_id: None,
            action: action.to_string(),
            content_mode: AuditContentPolicy::None.mode_name().to_string(),
            content_hash: None,
            content_preview: None,
            route: None,
            route_major_parameter: None,
            bucket: None,
            status,
            http_status: 0,
            discord_error_code: 0,
            dry_run: false,
            log_only: false,
            committed: None,
            commit_state: CommitState::NotSent.as_str().to_string(),
            commit_state_source: None,
            request_started_at: None,
            request_finished_at: None,
            duration_ms: None,
            why: why.to_string(),
            reason: None,
            fatal_reason: None,
            gateway_session_id: None,
            gateway_sequence: None,
            runtime_event_seq: None,
            resume_attempt: None,
            snapshot_age_ms: None,
            nonce: None,
            created_message_id: None,
            computed_until: None,
            identify_bucket: None,
            identify_remaining: None,
            identify_reset_after_ms: None,
            daily_identify_count: None,
            daily_identify_remaining: None,
            gateway_outbound_remaining: None,
            gateway_payload_bytes: None,
        }
    }

    pub fn action(action: &str, outcome: &RestOutcome, dry_run: bool, log_only: bool) -> Self {
        Self::action_with_content_policy(
            action,
            outcome,
            dry_run,
            log_only,
            &AuditContentPolicy::None,
        )
    }

    pub fn action_with_content_policy(
        action: &str,
        outcome: &RestOutcome,
        dry_run: bool,
        log_only: bool,
        content_policy: &AuditContentPolicy,
    ) -> Self {
        let request = outcome.request.as_ref();
        let [target_guild_id, target_channel_id, target_message_id, target_user_id] =
            audit_target_ids(request);
        let content = send_message_content(outcome);
        let content = content_policy.render(content.as_deref());
        Self {
            timestamp: current_timestamp_millis(),
            bot_process_pid: None,
            ipc_request_id: None,
            request_id: None,
            event_seq: None,
            event_type: None,
            guild_id: None,
            channel_id: None,
            message_id: None,
            user_id: None,
            target_guild_id,
            target_channel_id,
            target_message_id,
            target_user_id,
            action: action.to_string(),
            content_mode: content.mode,
            content_hash: content.hash,
            content_preview: content.preview,
            route: request.map(|request| request.route_template.to_string()),
            route_major_parameter: request.map(|request| request.major_parameter.clone()),
            bucket: None,
            status: outcome.status,
            http_status: outcome.http_status,
            discord_error_code: outcome.discord_error_code,
            dry_run,
            log_only,
            committed: committed_from_state(outcome.commit_state),
            commit_state: outcome.commit_state.as_str().to_string(),
            commit_state_source: Some("runtime".to_string()),
            request_started_at: None,
            request_finished_at: None,
            duration_ms: None,
            why: outcome.why.clone(),
            reason: request.and_then(audit_reason_from_request),
            fatal_reason: None,
            gateway_session_id: None,
            gateway_sequence: None,
            runtime_event_seq: None,
            resume_attempt: None,
            snapshot_age_ms: None,
            nonce: request.and_then(|request| request.nonce.clone()),
            created_message_id: outcome.created_message_id.clone(),
            computed_until: computed_until_from_body(
                request.and_then(|request| request.body.as_deref()),
            ),
            identify_bucket: None,
            identify_remaining: None,
            identify_reset_after_ms: None,
            daily_identify_count: None,
            daily_identify_remaining: None,
            gateway_outbound_remaining: None,
            gateway_payload_bytes: None,
        }
    }

    pub fn gateway_queue(
        action: &str,
        status: i32,
        why: &str,
        dry_run: bool,
        log_only: bool,
        payload_bytes: Option<usize>,
        remaining: Option<usize>,
    ) -> Self {
        let mut record = Self::base(action, status, why);
        record.dry_run = dry_run;
        record.log_only = log_only;
        record.committed = Some(status == 0);
        record.commit_state = if status == 0 {
            CommitState::Committed.as_str().to_string()
        } else {
            CommitState::NotSent.as_str().to_string()
        };
        record.commit_state_source = Some("gateway_queue".to_string());
        record.gateway_payload_bytes = payload_bytes;
        record.gateway_outbound_remaining = remaining.map(|value| value as i64);
        record
    }

    pub fn gateway_identify_warning(
        daily_count: u32,
        daily_remaining: u32,
        identify_bucket: Option<u32>,
        identify_remaining: Option<u32>,
        identify_reset_after_ms: Option<u64>,
        outbound_remaining: Option<usize>,
        payload_bytes: usize,
    ) -> Self {
        let mut record = Self::base("GATEWAY_IDENTIFY_WARNING", 0, "DAILY_IDENTIFY_WARNING");
        record.committed = Some(true);
        record.commit_state = CommitState::Committed.as_str().to_string();
        record.commit_state_source = Some("gateway_identify".to_string());
        record.identify_bucket = identify_bucket.map(|value| value.to_string());
        record.identify_remaining = identify_remaining.map(|value| value as i64);
        record.identify_reset_after_ms = identify_reset_after_ms;
        record.daily_identify_count = Some(daily_count as u64);
        record.daily_identify_remaining = Some(daily_remaining as i64);
        record.gateway_outbound_remaining = outbound_remaining.map(|value| value as i64);
        record.gateway_payload_bytes = Some(payload_bytes);
        record
    }

    pub fn to_json_line(&self) -> String {
        let mut out = String::new();
        out.push('{');
        field_string(&mut out, "timestamp", Some(&self.timestamp), true);
        field_u32(&mut out, "bot_process_pid", self.bot_process_pid);
        field_u64(&mut out, "ipc_request_id", self.ipc_request_id);
        field_string(&mut out, "request_id", self.request_id.as_deref(), false);
        field_i64(&mut out, "event_seq", self.event_seq);
        field_i32(&mut out, "event_type", self.event_type);
        field_string(&mut out, "guild_id", self.guild_id.as_deref(), false);
        field_string(&mut out, "channel_id", self.channel_id.as_deref(), false);
        field_string(&mut out, "message_id", self.message_id.as_deref(), false);
        field_string(&mut out, "user_id", self.user_id.as_deref(), false);
        field_string(
            &mut out,
            "target_guild_id",
            self.target_guild_id.as_deref(),
            false,
        );
        field_string(
            &mut out,
            "target_channel_id",
            self.target_channel_id.as_deref(),
            false,
        );
        field_string(
            &mut out,
            "target_message_id",
            self.target_message_id.as_deref(),
            false,
        );
        field_string(
            &mut out,
            "target_user_id",
            self.target_user_id.as_deref(),
            false,
        );
        field_string(&mut out, "action", Some(&self.action), false);
        field_string(&mut out, "content_mode", Some(&self.content_mode), false);
        field_string(
            &mut out,
            "content_hash",
            self.content_hash.as_deref(),
            false,
        );
        field_string(
            &mut out,
            "content_preview",
            self.content_preview.as_deref(),
            false,
        );
        field_string(&mut out, "route", self.route.as_deref(), false);
        field_string(
            &mut out,
            "route_major_parameter",
            self.route_major_parameter.as_deref(),
            false,
        );
        field_string(&mut out, "bucket", self.bucket.as_deref(), false);
        field_i32(&mut out, "status", Some(self.status));
        field_i32(&mut out, "http_status", Some(self.http_status));
        field_i32(
            &mut out,
            "discord_error_code",
            Some(self.discord_error_code),
        );
        field_bool(&mut out, "dry_run", Some(self.dry_run));
        field_bool(&mut out, "log_only", Some(self.log_only));
        field_bool(&mut out, "committed", self.committed);
        field_string(&mut out, "commit_state", Some(&self.commit_state), false);
        field_string(
            &mut out,
            "commit_state_source",
            self.commit_state_source.as_deref(),
            false,
        );
        field_string(
            &mut out,
            "request_started_at",
            self.request_started_at.as_deref(),
            false,
        );
        field_string(
            &mut out,
            "request_finished_at",
            self.request_finished_at.as_deref(),
            false,
        );
        field_u64(&mut out, "duration_ms", self.duration_ms);
        field_string(&mut out, "why", Some(&self.why), false);
        field_string(&mut out, "reason", self.reason.as_deref(), false);
        field_string(
            &mut out,
            "fatal_reason",
            self.fatal_reason.as_deref(),
            false,
        );
        field_string(
            &mut out,
            "gateway_session_id",
            self.gateway_session_id.as_deref(),
            false,
        );
        field_i64(&mut out, "gateway_sequence", self.gateway_sequence);
        field_i64(&mut out, "runtime_event_seq", self.runtime_event_seq);
        field_u32(&mut out, "resume_attempt", self.resume_attempt);
        field_u64(&mut out, "snapshot_age_ms", self.snapshot_age_ms);
        field_string(&mut out, "nonce", self.nonce.as_deref(), false);
        field_string(
            &mut out,
            "created_message_id",
            self.created_message_id.as_deref(),
            false,
        );
        field_string(
            &mut out,
            "computed_until",
            self.computed_until.as_deref(),
            false,
        );
        field_string(
            &mut out,
            "identify_bucket",
            self.identify_bucket.as_deref(),
            false,
        );
        field_i64(&mut out, "identify_remaining", self.identify_remaining);
        field_u64(
            &mut out,
            "identify_reset_after_ms",
            self.identify_reset_after_ms,
        );
        field_u64(&mut out, "daily_identify_count", self.daily_identify_count);
        field_i64(
            &mut out,
            "daily_identify_remaining",
            self.daily_identify_remaining,
        );
        field_i64(
            &mut out,
            "gateway_outbound_remaining",
            self.gateway_outbound_remaining,
        );
        field_usize(
            &mut out,
            "gateway_payload_bytes",
            self.gateway_payload_bytes,
        );
        out.push_str("}\n");
        out
    }
}

fn audit_target_ids(request: Option<&RestRequest>) -> [Option<String>; 4] {
    let Some(path) = request.and_then(|request| crate::rest::path::RestPath::parse(&request.path))
    else {
        return [None, None, None, None];
    };
    let parts = path.resource_parts();
    let mut targets = [None, None, None, None];
    let id = crate::rest::path::canonical_id;
    match parts.as_slice() {
        ["channels", channel, rest @ ..] => {
            targets[1] = id(channel);
            if let ["messages" | "polls", message, ..] = rest {
                targets[2] = id(message);
            }
        }
        ["guilds", guild, rest @ ..] => {
            targets[0] = id(guild);
            if let ["members" | "bans", user, ..] = rest {
                targets[3] = id(user);
            }
        }
        ["users", user, ..] => targets[3] = id(user),
        ["webhooks", _, _, "messages", message, ..] => targets[2] = id(message),
        _ => {}
    }
    targets
}

const AUDIT_REQUIRED_KEYS: &[&str] = &[
    "timestamp",
    "bot_process_pid",
    "ipc_request_id",
    "request_id",
    "event_seq",
    "event_type",
    "guild_id",
    "channel_id",
    "message_id",
    "user_id",
    "action",
    "content_mode",
    "content_hash",
    "content_preview",
    "route",
    "route_major_parameter",
    "bucket",
    "status",
    "http_status",
    "discord_error_code",
    "dry_run",
    "log_only",
    "committed",
    "commit_state",
    "commit_state_source",
    "request_started_at",
    "request_finished_at",
    "duration_ms",
    "why",
    "reason",
    "fatal_reason",
    "gateway_session_id",
    "gateway_sequence",
    "runtime_event_seq",
    "resume_attempt",
    "snapshot_age_ms",
    "nonce",
    "created_message_id",
    "computed_until",
    "identify_bucket",
    "identify_remaining",
    "identify_reset_after_ms",
    "daily_identify_count",
    "daily_identify_remaining",
    "gateway_outbound_remaining",
    "gateway_payload_bytes",
];

pub fn validate_audit_log_text(text: &str) -> Result<AuditLogValidation, AuditLogValidationError> {
    validate_audit_log_text_with_options(text, &AuditLogValidationOptions::default())
}

pub fn validate_audit_log_text_with_options(
    text: &str,
    options: &AuditLogValidationOptions,
) -> Result<AuditLogValidation, AuditLogValidationError> {
    let mut records = 0;
    let mut actions = Vec::new();
    let mut action_records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            return Err(AuditLogValidationError::new(format!(
                "line {line_number}: empty audit record"
            )));
        }
        let value = parse_json(line).map_err(|error| {
            AuditLogValidationError::new(format!("line {line_number}: invalid JSON: {error}"))
        })?;
        validate_audit_record(line_number, &value)?;
        if let Some(action) = value.get("action").and_then(JsonValue::as_str) {
            actions.push(action.to_string());
            action_records.push(ValidatedActionRecord {
                action: action.to_string(),
                dry_run: value.get("dry_run").and_then(JsonValue::as_bool),
                log_only: value.get("log_only").and_then(JsonValue::as_bool),
                committed: value.get("committed").and_then(JsonValue::as_bool),
                commit_state: value
                    .get("commit_state")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
                status: value.get("status").and_then(JsonValue::as_i64),
            });
        }
        records += 1;
    }
    if records == 0 {
        return Err(AuditLogValidationError::new("audit log has no records"));
    }
    for required in &options.required_actions {
        if !actions.iter().any(|action| action == required) {
            return Err(AuditLogValidationError::new(format!(
                "required action {required} not found"
            )));
        }
    }
    for forbidden in &options.forbidden_actions {
        if actions.iter().any(|action| action == forbidden) {
            return Err(AuditLogValidationError::new(format!(
                "forbidden action {forbidden} found"
            )));
        }
    }
    if options.require_live_actions {
        for required in &options.required_actions {
            if !action_records.iter().any(|record| {
                record.action == *required
                    && record.dry_run == Some(false)
                    && record.log_only == Some(false)
                    && record.committed == Some(true)
                    && record.commit_state.as_deref() == Some("committed")
                    && record.status == Some(0)
            }) {
                return Err(AuditLogValidationError::new(format!(
                    "required action {required} has no live committed record"
                )));
            }
        }
    }
    Ok(AuditLogValidation { records })
}

struct ValidatedActionRecord {
    action: String,
    dry_run: Option<bool>,
    log_only: Option<bool>,
    committed: Option<bool>,
    commit_state: Option<String>,
    status: Option<i64>,
}

fn validate_audit_record(
    line_number: usize,
    value: &JsonValue,
) -> Result<(), AuditLogValidationError> {
    let JsonValue::Object(fields) = value else {
        return Err(AuditLogValidationError::new(format!(
            "line {line_number}: audit record is not an object"
        )));
    };
    for key in AUDIT_REQUIRED_KEYS {
        if !fields.contains_key(*key) {
            return Err(AuditLogValidationError::new(format!(
                "line {line_number}: missing key {key}"
            )));
        }
    }
    for key in [
        "target_guild_id",
        "target_channel_id",
        "target_message_id",
        "target_user_id",
    ] {
        match fields.get(key) {
            None | Some(JsonValue::Null) => {}
            Some(JsonValue::String(value)) if crate::validation::validate_snowflake(value) => {}
            _ => {
                return Err(AuditLogValidationError::new(format!(
                    "line {line_number}: field {key} is not a Discord id"
                )))
            }
        }
    }
    if !matches!(fields.get("action"), Some(JsonValue::String(_))) {
        return Err(AuditLogValidationError::new(format!(
            "line {line_number}: action is not a string"
        )));
    }
    for (key, value) in fields {
        if let JsonValue::String(text) = value {
            if contains_definite_sensitive_text(text) {
                return Err(AuditLogValidationError::new(format!(
                    "line {line_number}: field {key} contains sensitive text"
                )));
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct AuditLogger {
    file: RollingLog,
    content_policy: AuditContentPolicy,
}

impl AuditLogger {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_policy(path, AuditContentPolicy::None)
    }

    pub fn open_with_policy(
        path: impl AsRef<Path>,
        content_policy: AuditContentPolicy,
    ) -> io::Result<Self> {
        let file = RollingLog::open(path.as_ref(), 64 * 1024 * 1024)?;
        Ok(Self {
            file,
            content_policy,
        })
    }

    pub fn content_policy(&self) -> &AuditContentPolicy {
        &self.content_policy
    }

    pub fn append(&mut self, record: &AuditRecord) -> io::Result<()> {
        self.file.append(record.to_json_line().as_bytes())?;
        self.file.flush()
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[derive(Debug)]
pub(crate) struct RollingLog {
    file: File,
    path: PathBuf,
    limit: u64,
}

impl RollingLog {
    pub(crate) fn open(path: &Path, limit: u64) -> io::Result<Self> {
        let file = Self::open_file(path)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            limit,
        })
    }

    fn open_file(path: &Path) -> io::Result<File> {
        let file = crate::private_file::open_append(path, true)?;
        crate::private_file::restrict_permissions(&file)?;
        Ok(file)
    }

    fn archive(&self, index: usize) -> PathBuf {
        let mut name = self.path.as_os_str().to_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() as u64 > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log record too large",
            ));
        }
        if self
            .file
            .metadata()?
            .len()
            .saturating_add(bytes.len() as u64)
            > self.limit
        {
            self.file.flush()?;
            match fs::rename(self.archive(1), self.archive(2)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            fs::rename(&self.path, self.archive(1))?;
            self.file = Self::open_file(&self.path)?;
        }
        self.file.write_all(bytes)
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn committed_from_state(state: CommitState) -> Option<bool> {
    match state {
        CommitState::Committed => Some(true),
        CommitState::NotSent | CommitState::Rejected => Some(false),
        CommitState::SentNoResponse | CommitState::Unknown => None,
    }
}

fn send_message_content(outcome: &RestOutcome) -> Option<String> {
    let request = outcome.request.as_ref()?;
    if request.action != RestAction::SendMessage {
        return None;
    }
    let body = request.body.as_deref()?;
    parse_json(body)
        .ok()?
        .get("content")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn computed_until_from_body(body: Option<&str>) -> Option<String> {
    match parse_json(body?)
        .ok()?
        .get("communication_disabled_until")?
    {
        JsonValue::String(value) => Some(value.clone()),
        JsonValue::Null => None,
        _ => None,
    }
}

fn audit_reason_from_request(request: &RestRequest) -> Option<String> {
    percent_decode(request.audit_reason_header.as_deref()?).ok()
}

fn utf8_prefix(value: &str, limit: usize) -> &str {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn sanitized_content_preview(value: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let redacted = redact_sensitive_text(value);
    utf8_prefix(&redacted, limit).to_string()
}

fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    let mut key_block = [0_u8; 64];
    if key.len() > 64 {
        let digest = sha256(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; 64];
    let mut outer_pad = [0x5c_u8; 64];
    for index in 0..64 {
        inner_pad[index] ^= key_block[index];
        outer_pad[index] ^= key_block[index];
    }
    let mut inner = Vec::with_capacity(64 + data.len());
    inner.extend_from_slice(&inner_pad);
    inner.extend_from_slice(data);
    let inner_digest = sha256(&inner);
    let mut outer = Vec::with_capacity(64 + inner_digest.len());
    outer.extend_from_slice(&outer_pad);
    outer.extend_from_slice(&inner_digest);
    hex_lower(&sha256(&outer))
}

pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hash = [
        0x6a09e667_u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(((data.len() + 72) / 64) * 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let mut a = hash[0];
        let mut b = hash[1];
        let mut c = hash[2];
        let mut d = hash[3];
        let mut e = hash[4];
        let mut f = hash[5];
        let mut g = hash[6];
        let mut h = hash[7];
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        hash[0] = hash[0].wrapping_add(a);
        hash[1] = hash[1].wrapping_add(b);
        hash[2] = hash[2].wrapping_add(c);
        hash[3] = hash[3].wrapping_add(d);
        hash[4] = hash[4].wrapping_add(e);
        hash[5] = hash[5].wrapping_add(f);
        hash[6] = hash[6].wrapping_add(g);
        hash[7] = hash[7].wrapping_add(h);
    }
    let mut out = [0_u8; 32];
    for (index, word) in hash.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn hex_lower(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn current_timestamp_millis() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("{}.{:03}Z", seconds_timestamp(ms / 1000), ms % 1000)
}

fn seconds_timestamp(seconds: u128) -> String {
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = (seconds % 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year,
        month,
        day,
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m as u32, d as u32)
}

fn comma(out: &mut String, first: bool) {
    if !first {
        out.push(',');
    }
}

fn key(out: &mut String, name: &str) {
    out.push('"');
    out.push_str(name);
    out.push_str("\":");
}

fn field_string(out: &mut String, name: &str, value: Option<&str>, first: bool) {
    comma(out, first);
    key(out, name);
    match value {
        Some(value) => {
            out.push('"');
            out.push_str(&json_log_escape(value));
            out.push('"');
        }
        None => out.push_str("null"),
    }
}

fn field_i32(out: &mut String, name: &str, value: Option<i32>) {
    field_number(out, name, value.map(|value| value.to_string()));
}

fn field_i64(out: &mut String, name: &str, value: Option<i64>) {
    field_number(out, name, value.map(|value| value.to_string()));
}

fn field_u32(out: &mut String, name: &str, value: Option<u32>) {
    field_number(out, name, value.map(|value| value.to_string()));
}

fn field_u64(out: &mut String, name: &str, value: Option<u64>) {
    field_number(out, name, value.map(|value| value.to_string()));
}

fn field_usize(out: &mut String, name: &str, value: Option<usize>) {
    field_number(out, name, value.map(|value| value.to_string()));
}

fn field_number(out: &mut String, name: &str, value: Option<String>) {
    comma(out, false);
    key(out, name);
    match value {
        Some(value) => out.push_str(&value),
        None => out.push_str("null"),
    }
}

fn field_bool(out: &mut String, name: &str, value: Option<bool>) {
    comma(out, false);
    key(out, name);
    match value {
        Some(true) => out.push_str("true"),
        Some(false) => out.push_str("false"),
        None => out.push_str("null"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn hash_key_is_hidden_from_debug_output() {
        let policy = AuditContentPolicy::Hash {
            key: b"private-test-audit-key".to_vec(),
        };
        assert_eq!(format!("{policy:?}"), "Hash { key: [REDACTED] }");
    }

    #[test]
    fn rolling_logs_bound_storage_and_keep_complete_records() {
        let root = std::env::temp_dir().join(format!(
            "dfor77-rotation-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("audit.jsonl");
        let mut log = RollingLog::open(&path, 8).unwrap();
        for _ in 0..10 {
            log.append(b"123\n").unwrap();
        }
        log.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"123\n123\n");
        for archive in [log.archive(1), log.archive(2)] {
            assert_eq!(fs::read(archive).unwrap(), b"123\n123\n");
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), 3);
        assert!(log.append(b"oversized record").is_err());
        drop(log);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rolling_log_rejects_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "dfor77-log-link-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("secret"), "private").unwrap();
        std::os::unix::fs::symlink(root.join("secret"), root.join("log")).unwrap();
        assert!(RollingLog::open(&root.join("log"), 100).is_err());
        assert_eq!(fs::read_to_string(root.join("secret")).unwrap(), "private");
        fs::remove_dir_all(root).unwrap();
    }
    use crate::rest::RestPlanner;

    fn fake_discord_token() -> String {
        [
            "abcdefghijklmnopqrstuvwx",
            "abcdef",
            "abcdefghijklmnopqrstuvwxyz1234567890",
        ]
        .join(".")
    }

    #[test]
    fn poll_audits_identify_the_message_without_treating_answers_or_cursors_as_users() {
        for (method, path) in [
            (
                crate::rest::HttpMethod::Get,
                "/api/v10/channels/20/polls/30/answers/12345?after=40&limit=100",
            ),
            (
                crate::rest::HttpMethod::Post,
                "/api/v10/channels/20/polls/30/expire",
            ),
        ] {
            let outcome = RestPlanner::new().raw_json_request(method, path, None, "");
            let record = AuditRecord::action("D7RST", &outcome, false, false);
            assert_eq!(record.target_channel_id.as_deref(), Some("20"));
            assert_eq!(record.target_message_id.as_deref(), Some("30"));
            assert_eq!(record.target_user_id, None);
            assert_eq!(record.target_guild_id, None);
            validate_audit_log_text(&record.to_json_line()).unwrap();
        }
    }

    #[test]
    fn audit_targets_follow_the_same_decoded_path_as_resource_mutations() {
        let request = crate::rest::RestPlanner::new()
            .raw_json_request(
                crate::rest::HttpMethod::Patch,
                "/api/v10/%67uilds/9/../010/members/%34%30?user=99",
                Some("{}"),
                "",
            )
            .request
            .unwrap();
        assert_eq!(
            audit_target_ids(Some(&request)),
            [Some("10".into()), None, None, Some("40".into())]
        );
    }

    #[test]
    fn audit_targets_are_distinct_from_event_context_and_never_use_webhook_tokens() {
        let mut planner = RestPlanner::new();
        let outcome = planner.delete_message("20", "30", "cleanup");
        let mut record = AuditRecord::action("D7DEL", &outcome, true, false);
        record.channel_id = Some("21".into());
        record.message_id = Some("31".into());
        let line = record.to_json_line();
        let body = parse_json(&line).unwrap();
        assert_eq!(body.get("channel_id").unwrap().as_str(), Some("21"));
        assert_eq!(body.get("message_id").unwrap().as_str(), Some("31"));
        assert_eq!(body.get("target_channel_id").unwrap().as_str(), Some("20"));
        assert_eq!(body.get("target_message_id").unwrap().as_str(), Some("30"));
        validate_audit_log_text(&line).unwrap();
        let outcome = planner.timeout_member("10", "40", 60, "test", 0);
        let record = AuditRecord::action("D7TMO", &outcome, true, false);
        assert_eq!(record.target_guild_id.as_deref(), Some("10"));
        assert_eq!(record.target_user_id.as_deref(), Some("40"));
        let outcome = planner.raw_json_request(
            crate::rest::HttpMethod::Get,
            "/api/v10/webhooks/10/private-token/messages/30",
            None,
            "",
        );
        let record = AuditRecord::action("D7RST", &outcome, true, false);
        assert_eq!(record.target_message_id.as_deref(), Some("30"));
        assert!(record.target_user_id.is_none());
        assert!(!record.to_json_line().contains("private-token"));
    }

    #[test]
    fn audit_validation_accepts_legacy_records_and_validates_optional_target_ids() {
        let mut planner = RestPlanner::new();
        let record =
            AuditRecord::action("D7SND", &planner.send_message("20", "hello"), true, false);
        let line = record.to_json_line();
        let legacy = line
            .replace(",\"target_guild_id\":null", "")
            .replace(",\"target_channel_id\":\"20\"", "")
            .replace(",\"target_message_id\":null", "")
            .replace(",\"target_user_id\":null", "");
        assert!(!legacy.contains("target_"));
        validate_audit_log_text(&legacy).unwrap();
        assert!(validate_audit_log_text(&line.replace(
            "\"target_channel_id\":\"20\"",
            "\"target_channel_id\":\"invalid\""
        ))
        .is_err());
    }

    #[test]
    fn audit_line_has_required_keys_and_escapes_strings() {
        let mut planner = RestPlanner::new();
        let outcome = planner.send_message("100000000000000001", "hello\n\"world\"");
        let record = AuditRecord::action("D7SND", &outcome, true, false);
        let line = record.to_json_line();
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"timestamp\":"));
        assert!(line.contains("\"bot_process_pid\":null"));
        assert!(line.contains("\"action\":\"D7SND\""));
        assert!(line.contains("\"content_mode\":\"none\""));
        assert!(line.contains("\"content_hash\":null"));
        assert!(line.contains("\"content_preview\":null"));
        assert!(line.contains("\"commit_state\":\"not_sent\""));
        assert!(line.contains("\"nonce\":"));
        assert!(!line.contains("hello"));
        assert!(!line.contains("Authorization"));
        assert!(!line.contains("Bot "));
    }

    #[test]
    fn audit_line_emits_all_nullable_schema_keys() {
        let mut planner = RestPlanner::new();
        let outcome = planner.delete_message("100000000000000001", "100000000000000002", "");
        let record = AuditRecord::action("D7DEL", &outcome, false, false);
        let line = record.to_json_line();
        for key in [
            "timestamp",
            "bot_process_pid",
            "ipc_request_id",
            "request_id",
            "event_seq",
            "event_type",
            "guild_id",
            "channel_id",
            "message_id",
            "user_id",
            "action",
            "route",
            "route_major_parameter",
            "bucket",
            "status",
            "http_status",
            "discord_error_code",
            "dry_run",
            "log_only",
            "committed",
            "commit_state",
            "commit_state_source",
            "request_started_at",
            "request_finished_at",
            "duration_ms",
            "why",
            "reason",
            "fatal_reason",
            "gateway_session_id",
            "gateway_sequence",
            "runtime_event_seq",
            "resume_attempt",
            "snapshot_age_ms",
            "nonce",
            "created_message_id",
            "computed_until",
            "identify_bucket",
            "identify_remaining",
            "identify_reset_after_ms",
            "daily_identify_count",
            "daily_identify_remaining",
            "gateway_outbound_remaining",
            "gateway_payload_bytes",
        ] {
            assert!(line.contains(&format!("\"{}\":", key)), "missing {}", key);
        }
    }

    #[test]
    fn audit_hashes_send_content() {
        let mut planner = RestPlanner::new();
        let outcome = planner.send_message(
            "100000000000000001",
            "The quick brown fox jumps over the lazy dog",
        );
        let record = AuditRecord::action_with_content_policy(
            "D7SND",
            &outcome,
            false,
            false,
            &AuditContentPolicy::Hash {
                key: b"key".to_vec(),
            },
        );
        let line = record.to_json_line();
        assert!(line.contains("\"content_mode\":\"hash\""));
        assert!(line.contains(
            "\"content_hash\":\"f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8\""
        ));
        assert!(line.contains("\"content_preview\":null"));
        assert!(!line.contains("The quick brown"));
    }

    #[test]
    fn audit_truncates_send_content_on_utf8_boundary() {
        let mut planner = RestPlanner::new();
        let outcome = planner.send_message("100000000000000001", "ab\u{3042}cd");
        let record = AuditRecord::action_with_content_policy(
            "D7SND",
            &outcome,
            false,
            false,
            &AuditContentPolicy::Truncated { preview_bytes: 4 },
        );
        let line = record.to_json_line();
        assert!(line.contains("\"content_mode\":\"truncated\""));
        assert!(line.contains("\"content_hash\":null"));
        assert_eq!(record.content_preview.as_deref(), Some("ab"));
        assert!(line.contains("\"content_preview\":\"ab\""));
    }

    #[test]
    fn audit_json_escapes_display_controls_and_retains_the_original_metadata() {
        let original = "metadata\n\u{0085}\u{2028}\u{202e}日本語\u{202c}\u{2066}👩‍💻\u{2069}";
        let mut record = AuditRecord::gateway_queue("D7GWS", 0, "OK", false, false, None, None);
        record.reason = Some(original.into());
        let line = record.to_json_line();
        for ch in [
            '\u{0085}', '\u{2028}', '\u{202e}', '\u{202c}', '\u{2066}', '\u{2069}',
        ] {
            assert!(!line.contains(ch));
        }
        let parsed = parse_json(line.trim_end()).unwrap();
        assert_eq!(
            parsed.get("reason").and_then(JsonValue::as_str),
            Some(original)
        );
    }

    #[test]
    fn authorization_headers_with_horizontal_tabs_do_not_expose_short_credentials() {
        for value in [
            "Authorization:\tBearer short-secret",
            "Authorization: Bearer\tshort-secret",
            "Authorization:\tBot\tshort-secret",
        ] {
            let redacted = redact_sensitive_text(value);
            assert!(!redacted.contains("short-secret"));
            assert!(redacted.contains("[REDACTED]"));
        }
    }

    #[test]
    fn audit_truncated_preview_redacts_authorization_and_controls() {
        let mut planner = RestPlanner::new();
        let token = fake_discord_token();
        let outcome = planner.send_message(
            "100000000000000001",
            &format!("Authorization: Bot {}\nnext\u{1b}[31m red", token),
        );
        let record = AuditRecord::action_with_content_policy(
            "D7SND",
            &outcome,
            false,
            false,
            &AuditContentPolicy::Truncated { preview_bytes: 120 },
        );
        let line = record.to_json_line();
        assert!(line.contains("\"content_mode\":\"truncated\""));
        assert!(line.contains("[REDACTED]"));
        assert!(line.contains("[CTRL]"));
        assert!(line.contains("next"));
        assert!(!line.contains("Authorization"));
        assert!(!line.contains("Bot "));
        assert!(!line.contains("abcdefghijkl"));
        assert!(!line.contains("\\u001B"));
        assert!(!line.contains("\\n"));
    }

    #[test]
    fn audit_truncated_preview_redacts_token_like_values_before_truncating() {
        let mut planner = RestPlanner::new();
        let token = fake_discord_token();
        let outcome = planner.send_message(
            "100000000000000001",
            &format!("{} suffix should not leak token prefix", token),
        );
        let record = AuditRecord::action_with_content_policy(
            "D7SND",
            &outcome,
            false,
            false,
            &AuditContentPolicy::Truncated { preview_bytes: 12 },
        );
        let line = record.to_json_line();
        assert!(line.contains("\"content_preview\":\"[REDACTED] s\""));
        assert!(!line.contains("abcdefghijkl"));
    }

    #[test]
    fn audit_extracts_timeout_computed_until_from_json_body() {
        let mut planner = RestPlanner::new();
        let outcome = planner.timeout_member("100000000000000001", "100000000000000002", 60, "", 0);
        let record = AuditRecord::action("D7TMO", &outcome, false, false);
        let line = record.to_json_line();
        assert!(
            line.contains("\"computed_until\":\"1970-01-01T00:01:00.000Z\""),
            "{line}"
        );
    }

    #[test]
    fn audit_records_untimeout_computed_until_as_null() {
        let mut planner = RestPlanner::new();
        let outcome = planner.untimeout_member("100000000000000001", "100000000000000002", "");
        let record = AuditRecord::action("D7UNT", &outcome, false, false);
        let line = record.to_json_line();
        assert!(line.contains("\"computed_until\":null"), "{line}");
    }

    #[test]
    fn audit_records_unencoded_reason_without_line_injection() {
        let mut planner = RestPlanner::new();
        let outcome =
            planner.delete_message("100000000000000001", "100000000000000002", "line\nreason");
        let record = AuditRecord::action("D7DEL", &outcome, false, false);
        let line = record.to_json_line();
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("\"reason\":\"line\\nreason\""), "{line}");
        assert!(!line.contains("line%0Areason"));
    }

    #[test]
    fn audit_log_validation_accepts_schema_records() {
        let mut planner = RestPlanner::new();
        let outcome = planner.send_message("100000000000000001", "hello");
        let record = AuditRecord::action_with_content_policy(
            "D7SND",
            &outcome,
            false,
            false,
            &AuditContentPolicy::Hash {
                key: b"key".to_vec(),
            },
        );
        let validation = validate_audit_log_text(&record.to_json_line()).unwrap();
        assert_eq!(validation.records, 1);
    }

    #[test]
    fn audit_log_validation_checks_required_actions() {
        let mut planner = RestPlanner::new();
        let outcome = planner.send_message("100000000000000001", "hello");
        let record = AuditRecord::action("D7SND", &outcome, false, false);
        let validation = validate_audit_log_text_with_options(
            &record.to_json_line(),
            &AuditLogValidationOptions {
                required_actions: vec!["D7SND".to_string()],
                forbidden_actions: Vec::new(),
                require_live_actions: false,
            },
        )
        .unwrap();
        assert_eq!(validation.records, 1);
        let error = validate_audit_log_text_with_options(
            &record.to_json_line(),
            &AuditLogValidationOptions {
                required_actions: vec!["D7DEL".to_string()],
                forbidden_actions: Vec::new(),
                require_live_actions: false,
            },
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("required action D7DEL not found"));
        let error = validate_audit_log_text_with_options(
            &record.to_json_line(),
            &AuditLogValidationOptions {
                required_actions: Vec::new(),
                forbidden_actions: vec!["D7SND".to_string()],
                require_live_actions: false,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("forbidden action D7SND found"));
        let error = validate_audit_log_text_with_options(
            &record.to_json_line(),
            &AuditLogValidationOptions {
                required_actions: vec!["D7SND".to_string()],
                forbidden_actions: Vec::new(),
                require_live_actions: true,
            },
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("required action D7SND has no live committed record"));
        let mut live_outcome = outcome.clone();
        live_outcome.status = 0;
        live_outcome.http_status = 200;
        live_outcome.commit_state = CommitState::Committed;
        let live_record = AuditRecord::action("D7SND", &live_outcome, false, false);
        let validation = validate_audit_log_text_with_options(
            &live_record.to_json_line(),
            &AuditLogValidationOptions {
                required_actions: vec!["D7SND".to_string()],
                forbidden_actions: Vec::new(),
                require_live_actions: true,
            },
        )
        .unwrap();
        assert_eq!(validation.records, 1);
    }

    #[test]
    fn audit_log_validation_rejects_missing_schema_key() {
        let error = validate_audit_log_text(r#"{"timestamp":"now"}"#).unwrap_err();
        assert!(error.to_string().contains("missing key bot_process_pid"));
    }

    #[test]
    fn audit_log_validation_rejects_sensitive_string_fields() {
        let mut planner = RestPlanner::new();
        let token = fake_discord_token();
        let outcome = planner.delete_message(
            "100000000000000001",
            "100000000000000002",
            &format!("Authorization: Bot {token}"),
        );
        let record = AuditRecord::action("D7DEL", &outcome, false, false);
        let error = validate_audit_log_text(&record.to_json_line()).unwrap_err();
        assert!(error
            .to_string()
            .contains("field reason contains sensitive text"));
    }

    #[test]
    fn sha256_and_hmac_match_known_vectors() {
        assert_eq!(
            hex_lower(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hmac_sha256_hex(b"key", b"The quick brown fox jumps over the lazy dog"),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn audit_logger_appends_jsonl() {
        let path =
            std::env::temp_dir().join(format!("dfor77-audit-test-{}.jsonl", std::process::id()));
        let mut planner = RestPlanner::new();
        let outcome = planner.delete_message("1", "2", "reason");
        let record = AuditRecord::action("D7DEL", &outcome, false, false);
        {
            let mut logger = AuditLogger::open(&path).unwrap();
            logger.append(&record).unwrap();
        }
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert!(content.contains("\"reason\":\"reason\""));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn audit_logger_creates_file_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "dfor77-audit-mode-test-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let mut logger = AuditLogger::open(&path).unwrap();
            let mut planner = RestPlanner::new();
            let outcome = planner.delete_message("1", "2", "");
            logger
                .append(&AuditRecord::action("D7DEL", &outcome, false, false))
                .unwrap();
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(path);
    }
}
