use std::collections::{HashMap, VecDeque};

use super::{header, method_text, RestRequest, RestResponse};
use crate::duration::seconds_to_ceil_ms as parse_seconds_to_ceil_ms;
use crate::json::{parse_json, JsonValue};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestAcquireDecision {
    pub allowed: bool,
    pub required_wait_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestGlobalLimiter {
    capacity: u32,
    refill_per_second: u32,
    available: u32,
    last_refill_ms: u64,
    stop_until_ms: Option<u64>,
}

impl RestGlobalLimiter {
    pub fn new() -> Self {
        Self {
            capacity: 50,
            refill_per_second: 50,
            available: 50,
            last_refill_ms: 0,
            stop_until_ms: None,
        }
    }

    pub fn try_acquire(&mut self, now_ms: u64, max_wait_ms: u64) -> RestAcquireDecision {
        self.refill(now_ms);
        let global_wait = self
            .stop_until_ms
            .map(|stop_until| stop_until.saturating_sub(now_ms))
            .unwrap_or(0);
        let token_wait = if self.available > 0 {
            0
        } else {
            self.ms_until_next_token(now_ms)
        };
        let required_wait_ms = global_wait.max(token_wait);
        if required_wait_ms > max_wait_ms {
            return RestAcquireDecision {
                allowed: false,
                required_wait_ms,
            };
        }
        if required_wait_ms > 0 {
            self.refill(now_ms.saturating_add(required_wait_ms));
        }
        self.available = self.available.saturating_sub(1);
        RestAcquireDecision {
            allowed: true,
            required_wait_ms,
        }
    }

    pub fn observe_response(&mut self, response: &RestResponse, now_ms: u64) {
        if response.http_status != 429 {
            return;
        }
        let global = header(response, "X-RateLimit-Global") == Some("true")
            || parse_json(&response.body)
                .ok()
                .and_then(|body| body.get("global").and_then(JsonValue::as_bool))
                == Some(true);
        if !global {
            return;
        }
        if let Some(wait_ms) = retry_after_ms(response, now_ms) {
            self.stop_until_ms = Some(
                self.stop_until_ms
                    .unwrap_or(0)
                    .max(now_ms.saturating_add(wait_ms)),
            );
        }
    }

    pub fn stop_until_ms(&self) -> Option<u64> {
        self.stop_until_ms
    }

    fn refill(&mut self, now_ms: u64) {
        if now_ms <= self.last_refill_ms {
            return;
        }
        let elapsed_ms = now_ms - self.last_refill_ms;
        let tokens = elapsed_ms.saturating_mul(u64::from(self.refill_per_second)) / 1000;
        if tokens > 0 {
            let available = u64::from(self.available)
                .saturating_add(tokens)
                .min(u64::from(self.capacity));
            self.available = available as u32;
            self.last_refill_ms = now_ms;
        }
        if self
            .stop_until_ms
            .is_some_and(|stop_until| now_ms >= stop_until)
        {
            self.stop_until_ms = None;
        }
    }

    fn ms_until_next_token(&self, now_ms: u64) -> u64 {
        let elapsed_ms = now_ms.saturating_sub(self.last_refill_ms);
        let period_ms = 1000 / u64::from(self.refill_per_second);
        period_ms.saturating_sub(elapsed_ms % period_ms).max(1)
    }
}

impl Default for RestGlobalLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct InvalidRequestBudget {
    warn: usize,
    recover: usize,
    stop: usize,
    window_ms: u64,
    events: VecDeque<u64>,
    stopped: bool,
}

impl InvalidRequestBudget {
    pub fn new(warn: usize, recover: usize, stop: usize) -> Result<Self, &'static str> {
        if warn == 0
            || recover == 0
            || stop == 0
            || stop >= 10_000
            || !(warn < recover && recover < stop)
        {
            return Err("invalid budget thresholds");
        }
        Ok(Self {
            warn,
            recover,
            stop,
            window_ms: 10 * 60 * 1000,
            events: VecDeque::new(),
            stopped: false,
        })
    }

    pub fn record(&mut self, response: &RestResponse, now_ms: u64) {
        self.prune(now_ms);
        if counts_as_invalid(response) {
            self.events.push_back(now_ms);
        }
        let count = self.events.len();
        if count >= self.stop {
            self.stopped = true;
        } else if self.stopped && count < self.recover {
            self.stopped = false;
        }
    }

    pub fn is_stopped(&mut self, now_ms: u64) -> bool {
        self.prune(now_ms);
        if self.stopped && self.events.len() < self.recover {
            self.stopped = false;
        }
        self.stopped
    }

    pub fn should_warn(&mut self, now_ms: u64) -> bool {
        self.prune(now_ms);
        self.events.len() >= self.warn
    }

    fn prune(&mut self, now_ms: u64) {
        while let Some(front) = self.events.front().copied() {
            if now_ms.saturating_sub(front) <= self.window_ms {
                break;
            }
            self.events.pop_front();
        }
    }
}

