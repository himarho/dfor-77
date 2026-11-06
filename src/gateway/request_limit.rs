use std::collections::HashMap;
use std::fmt;

use super::{
    json_escape, required_i32, required_str, GatewayEvent, GatewayEventData, GatewayNormalizeError,
    JsonValue,
};
use crate::duration::seconds_to_ceil_ms;
use crate::validation::validate_snowflake;

const MAX_TRACKED_LIMITS: usize = 1024;
const FULL_MEMBER_REQUEST_INTERVAL_MS: u64 = 30_000;

#[derive(Clone, Eq, PartialEq)]
pub struct GatewayRateLimit {
    pub opcode: i32,
    pub retry_ms: u64,
    pub guild_id: Option<String>,
    pub nonce: Option<String>,
}

impl GatewayRateLimit {
    pub(super) fn parse(data: &JsonValue, legacy: bool) -> Result<Self, GatewayNormalizeError> {
        let opcode = required_i32(data, "opcode")?;
        if opcode < 0 {
            return Err(GatewayNormalizeError::new("rate limit opcode invalid"));
        }
        let retry_ms = if legacy {
            data.get("retry_ms").and_then(JsonValue::as_u64)
        } else {
            match data.get("retry_after") {
                Some(JsonValue::Number(seconds)) => seconds_to_ceil_ms(seconds),
                _ => None,
            }
        }
        .ok_or_else(|| GatewayNormalizeError::new("rate limit delay invalid"))?;
        let (guild_id, nonce) = match data.get("meta") {
            None | Some(JsonValue::Null) => (None, None),
            Some(meta @ JsonValue::Object(_)) => {
                let guild_id = meta
                    .get("guild_id")
                    .map(|_| required_str(meta, "guild_id").map(str::to_string))
                    .transpose()?;
                let nonce = meta
                    .get("nonce")
                    .map(|value| {
                        value
                            .as_str()
                            .filter(|nonce| nonce.len() <= 32)
                            .map(str::to_string)
                            .ok_or_else(|| GatewayNormalizeError::new("rate limit nonce invalid"))
                    })
                    .transpose()?;
                (guild_id, nonce)
            }
            Some(_) => return Err(GatewayNormalizeError::new("rate limit metadata invalid")),
        };
        Ok(Self {
            opcode,
            retry_ms,
            guild_id,
            nonce,
        })
    }

    pub(super) fn event(&self, sequence: Option<i64>) -> GatewayEvent {
        let mut event = GatewayEvent::rate_limited(self.opcode, self.retry_ms);
        event.sequence = sequence;
        event.data = GatewayEventData::RateLimit(self.clone());
        event
    }

    pub(crate) fn metadata_json(&self) -> String {
        let mut fields = Vec::new();
        if let Some(guild_id) = &self.guild_id {
            fields.push(format!("\"guild_id\":\"{}\"", json_escape(guild_id)));
        }
        if let Some(nonce) = &self.nonce {
            fields.push(format!("\"nonce\":\"{}\"", json_escape(nonce)));
        }
        format!("{{{}}}", fields.join(","))
    }
}

impl fmt::Debug for GatewayRateLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayRateLimit")
            .field("opcode", &self.opcode)
            .field("retry_ms", &self.retry_ms)
            .field("has_guild", &self.guild_id.is_some())
            .field("has_nonce", &self.nonce.is_some())
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct GatewayRequestKey {
    opcode: u64,
    full_member_guild: Option<String>,
}

