use super::*;
use crate::snapshot::webhook::WebhookMessageTarget;

impl DforRuntime {
    pub(super) fn observe_rest_snapshots(&mut self, outcome: &RestOutcome) {
        let Some(request) = outcome.request.as_ref() else {
            return;
        };
        let changed = request.method != HttpMethod::Get
            && matches!(
                outcome.commit_state,
                crate::rest::CommitState::Committed
                    | crate::rest::CommitState::SentNoResponse
                    | crate::rest::CommitState::Unknown
            );
        if !changed
            && !(request.method == HttpMethod::Get
                && (outcome.commit_state == crate::rest::CommitState::Committed
                    && outcome.status == D7OK
                    || outcome.commit_state == crate::rest::CommitState::Rejected
                        && matches!(outcome.http_status, 403 | 404)
                        && matches!(outcome.status, D7FOR | D7NFD)))
        {
            return;
        }
        let Some(path) = crate::rest::path::RestPath::parse(&request.path) else {
            return;
        };
        let parts = path.parts();
        let ["api", "v10", collection, id, tail @ ..] = parts.as_slice() else {
            return;
        };
        let now_ms = now_ms_u64();
        if *collection == "webhooks" {
            if let Some(id) = canonical_mutation_id(id) {
                self.observe_webhook_message(&id, tail, request, outcome, now_ms);
            }
            return;
        }
        if request.method == HttpMethod::Get {
            self.observe_resource_read(collection, id, tail, outcome, now_ms);
            return;
        }
        if let ("users", "@me", ["guilds", guild_id]) = (*collection, *id, tail) {
            if request.method == HttpMethod::Delete {
                if let Some(guild_id) = canonical_mutation_id(guild_id) {
                    self.observe_guild_mutation(
                        &guild_id,
                        &[],
                        request,
                        outcome.commit_state,
                        now_ms,
                    );
                }
            }
            return;
        }
        let Some(id) = canonical_mutation_id(id) else {
            return;
        };
        match *collection {
            "guilds" => {
                self.observe_guild_mutation(&id, tail, request, outcome.commit_state, now_ms)
            }
            "channels" => self.observe_channel_mutation(
                &id,
                tail,
                request,
                outcome.commit_state,
                outcome.response_body.as_deref(),
                now_ms,
            ),
            _ => {}
        }
    }

    fn observe_webhook_message(
        &mut self,
        webhook_id: &str,
        path: &[&str],
        request: &RestRequest,
        outcome: &RestOutcome,
        now_ms: u64,
    ) {
        let (token, message_id) = match path {
            [token, "messages", message_id]
                if matches!(
                    request.method,
                    HttpMethod::Get | HttpMethod::Patch | HttpMethod::Delete
                ) =>
            {
                (*token, Some(*message_id))
            }
            [token] if request.method == HttpMethod::Post => (*token, None),
            _ => return,
        };
        let original = message_id == Some("@original");
        let numeric_id = message_id.and_then(canonical_mutation_id);
        if message_id.is_some() && !original && numeric_id.is_none() {
            return;
        }
        let targets = if original {
            self.gateway
                .snapshot_mut()
                .webhook_original_target(webhook_id, token, now_ms)
                .into_iter()
                .collect::<Vec<_>>()
        } else if let Some(id) = &numeric_id {
            self.gateway.snapshot().webhook_message_targets(id)
        } else {
            Vec::new()
        };
        let confirmed = outcome.commit_state == crate::rest::CommitState::Committed;
        if request.method != HttpMethod::Delete && (confirmed || request.method == HttpMethod::Get)
        {
            let response = outcome
                .response_body
                .as_deref()
                .and_then(|body| parse_json(body).ok());
            if let Some(response) = response {
                let target = response
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .and_then(canonical_mutation_id)
                    .zip(
                        response
                            .get("channel_id")
                            .and_then(JsonValue::as_str)
                            .and_then(canonical_mutation_id),
                    )
                    .map(|(message_id, channel_id)| WebhookMessageTarget {
                        channel_id,
                        message_id,
                    });
                if let Some(target) = target.filter(|target| {
                    numeric_id
                        .as_ref()
                        .is_none_or(|id| *id == target.message_id)
                        && targets.iter().all(|known| {
                            known.message_id.trim_start_matches('0') == target.message_id
                                && known.channel_id.trim_start_matches('0') == target.channel_id
                        })
                }) {
                    let applied = self
                        .gateway
                        .snapshot_mut()
                        .apply_webhook_message_response(&response, &target, now_ms);
                    if applied {
                        if original {
                            self.gateway
                                .snapshot_mut()
                                .remember_webhook_original(webhook_id, token, target, now_ms);
                        }
                        return;
                    }
                }
            }
        }
        if request.method == HttpMethod::Get {
            return;
        }
        if targets.is_empty() && numeric_id.is_none() {
            self.gateway
                .snapshot_mut()
                .invalidate_unresolved_webhook_messages(webhook_id, now_ms);
        }
        for target in targets {
            if request.method == HttpMethod::Delete && confirmed {
                self.gateway.snapshot_mut().tombstone_message(
                    &target.channel_id,
                    &target.message_id,
                    now_ms,
                );
            } else {
                self.gateway.snapshot_mut().invalidate_message(
                    &target.channel_id,
                    &target.message_id,
                    now_ms,
                );
            }
        }
    }

