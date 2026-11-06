use super::*;
use crate::gateway::request_limit::GatewayRequestKey;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum GatewayPayloadKind {
    Heartbeat,
    Presence,
    Other,
}

pub(super) struct QueuedGatewayPayload {
    pub(super) payload: String,
    pub(super) session_id: Option<String>,
    pub(super) kind: GatewayPayloadKind,
    pub(super) request_key: GatewayRequestKey,
}

impl DforRuntime {
    pub(super) fn send_gateway_user_payload<T: GatewayTransport>(
        &mut self,
        transport: &mut T,
        now_ms: u64,
        result: &mut GatewayPumpResult,
    ) -> bool {
        self.discard_stale_gateway_payloads();
        if self.gateway.machine().state() != GatewayState::Ready {
            return true;
        }
        for index in 0..self.gateway_user_outbound.len() {
            let queued = &self.gateway_user_outbound[index];
            if let Some(decision) = self
                .gateway_outbound
                .check_request(&queued.request_key, now_ms)
            {
                result.gateway_outbound_remaining = Some(decision.remaining);
                continue;
            }
            let heartbeat_payload = (queued.kind == GatewayPayloadKind::Heartbeat)
                .then(|| GatewayPayloads::heartbeat(self.gateway.machine().last_seen_seq()));
            let payload_bytes = heartbeat_payload.as_ref().unwrap_or(&queued.payload).len();
            let decision = if queued.kind == GatewayPayloadKind::Presence {
                self.gateway_outbound
                    .check_and_record_presence(payload_bytes, now_ms)
            } else {
                self.gateway_outbound.check_and_record(
                    GatewaySendKind::Other,
                    payload_bytes,
                    now_ms,
                )
            };
            result.gateway_outbound_remaining = Some(decision.remaining);
            if !decision.allowed {
                if decision.why == "GATEWAY_PRESENCE_BUDGET_EXHAUSTED" {
                    continue;
                }
                if decision.why == "GATEWAY_OUTBOUND_BUDGET_EXHAUSTED" {
                    self.errors.sev.clear();
                    return true;
                }
                let status = gateway_send_denied_status(GatewaySendKind::Other, decision.why);
                self.errors
                    .sev
                    .set(status, 0, 0, decision.why, decision.why);
                result.status = status;
                result.why = decision.why.to_string();
                result.action = gateway_send_denied_action(GatewaySendKind::Other, decision.why);
                return false;
            }
            let Some(queued) = self.gateway_user_outbound.remove(index) else {
                return true;
            };
            self.gateway_outbound
                .record_request(&queued.request_key, now_ms);
            let payload = heartbeat_payload.as_ref().unwrap_or(&queued.payload);
            if let Err(error) = transport.send_text(payload) {
                self.errors.sev.set(D7NET, 0, 0, error.why(), error.why());
                result.status = D7NET;
                result.why = error.why().to_string();
                return false;
            }
            if queued.kind == GatewayPayloadKind::Heartbeat {
                if let Some(heartbeat) = self.gateway.heartbeat_mut() {
                    heartbeat.mark_sent(now_ms);
                }
                result.heartbeat_sent = true;
                result.heartbeat_payload_bytes = payload.len();
            }
            return true;
        }
        self.errors.sev.clear();
        true
    }

    fn discard_stale_gateway_payloads(&mut self) {
        let session_id = self.gateway.machine().session_id();
        self.gateway_user_outbound
            .retain(|queued| queued.session_id.as_deref() == session_id);
    }

    pub(super) fn gateway_send(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let Some(payload) = request.field("payload") else {
            return self.action_arg_error(request.id);
        };
        self.queue_gateway_payload(request.id, payload.to_string(), "D7GWS")
    }

    pub(super) fn gateway_presence(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let Some(payload) = request.field("payload") else {
            return self.action_arg_error(request.id);
        };
        if !matches!(parse_json(payload), Ok(JsonValue::Object(_))) {
            return self.action_arg_error(request.id);
        }
        self.queue_gateway_payload(
            request.id,
            format!("{{\"op\":3,\"d\":{}}}", payload),
            "D7PRS",
        )
    }

