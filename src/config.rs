mod parse;

use std::collections::{HashMap, HashSet};
use std::fs;

use crate::validation::{
    validate_bot_token_value, validate_env_var_name, validate_rest_origin_for_token,
    validate_snowflake,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub token_env: String,
    pub api_version: u32,
    pub intents: Vec<String>,
    pub intent_bits: u64,
    pub warnings: Vec<String>,
    pub dry_run: bool,
    pub log_only: bool,
    pub audit_path: String,
    pub audit_content_mode: String,
    pub audit_content_preview_bytes: usize,
    pub audit_hash_key_env: String,
    pub runtime_log_path: Option<String>,
    pub runtime_log_rest_success: bool,
    pub runtime_log_gateway_events: bool,
    pub gateway_shard_id: u32,
    pub gateway_shard_count: u32,
    pub gateway_identify_max_wait_ms: u64,
    pub gateway_daily_identify_warn: u32,
    pub gateway_daily_identify_stop: u32,
    pub gateway_live: bool,
    pub next_event_timeout_ms: u64,
    pub rest_request_timeout_ms: u64,
    pub rest_http_timeout_ms: u64,
    pub rest_timeout_margin_ms: u64,
    pub rest_delete_max_wait_ms: u64,
    pub rest_timeout_max_wait_ms: u64,
    pub rest_send_max_wait_ms: u64,
    pub rest_read_max_wait_ms: u64,
    pub rest_base_url: String,
    pub rest_global_max_wait_ms: u64,
    pub rest_invalid_budget_warn: usize,
    pub rest_invalid_budget_recover: usize,
    pub rest_invalid_budget_stop: usize,
    pub snapshot_event_queue_capacity: usize,
    pub snapshot_event_queue_max_bytes: usize,
    pub snapshot_cache_max_bytes: usize,
    pub snapshot_message_ttl_ms: u64,
    pub snapshot_message_max_entries: usize,
    pub snapshot_message_channel_allowlist: Vec<String>,
    pub snapshot_message_tombstone_ttl_ms: u64,
    pub snapshot_message_tombstone_max_entries: usize,
    pub snapshot_member_ttl_ms: u64,
    pub snapshot_member_max_entries: usize,
    pub snapshot_member_tombstone_ttl_ms: u64,
    pub snapshot_member_tombstone_max_entries: usize,
    pub snapshot_bot_member_ttl_ms: u64,
    pub snapshot_thread_member_ttl_ms: u64,
    pub snapshot_resource_max_entries: usize,
    pub snapshot_resource_tombstone_ttl_ms: u64,
    pub snapshot_resource_tombstone_max_entries: usize,
}

type ConfigValues = HashMap<String, parse::Value>;