    fn observe_guild_mutation(
        &mut self,
        guild_id: &str,
        path: &[&str],
        request: &RestRequest,
        state: crate::rest::CommitState,
        now_ms: u64,
    ) {
        match path {
            [] => {
                if request.method == HttpMethod::Delete
                    && state == crate::rest::CommitState::Committed
                {
                    self.gateway
                        .snapshot_mut()
                        .tombstone_guild(guild_id, now_ms);
                } else {
                    self.gateway
                        .snapshot_mut()
                        .invalidate_guild(guild_id, now_ms);
                    if request.method == HttpMethod::Delete {
                        self.gateway
                            .snapshot_mut()
                            .invalidate_guild_roles(guild_id, now_ms);
                        self.gateway
                            .snapshot_mut()
                            .invalidate_guild_members(guild_id, now_ms);
                    }
                }
                self.invalidate_event_permissions(Some(guild_id), None);
            }
            ["roles", ..] => {
                self.gateway
                    .snapshot_mut()
                    .invalidate_guild_roles(guild_id, now_ms);
                if request.method == HttpMethod::Delete {
                    self.gateway
                        .snapshot_mut()
                        .invalidate_guild_members(guild_id, now_ms);
                }
                self.invalidate_event_permissions(Some(guild_id), None);
            }
            ["members" | "bans", user, ..] => {
                let destructive = matches!(
                    (path, request.method),
                    (["members", _], HttpMethod::Delete)
                        | (["bans", _], HttpMethod::Put | HttpMethod::Post)
                );
                let bot_user_id = self.known_bot_user_id();
                let user_id = if *user == "@me" {
                    bot_user_id.clone()
                } else {
                    canonical_mutation_id(user)
                };
                if let Some(user_id) = user_id {
                    if destructive {
                        self.gateway
                            .snapshot_mut()
                            .invalidate_owner_membership(guild_id, &user_id, now_ms);
                    }
                    self.gateway
                        .snapshot_mut()
                        .invalidate_member(guild_id, &user_id, now_ms);
                    self.invalidate_event_permissions(Some(guild_id), Some(&user_id));
                    if bot_user_id.is_none() || bot_user_id.as_deref() == Some(user_id.as_str()) {
                        self.gateway
                            .snapshot_mut()
                            .clear_private_thread_access_for_guild(guild_id, now_ms);
                    }
                } else if *user == "@me" {
                    if destructive {
                        self.gateway
                            .snapshot_mut()
                            .invalidate_guild(guild_id, now_ms);
                    }
                    self.gateway
                        .snapshot_mut()
                        .invalidate_guild_members(guild_id, now_ms);
                    self.gateway
                        .snapshot_mut()
                        .clear_private_thread_access_for_guild(guild_id, now_ms);
                    self.invalidate_event_permissions(Some(guild_id), None);
                }
            }
            ["channels"] if request.method == HttpMethod::Patch => {
                let ids = request
                    .body
                    .as_deref()
                    .and_then(|body| parse_json(body).ok())
                    .and_then(|body| {
                        body.as_array()?
                            .iter()
                            .map(|row| canonical_mutation_id(row.get("id")?.as_str()?))
                            .collect::<Option<Vec<_>>>()
                    });
                if let Some(ids) = ids {
                    for id in ids {
                        self.gateway.snapshot_mut().invalidate_channel(&id, now_ms);
                    }
                } else {
                    self.gateway
                        .snapshot_mut()
                        .invalidate_guild_channels(guild_id, now_ms);
                }
            }
            _ => {}
        }
    }

    fn observe_channel_mutation(
        &mut self,
        channel_id: &str,
        path: &[&str],
        request: &RestRequest,
        state: crate::rest::CommitState,
        response_body: Option<&str>,
        now_ms: u64,
    ) {
        match path {
            ["messages", message_id] if request.method == HttpMethod::Patch => {
                if let Some(message_id) = canonical_mutation_id(message_id) {
                    let response = (state == crate::rest::CommitState::Committed)
                        .then_some(response_body)
                        .flatten()
                        .and_then(|body| parse_json(body).ok());
                    if let Some(response) = response {
                        self.gateway.snapshot_mut().apply_rest_message_response(
                            &response,
                            channel_id,
                            &message_id,
                            now_ms,
                        );
                    } else {
                        self.gateway.snapshot_mut().invalidate_message(
                            channel_id,
                            &message_id,
                            now_ms,
                        );
                    }
                }
            }
            ["messages", message_id] if request.method == HttpMethod::Delete => {
                if let Some(message_id) = canonical_mutation_id(message_id) {
                    self.observe_message_deletion(channel_id, &message_id, state, now_ms);
                }
            }
            ["messages", "bulk-delete"] if request.method == HttpMethod::Post => {
                let ids = request
                    .body
                    .as_deref()
                    .and_then(|body| parse_json(body).ok())
                    .and_then(|body| {
                        body.get("messages")?
                            .as_array()?
                            .iter()
                            .map(|id| canonical_mutation_id(id.as_str()?))
                            .collect::<Option<Vec<_>>>()
                    });
                if let Some(ids) = ids {
                    for id in ids {
                        self.observe_message_deletion(channel_id, &id, state, now_ms);
                    }
                } else {
                    self.gateway
                        .snapshot_mut()
                        .invalidate_channel_messages(channel_id, now_ms);
                }
            }
            _ => {}
        }
        let membership = match path {
            ["thread-members", user] => {
                let bot_user_id = self.known_bot_user_id();
                *user == "@me"
                    || bot_user_id.is_none()
                    || canonical_mutation_id(user).as_deref() == bot_user_id.as_deref()
            }
            _ => false,
        };
        if path.is_empty()
            && request.method == HttpMethod::Delete
            && state == crate::rest::CommitState::Committed
        {
            self.gateway
                .snapshot_mut()
                .clear_private_thread_access_for_channel(channel_id, now_ms);
            self.gateway
                .snapshot_mut()
                .tombstone_channel(channel_id, now_ms);
        } else if path.is_empty() || matches!(path, ["permissions", ..]) || membership {
            self.gateway
                .snapshot_mut()
                .invalidate_channel(channel_id, now_ms);
        }
        if path.is_empty() || matches!(path, ["permissions", ..]) {
            self.gateway
                .snapshot_mut()
                .mark_channel_rest_result(channel_id, now_ms);
        }
    }

    fn observe_message_deletion(
        &mut self,
        channel_id: &str,
        message_id: &str,
        state: crate::rest::CommitState,
        now_ms: u64,
    ) {
        if state == crate::rest::CommitState::Committed {
            self.gateway
                .snapshot_mut()
                .tombstone_message(channel_id, message_id, now_ms);
        } else {
            self.gateway
                .snapshot_mut()
                .invalidate_message(channel_id, message_id, now_ms);
        }
    }

    pub(super) fn known_bot_user_id(&self) -> Option<String> {
        self.gateway
            .machine()
            .bot_user_id()
            .or(self.bot_user_id_cache.as_deref())
            .and_then(canonical_mutation_id)
    }
}