    pub(super) fn gateway_voice_state(&mut self, request: Request) -> Response {
        if self.lifecycle == Lifecycle::Ended {
            return Response::empty(request.id, D7EOF);
        }
        if self.lifecycle != Lifecycle::Initialized {
            return Response::empty(request.id, D7STA);
        }
        let Some(guild_id) = request.field("guild_id") else {
            return self.action_arg_error(request.id);
        };
        if !validate_snowflake(guild_id) {
            return self.action_arg_error(request.id);
        }
        let channel_id = request.field("channel_id").unwrap_or("");
        if !channel_id.is_empty() && !validate_snowflake(channel_id) {
            return self.action_arg_error(request.id);
        }
        let Some(self_mute) = parse_boolish(request.field("self_mute")) else {
            return self.action_arg_error(request.id);
        };
        let Some(self_deaf) = parse_boolish(request.field("self_deaf")) else {
            return self.action_arg_error(request.id);
        };
        let channel_json = if channel_id.is_empty() {
            "null".to_string()
        } else {
            format!("\"{}\"", channel_id)
        };
        let payload = format!(
            "{{\"op\":4,\"d\":{{\"guild_id\":\"{}\",\"channel_id\":{},\"self_mute\":{},\"self_deaf\":{}}}}}",
            guild_id,
            channel_json,
            json_bool(self_mute),
            json_bool(self_deaf)
        );
        self.queue_gateway_payload(request.id, payload, "D7VST")
    }

    fn queue_gateway_payload(&mut self, id: u64, payload: String, action_name: &str) -> Response {
        if self.gateway.machine().state() != GatewayState::Ready {
            self.errors
                .act
                .set(D7STA, 0, 0, "gateway is not ready", "GATEWAY_NOT_READY");
            if self
                .write_gateway_queue_audit(
                    id,
                    action_name,
                    D7STA,
                    "GATEWAY_NOT_READY",
                    Some(payload.len()),
                    None,
                )
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
            return Response::empty(id, D7STA);
        }
        let parsed = if payload.len() <= MAX_GATEWAY_PAYLOAD_BYTES {
            parse_json(&payload).ok()
        } else {
            None
        };
        let metadata = parsed.as_ref().and_then(|value| {
            Some((
                gateway_payload_kind(value)?,
                GatewayRequestKey::parse(value)?,
            ))
        });
        let Some((kind, request_key)) = metadata else {
            self.errors
                .act
                .set(D7ARG, 0, 0, WHY_INVALID_FORM_BODY, WHY_INVALID_FORM_BODY);
            if self
                .write_gateway_queue_audit(
                    id,
                    action_name,
                    D7ARG,
                    WHY_INVALID_FORM_BODY,
                    Some(payload.len()),
                    None,
                )
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
            return Response::empty(id, D7ARG);
        };
        self.discard_stale_gateway_payloads();
        if self.gateway_user_outbound.len() >= MAX_GATEWAY_USER_OUTBOUND_QUEUE {
            self.errors
                .act
                .set(D7RTL, 0, 0, "gateway outbound queue full", "RATE_LIMITED");
            if self
                .write_gateway_queue_audit(
                    id,
                    action_name,
                    D7RTL,
                    "RATE_LIMITED",
                    Some(payload.len()),
                    Some(0),
                )
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
            return Response::empty(id, D7RTL);
        }
        let payload_bytes = payload.len();
        let remaining =
            MAX_GATEWAY_USER_OUTBOUND_QUEUE.saturating_sub(self.gateway_user_outbound.len() + 1);
        if self
            .write_gateway_queue_audit(
                id,
                action_name,
                D7OK,
                WHY_OK,
                Some(payload_bytes),
                Some(remaining),
            )
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
        self.gateway_user_outbound.push_back(QueuedGatewayPayload {
            payload,
            session_id: self.gateway.machine().session_id().map(str::to_string),
            kind,
            request_key,
        });
        self.errors.act.clear();
        Response::empty(id, D7OK)
    }
}

