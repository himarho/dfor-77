use std::collections::VecDeque;

use crate::constants::{D7ERS, D7GAP, D7GRL, D7RDY};
use crate::memory::HeapSize;
use crate::validation::utf8_prefix;

pub(crate) const DEFAULT_EVENT_QUEUE_MAX_BYTES: usize = 16 * 1024 * 1024;
const CONTROL_QUEUE_CAPACITY: usize = 8;
const MAX_CONTROL_EVENT_BYTES: usize = 4096;
const MAX_GAP_REASON_BYTES: usize = 2048;

pub(crate) trait QueueEvent: HeapSize {
    fn kind(&self) -> i32;
    fn description(&self) -> &str;
    fn description_mut(&mut self) -> &mut String;
    fn opcode(&self) -> Option<i32>;
    fn rate_limit_scope(&self) -> Option<&str> {
        None
    }
    fn gap(reason: String) -> Self;
}

pub(crate) fn is_control(kind: i32) -> bool {
    matches!(kind, D7RDY | D7ERS | D7GAP | D7GRL)
}

#[derive(Clone)]
pub(crate) struct EventQueue<T> {
    events: VecDeque<T>,
    capacity: usize,
    max_bytes: usize,
    normal_count: usize,
    normal_bytes: usize,
    control_count: usize,
    gap_epoch: u64,
    prioritise_controls: bool,
}

impl<T> std::fmt::Debug for EventQueue<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventQueue")
            .field("len", &self.events.len())
            .field("capacity", &self.capacity)
            .field("max_bytes", &self.max_bytes)
            .field("normal_count", &self.normal_count)
            .field("normal_bytes", &self.normal_bytes)
            .field("control_count", &self.control_count)
            .field("gap_epoch", &self.gap_epoch)
            .field("prioritise_controls", &self.prioritise_controls)
            .finish()
    }
}

impl<T: QueueEvent> EventQueue<T> {
    pub(crate) fn new(capacity: usize, max_bytes: usize, prioritise_controls: bool) -> Self {
        Self {
            events: VecDeque::new(),
            capacity: capacity.max(1),
            max_bytes: max_bytes.max(1),
            normal_count: 0,
            normal_bytes: 0,
            control_count: 0,
            gap_epoch: 0,
            prioritise_controls,
        }
    }

    pub(crate) fn set_max_bytes(&mut self, max_bytes: usize) -> bool {
        self.max_bytes = max_bytes.max(1);
        if self.normal_bytes > self.max_bytes {
            self.push_gap("QUEUE_BYTES_FULL");
            true
        } else {
            false
        }
    }

    pub(crate) fn push(&mut self, event: T) -> bool {
        if is_control(event.kind()) {
            self.push_control(event);
            false
        } else {
            self.push_normal(event)
        }
    }

    pub(crate) fn push_normal(&mut self, event: T) -> bool {
        if is_control(event.kind()) {
            self.push_control(event);
            return false;
        }
        let size = event.retained_size();
        if self.normal_count >= self.capacity {
            self.push_gap("QUEUE_FULL");
            true
        } else if size > self.max_bytes.saturating_sub(self.normal_bytes) {
            self.push_gap("QUEUE_BYTES_FULL");
            true
        } else {
            self.normal_count += 1;
            self.normal_bytes += size;
            self.events.push_back(event);
            false
        }
    }

    pub(crate) fn push_control(&mut self, event: T) {
        if event.kind() == D7GAP {
            self.push_gap(event.description());
            return;
        }
        if event.retained_size() > MAX_CONTROL_EVENT_BYTES {
            self.push_gap("CONTROL_EVENT_TOO_LARGE");
            return;
        }
        if event.kind() == D7GRL {
            if let Some(existing) = self.events.iter_mut().find(|existing| {
                existing.kind() == D7GRL
                    && existing.opcode() == event.opcode()
                    && existing.rate_limit_scope() == event.rate_limit_scope()
            }) {
                *existing = event;
                return;
            }
        }
        self.reserve_control_slot();
        self.control_count += 1;
        self.events.push_back(event);
    }

