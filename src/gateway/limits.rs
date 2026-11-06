use std::collections::VecDeque;
use std::fmt;

use super::request_limit::{GatewayRateLimit, GatewayRequestKey, GatewayRequestLimiter};
use super::{DAILY_IDENTIFY_WINDOW_MS, MAX_GATEWAY_PAYLOAD_BYTES};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewaySendKind {
    Heartbeat,
    Resume,
    Identify,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewaySendDecision {
    pub allowed: bool,
    pub why: &'static str,
    pub remaining: usize,
    pub retry_after_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct GatewayOutboundLimiter {
    capacity: usize,
    window_ms: u64,
    heartbeat_reserve: usize,
    sent_at_ms: VecDeque<u64>,
    presence_sent_at_ms: VecDeque<u64>,
    requests: GatewayRequestLimiter,
}

impl GatewayOutboundLimiter {
    pub fn new() -> Self {
        Self {
            capacity: 120,
            window_ms: 60_000,
            heartbeat_reserve: 2,
            sent_at_ms: VecDeque::new(),
            presence_sent_at_ms: VecDeque::new(),
            requests: GatewayRequestLimiter::default(),
        }
    }

    pub fn configure_heartbeat_interval_ms(&mut self, interval_ms: u64) {
        let interval = interval_ms.max(1);
        let slots = self.window_ms.saturating_add(interval - 1) / interval;
        self.heartbeat_reserve = (slots.saturating_add(1) as usize).max(2).min(self.capacity);
    }

    pub fn reset_connection(&mut self) {
        self.sent_at_ms.clear();
        self.heartbeat_reserve = 2;
    }

    pub fn check_and_record(
        &mut self,
        kind: GatewaySendKind,
        payload_bytes: usize,
        now_ms: u64,
    ) -> GatewaySendDecision {
        let mut decision = self.check(kind, payload_bytes, now_ms);
        if decision.allowed {
            self.sent_at_ms.push_back(now_ms);
            decision.remaining = self.remaining();
        }
        decision
    }

    pub fn check_and_record_presence(
        &mut self,
        payload_bytes: usize,
        now_ms: u64,
    ) -> GatewaySendDecision {
        let mut decision = self.check(GatewaySendKind::Other, payload_bytes, now_ms);
        if !decision.allowed {
            return decision;
        }
        while self
            .presence_sent_at_ms
            .front()
            .is_some_and(|sent| now_ms.saturating_sub(*sent) >= 20_000)
        {
            self.presence_sent_at_ms.pop_front();
        }
        if self.presence_sent_at_ms.len() >= 5 {
            return GatewaySendDecision {
                allowed: false,
                why: "GATEWAY_PRESENCE_BUDGET_EXHAUSTED",
                remaining: self.remaining(),
                retry_after_ms: self
                    .presence_sent_at_ms
                    .front()
                    .map(|sent| 20_000_u64.saturating_sub(now_ms.saturating_sub(*sent))),
            };
        }
        self.sent_at_ms.push_back(now_ms);
        self.presence_sent_at_ms.push_back(now_ms);
        decision.remaining = self.remaining();
        decision
    }

    pub(crate) fn check_request(
        &mut self,
        key: &GatewayRequestKey,
        now_ms: u64,
    ) -> Option<GatewaySendDecision> {
        self.prune(now_ms);
        let wait = self.requests.wait_ms(key, now_ms);
        (wait > 0).then(|| GatewaySendDecision {
            allowed: false,
            why: "GATEWAY_REQUEST_RATE_LIMITED",
            remaining: self.remaining(),
            retry_after_ms: Some(wait),
        })
    }

    pub(crate) fn record_request(&mut self, key: &GatewayRequestKey, now_ms: u64) {
        self.requests.record_request(key, now_ms);
    }

    pub(crate) fn observe_rate_limit(&mut self, limit: &GatewayRateLimit, now_ms: u64) {
        self.requests.observe(limit, now_ms);
    }

    fn check(
        &mut self,
        kind: GatewaySendKind,
        payload_bytes: usize,
        now_ms: u64,
    ) -> GatewaySendDecision {
        self.prune(now_ms);
        if payload_bytes > MAX_GATEWAY_PAYLOAD_BYTES {
            return GatewaySendDecision {
                allowed: false,
                why: "GATEWAY_PAYLOAD_TOO_LARGE",
                remaining: self.remaining(),
                retry_after_ms: None,
            };
        }
        if self.sent_at_ms.len() >= self.capacity || self.remaining() <= self.reserve_for(kind) {
            return GatewaySendDecision {
                allowed: false,
                why: "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED",
                remaining: self.remaining(),
                retry_after_ms: self.retry_after_ms(now_ms),
            };
        }
        GatewaySendDecision {
            allowed: true,
            why: "OK",
            remaining: self.remaining(),
            retry_after_ms: None,
        }
    }

    pub fn remaining(&self) -> usize {
        self.capacity.saturating_sub(self.sent_at_ms.len())
    }

    pub fn heartbeat_reserve(&self) -> usize {
        self.heartbeat_reserve
    }

    fn prune(&mut self, now_ms: u64) {
        while let Some(front) = self.sent_at_ms.front().copied() {
            if now_ms.saturating_sub(front) < self.window_ms {
                break;
            }
            self.sent_at_ms.pop_front();
        }
    }

    fn retry_after_ms(&self, now_ms: u64) -> Option<u64> {
        let front = self.sent_at_ms.front().copied()?;
        Some(self.window_ms.saturating_sub(now_ms.saturating_sub(front)))
    }

    fn reserve_for(&self, kind: GatewaySendKind) -> usize {
        match kind {
            GatewaySendKind::Heartbeat | GatewaySendKind::Resume => 0,
            GatewaySendKind::Identify => self.heartbeat_reserve,
            GatewaySendKind::Other => self.heartbeat_reserve.saturating_add(1).min(self.capacity),
        }
    }
}

impl Default for GatewayOutboundLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct DailyIdentifyBudget {
    warn: u32,
    stop: u32,
    sent_at_ms: VecDeque<u64>,
    window_ms: u64,
}

impl DailyIdentifyBudget {
    pub fn new(warn: u32, stop: u32) -> Result<Self, &'static str> {
        if warn == 0 || stop >= 1000 || warn >= stop {
            return Err("daily identify budget must satisfy 0 < warn < stop < 1000");
        }
        Ok(Self {
            warn,
            stop,
            sent_at_ms: VecDeque::new(),
            window_ms: DAILY_IDENTIFY_WINDOW_MS,
        })
    }

    #[cfg(test)]
    pub(super) fn with_window(warn: u32, stop: u32, window_ms: u64) -> Result<Self, &'static str> {
        let mut budget = Self::new(warn, stop)?;
        budget.window_ms = window_ms;
        Ok(budget)
    }

    pub fn can_identify(&self) -> bool {
        self.count() < self.stop
    }

    pub fn can_identify_at(&mut self, now_ms: u64) -> bool {
        self.prune(now_ms);
        self.can_identify()
    }

    pub fn record_identify_sent(&mut self) -> Result<(), &'static str> {
        self.record_identify_sent_at(0)
    }

    pub fn record_identify_sent_at(&mut self, now_ms: u64) -> Result<(), &'static str> {
        self.prune(now_ms);
        if !self.can_identify() {
            return Err("DAILY_IDENTIFY_BUDGET_EXHAUSTED");
        }
        self.sent_at_ms.push_back(now_ms);
        Ok(())
    }

    pub fn should_warn(&self) -> bool {
        self.count() >= self.warn
    }

    pub fn should_warn_at(&mut self, now_ms: u64) -> bool {
        self.prune(now_ms);
        self.should_warn()
    }

    pub fn count(&self) -> u32 {
        self.sent_at_ms.len().try_into().unwrap_or(u32::MAX)
    }

    pub fn remaining(&self) -> u32 {
        self.stop.saturating_sub(self.count())
    }

    fn prune(&mut self, now_ms: u64) {
        while let Some(front) = self.sent_at_ms.front().copied() {
            if now_ms.saturating_sub(front) < self.window_ms {
                break;
            }
            self.sent_at_ms.pop_front();
        }
    }
}