const MAX_TRACKED_ROUTES: usize = 4_096;
const MAX_TRACKED_COOLDOWNS: usize = 16_384;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RouteLimiter {
    bucket_by_route: HashMap<String, String>,
    route_order: VecDeque<String>,
    stop_until_ms_by_bucket: HashMap<String, u64>,
    overflow_stop_until_ms: u64,
}

impl RouteLimiter {
    pub fn try_acquire(
        &mut self,
        request: &RestRequest,
        now_ms: u64,
        max_wait_ms: u64,
    ) -> RestAcquireDecision {
        if self.overflow_stop_until_ms <= now_ms {
            self.overflow_stop_until_ms = 0;
        }
        let bucket_key = self.bucket_key_for(request);
        let stop_until = self
            .stop_until_ms_by_bucket
            .get(&bucket_key)
            .copied()
            .unwrap_or(0);
        if now_ms >= stop_until {
            self.stop_until_ms_by_bucket.remove(&bucket_key);
        }
        let required_wait_ms = stop_until
            .max(self.overflow_stop_until_ms)
            .saturating_sub(now_ms);
        RestAcquireDecision {
            allowed: required_wait_ms <= max_wait_ms,
            required_wait_ms,
        }
    }

    pub fn observe(&mut self, request: &RestRequest, response: &RestResponse, now_ms: u64) {
        if self.overflow_stop_until_ms <= now_ms {
            self.overflow_stop_until_ms = 0;
        }
        self.stop_until_ms_by_bucket
            .retain(|_, stop| *stop > now_ms);
        let old_key = self.bucket_key_for(request);
        let previous_stop = self.stop_until_ms_by_bucket.get(&old_key).copied();
        if let Some(bucket) = header(response, "X-RateLimit-Bucket").filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            let route = route_key(request);
            if !self.bucket_by_route.contains_key(&route) {
                if self.bucket_by_route.len() >= MAX_TRACKED_ROUTES {
                    if let Some(old_route) = self.route_order.pop_front() {
                        self.bucket_by_route.remove(&old_route);
                        self.preserve_overflow_cooldowns(0);
                    }
                }
                self.route_order.push_back(route.clone());
            }
            self.bucket_by_route.insert(route, bucket.to_string());
        }
        let bucket_key = self.bucket_key_for(request);
        let should_stop = response.http_status == 429 || rate_limit_remaining_is_zero(response);
        let observed_stop = if should_stop {
            retry_after_ms(response, now_ms).map(|wait| now_ms.saturating_add(wait))
        } else {
            None
        };
        if let Some(stop_until) = observed_stop.into_iter().chain(previous_stop).max() {
            if stop_until > now_ms {
                self.record_stop(bucket_key, stop_until);
            }
        }
    }

    fn record_stop(&mut self, bucket: String, stop_until: u64) {
        if self.stop_until_ms_by_bucket.len() >= MAX_TRACKED_COOLDOWNS
            && !self.stop_until_ms_by_bucket.contains_key(&bucket)
        {
            self.preserve_overflow_cooldowns(stop_until);
            self.stop_until_ms_by_bucket.clear();
        }
        self.stop_until_ms_by_bucket
            .entry(bucket)
            .and_modify(|stored| *stored = (*stored).max(stop_until))
            .or_insert(stop_until);
    }

    fn preserve_overflow_cooldowns(&mut self, additional_stop: u64) {
        self.overflow_stop_until_ms = self
            .stop_until_ms_by_bucket
            .values()
            .copied()
            .chain([self.overflow_stop_until_ms, additional_stop])
            .max()
            .unwrap_or(0);
    }

    pub fn bucket_key_for(&self, request: &RestRequest) -> String {
        let route = route_key(request);
        let bucket = self.bucket_by_route.get(&route).unwrap_or(&route);
        format!("{} {}", bucket, major_key(request))
    }

    pub fn stop_until_ms_for(&self, request: &RestRequest) -> Option<u64> {
        self.stop_until_ms_by_bucket
            .get(&self.bucket_key_for(request))
            .copied()
            .into_iter()
            .chain((self.overflow_stop_until_ms > 0).then_some(self.overflow_stop_until_ms))
            .max()
    }
}