    pub(crate) fn pop_front(&mut self) -> Option<T> {
        let index = if self.prioritise_controls && self.control_count > 0 {
            self.events
                .iter()
                .position(|event| event.kind() == D7GAP)
                .or_else(|| {
                    self.events
                        .iter()
                        .position(|event| is_control(event.kind()))
                })
                .unwrap_or(0)
        } else {
            0
        };
        let event = self.events.remove(index)?;
        if is_control(event.kind()) {
            self.control_count -= 1;
        } else {
            self.normal_count -= 1;
            self.normal_bytes = self.normal_bytes.saturating_sub(event.retained_size());
        }
        if self.normal_count == 0 {
            self.events.shrink_to_fit();
        }
        Some(event)
    }

    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub(crate) fn gap_epoch(&self) -> u64 {
        self.gap_epoch
    }

    pub(crate) fn shrink_normal_events(&mut self, mut update: impl FnMut(&mut T)) -> bool {
        let mut released = 0_usize;
        let mut invalid = false;
        for event in &mut self.events {
            let kind = event.kind();
            if is_control(kind) {
                continue;
            }
            let previous = event.retained_size();
            update(event);
            let size = event.retained_size();
            if event.kind() != kind || size > previous {
                invalid = true;
                break;
            }
            released = released.saturating_add(previous - size);
        }
        if invalid {
            self.events.clear();
            self.normal_count = 0;
            self.normal_bytes = 0;
            self.control_count = 0;
            self.push_gap("INVALID_QUEUE_UPDATE");
            true
        } else {
            self.normal_bytes = self.normal_bytes.saturating_sub(released);
            false
        }
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.events
            .iter()
            .filter(|event| is_control(event.kind()))
            .fold(self.normal_bytes, |bytes, event| {
                bytes.saturating_add(event.retained_size())
            })
    }

    fn push_gap(&mut self, reason: &str) {
        self.events.retain(|event| is_control(event.kind()));
        self.events.shrink_to_fit();
        self.normal_count = 0;
        self.normal_bytes = 0;
        self.gap_epoch = self.gap_epoch.saturating_add(1);
        let reason = utf8_prefix(reason, MAX_GAP_REASON_BYTES);
        if let Some(existing) = self.events.iter_mut().find(|event| event.kind() == D7GAP) {
            let description = existing.description_mut();
            if !reason.is_empty() && !description.split(';').any(|part| part == reason) {
                let remaining = MAX_GAP_REASON_BYTES.saturating_sub(description.len());
                if remaining > 1 {
                    if !description.is_empty() {
                        description.push(';');
                    }
                    description.push_str(utf8_prefix(
                        reason,
                        MAX_GAP_REASON_BYTES - description.len(),
                    ));
                }
            }
            return;
        }
        self.reserve_control_slot();
        self.control_count += 1;
        self.events.push_back(T::gap(reason.to_string()));
    }