fn canonical_mutation_id(value: &str) -> Option<String> {
    crate::rest::path::canonical_id(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::CommitState;

    fn runtime(backend: MockRestBackend) -> DforRuntime {
        let mut runtime = DforRuntime::with_mock_rest_backend(false, false, backend);
        let now = now_ms_u64();
        runtime.bot_user_id_cache = Some("99".into());
        let store = runtime.gateway.snapshot_mut();
        let mut guild = GuildSnapshot::new("10", false, now);
        guild.owner_id = Some("99".into());
        store.upsert_guild(guild);
        store.upsert_guild(GuildSnapshot::new("11", false, now));
        store.upsert_guild_roles(GuildRolesSnapshot::new(
            "10",
            vec![crate::snapshot::RoleSnapshot {
                guild_id: "10".into(),
                role_id: "10".into(),
                permissions: PermissionSet::zero(),
                position: 0,
            }],
            now,
        ));
        store.upsert_member(MemberSnapshot::new("10", "99", vec![], now));
        store.upsert_member(MemberSnapshot::new("10", "40", vec![], now));
        for (guild, channel, message) in [
            ("10", "20", "30"),
            ("10", "20", "31"),
            ("10", "21", "32"),
            ("11", "22", "33"),
        ] {
            let mut snapshot = MessageSnapshot::new(guild, channel, message, "40", now);
            snapshot.image_attachment_count = 2;
            store.upsert_message(snapshot);
            let mut channel = ChannelSnapshot::new(guild, channel, 0, now);
            channel.has_permission_overwrites = true;
            store.upsert_channel(channel);
        }
        call(&mut runtime, "INI", vec![]);
        runtime
    }

    fn call(runtime: &mut DforRuntime, verb: &str, fields: Vec<Field>) -> Response {
        runtime.handle(Request {
            id: 1,
            verb: verb.into(),
            fields,
        })
    }

    fn outcome(
        method: HttpMethod,
        path: &str,
        body: Option<&str>,
        state: CommitState,
    ) -> RestOutcome {
        let mut outcome = RestPlanner::new().raw_json_request(method, path, body, "");
        assert!(outcome.request.is_some());
        outcome.commit_state = state;
        outcome
    }

    fn search(runtime: &mut DforRuntime) -> Response {
        call(
            runtime,
            "MSF",
            vec![
                Field::raw("guild_id", "10"),
                Field::raw("user_id", "40"),
                Field::raw("base_message_id", "40"),
                Field::raw("before_seconds", "60"),
                Field::raw("after_seconds", "60"),
                Field::raw("min_image_count", "1"),
                Field::raw("limit", "10"),
            ],
        )
    }

    #[test]
    fn uncertain_original_edits_are_not_retried_and_can_be_revalidated_by_get() {
        let mut backend = MockRestBackend::default();
        let body = r#"{"id":"30","channel_id":"20","webhook_id":"50","application_id":"50","author":{"id":"40"},"content":"confirmed","type":0,"attachments":[{"filename":"one.png"}]}"#;
        let mut response = crate::rest::RestResponse::ok();
        response.body = body.into();
        backend.push_response(response.clone());
        backend.push_error(crate::rest::RestTransportError::TimeoutAfterSend);
        backend.push_response(response);
        let mut runtime = runtime(backend);
        let fields = || {
            vec![
                Field::raw("method", "1"),
                Field::raw(
                    "path",
                    "/api/v10/webhooks/50/private-test-token/messages/@original",
                ),
            ]
        };
        assert_eq!(call(&mut runtime, "RST", fields()).status, D7OK);
        assert_eq!(
            call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "4"),
                    Field::raw(
                        "path",
                        "/api/v10/webhooks/50/private-test-token/messages/@original"
                    ),
                    Field::percent("payload", r#"{"attachments":[]}"#)
                ]
            )
            .status,
            D7UNC
        );
        assert_eq!(search(&mut runtime).field("count"), Some("2"));
        assert_eq!(
            call(&mut runtime, "RBD", vec![Field::raw("buf_len", "1024")]).status,
            D7STA
        );
        assert_eq!(call(&mut runtime, "RST", fields()).status, D7OK);
        assert_eq!(search(&mut runtime).field("count"), Some("3"));
        assert_eq!(
            call(&mut runtime, "RBD", vec![Field::raw("buf_len", "1024")]).field("body"),
            Some(body)
        );
        let RuntimeRest::Mock(executor) = &runtime.rest else {
            panic!("expected mock REST")
        };
        assert_eq!(
            executor
                .backend
                .requests()
                .iter()
                .map(|request| request.method)
                .collect::<Vec<_>>(),
            vec![HttpMethod::Get, HttpMethod::Patch, HttpMethod::Get]
        );
    }

    #[test]
    fn webhook_edit_replies_cannot_change_the_cached_resource_identity() {
        for body in [
            "{}",
            r#"{"id":"31","channel_id":"20","webhook_id":"50","author":{"id":"40"},"content":"wrong message","type":0,"attachments":[]}"#,
            r#"{"id":"30","channel_id":"21","webhook_id":"50","author":{"id":"40"},"content":"wrong channel","type":0,"attachments":[]}"#,
            r#"{"id":"30","channel_id":"20","webhook_id":"51","author":{"id":"40"},"content":"wrong origin","type":0,"attachments":[]}"#,
            r#"{"id":"30","channel_id":"20","webhook_id":"50","author":{"id":"41"},"content":"wrong author","type":0,"attachments":[]}"#,
            r#"{"id":"30","channel_id":"20","webhook_id":"50","guild_id":"11","author":{"id":"40"},"content":"wrong guild","type":0,"attachments":[]}"#,
        ] {
            let mut runtime = runtime(MockRestBackend::default());
            runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"id":"30","guild_id":"10","channel_id":"20","webhook_id":"50","type":0,"author":{"id":"40"},"content":"before","attachments":[]}}"#, now_ms_u64()).unwrap();
            let mut edit = outcome(
                HttpMethod::Patch,
                "/api/v10/webhooks/50/private-test-token/messages/30",
                Some("{}"),
                CommitState::Committed,
            );
            edit.response_body = Some(body.into());
            runtime.observe_rest_outcome(&edit);
            let now = now_ms_u64();
            assert_eq!(
                runtime.gateway.snapshot_mut().get_message("20", "30", now),
                SnapshotLookup::Missing,
                "body={body}"
            );
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_message("20", "31", now),
                SnapshotLookup::Found(_)
            ));
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_message("21", "32", now),
                SnapshotLookup::Found(_)
            ));
        }
    }

    #[test]
    fn original_webhook_targets_are_scoped_to_tokens_and_keep_confirmed_deletions() {
        let mut runtime = runtime(MockRestBackend::default());
        for (token, message_id) in [
            ("first-private-token", "30"),
            ("second-private-token", "31"),
        ] {
            let mut read = outcome(
                HttpMethod::Get,
                &format!("/api/v10/webhooks/50/{token}/messages/@original"),
                None,
                CommitState::Committed,
            );
            read.response_body = Some(format!(
                r#"{{"id":"{message_id}","channel_id":"20","webhook_id":"50","application_id":"50","author":{{"id":"40"}},"content":"reply","type":0,"attachments":[{{"filename":"one.png"}},{{"filename":"two.png"}}]}}"#
            ));
            runtime.observe_rest_outcome(&read);
        }
        runtime.observe_rest_outcome(&outcome(
            HttpMethod::Delete,
            "/api/v10/webhooks/050/first-private-token/messages/%40original",
            None,
            CommitState::Committed,
        ));
        let now = now_ms_u64();
        assert_eq!(
            runtime.gateway.snapshot_mut().get_message("20", "30", now),
            SnapshotLookup::Tombstone
        );
        assert!(matches!(
            runtime.gateway.snapshot_mut().get_message("20", "31", now),
            SnapshotLookup::Found(_)
        ));
        runtime.observe_rest_outcome(&outcome(
            HttpMethod::Patch,
            "/api/v10/webhooks/50/second-private-token/messages/@original",
            Some("{}"),
            CommitState::Unknown,
        ));
        assert_eq!(
            runtime.gateway.snapshot_mut().get_message("20", "31", now),
            SnapshotLookup::Missing
        );
        assert_eq!(
            runtime.gateway.snapshot_mut().get_message("20", "30", now),
            SnapshotLookup::Tombstone
        );
        assert!(matches!(
            runtime.gateway.snapshot_mut().get_message("21", "32", now),
            SnapshotLookup::Found(_)
        ));
        let debug = format!("{:?}", runtime.gateway.snapshot());
        assert!(!debug.contains("first-private-token"));
        assert!(!debug.contains("second-private-token"));
    }

    #[test]
    fn unresolved_originals_preserve_known_ordinary_messages_and_other_webhook_scopes() {
        for method in [HttpMethod::Patch, HttpMethod::Delete] {
            let mut runtime = runtime(MockRestBackend::default());
            for (seq, id, extra) in [
                (1, "30", r#""webhook_id":"50","application_id":"50","#),
                (2, "31", r#""webhook_id":"51","application_id":"50","#),
                (3, "32", ""),
                (4, "33", r#""webhook_id":"52","application_id":"52","#),
            ] {
                runtime.ingest_gateway_json(&format!(r#"{{"op":0,"s":{seq},"t":"MESSAGE_CREATE","d":{{"id":"{id}","guild_id":"10","channel_id":"20",{extra}"type":0,"author":{{"id":"40"}},"content":"cached","attachments":[]}}}}"#), now_ms_u64()).unwrap();
            }
            runtime.observe_rest_outcome(&outcome(
                method,
                "/api/v10/webhooks/50/unresolved-private-token/messages/@original",
                if method == HttpMethod::Patch {
                    Some("{}")
                } else {
                    None
                },
                CommitState::Committed,
            ));
            let now = now_ms_u64();
            for id in ["30", "31"] {
                assert_eq!(
                    runtime.gateway.snapshot_mut().get_message("20", id, now),
                    SnapshotLookup::Missing
                );
            }
            for id in ["32", "33"] {
                assert!(matches!(
                    runtime.gateway.snapshot_mut().get_message("20", id, now),
                    SnapshotLookup::Found(_)
                ));
            }
        }
    }

    #[test]
    fn component_originals_can_edit_ordinary_bot_messages_and_application_owned_webhooks() {
        for webhook in ["", r#""webhook_id":"51","application_id":"50","#] {
            let mut runtime = runtime(MockRestBackend::default());
            let now = now_ms_u64();
            runtime.ingest_gateway_json(&format!(r#"{{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{{"id":"30","guild_id":"10","channel_id":"20",{webhook}"type":0,"author":{{"id":"40","bot":true}},"content":"before","attachments":[{{"filename":"one.png"}}]}}}}"#), now).unwrap();
            let mut edit = outcome(
                HttpMethod::Patch,
                "/api/v10/webhooks/50/component-private-token/messages/@original",
                Some(r#"{"attachments":[]}"#),
                CommitState::Committed,
            );
            edit.response_body = Some(format!(
                r#"{{"id":"30","channel_id":"20",{webhook}"type":0,"author":{{"id":"40","bot":true}},"content":"component edited","attachments":[]}}"#
            ));
            runtime.observe_rest_outcome(&edit);
            let SnapshotLookup::Found(message) =
                runtime.gateway.snapshot_mut().get_message("20", "30", now)
            else {
                panic!("expected component message");
            };
            assert_eq!(message.content, "component edited");
            assert_eq!(message.image_attachment_count, 0);
            runtime.observe_rest_outcome(&outcome(
                HttpMethod::Delete,
                "/api/v10/webhooks/50/component-private-token/messages/@original",
                None,
                CommitState::Committed,
            ));
            assert_eq!(
                runtime.gateway.snapshot_mut().get_message("20", "30", now),
                SnapshotLookup::Tombstone
            );
        }
    }

    #[test]
    fn webhook_message_edits_refresh_searches_from_the_confirmed_response() {
        let mut runtime = runtime(MockRestBackend::default());
        assert_eq!(search(&mut runtime).field("count"), Some("3"));
        let mut mutation = outcome(
            HttpMethod::Patch,
            "/api/v10/webhooks/050/private-test-token/messages/%33%30?thread_id=20",
            Some(r#"{"attachments":[]}"#),
            CommitState::Committed,
        );
        mutation.response_body = Some(r#"{"id":"30","channel_id":"20","webhook_id":"50","author":{"id":"40"},"content":"webhook edit","type":0,"attachments":[]}"#.into());
        runtime.observe_rest_outcome(&mutation);
        let SnapshotLookup::Found(snapshot) =
            runtime
                .gateway
                .snapshot_mut()
                .get_message("20", "30", now_ms_u64())
        else {
            panic!("expected edited message");
        };
        assert_eq!(snapshot.content, "webhook edit");
        assert_eq!(snapshot.image_attachment_count, 0);
        assert_eq!(
            call(&mut runtime, "MSI", vec![Field::raw("index", "1")]).field("image_count"),
            Some("2")
        );
        assert_eq!(search(&mut runtime).field("count"), Some("2"));
    }

    #[test]
    fn webhook_message_deletions_follow_commit_state_without_touching_other_messages() {
        for state in [
            CommitState::NotSent,
            CommitState::Rejected,
            CommitState::Committed,
            CommitState::SentNoResponse,
            CommitState::Unknown,
        ] {
            let mut runtime = runtime(MockRestBackend::default());
            runtime.observe_rest_outcome(&outcome(
                HttpMethod::Delete,
                "/api/v10/webhooks/50/private-test-token/messages/030",
                None,
                state,
            ));
            let now = now_ms_u64();
            let lookup = runtime.gateway.snapshot_mut().get_message("20", "30", now);
            match state {
                CommitState::Committed => assert_eq!(lookup, SnapshotLookup::Tombstone),
                CommitState::SentNoResponse | CommitState::Unknown => {
                    assert_eq!(lookup, SnapshotLookup::Missing)
                }
                _ => assert!(matches!(lookup, SnapshotLookup::Found(_))),
            }
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_message("20", "31", now),
                SnapshotLookup::Found(_)
            ));
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_message("22", "33", now),
                SnapshotLookup::Found(_)
            ));
        }
    }

    #[test]
    fn destructive_owner_membership_writes_require_ownership_revalidation() {
        for (method, path, affects_owner) in [
            (HttpMethod::Delete, "/api/v10/guilds/10/members/99", true),
            (HttpMethod::Put, "/api/v10/guilds/10/bans/99", true),
            (HttpMethod::Post, "/api/v10/guilds/10/bans/99", true),
            (HttpMethod::Delete, "/api/v10/guilds/10/members/40", false),
            (HttpMethod::Patch, "/api/v10/guilds/10/members/99", false),
            (HttpMethod::Delete, "/api/v10/guilds/10/bans/99", false),
        ] {
            for state in [
                CommitState::NotSent,
                CommitState::Rejected,
                CommitState::Committed,
                CommitState::SentNoResponse,
                CommitState::Unknown,
            ] {
                let mut runtime = runtime(MockRestBackend::default());
                runtime.observe_rest_outcome(&outcome(method, path, Some("{}"), state));
                let SnapshotLookup::Found(guild) =
                    runtime.gateway.snapshot_mut().get_guild("10", now_ms_u64())
                else {
                    panic!("expected guild context");
                };
                let changed =
                    affects_owner && !matches!(state, CommitState::NotSent | CommitState::Rejected);
                assert_eq!(
                    guild.complete, !changed,
                    "method={method:?} path={path} state={state:?}"
                );
                assert!(matches!(
                    runtime.gateway.snapshot_mut().get_guild("11", now_ms_u64()),
                    SnapshotLookup::Found(GuildSnapshot { complete: true, .. })
                ));
            }
        }
    }

    #[test]
    fn message_edits_use_the_confirmed_response_and_preserve_existing_search_indices() {
        let mut backend = MockRestBackend::default();
        let body = r#"{"id":"30","channel_id":"20","author":{"id":"40"},"content":"server edit","type":0,"attachments":[]}"#;
        let mut response = crate::rest::RestResponse::ok();
        response.body = body.into();
        backend.push_response(response);
        let mut runtime = runtime(backend);
        assert_eq!(search(&mut runtime).field("count"), Some("3"));
        let result = call(
            &mut runtime,
            "RST",
            vec![
                Field::raw("method", "4"),
                Field::raw("path", "/api/v10/channels/020/messages/%33%30"),
                Field::percent("payload", r#"{"content":"requested edit"}"#),
            ],
        );
        assert_eq!(result.status, D7OK);
        assert_eq!(
            call(&mut runtime, "RBD", vec![Field::raw("buf_len", "1024")]).field("body"),
            Some(body)
        );
        let SnapshotLookup::Found(snapshot) =
            runtime
                .gateway
                .snapshot_mut()
                .get_message("20", "30", now_ms_u64())
        else {
            panic!("expected edited message");
        };
        assert_eq!(snapshot.content, "server edit");
        assert_eq!(snapshot.guild_id, "10");
        assert_eq!(snapshot.image_attachment_count, 0);
        assert_eq!(
            call(&mut runtime, "MSI", vec![Field::raw("index", "1")]).field("image_count"),
            Some("2")
        );
        assert_eq!(search(&mut runtime).field("count"), Some("2"));
        let RuntimeRest::Mock(executor) = &runtime.rest else {
            panic!("expected mock REST")
        };
        assert_eq!(executor.backend.requests().len(), 1);
    }

    #[test]
    fn unconfirmed_message_edits_invalidate_only_the_affected_positive_record() {
        for state in [
            CommitState::NotSent,
            CommitState::Rejected,
            CommitState::Committed,
            CommitState::SentNoResponse,
            CommitState::Unknown,
        ] {
            let mut runtime = runtime(MockRestBackend::default());
            runtime
                .gateway
                .snapshot_mut()
                .tombstone_message("20", "31", now_ms_u64());
            let mutation = outcome(
                HttpMethod::Patch,
                "/api/v10/channels/20/messages/30",
                Some(r#"{"attachments":[]}"#),
                state,
            );
            runtime.observe_rest_outcome(&mutation);
            let now = now_ms_u64();
            let lookup = runtime.gateway.snapshot_mut().get_message("20", "30", now);
            if matches!(state, CommitState::NotSent | CommitState::Rejected) {
                assert!(matches!(lookup, SnapshotLookup::Found(_)));
            } else {
                assert_eq!(lookup, SnapshotLookup::Missing, "state={state:?}");
            }
            assert_eq!(
                runtime.gateway.snapshot_mut().get_message("20", "31", now),
                SnapshotLookup::Tombstone
            );
            for (channel, message) in [("21", "32"), ("22", "33")] {
                assert!(matches!(
                    runtime
                        .gateway
                        .snapshot_mut()
                        .get_message(channel, message, now),
                    SnapshotLookup::Found(_)
                ));
            }
        }
    }

    #[test]
    fn message_edit_responses_cannot_move_identity_or_leave_an_old_mutable_record() {
        for body in [
            "not JSON",
            "[]",
            "{}",
            r#"{"id":"30","channel_id":"20","author":{"id":"40"},"content":"partial","type":0}"#,
            r#"{"id":"30","channel_id":"21","author":{"id":"40"},"content":"wrong channel","type":0,"attachments":[]}"#,
            r#"{"id":"31","channel_id":"20","author":{"id":"40"},"content":"wrong message","type":0,"attachments":[]}"#,
            r#"{"id":"30","channel_id":"20","guild_id":"11","author":{"id":"40"},"content":"wrong guild","type":0,"attachments":[]}"#,
            r#"{"id":"30","channel_id":"20","author":{"id":"41"},"content":"wrong author","type":0,"attachments":[]}"#,
            r#"{"id":"30","channel_id":"20","author":{"id":"40"},"content":null,"type":0,"attachments":[]}"#,
            r#"{"id":"30","channel_id":"20","author":{"id":"40"},"content":"one","content":"two","type":0,"attachments":[]}"#,
        ] {
            let mut runtime = runtime(MockRestBackend::default());
            let mut mutation = outcome(
                HttpMethod::Patch,
                "/api/v10/channels/20/messages/30",
                Some("{}"),
                CommitState::Committed,
            );
            mutation.response_body = Some(body.into());
            runtime.observe_rest_outcome(&mutation);
            let now = now_ms_u64();
            assert_eq!(
                runtime.gateway.snapshot_mut().get_message("20", "30", now),
                SnapshotLookup::Missing,
                "body={body}"
            );
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_message("20", "31", now),
                SnapshotLookup::Found(_)
            ));
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_message("21", "32", now),
                SnapshotLookup::Found(_)
            ));
        }
    }

    #[test]
    fn uncertain_message_edits_are_revalidated_without_repeating_the_patch() {
        let mut backend = MockRestBackend::default();
        backend.push_error(crate::rest::RestTransportError::TimeoutAfterSend);
        let mut response = crate::rest::RestResponse::ok();
        response.body = r#"{"id":"30","channel_id":"20","guild_id":"10","author":{"id":"40"},"content":"confirmed by GET","type":0,"attachments":[{"filename":"new.png"}]}"#.into();
        backend.push_response(response);
        let mut runtime = runtime(backend);
        assert_eq!(
            call(
                &mut runtime,
                "RST",
                vec![
                    Field::raw("method", "4"),
                    Field::raw("path", "/api/v10/channels/20/messages/30"),
                    Field::percent("payload", r#"{"content":"requested edit"}"#)
                ]
            )
            .status,
            D7UNC
        );
        assert_eq!(search(&mut runtime).field("count"), Some("2"));
        assert_eq!(
            call(&mut runtime, "RBD", vec![Field::raw("buf_len", "1024")]).status,
            D7STA
        );
        let check = call(
            &mut runtime,
            "CDL",
            vec![
                Field::raw("guild_id", "10"),
                Field::raw("channel_id", "20"),
                Field::raw("message_id", "30"),
            ],
        );
        assert_eq!(check.status, D7OK);
        assert_eq!(check.field("can"), Some("1"));
        assert_eq!(search(&mut runtime).field("count"), Some("3"));
        let RuntimeRest::Mock(executor) = &runtime.rest else {
            panic!("expected mock REST")
        };
        assert_eq!(executor.backend.requests().len(), 2);
        assert_eq!(executor.backend.requests()[0].method, HttpMethod::Patch);
        assert_eq!(executor.backend.requests()[1].method, HttpMethod::Get);
    }

    #[test]
    fn successful_typed_deletion_updates_new_searches_without_shifting_existing_results() {
        let mut backend = MockRestBackend::default();
        backend.push_response(crate::rest::RestResponse::ok());
        let mut runtime = runtime(backend);
        assert_eq!(search(&mut runtime).field("count"), Some("3"));
        let fields = vec![
            Field::raw("channel_id", "20"),
            Field::raw("message_id", "30"),
        ];
        assert_eq!(call(&mut runtime, "DEL", fields.clone()).status, D7OK);
        assert_eq!(
            runtime
                .gateway
                .snapshot_mut()
                .get_message("20", "30", now_ms_u64()),
            SnapshotLookup::Tombstone
        );
        assert_eq!(
            call(&mut runtime, "MSI", vec![Field::raw("index", "1")]).field("message_id"),
            Some("30")
        );
        assert_eq!(
            call(&mut runtime, "MSI", vec![Field::raw("index", "2")]).field("message_id"),
            Some("31")
        );
        assert_eq!(search(&mut runtime).field("count"), Some("2"));
        assert_eq!(call(&mut runtime, "DEL", fields).status, D7NFD);
        let RuntimeRest::Mock(executor) = &runtime.rest else {
            panic!("expected mock REST")
        };
        assert_eq!(executor.backend.requests().len(), 1);
        assert_eq!(
            executor.backend.requests()[0].path,
            "/api/v10/channels/20/messages/30"
        );
    }

    #[test]
    fn message_search_results_expire_when_gateway_continuity_is_lost() {
        for boundary in 0..4 {
            let mut runtime = runtime(MockRestBackend::default());
            let now = now_ms_u64();
            runtime.ingest_gateway_json(r#"{"op":0,"s":10,"t":"READY","d":{"session_id":"old","resume_gateway_url":"wss://resume","user":{"id":"99"}}}"#, now).unwrap();
            call(&mut runtime, "NXT", vec![]);
            assert_eq!(search(&mut runtime).field("count"), Some("3"));
            let session = GatewayResumeSession::new("old", "wss://resume", 5, None).unwrap();
            assert_eq!(
                runtime.restore_gateway_resume_session(session),
                Ok(GatewayAction::SendResume { seq: 10 })
            );
            assert_eq!(
                call(&mut runtime, "MSI", vec![Field::raw("index", "1")]).status,
                D7OK
            );
            match boundary {
                0 => {
                    runtime.ingest_gateway_json(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"new","resume_gateway_url":"wss://new","user":{"id":"99"}}}"#, now).unwrap();
                }
                1 => {
                    assert!(runtime.ingest_gateway_json("{", now).is_err());
                }
                2 => {
                    let mut transport = crate::gateway::MockGatewayTransport::default();
                    transport.push_close_code(4007);
                    runtime.pump_gateway_once(&mut transport, now);
                }
                _ => {
                    runtime.events = crate::event_queue::EventQueue::new(1, 1024 * 1024, false);
                    for sequence in [11, 12] {
                        let message = format!(
                            r#"{{"op":0,"s":{sequence},"t":"MESSAGE_CREATE","d":{{"id":"{sequence}","channel_id":"20","guild_id":"10","author":{{"id":"41"}},"content":"queued","type":0}}}}"#
                        );
                        runtime.ingest_gateway_json(&message, now).unwrap();
                    }
                }
            }
            assert_eq!(
                call(&mut runtime, "MSI", vec![Field::raw("index", "1")]).status,
                D7ARG,
                "boundary={boundary}"
            );
            assert!(runtime.last_message_search.is_empty());
        }
    }

    #[test]
    fn uncertain_typed_deletion_is_not_retried_and_can_be_revalidated() {
        for exists in [false, true] {
            let mut backend = MockRestBackend::default();
            backend.push_error(crate::rest::RestTransportError::TimeoutAfterSend);
            let response = if exists {
                let mut response = crate::rest::RestResponse::ok();
                response.body = r#"{"id":"30","channel_id":"20","guild_id":"10","author":{"id":"40"},"content":"still exists","type":0,"attachments":[{"filename":"new.png"}]}"#.into();
                response
            } else {
                crate::rest::RestResponse::discord_error(404, 10008)
            };
            backend.push_response(response);
            let mut runtime = runtime(backend);
            assert_eq!(
                call(
                    &mut runtime,
                    "DEL",
                    vec![
                        Field::raw("channel_id", "20"),
                        Field::raw("message_id", "30")
                    ]
                )
                .status,
                D7UNC
            );
            assert_eq!(
                runtime
                    .gateway
                    .snapshot_mut()
                    .get_message("20", "30", now_ms_u64()),
                SnapshotLookup::Missing
            );
            let check = call(
                &mut runtime,
                "CDL",
                vec![
                    Field::raw("guild_id", "10"),
                    Field::raw("channel_id", "20"),
                    Field::raw("message_id", "30"),
                ],
            );
            assert_eq!(check.status, if exists { D7OK } else { D7NFD });
            assert_eq!(check.field("can"), Some(if exists { "1" } else { "0" }));
            assert_eq!(
                search(&mut runtime).field("count"),
                Some(if exists { "3" } else { "2" })
            );
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(executor.backend.requests().len(), 2);
            assert_eq!(executor.backend.requests()[0].method, HttpMethod::Delete);
            assert_eq!(executor.backend.requests()[1].method, HttpMethod::Get);
        }
    }

    #[test]
    fn transient_channel_reads_and_unrelated_routes_preserve_existing_snapshots() {
        for (path, status, code) in [
            ("/api/v10/channels/20", 429, 0),
            ("/api/v10/channels/20", 500, 0),
            ("/api/v10/channels/20/messages/30", 502, 0),
            (
                "/api/v10/channels/20/messages/30/reactions/emoji",
                404,
                10008,
            ),
            ("/api/v10/channels/20/pins", 403, 50001),
        ] {
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::discord_error(status, code));
            let mut runtime = runtime(backend);
            call(
                &mut runtime,
                "RST",
                vec![Field::raw("method", "1"), Field::raw("path", path)],
            );
            let now = now_ms_u64();
            assert!(
                matches!(
                    runtime.gateway.snapshot_mut().get_channel("20", now),
                    SnapshotLookup::Found(_)
                ),
                "{path}"
            );
            assert!(
                matches!(
                    runtime.gateway.snapshot_mut().get_message("20", "30", now),
                    SnapshotLookup::Found(_)
                ),
                "{path}"
            );
            assert_eq!(search(&mut runtime).field("count"), Some("3"));
        }
    }

    #[test]
    fn direct_channel_and_message_read_failures_expire_only_the_affected_targets() {
        for (path, status, code, channel_gone, message_gone) in [
            (
                "/api/v10/channels/020/messages/%33%30",
                404,
                10008,
                false,
                true,
            ),
            ("/api/v10/channels/20/messages/30", 403, 50001, false, false),
            ("/api/v10/channels/20", 404, 10003, true, true),
            ("/api/v10/channels/20", 403, 50001, true, false),
            ("/api/v10/channels/20/messages/30", 404, 10003, true, true),
            ("/api/v10/channels/20/messages/30", 404, 0, false, true),
            ("/api/v10/channels/20", 404, 0, true, true),
        ] {
            let mut backend = MockRestBackend::default();
            backend.push_response(crate::rest::RestResponse::discord_error(status, code));
            let mut runtime = runtime(backend);
            let now = now_ms_u64();
            assert_eq!(search(&mut runtime).field("count"), Some("3"));
            let result = call(
                &mut runtime,
                "RST",
                vec![Field::raw("method", "1"), Field::raw("path", path)],
            );
            assert_eq!(result.status, if status == 404 { D7NFD } else { D7FOR });
            let store = runtime.gateway.snapshot_mut();
            let message = store.get_message("20", "30", now);
            assert_eq!(
                matches!(message, SnapshotLookup::Tombstone),
                message_gone && !channel_gone,
                "{path} {code}"
            );
            assert!(
                !matches!(message, SnapshotLookup::Found(_)),
                "{path} {code}"
            );
            assert_eq!(
                matches!(store.get_channel("20", now), SnapshotLookup::Found(_)),
                !channel_gone,
                "{path} {code}"
            );
            assert_eq!(
                matches!(store.get_message("20", "31", now), SnapshotLookup::Found(_)),
                !channel_gone,
                "{path} {code}"
            );
            assert!(matches!(
                store.get_message("21", "32", now),
                SnapshotLookup::Found(_)
            ));
            assert!(matches!(
                store.get_message("22", "33", now),
                SnapshotLookup::Found(_)
            ));
            assert_eq!(
                call(&mut runtime, "MSI", vec![Field::raw("index", "3")]).field("message_id"),
                Some("32")
            );
            assert_eq!(
                search(&mut runtime).field("count"),
                Some(if channel_gone { "1" } else { "2" })
            );
            if status == 404 {
                assert_eq!(
                    runtime
                        .delete_preflight_decision("10", "20", "30", now)
                        .status,
                    D7NFD
                );
            }
            let RuntimeRest::Mock(executor) = &runtime.rest else {
                panic!("expected mock REST")
            };
            assert_eq!(executor.backend.requests().len(), 1);
        }
    }

    #[test]
    fn confirmed_channel_deletion_removes_its_messages_without_shifting_search_results() {
        let mut runtime = runtime(MockRestBackend::default());
        assert_eq!(search(&mut runtime).field("count"), Some("3"));
        runtime.observe_rest_outcome(&outcome(
            HttpMethod::Delete,
            "/api/v10/channels/20",
            None,
            CommitState::Committed,
        ));
        assert_eq!(
            runtime
                .gateway
                .snapshot_mut()
                .get_message("20", "30", now_ms_u64()),
            SnapshotLookup::Missing
        );
        assert_eq!(
            runtime
                .gateway
                .snapshot_mut()
                .get_message("20", "31", now_ms_u64()),
            SnapshotLookup::Missing
        );
        assert_eq!(
            call(&mut runtime, "MSI", vec![Field::raw("index", "3")]).field("message_id"),
            Some("32")
        );
        assert_eq!(search(&mut runtime).field("count"), Some("1"));
        assert!(matches!(
            runtime
                .gateway
                .snapshot_mut()
                .get_message("21", "32", now_ms_u64()),
            SnapshotLookup::Found(_)
        ));
        assert!(matches!(
            runtime
                .gateway
                .snapshot_mut()
                .get_message("22", "33", now_ms_u64()),
            SnapshotLookup::Found(_)
        ));
    }

    #[test]
    fn raw_single_and_bulk_deletions_follow_commit_state_and_resource_scope() {
        for state in [
            CommitState::NotSent,
            CommitState::Rejected,
            CommitState::Committed,
            CommitState::SentNoResponse,
            CommitState::Unknown,
        ] {
            for bulk in [false, true] {
                let mut runtime = runtime(MockRestBackend::default());
                let mutation = if bulk {
                    outcome(
                        HttpMethod::Post,
                        "/api/v10/channels/020/messages/bulk-delete?test=1",
                        Some(r#"{"messages":["030","31"]}"#),
                        state,
                    )
                } else {
                    outcome(
                        HttpMethod::Delete,
                        "/api/v10/channels/%32%30/messages/030",
                        None,
                        state,
                    )
                };
                runtime.observe_rest_outcome(&mutation);
                let now = now_ms_u64();
                let changed = !matches!(state, CommitState::NotSent | CommitState::Rejected);
                for message in ["30", "31"] {
                    let lookup = runtime
                        .gateway
                        .snapshot_mut()
                        .get_message("20", message, now);
                    if changed && (bulk || message == "30") {
                        assert_eq!(
                            lookup,
                            if state == CommitState::Committed {
                                SnapshotLookup::Tombstone
                            } else {
                                SnapshotLookup::Missing
                            }
                        );
                    } else {
                        assert!(matches!(lookup, SnapshotLookup::Found(_)));
                    }
                }
                assert!(matches!(
                    runtime.gateway.snapshot_mut().get_message("21", "32", now),
                    SnapshotLookup::Found(_)
                ));
                assert!(matches!(
                    runtime.gateway.snapshot_mut().get_message("22", "33", now),
                    SnapshotLookup::Found(_)
                ));
                assert!(matches!(
                    runtime.gateway.snapshot_mut().get_channel("20", now),
                    SnapshotLookup::Found(_)
                ));
            }
        }
    }

    #[test]
    fn unknown_bulk_targets_invalidate_only_the_channel_and_preserve_known_deletions() {
        for body in [None, Some("{}"), Some(r#"{"messages":["30",false]}"#)] {
            let mut runtime = runtime(MockRestBackend::default());
            runtime
                .gateway
                .snapshot_mut()
                .tombstone_message("20", "30", now_ms_u64());
            runtime.observe_rest_outcome(&outcome(
                HttpMethod::Post,
                "/api/v10/channels/20/messages/bulk-delete",
                body,
                CommitState::Unknown,
            ));
            let now = now_ms_u64();
            assert_eq!(
                runtime.gateway.snapshot_mut().get_message("20", "30", now),
                SnapshotLookup::Tombstone
            );
            assert_eq!(
                runtime.gateway.snapshot_mut().get_message("20", "31", now),
                SnapshotLookup::Missing
            );
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_message("21", "32", now),
                SnapshotLookup::Found(_)
            ));
        }
    }

    #[test]
    fn guild_leave_distinguishes_confirmed_and_uncertain_departure() {
        for state in [
            CommitState::NotSent,
            CommitState::Rejected,
            CommitState::Committed,
            CommitState::SentNoResponse,
            CommitState::Unknown,
        ] {
            let mut runtime = runtime(MockRestBackend::default());
            runtime.observe_rest_outcome(&outcome(
                HttpMethod::Delete,
                "/api/v10/users/%40me/guilds/0010",
                None,
                state,
            ));
            let now = now_ms_u64();
            let guild = runtime.gateway.snapshot_mut().get_guild("10", now);
            if state == CommitState::Committed {
                assert_eq!(guild, SnapshotLookup::Tombstone);
                assert_eq!(
                    runtime.gateway.snapshot_mut().get_message("20", "30", now),
                    SnapshotLookup::Missing
                );
            } else if matches!(state, CommitState::Unknown | CommitState::SentNoResponse) {
                assert!(matches!(
                    guild,
                    SnapshotLookup::Found(GuildSnapshot {
                        complete: false,
                        ..
                    })
                ));
                assert_eq!(
                    runtime.gateway.snapshot_mut().get_guild_roles("10", now),
                    SnapshotLookup::Missing
                );
                assert_eq!(
                    runtime.gateway.snapshot_mut().get_member("10", "99", now),
                    SnapshotLookup::Missing
                );
                assert!(matches!(
                    runtime.gateway.snapshot_mut().get_message("20", "30", now),
                    SnapshotLookup::Found(_)
                ));
            } else {
                assert!(matches!(
                    guild,
                    SnapshotLookup::Found(GuildSnapshot { complete: true, .. })
                ));
            }
            assert!(matches!(
                runtime.gateway.snapshot_mut().get_message("22", "33", now),
                SnapshotLookup::Found(_)
            ));
        }
    }

    #[test]
    fn reaction_and_pin_deletions_do_not_delete_the_message_snapshot() {
        for path in [
            "/api/v10/channels/20/messages/30/reactions",
            "/api/v10/channels/20/messages/pins/30",
            "/api/v10/channels/20/pins/30",
        ] {
            let mut runtime = runtime(MockRestBackend::default());
            runtime.observe_rest_outcome(&outcome(
                HttpMethod::Delete,
                path,
                None,
                CommitState::Committed,
            ));
            assert!(matches!(
                runtime
                    .gateway
                    .snapshot_mut()
                    .get_message("20", "30", now_ms_u64()),
                SnapshotLookup::Found(_)
            ));
        }
    }
}