pub(super) fn route_key(request: &RestRequest) -> String {
    let Some(path) = super::path::RestPath::parse(&request.path) else {
        return format!("{} /INVALID_PATH", method_text(request.method));
    };
    let parts = path.resource_parts();
    let mut route = method_text(request.method).to_string();
    for (index, part) in parts.iter().enumerate() {
        route.push('/');
        if (index == 2 && matches!(parts.first().copied(), Some("webhooks" | "interactions")))
            || (index > 0 && parts[index - 1] == "reactions")
            || (index > 0 && parts[index - 1] == "invites")
        {
            route.push_str(":value");
        } else if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) {
            route.push_str(":id");
        } else {
            route.push_str(part);
        }
    }
    route
}

pub(super) fn major_key(request: &RestRequest) -> String {
    let Some(path) = super::path::RestPath::parse(&request.path) else {
        return "global".to_string();
    };
    let parts = path.resource_parts();
    match parts.as_slice() {
        ["channels" | "guilds", id, ..] => {
            super::path::canonical_id(id).unwrap_or_else(|| "global".to_string())
        }
        ["webhooks", id, token, ..] => {
            let id = super::path::canonical_id(id).unwrap_or_else(|| "global".to_string());
            let hash = crate::audit::sha256(token.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            format!("{id}:{hash}")
        }
        ["webhooks", id] => super::path::canonical_id(id).unwrap_or_else(|| "global".to_string()),
        _ => "global".to_string(),
    }
}

pub fn retry_after_ms(response: &RestResponse, now_ms: u64) -> Option<u64> {
    let relative_wait = ["Retry-After", "X-RateLimit-Reset-After"]
        .iter()
        .filter_map(|key| header(response, key))
        .filter_map(parse_seconds_to_ceil_ms)
        .chain(body_retry_after_ms(response))
        .max();
    let reset_wait = header(response, "X-RateLimit-Reset")
        .and_then(parse_seconds_to_ceil_ms)
        .map(|reset_ms| reset_ms.saturating_sub(now_ms));
    relative_wait.into_iter().chain(reset_wait).max()
}

impl Default for InvalidRequestBudget {
    fn default() -> Self {
        Self {
            warn: 6000,
            recover: 7000,
            stop: 8000,
            window_ms: 10 * 60 * 1000,
            events: VecDeque::new(),
            stopped: false,
        }
    }
}

fn counts_as_invalid(response: &RestResponse) -> bool {
    match response.http_status {
        401 | 403 => true,
        429 => header(response, "X-RateLimit-Scope").is_none_or(|scope| scope != "shared"),
        _ => false,
    }
}

fn rate_limit_remaining_is_zero(response: &RestResponse) -> bool {
    header(response, "X-RateLimit-Remaining").and_then(|value| value.trim().parse::<i64>().ok())
        == Some(0)
}

fn body_retry_after_ms(response: &RestResponse) -> Option<u64> {
    let body = parse_json(&response.body).ok()?;
    let value = body.get("retry_after")?;
    match value {
        JsonValue::Number(text) | JsonValue::String(text) => parse_seconds_to_ceil_ms(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::{HttpMethod, RestPlanner};

    fn request(method: HttpMethod, path: &str) -> RestRequest {
        RestPlanner::new()
            .raw_json_request(method, path, None, "")
            .request
            .unwrap()
    }

    fn limited(bucket: &str, seconds: &str) -> RestResponse {
        let mut response = RestResponse::ok();
        response
            .headers
            .insert("X-RateLimit-Bucket".into(), bucket.into());
        response
            .headers
            .insert("X-RateLimit-Remaining".into(), "0".into());
        response
            .headers
            .insert("X-RateLimit-Reset-After".into(), seconds.into());
        response
    }

    #[test]
    fn routes_are_shared_between_typed_and_raw_requests_but_not_distinct_methods() {
        let raw = request(HttpMethod::Post, "/api/v10/channels/10/messages");
        let typed = RestPlanner::new()
            .send_message("10", "hello")
            .request
            .unwrap();
        let other_method = request(HttpMethod::Get, "/api/v10/channels/10/messages");
        let other_route = request(HttpMethod::Post, "/api/v10/channels/10/typing");
        let mut limiter = RouteLimiter::default();
        limiter.observe(&raw, &limited("message", "2"), 100);
        assert!(!limiter.try_acquire(&typed, 100, 0).allowed);
        assert!(limiter.try_acquire(&other_method, 100, 0).allowed);
        assert!(limiter.try_acquire(&other_route, 100, 0).allowed);
        limiter.observe(&other_method, &limited("read", "3"), 100);
        assert_eq!(limiter.stop_until_ms_for(&typed), Some(2100));
        assert_eq!(limiter.stop_until_ms_for(&other_method), Some(3100));
    }

    #[test]
    fn learned_routes_do_not_grow_for_message_ids_emojis_or_query_parameters() {
        let first = request(
            HttpMethod::Get,
            "/api/v10/channels/10/messages/20?before=30",
        );
        let second = request(HttpMethod::Get, "/api/v10/channels/11/messages/21?limit=50");
        assert_eq!(route_key(&first), route_key(&second));
        assert_ne!(major_key(&first), major_key(&second));
        let reaction = request(
            HttpMethod::Put,
            "/api/v10/channels/10/messages/20/reactions/a%3A50/@me",
        );
        let other_reaction = request(
            HttpMethod::Put,
            "/api/v10/channels/10/messages/21/reactions/b%3A51/@me",
        );
        assert_eq!(route_key(&reaction), route_key(&other_reaction));
        let mut limiter = RouteLimiter::default();
        for id in 1..1000 {
            let request = request(
                HttpMethod::Get,
                &format!("/api/v10/channels/{id}/messages/{id}"),
            );
            limiter.observe(&request, &limited("message", "1"), 0);
        }
        assert_eq!(limiter.bucket_by_route.len(), 1);
        assert_eq!(limiter.route_order.len(), 1);
    }

    #[test]
    fn shared_bucket_limits_respect_major_resources() {
        let first = request(HttpMethod::Post, "/api/v10/channels/10/messages");
        let alias = request(HttpMethod::Post, "/api/v10/channels/10/typing");
        let other_channel = request(HttpMethod::Post, "/api/v10/channels/11/messages");
        let mut limiter = RouteLimiter::default();
        limiter.observe(&first, &limited("shared", "2"), 0);
        let mut learned = RestResponse::ok();
        learned
            .headers
            .insert("X-RateLimit-Bucket".into(), "shared".into());
        limiter.observe(&alias, &learned, 0);
        assert!(!limiter.try_acquire(&alias, 0, 0).allowed);
        assert!(limiter.try_acquire(&other_channel, 0, 0).allowed);
    }

    #[test]
    fn encoded_identifiers_and_relative_paths_cannot_bypass_a_route_cooldown() {
        let canonical = request(HttpMethod::Delete, "/api/v10/channels/20/messages/30");
        for path in [
            "/api/v10/channels/020/messages/030?limit=1",
            "/api/v10/channels/%32%30/messages/31",
            "/api/v10/%63hannels/20/messages/32",
            "/api/v10/channels/99/../20/messages/33",
        ] {
            let variant = request(HttpMethod::Delete, path);
            assert_eq!(route_key(&variant), route_key(&canonical), "path={path}");
            assert_eq!(major_key(&variant), major_key(&canonical), "path={path}");
            let mut limiter = RouteLimiter::default();
            limiter.observe(&canonical, &limited("canonical-route", "2"), 0);
            assert!(!limiter.try_acquire(&variant, 0, 0).allowed, "path={path}");
        }
        let canonical = request(HttpMethod::Patch, "/api/v10/webhooks/50/secret/messages/30");
        let variant = request(
            HttpMethod::Patch,
            "/api/v10/%77ebhooks/050/%73ecret/messages/31",
        );
        assert_eq!(route_key(&variant), route_key(&canonical));
        assert_eq!(major_key(&variant), major_key(&canonical));
        assert!(!variant.provisional_key().contains("ecret"));
    }

    #[test]
    fn webhook_tokens_are_partitioned_without_retaining_plaintext_secrets() {
        let first = request(HttpMethod::Post, "/api/v10/webhooks/10/secret-one");
        let second = request(HttpMethod::Post, "/api/v10/webhooks/10/secret-two");
        let encoded = request(HttpMethod::Post, "/api/v10/webhooks/10/%73ecret-one");
        assert_eq!(route_key(&first), route_key(&second));
        assert_ne!(major_key(&first), major_key(&second));
        assert_eq!(major_key(&first), major_key(&encoded));
        let mut limiter = RouteLimiter::default();
        limiter.observe(&first, &limited("webhook", "2"), 0);
        assert!(!limiter.try_acquire(&first, 0, 0).allowed);
        assert!(limiter.try_acquire(&second, 0, 0).allowed);
        assert!(!format!("{limiter:?}").contains("secret-one"));
        assert!(!first.provisional_key().contains("secret-one"));
    }

    #[test]
    fn approved_wait_does_not_consume_a_cooldown_before_the_request_is_sent() {
        let request = request(HttpMethod::Get, "/api/v10/channels/10/messages");
        let mut limiter = RouteLimiter::default();
        limiter.observe(&request, &limited("message", "2"), 0);
        assert_eq!(
            limiter.try_acquire(&request, 100, 2000).required_wait_ms,
            1900
        );
        assert!(!limiter.try_acquire(&request, 100, 0).allowed);
        assert!(limiter.try_acquire(&request, 2000, 0).allowed);
        assert_eq!(limiter.stop_until_ms_for(&request), None);
    }

    #[test]
    fn bucket_hash_changes_and_shorter_observations_cannot_erase_a_cooldown() {
        let request = request(HttpMethod::Get, "/api/v10/channels/10/messages");
        let mut limiter = RouteLimiter::default();
        limiter.observe(&request, &limited("old", "10"), 0);
        limiter.observe(&request, &limited("new", "1"), 100);
        assert_eq!(limiter.stop_until_ms_for(&request), Some(10_000));
        assert_eq!(limiter.bucket_key_for(&request), "new 10");
    }

    #[test]
    fn route_cache_eviction_keeps_active_cooldowns_effective() {
        let first = request(HttpMethod::Get, "/api/v10/custom/first");
        let mut limiter = RouteLimiter::default();
        limiter.observe(&first, &limited("first", "10"), 0);
        let mut response = RestResponse::ok();
        response
            .headers
            .insert("X-RateLimit-Bucket".into(), "normal".into());
        for index in 0..MAX_TRACKED_ROUTES {
            let request = request(HttpMethod::Get, &format!("/api/v10/custom/route-{index}"));
            limiter.observe(&request, &response, 0);
        }
        assert_eq!(limiter.bucket_by_route.len(), MAX_TRACKED_ROUTES);
        assert_eq!(limiter.route_order.len(), MAX_TRACKED_ROUTES);
        assert!(!limiter.try_acquire(&first, 100, 0).allowed);
        assert!(limiter.try_acquire(&first, 10_000, 0).allowed);
    }

    #[test]
    fn cooldown_overflow_remains_bounded_and_blocks_until_existing_limits_expire() {
        let mut limiter = RouteLimiter::default();
        for index in 0..MAX_TRACKED_COOLDOWNS {
            limiter.record_stop(format!("bucket-{index}"), 5000);
        }
        limiter.record_stop("extra".into(), 6000);
        assert!(limiter.stop_until_ms_by_bucket.len() <= MAX_TRACKED_COOLDOWNS);
        let request = request(HttpMethod::Get, "/api/v10/channels/10/messages");
        assert!(!limiter.try_acquire(&request, 5999, 0).allowed);
        assert!(limiter.try_acquire(&request, 6000, 0).allowed);
        limiter.observe(&request, &RestResponse::ok(), 6000);
        assert!(limiter.stop_until_ms_by_bucket.is_empty());
    }

    #[test]
    fn decimal_retry_delays_round_up_without_float_precision_loss() {
        for (text, expected) in [
            ("0", 0),
            ("0.0001", 1),
            ("1.000000", 1000),
            ("1.00000001", 1001),
            ("1e-6", 1),
            ("1.234e2", 123_400),
            (" 2.5E+1 ", 25_000),
            ("999999999999999999999e-20", 10_000),
            ("18446744073709551.615", u64::MAX),
        ] {
            assert_eq!(parse_seconds_to_ceil_ms(text), Some(expected), "{text}");
        }
        for text in [
            "",
            ".5",
            "1.",
            "-1",
            "+1",
            "1.000x",
            "1.0000x",
            "1e",
            "1e1e2",
            "NaN",
            "inf",
            "1e309",
            "18446744073709551.616",
        ] {
            assert_eq!(parse_seconds_to_ceil_ms(text), None, "{text}");
        }
    }

    #[test]
    fn body_global_flag_applies_even_without_the_global_header() {
        let mut limiter = RestGlobalLimiter::default();
        let mut response = RestResponse::ok();
        response.http_status = 429;
        response.body = r#"{"global":true,"retry_after":1e-3}"#.into();
        limiter.observe_response(&response, 100);
        assert_eq!(limiter.stop_until_ms(), Some(101));
        assert!(!limiter.try_acquire(100, 0).allowed);
        assert!(limiter.try_acquire(101, 0).allowed);
    }
}