    fn reserve_control_slot(&mut self) {
        if self.control_count < CONTROL_QUEUE_CAPACITY {
            return;
        }
        let index = self
            .events
            .iter()
            .position(|event| event.kind() == D7GRL)
            .or_else(|| {
                self.events
                    .iter()
                    .rposition(|event| is_control(event.kind()) && event.kind() != D7GAP)
            });
        if let Some(index) = index {
            self.events.remove(index);
            self.control_count -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayEvent;

    #[test]
    fn shrinking_metadata_preserves_order_controls_counts_and_releases_the_byte_budget() {
        let mut queue = EventQueue::new(3, 6000, false);
        queue.push_control(GatewayEvent::ready(0));
        for sequence in 1..=2 {
            let mut description = String::with_capacity(2000);
            description.push('x');
            assert!(!queue.push_normal(GatewayEvent::message(sequence, description)));
        }
        let before = queue.retained_bytes();
        assert!(!queue.shrink_normal_events(|event| event.description = String::new()));
        assert!(queue.retained_bytes() <= before - 4000);
        assert_eq!(queue.normal_count, 2);
        assert_eq!(queue.control_count, 1);
        assert_eq!(queue.gap_epoch(), 0);
        assert!(!queue.push_normal(GatewayEvent::message(3, "y".repeat(3000))));
        assert_eq!(queue.pop_front().unwrap().description, "READY");
        for sequence in 1..=3 {
            assert_eq!(queue.pop_front().unwrap().sequence, Some(sequence));
        }
        assert_eq!(queue.retained_bytes(), 0);
        assert!(queue.is_empty());
    }

    #[test]
    fn shrinking_metadata_cannot_change_event_classes_or_increase_retained_memory() {
        for change_kind in [false, true] {
            let mut queue = EventQueue::new(3, 6000, false);
            queue.push_control(GatewayEvent::ready(0));
            queue.push_normal(GatewayEvent::message(1, "x"));
            assert!(queue.shrink_normal_events(|event| {
                if change_kind {
                    event.event_type = D7RDY;
                } else {
                    event.description = "y".repeat(1000);
                }
            }));
            assert_eq!(queue.normal_count, 0);
            assert_eq!(queue.normal_bytes, 0);
            assert_eq!(queue.control_count, 1);
            let event = queue.pop_front().unwrap();
            assert_eq!(event.event_type, D7GAP);
            assert_eq!(event.description, "INVALID_QUEUE_UPDATE");
            assert!(queue.is_empty());
        }
    }

    #[test]
    fn allocated_capacity_is_counted_and_overflow_always_keeps_a_gap() {
        let mut queue = EventQueue::new(10, 1024, true);
        let mut description = String::with_capacity(4096);
        description.push_str("small");
        assert!(queue.push_normal(GatewayEvent::message(1, description)));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop_front().unwrap().event_type, D7GAP);
        assert_eq!(queue.retained_bytes(), 0);
        let event = GatewayEvent::message(2, "accepted");
        let bytes = event.retained_size();
        assert!(!queue.push_normal(event));
        assert_eq!(queue.retained_bytes(), bytes);
        queue.pop_front();
        assert_eq!(queue.retained_bytes(), 0);
    }

    #[test]
    fn controls_and_coalesced_gap_reasons_remain_bounded_under_flooding() {
        let mut queue = EventQueue::new(100, 1024, false);
        for sequence in 0..10_000 {
            queue.push_control(GatewayEvent::ready(sequence));
            queue.push_control(GatewayEvent::rate_limited(sequence as i32, 1000));
        }
        assert!(queue.len() <= CONTROL_QUEUE_CAPACITY);
        for index in 0..10_000 {
            queue.push_control(GatewayEvent::gap(format!(
                "reason-{index}-{}",
                "事由".repeat(200)
            )));
        }
        assert!(queue.len() <= CONTROL_QUEUE_CAPACITY);
        assert!(queue.retained_bytes() <= CONTROL_QUEUE_CAPACITY * MAX_CONTROL_EVENT_BYTES);
        assert_eq!(queue.gap_epoch(), 10_000);
        let mut gaps = 0;
        while let Some(event) = queue.pop_front() {
            if event.event_type == D7GAP {
                gaps += 1;
                assert!(event.description.len() <= MAX_GAP_REASON_BYTES);
                assert!(std::str::from_utf8(event.description.as_bytes()).is_ok());
            }
        }
        assert_eq!(gaps, 1);
    }

    #[test]
    fn lowering_the_byte_limit_invalidates_pending_normals_and_releases_accounting() {
        let mut queue = EventQueue::new(10, 4096, false);
        queue.push_control(GatewayEvent::ready(0));
        assert!(!queue.push_normal(GatewayEvent::message(1, "text".repeat(100))));
        assert!(queue.set_max_bytes(1));
        assert_eq!(queue.normal_count, 0);
        assert_eq!(queue.normal_bytes, 0);
        assert_eq!(queue.pop_front().unwrap().event_type, D7RDY);
        assert_eq!(queue.pop_front().unwrap().event_type, D7GAP);
        assert!(queue.is_empty());
    }
}