impl GatewayRequestKey {
    pub(crate) fn parse(value: &JsonValue) -> Option<Self> {
        let opcode = value.get("op")?.as_u64()?;
        let full_member_guild = if opcode == 8
            && value.get("d")?.get("query").and_then(JsonValue::as_str) == Some("")
            && value.get("d")?.get("limit").and_then(JsonValue::as_u64) == Some(0)
        {
            value
                .get("d")?
                .get("guild_id")
                .and_then(JsonValue::as_str)
                .filter(|id| validate_snowflake(id))
                .map(canonical_id)
        } else {
            None
        };
        Some(Self {
            opcode,
            full_member_guild,
        })
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum RateScope {
    Opcode(u64),
    FullMemberGuild(String),
}

#[derive(Clone, Default)]
pub(crate) struct GatewayRequestLimiter {
    stops: HashMap<RateScope, u64>,
    overflow_stop_until_ms: u64,
}

impl GatewayRequestLimiter {
    pub(crate) fn wait_ms(&self, key: &GatewayRequestKey, now_ms: u64) -> u64 {
        let guild_stop = key
            .full_member_guild
            .as_ref()
            .and_then(|guild| self.stops.get(&RateScope::FullMemberGuild(guild.clone())))
            .copied()
            .unwrap_or(0);
        self.stops
            .get(&RateScope::Opcode(key.opcode))
            .copied()
            .unwrap_or(0)
            .max(guild_stop)
            .max(self.overflow_stop_until_ms)
            .saturating_sub(now_ms)
    }

    pub(crate) fn record_request(&mut self, key: &GatewayRequestKey, now_ms: u64) {
        if let Some(guild) = &key.full_member_guild {
            self.record(
                RateScope::FullMemberGuild(guild.clone()),
                now_ms.saturating_add(FULL_MEMBER_REQUEST_INTERVAL_MS),
                now_ms,
            );
        }
    }

    pub(crate) fn observe(&mut self, limit: &GatewayRateLimit, now_ms: u64) {
        let Ok(opcode) = u64::try_from(limit.opcode) else {
            return;
        };
        let scope = if opcode == 8 {
            limit
                .guild_id
                .as_deref()
                .filter(|id| validate_snowflake(id))
                .map(canonical_id)
                .map(RateScope::FullMemberGuild)
                .unwrap_or(RateScope::Opcode(opcode))
        } else {
            RateScope::Opcode(opcode)
        };
        self.record(scope, now_ms.saturating_add(limit.retry_ms), now_ms);
    }

    fn record(&mut self, scope: RateScope, stop_until: u64, now_ms: u64) {
        self.stops.retain(|_, deadline| *deadline > now_ms);
        if stop_until <= now_ms {
            return;
        }
        if self.stops.len() >= MAX_TRACKED_LIMITS && !self.stops.contains_key(&scope) {
            self.overflow_stop_until_ms = self
                .stops
                .values()
                .copied()
                .chain([self.overflow_stop_until_ms, stop_until])
                .max()
                .unwrap_or(0);
            self.stops.clear();
            return;
        }
        self.stops
            .entry(scope)
            .and_modify(|deadline| *deadline = (*deadline).max(stop_until))
            .or_insert(stop_until);
    }
}

impl fmt::Debug for GatewayRequestLimiter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayRequestLimiter")
            .field("tracked_limits", &self.stops.len())
            .field("overflow_stop_until_ms", &self.overflow_stop_until_ms)
            .finish()
    }
}