impl Default for DailyIdentifyBudget {
    fn default() -> Self {
        Self {
            warn: 900,
            stop: 999,
            sent_at_ms: VecDeque::new(),
            window_ms: DAILY_IDENTIFY_WINDOW_MS,
        }
    }
}

impl fmt::Debug for DailyIdentifyBudget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DailyIdentifyBudget")
            .field("warn", &self.warn)
            .field("stop", &self.stop)
            .field("count", &self.sent_at_ms.len())
            .field("window_ms", &self.window_ms)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_budget_uses_a_sliding_window_and_reports_the_next_available_slot() {
        let mut limiter = GatewayOutboundLimiter::new();
        for now in [0, 100, 200, 300, 400] {
            assert!(limiter.check_and_record_presence(10, now).allowed);
        }
        let denied = limiter.check_and_record_presence(10, 19_999);
        assert!(!denied.allowed);
        assert_eq!(denied.why, "GATEWAY_PRESENCE_BUDGET_EXHAUSTED");
        assert_eq!(denied.retry_after_ms, Some(1));
        assert_eq!(denied.remaining, 115);
        assert!(limiter.check_and_record_presence(10, 20_000).allowed);
        let denied = limiter.check_and_record_presence(10, 20_000);
        assert!(!denied.allowed);
        assert_eq!(denied.retry_after_ms, Some(100));
        assert!(limiter.check_and_record_presence(10, 20_100).allowed);
    }

    #[test]
    fn denied_payloads_do_not_spend_presence_slots_and_reconnection_cannot_reset_them() {
        let mut limiter = GatewayOutboundLimiter::new();
        for _ in 0..4 {
            assert!(limiter.check_and_record_presence(10, 0).allowed);
        }
        let oversized = limiter.check_and_record_presence(MAX_GATEWAY_PAYLOAD_BYTES + 1, 0);
        assert!(!oversized.allowed);
        assert_eq!(oversized.why, "GATEWAY_PAYLOAD_TOO_LARGE");
        limiter.configure_heartbeat_interval_ms(1);
        let blocked = limiter.check_and_record_presence(10, 0);
        assert!(!blocked.allowed);
        assert_eq!(blocked.why, "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED");
        limiter.reset_connection();
        assert_eq!(limiter.remaining(), 120);
        assert!(limiter.check_and_record_presence(10, 0).allowed);
        let blocked = limiter.check_and_record_presence(10, 0);
        assert!(!blocked.allowed);
        assert_eq!(blocked.why, "GATEWAY_PRESENCE_BUDGET_EXHAUSTED");
        assert_eq!(limiter.remaining(), 119);
        assert!(
            limiter
                .check_and_record(GatewaySendKind::Other, 10, 0)
                .allowed
        );
        limiter.reset_connection();
        assert!(!limiter.check_and_record_presence(10, 19_999).allowed);
        assert!(limiter.check_and_record_presence(10, 20_000).allowed);
    }
}