impl Config {
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)
            .map_err(|error| ConfigError::new(format!("failed to read config: {}", error)))?;
        Self::from_toml_str(&content)
    }

    pub fn from_toml_str(content: &str) -> Result<Self, ConfigError> {
        Self::from_toml_str_with_env(content, |name| std::env::var(name).ok())
    }

    pub fn from_toml_str_with_env(
        content: &str,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let raw = parse::document(content)?;
        reject_unknown_fields(&raw)?;

        let token_env = required_string(&raw, "discord.token_env")?;
        if token_env.is_empty() {
            return Err(ConfigError::new("discord.token_env must not be empty"));
        }
        if !validate_env_var_name(&token_env) {
            return Err(ConfigError::new(
                "discord.token_env must be an environment variable name",
            ));
        }
        let token_value = lookup(&token_env).ok_or_else(|| {
            ConfigError::new(format!("environment variable {} is not set", token_env))
        })?;
        validate_bot_token_value(&token_value).map_err(|error| {
            ConfigError::new(format!(
                "environment variable {} is invalid: {}",
                token_env, error
            ))
        })?;

        let api_version = required_u32(&raw, "discord.api_version")?;
        if api_version != 10 {
            return Err(ConfigError::new("discord.api_version must be 10"));
        }

        let intents = required_array(&raw, "discord.intents")?;
        let intent_bits = intent_bits(&intents)?;

        let encoding = required_string(&raw, "gateway.encoding")?;
        if encoding != "json" {
            return Err(ConfigError::new("gateway.encoding must be json"));
        }
        let compression = required_string(&raw, "gateway.compression")?;
        if compression != "none" {
            return Err(ConfigError::new("gateway.compression must be none"));
        }

        let shard_id = required_u32(&raw, "gateway.shard_id")?;
        let shard_count = required_u32(&raw, "gateway.shard_count")?;
        if shard_count == 0 || shard_id >= shard_count {
            return Err(ConfigError::new(
                "gateway.shard_count must be >= 1 and shard_id < shard_count",
            ));
        }

        let identify_max_wait = required_u64(&raw, "gateway.identify_max_wait_ms")?;
        let daily_warn = required_u32(&raw, "gateway.daily_identify_warn")?;
        let daily_stop = required_u32(&raw, "gateway.daily_identify_stop")?;
        if daily_warn == 0 || daily_stop >= 1000 || daily_warn >= daily_stop {
            return Err(ConfigError::new(
                "daily identify budget must satisfy 0 < warn < stop < 1000",
            ));
        }
        let gateway_live = required_bool(&raw, "gateway.live")?;

        let request_timeout = validate_positive_u64(&raw, "rest.request_timeout_ms")?;
        let http_timeout = validate_positive_u64(&raw, "rest.http_timeout_ms")?;
        let timeout_margin = required_u64(&raw, "rest.timeout_margin_ms")?;
        let delete_max_wait = required_u64(&raw, "rest.delete_max_wait_ms")?;
        let timeout_max_wait = required_u64(&raw, "rest.timeout_max_wait_ms")?;
        let send_max_wait = required_u64(&raw, "rest.send_max_wait_ms")?;
        let read_max_wait = required_u64(&raw, "rest.read_max_wait_ms")?;
        let rest_base_url = required_string(&raw, "rest.base_url")?;
        validate_rest_base_url(&rest_base_url)?;
        let global_max_wait = required_u64(&raw, "rest.global_max_wait_ms")?;
        let max_wait = [
            delete_max_wait,
            timeout_max_wait,
            send_max_wait,
            read_max_wait,
            global_max_wait,
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        let required_timeout = max_wait
            .checked_add(http_timeout)
            .and_then(|value| value.checked_add(timeout_margin))
            .ok_or_else(|| ConfigError::new("rest timeout budget overflows u64"))?;
        if request_timeout <= required_timeout {
            return Err(ConfigError::new(
                "rest.request_timeout_ms is too small for configured wait budgets",
            ));
        }

        let invalid_warn = required_usize(&raw, "rest.invalid_budget_warn")?;
        let invalid_recover = required_usize(&raw, "rest.invalid_budget_recover")?;
        let invalid_stop = required_usize(&raw, "rest.invalid_budget_stop")?;
        if invalid_warn == 0
            || invalid_recover == 0
            || invalid_stop == 0
            || invalid_stop >= 10_000
            || !(invalid_warn < invalid_recover && invalid_recover < invalid_stop)
        {
            return Err(ConfigError::new(
                "invalid budget must satisfy 0 < warn < recover < stop < 10000",
            ));
        }

        let event_queue_capacity = required_usize(&raw, "snapshot.event_queue_capacity")?;
        let event_queue_max_bytes =
            validate_optional_positive_usize(&raw, "snapshot.event_queue_max_bytes")?
                .unwrap_or(crate::event_queue::DEFAULT_EVENT_QUEUE_MAX_BYTES);
        let cache_max_bytes = validate_optional_positive_usize(&raw, "snapshot.cache_max_bytes")?
            .unwrap_or(crate::snapshot::DEFAULT_SNAPSHOT_MAX_BYTES);
        if event_queue_capacity == 0 {
            return Err(ConfigError::new(
                "snapshot.event_queue_capacity must be >= 1",
            ));
        }

        let message_ttl = validate_positive_u64(&raw, "snapshot.message_snapshot_ttl_ms")?;
        let message_max_entries =
            validate_positive_usize(&raw, "snapshot.message_snapshot_max_entries")?;
        let message_channel_allowlist =
            optional_array(&raw, "snapshot.message_snapshot_channel_allowlist")?
                .unwrap_or_default();
        for channel_id in &message_channel_allowlist {
            if !validate_snowflake(channel_id) {
                return Err(ConfigError::new(
                    "snapshot.message_snapshot_channel_allowlist contains an invalid Discord id",
                ));
            }
        }
        let message_tombstone_ttl = required_u64(&raw, "snapshot.message_tombstone_ttl_ms")?;
        let message_tombstone_max_entries =
            validate_positive_usize(&raw, "snapshot.message_tombstone_max_entries")?;
        let member_ttl = validate_positive_u64(&raw, "snapshot.member_snapshot_ttl_ms")?;
        let member_max_entries =
            validate_optional_positive_usize(&raw, "snapshot.member_snapshot_max_entries")?
                .unwrap_or(message_max_entries);
        let member_tombstone_ttl = required_u64(&raw, "snapshot.member_tombstone_ttl_ms")?;
        let member_tombstone_max_entries =
            validate_positive_usize(&raw, "snapshot.member_tombstone_max_entries")?;
        let bot_member_ttl = validate_positive_u64(&raw, "snapshot.bot_member_snapshot_ttl_ms")?;
        let thread_member_ttl =
            validate_positive_u64(&raw, "snapshot.thread_member_snapshot_ttl_ms")?;
        let resource_max_entries =
            validate_optional_positive_usize(&raw, "snapshot.resource_snapshot_max_entries")?
                .unwrap_or(message_max_entries);
        let resource_tombstone_ttl = required_u64(&raw, "snapshot.resource_tombstone_ttl_ms")?;
        let resource_tombstone_max_entries =
            validate_positive_usize(&raw, "snapshot.resource_tombstone_max_entries")?;

        let content_mode = required_string(&raw, "audit.content_mode")?;
        if !matches!(content_mode.as_str(), "none" | "hash" | "truncated") {
            return Err(ConfigError::new("audit.content_mode is invalid"));
        }
        let preview = required_usize(&raw, "audit.content_preview_bytes")?;
        if preview > 512 {
            return Err(ConfigError::new(
                "audit.content_preview_bytes must be in 0..512",
            ));
        }
        let hash_key_env = optional_string(&raw, "audit.hash_key_env")?.unwrap_or_default();
        if content_mode == "hash" && hash_key_env.is_empty() {
            return Err(ConfigError::new(
                "audit.hash_key_env is required in hash mode",
            ));
        }
        if !hash_key_env.is_empty() && !validate_env_var_name(&hash_key_env) {
            return Err(ConfigError::new(
                "audit.hash_key_env must be an environment variable name",
            ));
        }
        if content_mode == "hash" {
            match lookup(&hash_key_env) {
                Some(value) if !value.is_empty() => {}
                _ => {
                    return Err(ConfigError::new(format!(
                        "environment variable {} is not set or empty",
                        hash_key_env
                    )));
                }
            }
        }

        let dry_run = required_bool(&raw, "mode.dry_run")?;
        let log_only = required_bool(&raw, "mode.log_only")?;
        let warnings = config_warnings(&intents, dry_run, log_only);

        Ok(Self {
            token_env,
            api_version,
            intents,
            intent_bits,
            warnings,
            dry_run,
            log_only,
            audit_path: required_string(&raw, "audit.path")?,
            audit_content_mode: content_mode,
            audit_content_preview_bytes: preview,
            audit_hash_key_env: hash_key_env,
            runtime_log_path: optional_string(&raw, "runtime.log_path")?
                .filter(|value| !value.is_empty()),
            runtime_log_rest_success: optional_bool(&raw, "runtime.log_rest_success")?
                .unwrap_or(true),
            runtime_log_gateway_events: optional_bool(&raw, "runtime.log_gateway_events")?
                .unwrap_or(true),
            gateway_shard_id: shard_id,
            gateway_shard_count: shard_count,
            gateway_identify_max_wait_ms: identify_max_wait,
            gateway_daily_identify_warn: daily_warn,
            gateway_daily_identify_stop: daily_stop,
            gateway_live,
            next_event_timeout_ms: required_u64(&raw, "gateway.next_event_timeout_ms")?,
            rest_request_timeout_ms: request_timeout,
            rest_http_timeout_ms: http_timeout,
            rest_timeout_margin_ms: timeout_margin,
            rest_delete_max_wait_ms: delete_max_wait,
            rest_timeout_max_wait_ms: timeout_max_wait,
            rest_send_max_wait_ms: send_max_wait,
            rest_read_max_wait_ms: read_max_wait,
            rest_base_url,
            rest_global_max_wait_ms: global_max_wait,
            rest_invalid_budget_warn: invalid_warn,
            rest_invalid_budget_recover: invalid_recover,
            rest_invalid_budget_stop: invalid_stop,
            snapshot_event_queue_capacity: event_queue_capacity,
            snapshot_event_queue_max_bytes: event_queue_max_bytes,
            snapshot_cache_max_bytes: cache_max_bytes,
            snapshot_message_ttl_ms: message_ttl,
            snapshot_message_max_entries: message_max_entries,
            snapshot_message_channel_allowlist: message_channel_allowlist,
            snapshot_message_tombstone_ttl_ms: message_tombstone_ttl,
            snapshot_message_tombstone_max_entries: message_tombstone_max_entries,
            snapshot_member_ttl_ms: member_ttl,
            snapshot_member_max_entries: member_max_entries,
            snapshot_member_tombstone_ttl_ms: member_tombstone_ttl,
            snapshot_member_tombstone_max_entries: member_tombstone_max_entries,
            snapshot_bot_member_ttl_ms: bot_member_ttl,
            snapshot_thread_member_ttl_ms: thread_member_ttl,
            snapshot_resource_max_entries: resource_max_entries,
            snapshot_resource_tombstone_ttl_ms: resource_tombstone_ttl,
            snapshot_resource_tombstone_max_entries: resource_tombstone_max_entries,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

fn reject_unknown_fields(raw: &ConfigValues) -> Result<(), ConfigError> {
    let allowed: HashSet<&str> = [
        "discord.token_env",
        "discord.api_version",
        "discord.intents",
        "gateway.encoding",
        "gateway.compression",
        "gateway.shard_id",
        "gateway.shard_count",
        "gateway.identify_max_wait_ms",
        "gateway.daily_identify_warn",
        "gateway.daily_identify_stop",
        "gateway.live",
        "gateway.next_event_timeout_ms",
        "rest.request_timeout_ms",
        "rest.http_timeout_ms",
        "rest.timeout_margin_ms",
        "rest.delete_max_wait_ms",
        "rest.timeout_max_wait_ms",
        "rest.send_max_wait_ms",
        "rest.read_max_wait_ms",
        "rest.base_url",
        "rest.global_max_wait_ms",
        "rest.invalid_budget_warn",
        "rest.invalid_budget_recover",
        "rest.invalid_budget_stop",
        "snapshot.event_queue_capacity",
        "snapshot.event_queue_max_bytes",
        "snapshot.cache_max_bytes",
        "snapshot.message_snapshot_ttl_ms",
        "snapshot.message_snapshot_max_entries",
        "snapshot.message_snapshot_channel_allowlist",
        "snapshot.message_tombstone_ttl_ms",
        "snapshot.message_tombstone_max_entries",
        "snapshot.member_snapshot_ttl_ms",
        "snapshot.member_snapshot_max_entries",
        "snapshot.member_tombstone_ttl_ms",
        "snapshot.member_tombstone_max_entries",
        "snapshot.bot_member_snapshot_ttl_ms",
        "snapshot.thread_member_snapshot_ttl_ms",
        "snapshot.resource_snapshot_max_entries",
        "snapshot.resource_tombstone_ttl_ms",
        "snapshot.resource_tombstone_max_entries",
        "audit.path",
        "audit.content_mode",
        "audit.content_preview_bytes",
        "audit.hash_key_env",
        "runtime.log_path",
        "runtime.log_rest_success",
        "runtime.log_gateway_events",
        "mode.dry_run",
        "mode.log_only",
    ]
    .into_iter()
    .collect();
    for key in raw.keys() {
        if !allowed.contains(key.as_str()) {
            return Err(ConfigError::new(format!("unknown config field {}", key)));
        }
    }
    Ok(())
}

fn required_string(raw: &ConfigValues, key: &str) -> Result<String, ConfigError> {
    optional_string(raw, key)?.ok_or_else(|| ConfigError::new(format!("{} is required", key)))
}

fn optional_string(raw: &ConfigValues, key: &str) -> Result<Option<String>, ConfigError> {
    match raw.get(key) {
        None => Ok(None),
        Some(parse::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ConfigError::new(format!("{} must be a string", key))),
    }
}

fn required_u64(raw: &ConfigValues, key: &str) -> Result<u64, ConfigError> {
    raw.get(key)
        .ok_or_else(|| ConfigError::new(format!("{} is required", key)))?
        .scalar()
        .ok_or_else(|| ConfigError::new(format!("{} must be an integer", key)))?
        .parse::<u64>()
        .map_err(|_| ConfigError::new(format!("{} must be an integer", key)))
}

fn required_u32(raw: &ConfigValues, key: &str) -> Result<u32, ConfigError> {
    let value = required_u64(raw, key)?;
    u32::try_from(value).map_err(|_| ConfigError::new(format!("{} is out of range", key)))
}

fn required_usize(raw: &ConfigValues, key: &str) -> Result<usize, ConfigError> {
    let value = required_u64(raw, key)?;
    usize::try_from(value).map_err(|_| ConfigError::new(format!("{} is out of range", key)))
}

fn optional_usize(raw: &ConfigValues, key: &str) -> Result<Option<usize>, ConfigError> {
    let Some(value) = raw.get(key) else {
        return Ok(None);
    };
    let value = value
        .scalar()
        .ok_or_else(|| ConfigError::new(format!("{} must be an integer", key)))?
        .parse::<u64>()
        .map_err(|_| ConfigError::new(format!("{} must be an integer", key)))?;
    usize::try_from(value)
        .map(Some)
        .map_err(|_| ConfigError::new(format!("{} is out of range", key)))
}

fn required_bool(raw: &ConfigValues, key: &str) -> Result<bool, ConfigError> {
    match raw
        .get(key)
        .ok_or_else(|| ConfigError::new(format!("{} is required", key)))?
        .scalar()
    {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err(ConfigError::new(format!("{} must be true or false", key))),
    }
}

fn optional_bool(raw: &ConfigValues, key: &str) -> Result<Option<bool>, ConfigError> {
    match raw.get(key).map(parse::Value::scalar) {
        Some(Some("true")) => Ok(Some(true)),
        Some(Some("false")) => Ok(Some(false)),
        Some(_) => Err(ConfigError::new(format!("{} must be true or false", key))),
        None => Ok(None),
    }
}

fn required_array(raw: &ConfigValues, key: &str) -> Result<Vec<String>, ConfigError> {
    let value = raw
        .get(key)
        .ok_or_else(|| ConfigError::new(format!("{} is required", key)))?;
    match value {
        parse::Value::StringArray(value) => Ok(value.clone()),
        _ => Err(ConfigError::new(format!("{} must be an array", key))),
    }
}

fn optional_array(raw: &ConfigValues, key: &str) -> Result<Option<Vec<String>>, ConfigError> {
    if raw.contains_key(key) {
        required_array(raw, key).map(Some)
    } else {
        Ok(None)
    }
}

fn validate_positive_u64(raw: &ConfigValues, key: &str) -> Result<u64, ConfigError> {
    let value = required_u64(raw, key)?;
    if value == 0 {
        return Err(ConfigError::new(format!("{} must be > 0", key)));
    }
    Ok(value)
}

fn validate_positive_usize(raw: &ConfigValues, key: &str) -> Result<usize, ConfigError> {
    let value = required_usize(raw, key)?;
    if value == 0 {
        return Err(ConfigError::new(format!("{} must be > 0", key)));
    }
    Ok(value)
}

fn validate_optional_positive_usize(
    raw: &ConfigValues,
    key: &str,
) -> Result<Option<usize>, ConfigError> {
    let Some(value) = optional_usize(raw, key)? else {
        return Ok(None);
    };
    if value == 0 {
        return Err(ConfigError::new(format!("{} must be > 0", key)));
    }
    Ok(Some(value))
}

fn validate_rest_base_url(value: &str) -> Result<(), ConfigError> {
    validate_rest_origin_for_token(value)
        .map(|_| ())
        .map_err(|error| ConfigError::new(format!("rest.{}", error)))
}

fn intent_bits(intents: &[String]) -> Result<u64, ConfigError> {
    let mut bits = 0_u64;
    for intent in intents {
        let bit = match intent.as_str() {
            "GUILDS" => 0,
            "GUILD_MEMBERS" => 1,
            "GUILD_MODERATION" | "GUILD_BANS" => 2,
            "GUILD_EXPRESSIONS" | "GUILD_EMOJIS_AND_STICKERS" => 3,
            "GUILD_INTEGRATIONS" => 4,
            "GUILD_WEBHOOKS" => 5,
            "GUILD_INVITES" => 6,
            "GUILD_VOICE_STATES" => 7,
            "GUILD_PRESENCES" => 8,
            "GUILD_MESSAGES" => 9,
            "GUILD_MESSAGE_REACTIONS" => 10,
            "GUILD_MESSAGE_TYPING" => 11,
            "DIRECT_MESSAGES" => 12,
            "DIRECT_MESSAGE_REACTIONS" => 13,
            "DIRECT_MESSAGE_TYPING" => 14,
            "MESSAGE_CONTENT" => 15,
            "GUILD_SCHEDULED_EVENTS" => 16,
            "AUTO_MODERATION_CONFIGURATION" => 20,
            "AUTO_MODERATION_EXECUTION" => 21,
            "GUILD_MESSAGE_POLLS" => 24,
            "DIRECT_MESSAGE_POLLS" => 25,
            _ => return Err(ConfigError::new(format!("unknown intent {}", intent))),
        };
        bits |= 1_u64 << bit;
    }
    Ok(bits)
}

fn has_intent(intents: &[String], intent: &str) -> bool {
    intents.iter().any(|value| value == intent)
}

fn config_warnings(intents: &[String], dry_run: bool, log_only: bool) -> Vec<String> {
    let mut warnings = Vec::new();
    if has_intent(intents, "GUILD_MESSAGES") && !has_intent(intents, "MESSAGE_CONTENT") {
        warnings.push(
            "MESSAGE_CONTENT intent is not configured; guild message content may be unavailable; check D7TAV"
                .to_string(),
        );
    }
    if has_intent(intents, "GUILDS") && !has_intent(intents, "GUILD_MEMBERS") {
        warnings.push(
            "GUILD_MEMBERS intent is not configured; moderation snapshots may be stale until REST refresh"
                .to_string(),
        );
    }
    if dry_run && log_only {
        warnings.push(
            "mode.dry_run and mode.log_only are both true; dry_run takes precedence".to_string(),
        );
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnvironment(HashMap<String, String>);

    impl TestEnvironment {
        fn new(values: &[(&str, &str)]) -> Self {
            Self(
                values
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
            )
        }

        fn parse(&self, content: &str) -> Result<Config, ConfigError> {
            Config::from_toml_str_with_env(content, |name| self.0.get(name).cloned())
        }
    }

    fn sample_config(token_env: &str, intents: &str) -> String {
        format!(
            r#"
[discord]
token_env = "{}"
api_version = 10
intents = [{}]

[gateway]
encoding = "json"
compression = "none"
shard_id = 0
shard_count = 1
identify_max_wait_ms = 600000
daily_identify_warn = 800
daily_identify_stop = 950
live = false
next_event_timeout_ms = 100

[rest]
request_timeout_ms = 30000
http_timeout_ms = 10000
timeout_margin_ms = 1000
delete_max_wait_ms = 10000
timeout_max_wait_ms = 10000
send_max_wait_ms = 10000
read_max_wait_ms = 10000
base_url = "https://discord.com"
global_max_wait_ms = 10000
invalid_budget_warn = 6000
invalid_budget_recover = 7000
invalid_budget_stop = 8000

[snapshot]
event_queue_capacity = 1024
message_snapshot_ttl_ms = 3600000
message_snapshot_max_entries = 100000
message_tombstone_ttl_ms = 3600000
message_tombstone_max_entries = 100000
member_snapshot_ttl_ms = 3600000
member_snapshot_max_entries = 100000
member_tombstone_ttl_ms = 3600000
member_tombstone_max_entries = 100000
bot_member_snapshot_ttl_ms = 3600000
thread_member_snapshot_ttl_ms = 3600000
resource_snapshot_max_entries = 100000
resource_tombstone_ttl_ms = 3600000
resource_tombstone_max_entries = 100000

[audit]
path = "./dfor77-audit.jsonl"
content_mode = "none"
content_preview_bytes = 0
hash_key_env = ""

[runtime]
log_path = "./dfor77-runtime.jsonl"

[mode]
dry_run = false
log_only = false
"#,
            token_env, intents
        )
    }

    #[test]
    fn config_computes_discord_intent_bits() {
        let token_env = format!("DFOR77_TEST_TOKEN_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let config = environment
            .parse(&sample_config(
                &token_env,
                r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
            ))
            .unwrap();
        assert_eq!(
            config.intent_bits,
            (1 << 0) | (1 << 1) | (1 << 9) | (1 << 15)
        );
        assert_eq!(config.gateway_shard_id, 0);
        assert_eq!(config.gateway_shard_count, 1);
        assert_eq!(config.gateway_identify_max_wait_ms, 600000);
        assert_eq!(config.gateway_daily_identify_warn, 800);
        assert_eq!(config.gateway_daily_identify_stop, 950);
        assert!(!config.gateway_live);
        assert_eq!(config.next_event_timeout_ms, 100);
        assert_eq!(config.rest_base_url, "https://discord.com");
        assert_eq!(config.rest_global_max_wait_ms, 10000);
        assert_eq!(config.rest_invalid_budget_warn, 6000);
        assert_eq!(config.rest_invalid_budget_recover, 7000);
        assert_eq!(config.rest_invalid_budget_stop, 8000);
        assert_eq!(config.snapshot_event_queue_capacity, 1024);
        assert_eq!(config.snapshot_message_ttl_ms, 3600000);
        assert_eq!(config.snapshot_message_max_entries, 100000);
        assert!(config.snapshot_message_channel_allowlist.is_empty());
        assert_eq!(config.snapshot_message_tombstone_ttl_ms, 3600000);
        assert_eq!(config.snapshot_message_tombstone_max_entries, 100000);
        assert_eq!(config.snapshot_member_ttl_ms, 3600000);
        assert_eq!(config.snapshot_member_max_entries, 100000);
        assert_eq!(config.snapshot_member_tombstone_ttl_ms, 3600000);
        assert_eq!(config.snapshot_member_tombstone_max_entries, 100000);
        assert_eq!(config.snapshot_bot_member_ttl_ms, 3600000);
        assert_eq!(config.snapshot_thread_member_ttl_ms, 3600000);
        assert_eq!(config.snapshot_resource_max_entries, 100000);
        assert_eq!(config.snapshot_resource_tombstone_ttl_ms, 3600000);
        assert_eq!(config.snapshot_resource_tombstone_max_entries, 100000);
        assert_eq!(config.audit_content_mode, "none");
        assert_eq!(config.audit_content_preview_bytes, 0);
        assert_eq!(config.audit_hash_key_env, "");
        assert_eq!(
            config.runtime_log_path.as_deref(),
            Some("./dfor77-runtime.jsonl")
        );
        assert!(config.runtime_log_rest_success);
        assert!(config.runtime_log_gateway_events);
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn config_accepts_legacy_snapshot_max_entry_fields() {
        let token_env = format!("DFOR77_TEST_TOKEN_LEGACY_SNAPSHOT_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace("member_snapshot_max_entries = 100000\n", "")
        .replace("resource_snapshot_max_entries = 100000\n", "");
        let config = environment.parse(&text).unwrap();
        assert_eq!(config.snapshot_message_max_entries, 100000);
        assert_eq!(config.snapshot_member_max_entries, 100000);
        assert_eq!(config.snapshot_resource_max_entries, 100000);
    }

    #[test]
    fn config_parser_handles_string_escapes_hashes_and_trailing_array_commas() {
        let token_env = format!("DFOR77_TEST_TOKEN_ESCAPED_CONFIG_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS","#,
        )
        .replace(
            r#"path = "./dfor77-audit.jsonl""#,
            r#"path = "./logs/#audit\\prod.jsonl" # external comment"#,
        );
        let config = environment.parse(&text).unwrap();
        assert_eq!(config.audit_path, "./logs/#audit\\prod.jsonl");
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn config_array_parser_keeps_commas_inside_strings() {
        let raw = parse::document(
            "[discord]\nintents = [\"GUILDS\", \"GUILD_MESSAGES\", \"MESSAGE,CONTENT\"]",
        )
        .unwrap();
        assert_eq!(
            required_array(&raw, "discord.intents").unwrap(),
            vec!["GUILDS", "GUILD_MESSAGES", "MESSAGE,CONTENT"]
        );
    }

    #[test]
    fn config_rejects_malformed_strings_and_arrays() {
        assert!(parse::string(r#""./bad\q.jsonl""#, "audit.path")
            .unwrap_err()
            .to_string()
            .contains("invalid escape"));
        assert!(
            parse::string_array(r#"["GUILDS",, "GUILD_MESSAGES"]"#, "discord.intents")
                .unwrap_err()
                .to_string()
                .contains("empty value")
        );
    }

    #[test]
    fn config_strings_reject_adjacent_tokens_and_unescaped_delimiters() {
        for value in [
            r#""first" "second""#,
            r#""first" path = "second""#,
            r#""first"suffix""#,
            r#""""unsupported""""#,
            "'first' 'second'",
            "'first' path = 'second'",
            "'first'suffix'",
        ] {
            assert!(parse::string(value, "audit.path").is_err(), "{value:?}");
        }
    }

    #[test]
    fn config_strings_support_literal_paths_hashes_quotes_and_tabs() {
        for (value, expected) in [
            (r#"'C:\Bots\#logs\new'"#, r#"C:\Bots\#logs\new"#),
            (r#"'a"b # c'"#, r#"a"b # c"#),
            ("\"a\tb\"", "a\tb"),
            ("'a\tb'", "a\tb"),
            (r#""a\"b\\c\t#d""#, "a\"b\\c\t#d"),
            (r#""\u007F""#, "\u{7f}"),
            (r#""\u65E5\U0001F600""#, "日😀"),
        ] {
            assert_eq!(parse::string(value, "audit.path").unwrap(), expected);
        }
        for control in (0_u8..=31).filter(|byte| *byte != b'\t').chain([127]) {
            for quote in ['\'', '"'] {
                let value = format!("{quote}a{}{quote}", char::from(control));
                assert!(parse::string(&value, "audit.path").is_err(), "{value:?}");
            }
        }
    }

    #[test]
    fn config_arrays_support_multiple_lines_and_both_string_delimiters() {
        let raw = parse::document(concat!(
            "[discord]\r\nintents = [ # comment\r\n",
            "  \"GUILDS\", # first\r\n\r\n",
            "  'DIRECT_MESSAGES', # second\r\n",
            "  'literal \\\" # value',\r\n",
            "] # closing\r\n[gateway]\r\nencoding = 'json'\r\n",
        ))
        .unwrap();
        assert_eq!(
            required_array(&raw, "discord.intents").unwrap(),
            ["GUILDS", "DIRECT_MESSAGES", "literal \\\" # value"]
        );
        assert_eq!(required_string(&raw, "gateway.encoding").unwrap(), "json");
        let raw = parse::document("[discord]\nintents = [\n # empty\n ]\n").unwrap();
        assert!(required_array(&raw, "discord.intents").unwrap().is_empty());
    }

    #[test]
    fn config_document_rejects_broken_value_boundaries_and_raw_controls() {
        for value in [
            "[\"GUILDS\" \"DIRECT_MESSAGES\"]",
            "[\"GUILDS\",, 'DIRECT_MESSAGES']",
            "[\"GUILDS\",\n[gateway]\nencoding = \"json\"",
            "[\"GUILDS\"] garbage",
            "[\"GUILDS\",\n'broken\nstring']",
            "\"first\" encoding = \"json\"",
        ] {
            assert!(
                parse::document(&format!("[discord]\nintents = {value}\n")).is_err(),
                "{value:?}"
            );
        }
        for suffix in ["# control \u{1b}[31m", "# DEL \u{7f}", "\u{0}"] {
            assert!(parse::document(&format!("[discord]\nintents = [] {suffix}\n")).is_err());
        }
    }

    #[test]
    fn config_typed_values_do_not_coerce_strings_scalars_or_arrays() {
        let raw = parse::document(concat!(
            "[discord]\ntoken_env = true\napi_version = '10'\nintents = 'GUILDS'\n",
            "[mode]\ndry_run = 'true'\nlog_only = []\n",
            "[snapshot]\ncache_max_bytes = '4096'\n",
        ))
        .unwrap();
        assert!(required_string(&raw, "discord.token_env").is_err());
        assert!(required_u64(&raw, "discord.api_version").is_err());
        assert!(required_array(&raw, "discord.intents").is_err());
        assert!(required_bool(&raw, "mode.dry_run").is_err());
        assert!(optional_bool(&raw, "mode.log_only").is_err());
        assert!(optional_usize(&raw, "snapshot.cache_max_bytes").is_err());
    }

    #[test]
    fn config_unicode_escapes_match_unicode_scalar_boundaries() {
        let boundaries = [
            0,
            1,
            9,
            31,
            32,
            34,
            39,
            92,
            127,
            128,
            0xd7ff,
            0xd800,
            0xdfff,
            0xe000,
            0xffff,
            0x10000,
            0x10ffff,
            0x110000,
            u32::MAX,
        ];
        let mut seed = 0x514fd72e_u32;
        let samples = (0..8192).map(|_| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed % 0x120000
        });
        for value in boundaries.into_iter().chain(samples) {
            for width in [4, 8] {
                if width == 4 && value > 0xffff {
                    continue;
                }
                let input = if width == 4 {
                    format!(r#""\u{value:04X}""#)
                } else {
                    format!(r#""\U{value:08X}""#)
                };
                let result = parse::string(&input, "audit.path").ok();
                assert_eq!(
                    result,
                    char::from_u32(value).map(|ch| ch.to_string()),
                    "{input}"
                );
            }
        }
    }

    #[test]
    fn config_rejects_zero_optional_snapshot_max_entries() {
        let token_env = format!("DFOR77_TEST_TOKEN_ZERO_SNAPSHOT_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace(
            "resource_snapshot_max_entries = 100000",
            "resource_snapshot_max_entries = 0",
        );
        let error = environment.parse(&text).unwrap_err();
        assert!(error
            .to_string()
            .contains("snapshot.resource_snapshot_max_entries must be > 0"));
    }

    #[test]
    fn config_accepts_and_validates_message_snapshot_channel_allowlist() {
        let token_env = format!("DFOR77_TEST_TOKEN_CHANNEL_ALLOWLIST_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace(
            "message_snapshot_max_entries = 100000\n",
            "message_snapshot_max_entries = 100000\nmessage_snapshot_channel_allowlist = [\"20\", \"30\"]\n",
        );
        let config = environment.parse(&text).unwrap();
        assert_eq!(
            config.snapshot_message_channel_allowlist,
            vec!["20".to_string(), "30".to_string()]
        );

        let invalid = text.replace("\"30\"", "\"bad\"");
        let error = environment.parse(&invalid).unwrap_err();
        assert!(error.to_string().contains(
            "snapshot.message_snapshot_channel_allowlist contains an invalid Discord id"
        ));
    }

    #[test]
    fn config_accepts_runtime_log_filters() {
        let token_env = format!("DFOR77_TEST_TOKEN_RUNTIME_LOG_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace(
            "log_path = \"./dfor77-runtime.jsonl\"",
            "log_path = \"./dfor77-runtime.jsonl\"\nlog_rest_success = false\nlog_gateway_events = false",
        );
        let config = environment.parse(&text).unwrap();
        assert!(!config.runtime_log_rest_success);
        assert!(!config.runtime_log_gateway_events);
    }

    #[test]
    fn config_warns_for_optional_intents_and_mode_precedence() {
        let token_env = format!("DFOR77_TEST_TOKEN_WARN_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(&token_env, r#""GUILDS", "GUILD_MESSAGES""#)
            .replace("dry_run = false", "dry_run = true")
            .replace("log_only = false", "log_only = true");
        let config = environment.parse(&text).unwrap();
        assert_eq!(
            config.warnings,
            vec![
                "MESSAGE_CONTENT intent is not configured; guild message content may be unavailable; check D7TAV",
                "GUILD_MEMBERS intent is not configured; moderation snapshots may be stale until REST refresh",
                "mode.dry_run and mode.log_only are both true; dry_run takes precedence",
            ]
        );
        assert!(config.dry_run);
        assert!(config.log_only);
    }

    #[test]
    fn config_memory_budgets_are_optional_positive_values() {
        let token_env = format!("DFOR77_TEST_TOKEN_MEMORY_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let base = sample_config(&token_env, r#""DIRECT_MESSAGES""#);
        let default = environment.parse(&base).unwrap();
        assert_eq!(default.snapshot_event_queue_max_bytes, 16 * 1024 * 1024);
        assert_eq!(default.snapshot_cache_max_bytes, 64 * 1024 * 1024);
        let config = environment
            .parse(&base.replace(
                "[snapshot]",
                "[snapshot]\nevent_queue_max_bytes = 4096\ncache_max_bytes = 8192",
            ))
            .unwrap();
        let runtime = crate::runtime::RuntimeSettings::from_config(&config);
        assert_eq!(runtime.event_queue_max_bytes, 4096);
        assert_eq!(runtime.snapshot_max_bytes, 8192);
        for field in ["event_queue_max_bytes", "cache_max_bytes"] {
            for value in ["0", "-1", "184467440737095516160"] {
                assert!(environment
                    .parse(&base.replace("[snapshot]", &format!("[snapshot]\n{field} = {value}")))
                    .is_err());
            }
        }
    }

    #[test]
    fn config_accepts_direct_message_only_and_empty_intents() {
        let token_env = format!("DFOR77_TEST_TOKEN_DM_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        for (intents, expected) in [
            (
                r#""DIRECT_MESSAGES", "DIRECT_MESSAGE_REACTIONS""#,
                (1 << 12) | (1 << 13),
            ),
            ("", 0),
        ] {
            let config = environment
                .parse(&sample_config(&token_env, intents))
                .unwrap();
            assert_eq!(config.intent_bits, expected);
            assert!(config.warnings.is_empty());
        }
    }

    #[test]
    fn config_recognises_documented_gateway_intents_and_legacy_aliases() {
        let names = [
            "GUILDS",
            "GUILD_MEMBERS",
            "GUILD_MODERATION",
            "GUILD_EXPRESSIONS",
            "GUILD_INTEGRATIONS",
            "GUILD_WEBHOOKS",
            "GUILD_INVITES",
            "GUILD_VOICE_STATES",
            "GUILD_PRESENCES",
            "GUILD_MESSAGES",
            "GUILD_MESSAGE_REACTIONS",
            "GUILD_MESSAGE_TYPING",
            "DIRECT_MESSAGES",
            "DIRECT_MESSAGE_REACTIONS",
            "DIRECT_MESSAGE_TYPING",
            "MESSAGE_CONTENT",
            "GUILD_SCHEDULED_EVENTS",
            "AUTO_MODERATION_CONFIGURATION",
            "AUTO_MODERATION_EXECUTION",
            "GUILD_MESSAGE_POLLS",
            "DIRECT_MESSAGE_POLLS",
        ]
        .map(str::to_string);
        assert_eq!(
            intent_bits(&names).unwrap(),
            ((1 << 17) - 1) | (1 << 20) | (1 << 21) | (1 << 24) | (1 << 25)
        );
        assert_eq!(intent_bits(&["GUILD_BANS".into()]).unwrap(), 1 << 2);
        assert_eq!(
            intent_bits(&["GUILD_EMOJIS_AND_STICKERS".into()]).unwrap(),
            1 << 3
        );
        assert_eq!(
            intent_bits(&["DIRECT_MESSAGES".into(), "DIRECT_MESSAGES".into()]).unwrap(),
            1 << 12
        );
    }

    #[test]
    fn config_rejects_unknown_intent() {
        let token_env = format!("DFOR77_TEST_TOKEN_UNKNOWN_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let error = environment
            .parse(&sample_config(
                &token_env,
                r#""GUILDS", "GUILD_MESSAGES", "NOPE""#,
            ))
            .unwrap_err();
        assert!(error.to_string().contains("unknown intent NOPE"));
    }

    #[test]
    fn config_rejects_duplicate_fields() {
        let token_env = format!("DFOR77_TEST_TOKEN_DUP_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace("api_version = 10", "api_version = 10\napi_version = 10");
        let error = environment.parse(&text).unwrap_err();
        assert!(error
            .to_string()
            .contains("duplicate config field discord.api_version"));
    }

    #[test]
    fn config_rejects_unknown_empty_section() {
        let token_env = format!("DFOR77_TEST_TOKEN_UNKNOWN_SECTION_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = format!(
            "{}\n[unexpected]\n",
            sample_config(
                &token_env,
                r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
            )
        );
        let error = environment.parse(&text).unwrap_err();
        assert!(error
            .to_string()
            .contains("unknown config section unexpected"));
    }

    #[test]
    fn config_rejects_duplicate_sections() {
        let token_env = format!("DFOR77_TEST_TOKEN_DUP_SECTION_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace("[mode]", "[discord]\n[mode]");
        let error = environment.parse(&text).unwrap_err();
        assert!(error
            .to_string()
            .contains("duplicate config section discord"));
    }

    #[test]
    fn config_rejects_invalid_token_env_name_before_lookup() {
        let error = Config::from_toml_str_with_env(
            &sample_config(
                "DISCORD BOT TOKEN",
                r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
            ),
            |_| panic!("invalid names must be rejected before credential lookup"),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("discord.token_env must be an environment variable name"));
    }

    #[test]
    fn config_rejects_invalid_token_env_value_without_leaking_it() {
        let token_env = format!("DFOR77_TEST_TOKEN_BAD_VALUE_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret token")]);
        let error = environment
            .parse(&sample_config(
                &token_env,
                r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
            ))
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("is invalid"));
        assert!(message.contains("whitespace or control characters"));
        assert!(!message.contains("secret token"));
    }

    #[test]
    fn config_rejects_rest_base_url_with_path() {
        let token_env = format!("DFOR77_TEST_TOKEN_REST_URL_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        for (url, message) in [
            (
                "https://discord.com/api/v10",
                "rest.base_url must not include a path",
            ),
            (
                "https://discord.com?x=1",
                "rest.base_url must not include query or fragment",
            ),
            (
                "https://user@discord.com",
                "rest.base_url must not include credentials",
            ),
            ("https://discord.com:abc", "rest.base_url port is invalid"),
            (
                "https://dis cord.com",
                "rest.base_url must not contain whitespace",
            ),
        ] {
            let text = sample_config(
                &token_env,
                r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
            )
            .replace(
                r#"base_url = "https://discord.com""#,
                &format!(r#"base_url = "{url}""#),
            );
            let error = environment.parse(&text).unwrap_err();
            assert!(error.to_string().contains(message), "{url}: {error}");
        }
    }

    #[test]
    fn config_rejects_zero_rest_request_timeout() {
        let token_env = format!(
            "DFOR77_TEST_TOKEN_ZERO_REQUEST_TIMEOUT_{}",
            std::process::id()
        );
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace("request_timeout_ms = 30000", "request_timeout_ms = 0");
        let error = environment.parse(&text).unwrap_err();
        assert!(error
            .to_string()
            .contains("rest.request_timeout_ms must be > 0"));
    }

    #[test]
    fn config_rejects_zero_rest_http_timeout() {
        let token_env = format!("DFOR77_TEST_TOKEN_ZERO_HTTP_TIMEOUT_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace("http_timeout_ms = 10000", "http_timeout_ms = 0");
        let error = environment.parse(&text).unwrap_err();
        assert!(error
            .to_string()
            .contains("rest.http_timeout_ms must be > 0"));
    }

    #[test]
    fn config_rejects_rest_timeout_budget_overflow() {
        let token_env = format!(
            "DFOR77_TEST_TOKEN_TIMEOUT_BUDGET_OVERFLOW_{}",
            std::process::id()
        );
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace(
            "http_timeout_ms = 10000",
            "http_timeout_ms = 18446744073709551615",
        );
        let error = environment.parse(&text).unwrap_err();
        assert!(error
            .to_string()
            .contains("rest timeout budget overflows u64"));
    }

    #[test]
    fn config_accepts_hash_audit_mode_with_env_value() {
        let token_env = format!("DFOR77_TEST_TOKEN_HASH_{}", std::process::id());
        let hash_env = format!("DFOR77_TEST_HASH_KEY_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret"), (&hash_env, "audit-key")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace(r#"content_mode = "none""#, r#"content_mode = "hash""#)
        .replace(
            r#"hash_key_env = """#,
            &format!(r#"hash_key_env = "{}""#, hash_env),
        );
        let config = environment.parse(&text).unwrap();
        assert_eq!(config.audit_content_mode, "hash");
        assert_eq!(config.audit_hash_key_env, hash_env);
    }

    #[test]
    fn config_lookup_reads_only_required_credentials_and_does_not_retain_values() {
        let text = sample_config("CUSTOM_TOKEN", r#""DIRECT_MESSAGES""#)
            .replace(r#"content_mode = "none""#, r#"content_mode = "hash""#)
            .replace(
                r#"hash_key_env = """#,
                r#"hash_key_env = "CUSTOM_AUDIT_KEY""#,
            );
        let mut requested = Vec::new();
        let config = Config::from_toml_str_with_env(&text, |name| {
            requested.push(name.to_string());
            match name {
                "CUSTOM_TOKEN" => Some("private-test-token".into()),
                "CUSTOM_AUDIT_KEY" => Some("private-test-audit-key".into()),
                _ => panic!("unexpected configuration lookup"),
            }
        })
        .unwrap();
        assert_eq!(requested, ["CUSTOM_TOKEN", "CUSTOM_AUDIT_KEY"]);
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("private-test-token"));
        assert!(!rendered.contains("private-test-audit-key"));
    }

    #[test]
    fn config_lookup_does_not_read_unused_hash_key_or_fall_back_to_process_environment() {
        let text = sample_config("CUSTOM_TOKEN", r#""DIRECT_MESSAGES""#).replace(
            r#"hash_key_env = """#,
            r#"hash_key_env = "UNUSED_AUDIT_KEY""#,
        );
        let mut requested = Vec::new();
        Config::from_toml_str_with_env(&text, |name| {
            requested.push(name.to_string());
            Some("provided-test-token".into())
        })
        .unwrap();
        assert_eq!(requested, ["CUSTOM_TOKEN"]);

        let text = sample_config("PATH", r#""DIRECT_MESSAGES""#);
        let error = Config::from_toml_str_with_env(&text, |_| None).unwrap_err();
        assert_eq!(error.to_string(), "environment variable PATH is not set");
    }

    #[test]
    fn config_lookup_preserves_empty_token_and_hash_key_validation() {
        let text = sample_config("CUSTOM_TOKEN", r#""DIRECT_MESSAGES""#);
        let error = Config::from_toml_str_with_env(&text, |_| Some(String::new())).unwrap_err();
        assert!(error.to_string().contains("bot token must not be empty"));

        let text = text
            .replace(r#"content_mode = "none""#, r#"content_mode = "hash""#)
            .replace(
                r#"hash_key_env = """#,
                r#"hash_key_env = "CUSTOM_AUDIT_KEY""#,
            );
        let error = Config::from_toml_str_with_env(&text, |name| {
            Some(if name == "CUSTOM_TOKEN" {
                "private-test-token".into()
            } else {
                String::new()
            })
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "environment variable CUSTOM_AUDIT_KEY is not set or empty"
        );
    }

    #[test]
    fn config_rejects_hash_audit_mode_without_env_value() {
        let token_env = format!("DFOR77_TEST_TOKEN_HASH_MISSING_{}", std::process::id());
        let hash_env = format!("DFOR77_TEST_HASH_KEY_MISSING_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace(r#"content_mode = "none""#, r#"content_mode = "hash""#)
        .replace(
            r#"hash_key_env = """#,
            &format!(r#"hash_key_env = "{}""#, hash_env),
        );
        let error = environment.parse(&text).unwrap_err();
        assert!(error.to_string().contains("is not set or empty"));
    }

    #[test]
    fn config_rejects_invalid_hash_key_env_name() {
        let token_env = format!("DFOR77_TEST_TOKEN_HASH_BAD_NAME_{}", std::process::id());
        let environment = TestEnvironment::new(&[(&token_env, "secret")]);
        let text = sample_config(
            &token_env,
            r#""GUILDS", "GUILD_MESSAGES", "MESSAGE_CONTENT", "GUILD_MEMBERS""#,
        )
        .replace(r#"content_mode = "none""#, r#"content_mode = "hash""#)
        .replace(r#"hash_key_env = """#, r#"hash_key_env = "HASH KEY""#);
        let error = environment.parse(&text).unwrap_err();
        assert!(error
            .to_string()
            .contains("audit.hash_key_env must be an environment variable name"));
    }
}