fn canonical_id(value: &str) -> String {
    value.trim_start_matches('0').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayOutboundLimiter;
    use crate::json::parse_json;

    fn full_members(guild: &str) -> GatewayRequestKey {
        GatewayRequestKey::parse(
            &parse_json(&format!(
                r#"{{"op":8,"d":{{"guild_id":"{guild}","query":"","limit":0}}}}"#
            ))
            .unwrap(),
        )
        .unwrap()
    }

    fn targeted_members() -> GatewayRequestKey {
        GatewayRequestKey::parse(
            &parse_json(r#"{"op":8,"d":{"guild_id":"10","user_ids":["40"]}}"#).unwrap(),
        )
        .unwrap()
    }

    fn limit(opcode: i32, retry_ms: u64, guild: Option<&str>) -> GatewayRateLimit {
        GatewayRateLimit {
            opcode,
            retry_ms,
            guild_id: guild.map(str::to_string),
            nonce: Some("private-rate-nonce".into()),
        }
    }

    #[test]
    fn rate_limit_delays_round_up_exactly_and_reject_invalid_core_fields() {
        for (seconds, expected) in [
            ("0", Some(0)),
            ("1.5001", Some(1501)),
            ("0.00000000001", Some(1)),
            ("1.000000000000000000001", Some(1001)),
            ("15.001e-1", Some(1501)),
            ("18446744073709551.615", Some(u64::MAX)),
            ("18446744073709551.616", None),
            ("-1", None),
            ("1e100", None),
            ("\"1.5\"", None),
            ("null", None),
        ] {
            let data = parse_json(&format!(r#"{{"opcode":8,"retry_after":{seconds},"meta":{{"guild_id":"10","nonce":"private-rate-nonce"}}}}"#)).unwrap();
            assert_eq!(
                GatewayRateLimit::parse(&data, false)
                    .ok()
                    .map(|limit| limit.retry_ms),
                expected,
                "{seconds}"
            );
        }
        for data in [
            r#"{"opcode":-1,"retry_after":1}"#,
            r#"{"opcode":8.0,"retry_after":1}"#,
            r#"{"opcode":8,"retry_after":1,"meta":{"guild_id":"invalid"}}"#,
            r#"{"opcode":8,"retry_after":1,"meta":[]}"#,
        ] {
            assert!(GatewayRateLimit::parse(&parse_json(data).unwrap(), false).is_err());
        }
        let limit = limit(8, 1000, Some("10"));
        assert!(!format!("{limit:?} {:?}", limit.event(Some(1))).contains("private-rate-nonce"));
        assert_eq!(
            parse_json(&limit.metadata_json())
                .unwrap()
                .get("nonce")
                .and_then(JsonValue::as_str),
            Some("private-rate-nonce")
        );
    }

    #[test]
    fn scopes_are_conservative_without_metadata_and_never_shorten_an_existing_deadline() {
        let full = full_members("10");
        let padded = full_members("010");
        let other = full_members("11");
        let targeted = targeted_members();
        let mut limiter = GatewayRequestLimiter::default();
        limiter.observe(&limit(8, 2000, Some("10")), 0);
        limiter.observe(&limit(8, 100, Some("010")), 100);
        assert_eq!(limiter.wait_ms(&padded, 100), 1900);
        assert_eq!(limiter.wait_ms(&other, 100), 0);
        assert_eq!(limiter.wait_ms(&targeted, 100), 0);
        limiter.observe(&limit(8, 500, None), 100);
        assert_eq!(limiter.wait_ms(&other, 100), 500);
        assert_eq!(limiter.wait_ms(&targeted, 100), 500);
        assert_eq!(limiter.wait_ms(&full, 100), 1900);
        assert_eq!(limiter.wait_ms(&full, 2000), 0);
    }

    #[test]
    fn limit_tracking_stays_bounded_and_preserves_deadlines_when_it_overflows() {
        let mut limiter = GatewayRequestLimiter::default();
        for guild in 1..=MAX_TRACKED_LIMITS + 1 {
            limiter.observe(&limit(8, 1000 + guild as u64, Some(&guild.to_string())), 0);
            assert!(limiter.stops.len() <= MAX_TRACKED_LIMITS);
        }
        assert!(limiter.stops.is_empty());
        let deadline = 1000 + MAX_TRACKED_LIMITS as u64 + 1;
        assert_eq!(limiter.wait_ms(&full_members("1"), 0), deadline);
        assert_eq!(limiter.wait_ms(&targeted_members(), 0), deadline);
        limiter.observe(&limit(255, 10, None), 1);
        assert_eq!(limiter.wait_ms(&full_members("1"), 1), deadline - 1);
        assert_eq!(limiter.wait_ms(&targeted_members(), deadline), 0);
        limiter.record_request(&full_members("10"), deadline);
        assert_eq!(limiter.wait_ms(&full_members("10"), deadline), 30_000);
        assert_eq!(limiter.wait_ms(&targeted_members(), deadline), 0);
        assert!(!format!("{limiter:?}").contains("private-rate-nonce"));
    }

    #[test]
    fn reconnects_cannot_reset_guild_cooldowns_or_opcode_notifications() {
        let mut limiter = GatewayOutboundLimiter::new();
        let full = full_members("10");
        limiter.record_request(&full, 0);
        let unknown =
            GatewayRequestKey::parse(&parse_json(r#"{"op":255,"d":null}"#).unwrap()).unwrap();
        limiter.observe_rate_limit(&limit(255, 45_000, None), 0);
        limiter.reset_connection();
        assert_eq!(
            limiter.check_request(&full, 29_999).unwrap().retry_after_ms,
            Some(1)
        );
        assert!(limiter.check_request(&full, 30_000).is_none());
        assert_eq!(
            limiter
                .check_request(&unknown, 44_999)
                .unwrap()
                .retry_after_ms,
            Some(1)
        );
        assert!(limiter.check_request(&unknown, 45_000).is_none());
    }
}