fn gateway_payload_kind(value: &JsonValue) -> Option<GatewayPayloadKind> {
    let data = value.get("d")?;
    let opcode = value.get("op")?.as_u64()?;
    if !crate::gateway::payload::valid_send_data(opcode, data) {
        return None;
    }
    match opcode {
        0 | 2 | 6 | 7 | 9 | 10 | 11 => None,
        1 if matches!(data, JsonValue::Null)
            || data.as_i64().is_some_and(|sequence| sequence >= 0) =>
        {
            Some(GatewayPayloadKind::Heartbeat)
        }
        1 => None,
        3 => Some(GatewayPayloadKind::Presence),
        _ => Some(GatewayPayloadKind::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::MockGatewayTransport;

    const PRESENCE: &str = r#"{"since":null,"activities":[],"status":"online","afk":false}"#;

    fn runtime() -> DforRuntime {
        let mut runtime = DforRuntime::new(false, false);
        call(&mut runtime, "INI", vec![]);
        runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"old","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, 0).unwrap();
        runtime
    }

    fn call(runtime: &mut DforRuntime, verb: &str, fields: Vec<Field>) -> Response {
        runtime.handle(Request {
            id: 1,
            verb: verb.into(),
            fields,
        })
    }

    fn presence(runtime: &mut DforRuntime) {
        assert_eq!(
            call(runtime, "PRS", vec![Field::percent("payload", PRESENCE)]).status,
            D7OK
        );
    }

    fn voice(runtime: &mut DforRuntime) {
        assert_eq!(
            call(
                runtime,
                "VST",
                vec![
                    Field::raw("guild_id", "10"),
                    Field::raw("channel_id", "20"),
                    Field::raw("self_mute", "0"),
                    Field::raw("self_deaf", "0")
                ]
            )
            .status,
            D7OK
        );
    }

    fn pump(
        runtime: &mut DforRuntime,
        transport: &mut MockGatewayTransport,
        now: u64,
    ) -> GatewayPumpResult {
        transport.push_error(GatewayTransportError::NoFrame);
        runtime.pump_gateway_once(transport, now)
    }

    fn count(transport: &MockGatewayTransport, opcode: i64) -> usize {
        transport
            .sent_texts()
            .iter()
            .filter(|payload| {
                parse_json(payload)
                    .unwrap()
                    .get("op")
                    .and_then(JsonValue::as_i64)
                    == Some(opcode)
            })
            .count()
    }

    #[test]
    fn raw_gateway_control_frames_cannot_bypass_connection_management() {
        let mut runtime = runtime();
        let mut transport = MockGatewayTransport::default();
        for payload in [
            r#"{"op":0,"d":{}}"#,
            r#"{"op":2,"d":{"token":"raw-control-token","intents":0,"properties":{}}}"#,
            r#"{"op":6,"d":{"token":"raw-control-token","session_id":"foreign","seq":100}}"#,
            r#"{"op":7,"d":null}"#,
            r#"{"op":9,"d":false}"#,
            r#"{"op":10,"d":{"heartbeat_interval":1}}"#,
            r#"{"op":11,"d":null}"#,
        ] {
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7ARG,
                "{payload}"
            );
            assert!(runtime.gateway_user_outbound.is_empty());
        }
        assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
        assert!(transport.sent_texts().is_empty());
        assert_eq!(runtime.gateway.machine().state(), GatewayState::Ready);
        assert_eq!(runtime.gateway.machine().session_id(), Some("old"));
        assert_eq!(runtime.gateway_manager.daily_budget().count(), 0);
    }

    #[test]
    fn periodic_heartbeats_close_an_unacknowledged_connection_at_the_first_deadline() {
        let mut runtime = runtime();
        runtime
            .ingest_gateway_json(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#, 0)
            .unwrap();
        let due = runtime.gateway.heartbeat().unwrap().next_due_ms().unwrap();
        let mut transport = MockGatewayTransport::default();
        assert!(pump(&mut runtime, &mut transport, due).heartbeat_sent);
        let result = pump(&mut runtime, &mut transport, due + 1000);
        assert_ne!(result.action, GatewayAction::None);
        assert!(!result.heartbeat_sent);
        assert_eq!(count(&transport, 1), 1);
        assert_ne!(runtime.gateway.machine().state(), GatewayState::Ready);
    }

    #[test]
    fn additional_heartbeat_requests_do_not_hide_an_unacknowledged_connection() {
        let mut runtime = runtime();
        runtime
            .ingest_gateway_json(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#, 0)
            .unwrap();
        let due = runtime.gateway.heartbeat().unwrap().next_due_ms().unwrap();
        let mut transport = MockGatewayTransport::default();
        assert!(pump(&mut runtime, &mut transport, due).heartbeat_sent);
        for now in [due + 400, due + 800] {
            transport.push_incoming(r#"{"op":1,"d":null}"#);
            assert!(
                runtime
                    .pump_gateway_once(&mut transport, now)
                    .heartbeat_sent
            );
        }
        let result = pump(&mut runtime, &mut transport, due + 1000);
        assert_ne!(result.action, GatewayAction::None);
        assert!(!result.heartbeat_sent);
        assert_eq!(count(&transport, 1), 3);
    }

    #[test]
    fn queued_raw_heartbeats_use_the_current_cursor_and_share_ack_tracking() {
        for acknowledge in [false, true] {
            let mut runtime = runtime();
            runtime
                .ingest_gateway_json(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#, 0)
                .unwrap();
            let heartbeat = runtime.gateway.heartbeat_mut().unwrap();
            heartbeat.mark_sent(0);
            heartbeat.ack(0);
            let payload = r#"{"op":1,"d":1}"#;
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7OK
            );
            runtime
                .ingest_gateway_json(r#"{"op":0,"s":2,"t":"FUTURE_EVENT","d":{}}"#, 100)
                .unwrap();
            let mut transport = MockGatewayTransport::default();
            assert!(pump(&mut runtime, &mut transport, 100).heartbeat_sent);
            assert_eq!(transport.sent_texts(), &[r#"{"op":1,"d":2}"#.to_string()]);
            if acknowledge {
                transport.push_incoming(r#"{"op":11}"#);
                assert_eq!(runtime.pump_gateway_once(&mut transport, 1099).status, D7OK);
                let result = pump(&mut runtime, &mut transport, 1100);
                assert_eq!(result.action, GatewayAction::None);
                assert!(result.heartbeat_sent);
                assert_eq!(runtime.gateway.machine().state(), GatewayState::Ready);
            } else {
                assert_eq!(
                    call(
                        &mut runtime,
                        "GWS",
                        vec![Field::percent("payload", r#"{"op":1,"d":null}"#)]
                    )
                    .status,
                    D7OK
                );
                assert!(pump(&mut runtime, &mut transport, 600).heartbeat_sent);
                let result = pump(&mut runtime, &mut transport, 1100);
                assert_ne!(result.action, GatewayAction::None);
                assert!(!result.heartbeat_sent);
            }
            assert_eq!(count(&transport, 1), 2);
            assert!(runtime.gateway_user_outbound.is_empty());
        }
    }

    #[test]
    fn raw_heartbeats_cannot_spend_the_automatic_heartbeat_reserve() {
        let mut runtime = runtime();
        runtime
            .ingest_gateway_json(r#"{"op":10,"d":{"heartbeat_interval":1000}}"#, 0)
            .unwrap();
        let heartbeat = runtime.gateway.heartbeat_mut().unwrap();
        heartbeat.mark_sent(0);
        heartbeat.ack(0);
        while runtime
            .gateway_outbound
            .check_and_record(GatewaySendKind::Other, 1, 0)
            .allowed
        {}
        assert_eq!(
            call(
                &mut runtime,
                "GWS",
                vec![Field::percent("payload", r#"{"op":1,"d":null}"#)]
            )
            .status,
            D7OK
        );
        let mut transport = MockGatewayTransport::default();
        transport.push_incoming(r#"{"op":1,"d":null}"#);
        let result = runtime.pump_gateway_once(&mut transport, 0);
        assert_eq!(result.status, D7OK);
        assert!(result.heartbeat_sent);
        assert_eq!(count(&transport, 1), 1);
        assert_eq!(runtime.gateway_user_outbound.len(), 1);
    }

    #[test]
    fn full_member_requests_wait_per_guild_without_blocking_targeted_requests_or_voice() {
        let mut runtime = runtime();
        let first = r#"{"op":8,"d":{"guild_id":"10","query":"","limit":0}}"#;
        let padded = r#"{"op":8,"d":{"guild_id":"010","query":"","limit":0}}"#;
        let targeted = r#"{"op":8,"d":{"guild_id":"10","user_ids":["40"]}}"#;
        let other = r#"{"op":8,"d":{"guild_id":"11","query":"","limit":0}}"#;
        for payload in [first, padded, targeted, other] {
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7OK
            );
        }
        voice(&mut runtime);
        let mut transport = MockGatewayTransport::default();
        for _ in 0..5 {
            assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
        }
        assert_eq!(count(&transport, 8), 3);
        assert_eq!(count(&transport, 4), 1);
        assert_eq!(runtime.gateway_user_outbound.len(), 1);
        assert_eq!(transport.sent_texts()[0], first);
        assert_eq!(transport.sent_texts()[1], targeted);
        assert_eq!(transport.sent_texts()[2], other);
        assert_eq!(pump(&mut runtime, &mut transport, 29_999).status, D7OK);
        assert_eq!(count(&transport, 8), 3);
        assert_eq!(pump(&mut runtime, &mut transport, 30_000).status, D7OK);
        assert_eq!(count(&transport, 8), 4);
        assert!(runtime.gateway_user_outbound.is_empty());
    }

    #[test]
    fn discord_rate_limit_notifications_delay_only_the_affected_requests() {
        let mut runtime = runtime();
        runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"RATE_LIMITED","d":{"opcode":8,"retry_after":1.5001,"meta":{"guild_id":"10","nonce":"private-request"}}}"#, 0).unwrap();
        for payload in [
            r#"{"op":8,"d":{"guild_id":"10","query":"","limit":0}}"#,
            r#"{"op":8,"d":{"guild_id":"11","query":"","limit":0}}"#,
            r#"{"op":8,"d":{"guild_id":"10","user_ids":["40"]}}"#,
        ] {
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7OK
            );
        }
        voice(&mut runtime);
        let mut transport = MockGatewayTransport::default();
        for _ in 0..3 {
            assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
        }
        assert_eq!(count(&transport, 8), 2);
        assert!(transport.sent_texts()[0].contains(r#""guild_id":"11""#));
        assert_eq!(count(&transport, 4), 1);
        assert_eq!(runtime.gateway_user_outbound.len(), 1);
        transport.push_incoming(r#"{"op":1,"d":null}"#);
        assert!(runtime.pump_gateway_once(&mut transport, 0).heartbeat_sent);
        runtime.ingest_gateway_json(r#"{"op":0,"s":2,"t":"RATE_LIMITED","d":{"opcode":8,"retry_after":1.5001,"meta":{"guild_id":"10","nonce":"private-request"}}}"#, 1000).unwrap();
        assert_eq!(pump(&mut runtime, &mut transport, 1500).status, D7OK);
        assert_eq!(count(&transport, 8), 2);
        assert_eq!(pump(&mut runtime, &mut transport, 1501).status, D7OK);
        assert_eq!(count(&transport, 8), 3);
    }

    #[test]
    fn full_member_cooldowns_charge_attempted_sends_and_leave_locally_blocked_requests_uncharged() {
        for send_failure in [false, true] {
            let mut runtime = runtime();
            let payload = r#"{"op":8,"d":{"guild_id":"10","query":"","limit":0}}"#;
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7OK
            );
            let mut transport = MockGatewayTransport::default();
            if send_failure {
                transport.set_send_error(GatewayTransportError::SendFailed);
            } else {
                while runtime
                    .gateway_outbound
                    .check_and_record(GatewaySendKind::Other, 1, 0)
                    .allowed
                {}
            }
            assert_eq!(
                pump(&mut runtime, &mut transport, 0).status,
                if send_failure { D7NET } else { D7OK }
            );
            assert!(transport.sent_texts().is_empty());
            runtime.gateway_outbound.reset_connection();
            if send_failure {
                assert!(runtime.gateway_user_outbound.is_empty());
                assert_eq!(
                    call(
                        &mut runtime,
                        "GWS",
                        vec![Field::percent("payload", payload)]
                    )
                    .status,
                    D7OK
                );
            }
            let mut transport = MockGatewayTransport::default();
            assert_eq!(pump(&mut runtime, &mut transport, 1).status, D7OK);
            if send_failure {
                assert!(transport.sent_texts().is_empty());
                assert_eq!(pump(&mut runtime, &mut transport, 30_000).status, D7OK);
            }
            assert_eq!(transport.sent_texts(), &[payload.to_string()]);
            assert!(runtime.gateway_user_outbound.is_empty());
        }
    }

    #[test]
    fn unscoped_legacy_and_unsequenced_notifications_apply_opcode_cooldowns() {
        for notification in [
            r#"{"op":0,"t":"RATE_LIMITED","d":{"opcode":255,"retry_after":1.5,"meta":{}}}"#,
            r#"{"op":0,"s":null,"t":"GATEWAY_RATE_LIMITED","d":{"opcode":255,"retry_ms":1500}}"#,
        ] {
            let mut runtime = runtime();
            runtime.ingest_gateway_json(notification, 0).unwrap();
            let payload = r#"{"op":255,"d":null}"#;
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7OK
            );
            voice(&mut runtime);
            let mut transport = MockGatewayTransport::default();
            assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
            assert_eq!(count(&transport, 4), 1);
            assert_eq!(count(&transport, 255), 0);
            runtime.gateway_outbound.reset_connection();
            assert_eq!(pump(&mut runtime, &mut transport, 1499).status, D7OK);
            assert_eq!(count(&transport, 255), 0);
            assert_eq!(pump(&mut runtime, &mut transport, 1500).status, D7OK);
            assert_eq!(count(&transport, 255), 1);
        }
    }

    #[test]
    fn malformed_known_gateway_data_never_enters_the_outbound_queue() {
        let mut runtime = runtime();
        for payload in [
            r#"{"op":3,"d":null}"#,
            r#"{"op":3,"d":{"since":null,"activities":[],"status":"online","afk":0}}"#,
            r#"{"op":3,"d":{"since":-1,"activities":[],"status":"online","afk":false}}"#,
            r#"{"op":3,"d":{"since":null,"activities":{},"status":"online","afk":false}}"#,
            r#"{"op":3,"d":{"since":null,"activities":[{}],"status":"online","afk":false}}"#,
            r#"{"op":4,"d":{"guild_id":"10","channel_id":"20","self_mute":1,"self_deaf":false}}"#,
            r#"{"op":4,"d":{"guild_id":"10","channel_id":"invalid","self_mute":false,"self_deaf":false}}"#,
            r#"{"op":8,"d":{"guild_id":"10","query":""}}"#,
            r#"{"op":8,"d":{"guild_id":"10","query":"","limit":-1}}"#,
            r#"{"op":8,"d":{"guild_id":"10","query":"","limit":0,"user_ids":["40"]}}"#,
            r#"{"op":8,"d":{"guild_id":"10","user_ids":[]}}"#,
            r#"{"op":8,"d":{"guild_id":"10","user_ids":["40"],"presences":"true"}}"#,
            r#"{"op":31,"d":{"guild_ids":"10"}}"#,
            r#"{"op":43,"d":{"guild_id":"10","fields":[false]}}"#,
        ] {
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7ARG,
                "{payload}"
            );
            assert!(runtime.gateway_user_outbound.is_empty());
        }
        assert_eq!(
            call(
                &mut runtime,
                "PRS",
                vec![Field::percent(
                    "payload",
                    r#"{"since":null,"activities":[],"status":"online","afk":0}"#
                )]
            )
            .status,
            D7ARG
        );
        let mut transport = MockGatewayTransport::default();
        assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
        assert!(transport.sent_texts().is_empty());
    }

    #[test]
    fn valid_known_gateway_data_preserves_optional_and_future_fields() {
        for payload in [
            r#"{"op":3,"d":{"since":null,"activities":[{"name":"音楽 🎵","type":2,"state":"playing","future":true}],"status":"online","afk":false,"future":{}}}"#,
            r#"{"op":3,"d":{"since":0,"activities":[],"status":"offline","afk":true}}"#,
            r#"{"op":4,"d":{"guild_id":"10","channel_id":null,"self_mute":false,"self_deaf":true,"future":[]}}"#,
            r#"{"op":8,"d":{"guild_id":"10","query":"","limit":0,"presences":false,"nonce":"request"}}"#,
            r#"{"op":8,"d":{"guild_id":"10","user_ids":"40","presences":true,"future":1}}"#,
            r#"{"op":43,"d":{"guild_id":"10","fields":["status","future_field"]}}"#,
        ] {
            let mut runtime = runtime();
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7OK,
                "{payload}"
            );
            let mut transport = MockGatewayTransport::default();
            assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
            assert_eq!(transport.sent_texts(), &[payload.to_string()]);
        }
    }

    #[test]
    fn member_request_limits_use_member_counts_and_utf8_nonce_bytes() {
        for (count, nonce, expected) in [
            (100, "a".repeat(32), D7OK),
            (101, "a".repeat(32), D7ARG),
            (1, "あ".repeat(10), D7OK),
            (1, "あ".repeat(11), D7ARG),
        ] {
            let mut runtime = runtime();
            let users = (1..=count)
                .map(|id| format!("\"{id}\""))
                .collect::<Vec<_>>()
                .join(",");
            let payload = format!(
                r#"{{"op":8,"d":{{"guild_id":"10","user_ids":[{users}],"nonce":"{nonce}"}}}}"#
            );
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                expected
            );
            assert_eq!(
                runtime.gateway_user_outbound.len(),
                usize::from(expected == D7OK)
            );
        }
    }

    #[test]
    fn known_and_future_application_opcodes_keep_their_payloads() {
        let mut runtime = runtime();
        let mut transport = MockGatewayTransport::default();
        let payloads = [
            r#"{"op":8,"d":{"guild_id":"10","user_ids":["40"]}}"#,
            r#"{"op":31,"d":{"guild_ids":["10"]}}"#,
            r#"{"op":43,"d":{"guild_id":"10","fields":["status"]}}"#,
            r#"{"op":255,"d":null,"future":"unchanged"}"#,
        ];
        for payload in payloads {
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7OK
            );
            assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
        }
        assert_eq!(transport.sent_texts(), payloads.map(str::to_string));
    }

    #[test]
    fn raw_gateway_envelopes_require_integer_opcodes_and_data_without_limiting_future_opcodes() {
        let mut runtime = runtime();
        for payload in [
            "{}",
            r#"{"op":3}"#,
            r#"{"op":3.0,"d":{}}"#,
            r#"{"op":3e0,"d":{}}"#,
            r#"{"op":30e-1,"d":{}}"#,
            r#"{"op":"3","d":{}}"#,
            r#"{"op":true,"d":{}}"#,
            r#"{"op":null,"d":{}}"#,
            r#"{"op":-3,"d":{}}"#,
            r#"{"op":1,"d":{}}"#,
            r#"{"op":1,"d":[]}"#,
            r#"{"op":1,"d":-1}"#,
            r#"{"op":1,"d":"1"}"#,
            r#"{"op":1,"d":1.0}"#,
            r#"{"op":1,"d":true}"#,
            r#"{"op":1,"d":9223372036854775808}"#,
        ] {
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7ARG,
                "{payload}"
            );
            assert!(runtime.gateway_user_outbound.is_empty());
        }
        for payload in [r#"{"op":1,"d":null}"#, r#"{"op":255,"d":{}}"#] {
            assert_eq!(
                call(
                    &mut runtime,
                    "GWS",
                    vec![Field::percent("payload", payload)]
                )
                .status,
                D7OK
            );
        }
        assert_eq!(runtime.gateway_user_outbound.len(), 2);
    }

    #[test]
    fn typed_and_raw_presence_updates_share_a_budget_without_blocking_voice_or_heartbeats() {
        let mut runtime = runtime();
        for index in 0..6 {
            if index % 2 == 0 {
                presence(&mut runtime);
            } else {
                assert_eq!(
                    call(
                        &mut runtime,
                        "GWS",
                        vec![Field::percent(
                            "payload",
                            format!(r#"{{"op":3,"d":{PRESENCE}}}"#)
                        )]
                    )
                    .status,
                    D7OK
                );
            }
        }
        voice(&mut runtime);
        let mut transport = MockGatewayTransport::default();
        for _ in 0..6 {
            assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
        }
        assert_eq!(count(&transport, 3), 5);
        assert_eq!(count(&transport, 4), 1);
        assert_eq!(runtime.gateway_user_outbound.len(), 1);
        transport.push_incoming(r#"{"op":1,"d":null}"#);
        let result = runtime.pump_gateway_once(&mut transport, 19_999);
        assert!(result.heartbeat_sent);
        assert_eq!(count(&transport, 3), 5);
        assert_eq!(count(&transport, 1), 1);
        assert_eq!(pump(&mut runtime, &mut transport, 20_000).status, D7OK);
        assert_eq!(count(&transport, 3), 6);
        assert!(runtime.gateway_user_outbound.is_empty());
    }

    #[test]
    fn replacement_sessions_cannot_receive_payloads_queued_for_the_old_session() {
        for restore in [false, true] {
            let mut runtime = runtime();
            presence(&mut runtime);
            voice(&mut runtime);
            for _ in 2..MAX_GATEWAY_USER_OUTBOUND_QUEUE {
                presence(&mut runtime);
            }
            let mut transport = MockGatewayTransport::default();
            if restore {
                let session =
                    GatewayResumeSession::new("new", "wss://new", 5, Some("99".into())).unwrap();
                runtime.restore_gateway_resume_session(session).unwrap();
                assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
                assert!(transport.sent_texts().is_empty());
                runtime
                    .ingest_gateway_json(r#"{"op":0,"s":6,"t":"RESUMED","d":{}}"#, 1)
                    .unwrap();
            } else {
                runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"new","resume_gateway_url":"wss://new","user":{"id":"99"}}}"#, 0).unwrap();
            }
            let fresh = r#"{"op":8,"d":{"guild_id":"10","query":"","limit":1}}"#;
            assert_eq!(
                call(&mut runtime, "GWS", vec![Field::percent("payload", fresh)]).status,
                D7OK
            );
            assert_eq!(pump(&mut runtime, &mut transport, 2).status, D7OK);
            assert_eq!(transport.sent_texts(), &[fresh.to_string()]);
            assert!(runtime.gateway_user_outbound.is_empty());
        }
    }

    #[test]
    fn normal_resume_waits_for_completion_and_preserves_queued_payload_order() {
        let mut runtime = runtime();
        presence(&mut runtime);
        voice(&mut runtime);
        let session = GatewayResumeSession::new("old", "wss://resume", 1, None).unwrap();
        runtime.restore_gateway_resume_session(session).unwrap();
        let mut transport = MockGatewayTransport::default();
        assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
        assert!(transport.sent_texts().is_empty());
        assert_eq!(runtime.gateway_user_outbound.len(), 2);
        transport.push_incoming(r#"{"op":0,"s":2,"t":"RESUMED","d":{}}"#);
        assert_eq!(runtime.pump_gateway_once(&mut transport, 1).status, D7OK);
        assert!(transport.sent_texts().is_empty());
        assert_eq!(pump(&mut runtime, &mut transport, 2).status, D7OK);
        assert_eq!(pump(&mut runtime, &mut transport, 3).status, D7OK);
        assert_eq!(
            transport
                .sent_texts()
                .iter()
                .map(|payload| parse_json(payload)
                    .unwrap()
                    .get("op")
                    .unwrap()
                    .as_i64()
                    .unwrap())
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert!(runtime.gateway_user_outbound.is_empty());
    }

    #[test]
    fn an_ambiguous_send_failure_does_not_replay_the_attempted_payload() {
        struct AmbiguousTransport {
            sent: Vec<String>,
            fail: bool,
        }
        impl GatewayTransport for AmbiguousTransport {
            fn connect(&mut self, _: &str) -> Result<(), GatewayTransportError> {
                Ok(())
            }
            fn send_text(&mut self, payload: &str) -> Result<(), GatewayTransportError> {
                self.sent.push(payload.to_string());
                if std::mem::take(&mut self.fail) {
                    Err(GatewayTransportError::SendFailed)
                } else {
                    Ok(())
                }
            }
            fn receive_text(&mut self) -> Result<Option<String>, GatewayTransportError> {
                Err(GatewayTransportError::NoFrame)
            }
        }
        let mut runtime = runtime();
        presence(&mut runtime);
        voice(&mut runtime);
        let mut transport = AmbiguousTransport {
            sent: vec![],
            fail: true,
        };
        assert_eq!(runtime.pump_gateway_once(&mut transport, 0).status, D7NET);
        assert_eq!(runtime.gateway_user_outbound.len(), 1);
        assert_eq!(runtime.pump_gateway_once(&mut transport, 1).status, D7OK);
        assert_eq!(
            transport
                .sent
                .iter()
                .map(|payload| parse_json(payload)
                    .unwrap()
                    .get("op")
                    .unwrap()
                    .as_i64()
                    .unwrap())
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert!(runtime.gateway_user_outbound.is_empty());
    }

    #[test]
    fn a_failed_audit_does_not_enqueue_a_request_or_discard_existing_requests() {
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let path = std::env::temp_dir().join(format!(
            "dfor77-gateway-audit-failure-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir(&path).unwrap();
        let fixture = Fixture(path);
        let audit = fixture.0.join("audit.jsonl");
        let mut runtime = runtime();
        voice(&mut runtime);
        runtime.enable_audit(audit.to_str().unwrap()).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&audit)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        for suffix in [".1", ".2"] {
            let archive = fixture.0.join(format!("audit.jsonl{suffix}"));
            std::fs::create_dir(&archive).unwrap();
            std::fs::write(archive.join("occupied"), b"test fixture").unwrap();
        }
        let response = call(
            &mut runtime,
            "PRS",
            vec![Field::percent("payload", PRESENCE)],
        );
        assert_eq!(response.status, D7INT);
        assert_eq!(runtime.gateway_user_outbound.len(), 1);
        let mut transport = MockGatewayTransport::default();
        assert_eq!(pump(&mut runtime, &mut transport, 0).status, D7OK);
        assert_eq!(count(&transport, 3), 0);
        assert_eq!(count(&transport, 4), 1);
        assert!(runtime.gateway_user_outbound.is_empty());
    }
}
