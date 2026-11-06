use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use native_tls::TlsConnector;

use crate::constants::*;
use crate::ipc::percent_encode;
use crate::json::{parse_json, JsonValue};
use crate::network::NetworkStream;
use crate::validation::{
    validate_bot_token_value, validate_message_content, validate_reason,
    validate_rest_origin_for_token, validate_snowflake, HttpOriginScheme,
};

mod debug;
mod http;
pub(crate) mod path;
mod rate_limit;

use http::valid_header_value as valid_http_header_value;
#[cfg(test)]
use http::{MAX_HTTP_HEAD_BYTES, MAX_HTTP_RESPONSE_BYTES};

pub use rate_limit::{
    retry_after_ms, InvalidRequestBudget, RestAcquireDecision, RestGlobalLimiter, RouteLimiter,
};

pub const MAX_REST_JSON_BODY_BYTES: usize = 60_000;
pub const MAX_REST_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_REST_ATTACHMENTS: usize = 10;
pub const MAX_REST_STICKERS: usize = 3;
pub const MAX_REST_POLL_ANSWERS: usize = 10;
const MAX_REST_POLL_QUESTION_CHARS: usize = 300;
const MAX_REST_POLL_ANSWER_CHARS: usize = 55;
const MAX_REST_POLL_DURATION_HOURS: u32 = 32 * 24;

#[derive(Clone, Eq, PartialEq)]
pub struct AttachmentUpload {
    pub file_name: String,
    pub file_bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchivedThreadKind {
    JoinedPrivate,
    Private,
    Public,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestAction {
    SendMessage,
    DeleteMessage,
    TimeoutMember,
    UntimeoutMember,
    MessageReaction,
    InteractionCallback,
    ApplicationCommand,
    TypingIndicator,
    GuildManagement,
    ChannelManagement,
    RoleManagement,
    MemberManagement,
    GatewayVoice,
    LowLevel,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReactionOperation {
    AddOwn,
    DeleteOwn,
    DeleteUser,
    DeleteAll,
    DeleteEmoji,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitState {
    NotSent,
    SentNoResponse,
    Committed,
    Unknown,
    Rejected,
}

impl CommitState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotSent => "not_sent",
            Self::SentNoResponse => "sent_no_response",
            Self::Committed => "committed",
            Self::Unknown => "unknown",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RestRequest {
    pub method: HttpMethod,
    pub path: String,
    pub route_template: &'static str,
    pub major_parameter: String,
    pub action: RestAction,
    pub body: Option<String>,
    pub body_bytes: Option<Vec<u8>>,
    pub content_type: Option<String>,
    pub audit_reason_header: Option<String>,
    pub nonce: Option<String>,
}

impl RestRequest {
    pub fn provisional_key(&self) -> String {
        format!(
            "{} {}",
            rate_limit::route_key(self),
            rate_limit::major_key(self)
        )
    }

    pub fn has_json_body(&self) -> bool {
        self.body.is_some() && self.content_type.as_deref() == Some("application/json")
    }

    pub fn body_bytes(&self) -> &[u8] {
        if let Some(bytes) = &self.body_bytes {
            bytes
        } else {
            self.body.as_deref().unwrap_or("").as_bytes()
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RestOutcome {
    pub status: i32,
    pub why: String,
    pub commit_state: CommitState,
    pub request: Option<RestRequest>,
    pub http_status: i32,
    pub discord_error_code: i32,
    pub created_message_id: Option<String>,
    pub response_body: Option<String>,
}

impl RestOutcome {
    pub fn not_sent(status: i32, why: &str) -> Self {
        Self {
            status,
            why: why.to_string(),
            commit_state: CommitState::NotSent,
            request: None,
            http_status: 0,
            discord_error_code: 0,
            created_message_id: None,
            response_body: None,
        }
    }

    pub fn planned(request: RestRequest) -> Self {
        Self {
            status: D7OK,
            why: WHY_OK.to_string(),
            commit_state: CommitState::NotSent,
            request: Some(request),
            http_status: 0,
            discord_error_code: 0,
            created_message_id: None,
            response_body: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RestPlanner {
    nonce: NonceGenerator,
}

impl RestPlanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_gateway_bot(&mut self) -> RestOutcome {
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Get,
            path: "/api/v10/gateway/bot".to_string(),
            route_template: "GET /gateway/bot",
            major_parameter: "global".to_string(),
            action: RestAction::ReadOnly,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn get_current_user(&mut self) -> RestOutcome {
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Get,
            path: "/api/v10/users/@me".to_string(),
            route_template: "GET /users/@me",
            major_parameter: "global".to_string(),
            action: RestAction::ReadOnly,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn get_message(&mut self, channel_id: &str, message_id: &str) -> RestOutcome {
        if !validate_snowflake(channel_id) || !validate_snowflake(message_id) {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Get,
            path: format!("/api/v10/channels/{}/messages/{}", channel_id, message_id),
            route_template: "GET /channels/{channel_id}/messages/{message_id}",
            major_parameter: channel_id.to_string(),
            action: RestAction::ReadOnly,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn get_channel(&mut self, channel_id: &str) -> RestOutcome {
        if !validate_snowflake(channel_id) {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Get,
            path: format!("/api/v10/channels/{}", channel_id),
            route_template: "GET /channels/{channel_id}",
            major_parameter: channel_id.to_string(),
            action: RestAction::ReadOnly,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn get_thread_member(&mut self, channel_id: &str, user_id: &str) -> RestOutcome {
        if !validate_snowflake(channel_id) || !validate_snowflake(user_id) {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Get,
            path: format!(
                "/api/v10/channels/{}/thread-members/{}",
                channel_id, user_id
            ),
            route_template: "GET /channels/{channel_id}/thread-members/{user_id}",
            major_parameter: channel_id.to_string(),
            action: RestAction::ReadOnly,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn get_joined_private_archived_threads(&mut self, channel_id: &str) -> RestOutcome {
        self.get_archived_threads(
            channel_id,
            "/users/@me/threads/archived/private",
            "GET /channels/{channel_id}/users/@me/threads/archived/private",
        )
    }

    pub fn get_archived_public_threads(&mut self, channel_id: &str) -> RestOutcome {
        self.get_archived_threads(
            channel_id,
            "/threads/archived/public",
            "GET /channels/{channel_id}/threads/archived/public",
        )
    }

    pub fn get_archived_private_threads(&mut self, channel_id: &str) -> RestOutcome {
        self.get_archived_threads(
            channel_id,
            "/threads/archived/private",
            "GET /channels/{channel_id}/threads/archived/private",
        )
    }

    pub fn get_archived_threads_page(
        &mut self,
        channel_id: &str,
        kind: ArchivedThreadKind,
        before: Option<&str>,
        limit: u16,
    ) -> RestOutcome {
        if !(1..=100).contains(&limit)
            || before.is_some_and(|value| match kind {
                ArchivedThreadKind::JoinedPrivate => !validate_snowflake(value),
                ArchivedThreadKind::Private | ArchivedThreadKind::Public => {
                    value.len() > 128
                        || value.trim() != value
                        || chrono::DateTime::parse_from_rfc3339(value).is_err()
                }
            })
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        let mut outcome = match kind {
            ArchivedThreadKind::JoinedPrivate => {
                self.get_joined_private_archived_threads(channel_id)
            }
            ArchivedThreadKind::Private => self.get_archived_private_threads(channel_id),
            ArchivedThreadKind::Public => self.get_archived_public_threads(channel_id),
        };
        if let Some(request) = &mut outcome.request {
            request.path.push_str(&format!("?limit={limit}"));
            if let Some(before) = before {
                request.path.push_str("&before=");
                request.path.push_str(&percent_encode(before));
            }
        }
        outcome
    }

    pub fn get_guild(&mut self, guild_id: &str) -> RestOutcome {
        if !validate_snowflake(guild_id) {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Get,
            path: format!("/api/v10/guilds/{guild_id}"),
            route_template: "GET /guilds/{guild_id}",
            major_parameter: guild_id.to_string(),
            action: RestAction::ReadOnly,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn get_guild_member(&mut self, guild_id: &str, user_id: &str) -> RestOutcome {
        if !validate_snowflake(guild_id) || !validate_snowflake(user_id) {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Get,
            path: format!("/api/v10/guilds/{}/members/{}", guild_id, user_id),
            route_template: "GET /guilds/{guild_id}/members/{user_id}",
            major_parameter: guild_id.to_string(),
            action: RestAction::ReadOnly,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    fn get_archived_threads(
        &mut self,
        channel_id: &str,
        suffix: &str,
        route_template: &'static str,
    ) -> RestOutcome {
        if !validate_snowflake(channel_id) {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Get,
            path: format!("/api/v10/channels/{}{}", channel_id, suffix),
            route_template,
            major_parameter: channel_id.to_string(),
            action: RestAction::ReadOnly,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn get_guild_roles(&mut self, guild_id: &str) -> RestOutcome {
        if !validate_snowflake(guild_id) {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Get,
            path: format!("/api/v10/guilds/{}/roles", guild_id),
            route_template: "GET /guilds/{guild_id}/roles",
            major_parameter: guild_id.to_string(),
            action: RestAction::ReadOnly,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn send_message(&mut self, channel_id: &str, content: &str) -> RestOutcome {
        if !validate_snowflake(channel_id) || validate_message_content(content) != D7OK {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        let nonce = self.nonce.next_nonce();
        let body = format!(
            "{{\"content\":\"{}\",\"allowed_mentions\":{{\"parse\":[]}},\"nonce\":\"{}\",\"enforce_nonce\":true}}",
            json_escape(content),
            nonce
        );
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Post,
            path: format!("/api/v10/channels/{}/messages", channel_id),
            route_template: "POST /channels/{channel_id}/messages",
            major_parameter: channel_id.to_string(),
            action: RestAction::SendMessage,
            body: Some(body),
            body_bytes: None,
            content_type: Some("application/json".to_string()),
            audit_reason_header: None,
            nonce: Some(nonce),
        })
    }

    pub fn send_message_payload(&mut self, channel_id: &str, payload_json: &str) -> RestOutcome {
        if !validate_snowflake(channel_id) || !validate_json_object_payload(payload_json) {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Post,
            path: format!("/api/v10/channels/{}/messages", channel_id),
            route_template: "POST /channels/{channel_id}/messages",
            major_parameter: channel_id.to_string(),
            action: RestAction::SendMessage,
            body: Some(payload_json.to_string()),
            body_bytes: None,
            content_type: Some("application/json".to_string()),
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn send_message_stickers(
        &mut self,
        channel_id: &str,
        content: &str,
        sticker_ids: Vec<String>,
    ) -> RestOutcome {
        if !validate_snowflake(channel_id)
            || (!content.is_empty() && validate_message_content(content) != D7OK)
            || sticker_ids.is_empty()
            || sticker_ids.len() > MAX_REST_STICKERS
            || sticker_ids
                .iter()
                .any(|sticker_id| !validate_snowflake(sticker_id))
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        let nonce = self.nonce.next_nonce();
        let body = sticker_message_payload_json(content, &sticker_ids, &nonce);
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Post,
            path: format!("/api/v10/channels/{}/messages", channel_id),
            route_template: "POST /channels/{channel_id}/messages",
            major_parameter: channel_id.to_string(),
            action: RestAction::SendMessage,
            body: Some(body),
            body_bytes: None,
            content_type: Some("application/json".to_string()),
            audit_reason_header: None,
            nonce: Some(nonce),
        })
    }

    pub fn send_message_poll(
        &mut self,
        channel_id: &str,
        content: &str,
        question: &str,
        answers: Vec<String>,
        duration_hours: u32,
        allow_multiselect: bool,
    ) -> RestOutcome {
        if !validate_snowflake(channel_id)
            || (!content.is_empty() && validate_message_content(content) != D7OK)
            || !validate_poll_text(question, MAX_REST_POLL_QUESTION_CHARS)
            || !(2..=MAX_REST_POLL_ANSWERS).contains(&answers.len())
            || answers
                .iter()
                .any(|answer| !validate_poll_text(answer, MAX_REST_POLL_ANSWER_CHARS))
            || duration_hours > MAX_REST_POLL_DURATION_HOURS
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        let nonce = self.nonce.next_nonce();
        let body = poll_message_payload_json(
            content,
            question,
            &answers,
            duration_hours,
            allow_multiselect,
            &nonce,
        );
        if body.len() > MAX_REST_JSON_BODY_BYTES {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Post,
            path: format!("/api/v10/channels/{}/messages", channel_id),
            route_template: "POST /channels/{channel_id}/messages",
            major_parameter: channel_id.to_string(),
            action: RestAction::SendMessage,
            body: Some(body),
            body_bytes: None,
            content_type: Some("application/json".to_string()),
            audit_reason_header: None,
            nonce: Some(nonce),
        })
    }

    pub fn send_message_attachment(
        &mut self,
        channel_id: &str,
        content: &str,
        file_name: &str,
        file_bytes: Vec<u8>,
    ) -> RestOutcome {
        self.send_message_attachments(
            channel_id,
            content,
            vec![AttachmentUpload {
                file_name: file_name.to_string(),
                file_bytes,
            }],
        )
    }

    pub fn send_message_attachments(
        &mut self,
        channel_id: &str,
        content: &str,
        files: Vec<AttachmentUpload>,
    ) -> RestOutcome {
        if !validate_snowflake(channel_id)
            || (!content.is_empty() && validate_message_content(content) != D7OK)
            || files.is_empty()
            || files.len() > MAX_REST_ATTACHMENTS
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        let mut total_bytes = 0usize;
        for file in &files {
            if !validate_attachment_file_name(&file.file_name) || file.file_bytes.is_empty() {
                return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            }
            let Some(next_total) = total_bytes.checked_add(file.file_bytes.len()) else {
                return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            };
            total_bytes = next_total;
        }
        if total_bytes > MAX_REST_ATTACHMENT_BYTES {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        let nonce = self.nonce.next_nonce();
        let boundary = format!("dfor77-{}", nonce);
        let payload_json = attachment_payload_json(content, &files, &nonce);
        let body_bytes = multipart_message_body(&boundary, &payload_json, &files);
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Post,
            path: format!("/api/v10/channels/{}/messages", channel_id),
            route_template: "POST /channels/{channel_id}/messages",
            major_parameter: channel_id.to_string(),
            action: RestAction::SendMessage,
            body: Some(payload_json),
            body_bytes: Some(body_bytes),
            content_type: Some(format!("multipart/form-data; boundary={}", boundary)),
            audit_reason_header: None,
            nonce: Some(nonce),
        })
    }

    pub fn interaction_callback(
        &mut self,
        interaction_id: &str,
        interaction_token: &str,
        payload_json: &str,
    ) -> RestOutcome {
        if !validate_snowflake(interaction_id)
            || !validate_path_segment(interaction_token)
            || !validate_json_object_payload(payload_json)
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Post,
            path: format!(
                "/api/v10/interactions/{}/{}/callback",
                interaction_id, interaction_token
            ),
            route_template: "POST /interactions/{interaction_id}/{interaction_token}/callback",
            major_parameter: interaction_id.to_string(),
            action: RestAction::InteractionCallback,
            body: Some(payload_json.to_string()),
            body_bytes: None,
            content_type: Some("application/json".to_string()),
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn application_command(
        &mut self,
        application_id: &str,
        guild_id: Option<&str>,
        command_id: Option<&str>,
        method: HttpMethod,
        payload_json: Option<&str>,
    ) -> RestOutcome {
        if !validate_snowflake(application_id)
            || guild_id.is_some_and(|guild_id| !validate_snowflake(guild_id))
            || command_id.is_some_and(|command_id| !validate_snowflake(command_id))
            || payload_json.is_some_and(|payload_json| !validate_json_payload(payload_json))
            || !matches!(
                method,
                HttpMethod::Get
                    | HttpMethod::Post
                    | HttpMethod::Put
                    | HttpMethod::Patch
                    | HttpMethod::Delete
            )
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        let mut path = format!("/api/v10/applications/{}", application_id);
        if let Some(guild_id) = guild_id {
            path.push_str("/guilds/");
            path.push_str(guild_id);
        }
        path.push_str("/commands");
        if let Some(command_id) = command_id {
            path.push('/');
            path.push_str(command_id);
        }
        RestOutcome::planned(RestRequest {
            method,
            path,
            route_template: "APPLICATION_COMMAND",
            major_parameter: application_id.to_string(),
            action: RestAction::ApplicationCommand,
            body: payload_json.map(ToOwned::to_owned),
            body_bytes: None,
            content_type: payload_json.map(|_| "application/json".to_string()),
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn typing_indicator(&mut self, channel_id: &str) -> RestOutcome {
        if !validate_snowflake(channel_id) {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Post,
            path: format!("/api/v10/channels/{}/typing", channel_id),
            route_template: "POST /channels/{channel_id}/typing",
            major_parameter: channel_id.to_string(),
            action: RestAction::TypingIndicator,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn message_reaction(
        &mut self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
        user_id: Option<&str>,
        operation: ReactionOperation,
    ) -> RestOutcome {
        if !validate_snowflake(channel_id)
            || !validate_snowflake(message_id)
            || user_id.is_some_and(|user_id| !validate_snowflake(user_id))
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        if matches!(operation, ReactionOperation::DeleteAll) {
            if !emoji.is_empty() || user_id.is_some() {
                return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
            }
        } else if !validate_reaction_emoji(emoji) {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        if operation == ReactionOperation::DeleteUser && user_id.is_none() {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        if operation != ReactionOperation::DeleteUser && user_id.is_some() {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        let encoded_emoji = if emoji.is_empty() {
            String::new()
        } else {
            percent_encode(emoji)
        };
        let (method, path, route_template) = match operation {
            ReactionOperation::AddOwn => (
                HttpMethod::Put,
                format!(
                    "/api/v10/channels/{}/messages/{}/reactions/{}/@me",
                    channel_id, message_id, encoded_emoji
                ),
                "PUT /channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me",
            ),
            ReactionOperation::DeleteOwn => (
                HttpMethod::Delete,
                format!(
                    "/api/v10/channels/{}/messages/{}/reactions/{}/@me",
                    channel_id, message_id, encoded_emoji
                ),
                "DELETE /channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me",
            ),
            ReactionOperation::DeleteUser => (
                HttpMethod::Delete,
                format!(
                    "/api/v10/channels/{}/messages/{}/reactions/{}/{}",
                    channel_id,
                    message_id,
                    encoded_emoji,
                    user_id.unwrap_or_default()
                ),
                "DELETE /channels/{channel_id}/messages/{message_id}/reactions/{emoji}/{user_id}",
            ),
            ReactionOperation::DeleteAll => (
                HttpMethod::Delete,
                format!(
                    "/api/v10/channels/{}/messages/{}/reactions",
                    channel_id, message_id
                ),
                "DELETE /channels/{channel_id}/messages/{message_id}/reactions",
            ),
            ReactionOperation::DeleteEmoji => (
                HttpMethod::Delete,
                format!(
                    "/api/v10/channels/{}/messages/{}/reactions/{}",
                    channel_id, message_id, encoded_emoji
                ),
                "DELETE /channels/{channel_id}/messages/{message_id}/reactions/{emoji}",
            ),
        };
        RestOutcome::planned(RestRequest {
            method,
            path,
            route_template,
            major_parameter: channel_id.to_string(),
            action: RestAction::MessageReaction,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: None,
            nonce: None,
        })
    }

    pub fn raw_json_request(
        &mut self,
        method: HttpMethod,
        path: &str,
        payload_json: Option<&str>,
        reason: &str,
    ) -> RestOutcome {
        let Some(parsed_path) = path::RestPath::parse(path) else {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        };
        if payload_json.is_some_and(|payload_json| !validate_json_payload(payload_json))
            || validate_reason(reason) != D7OK
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method,
            path: path.to_string(),
            route_template: "CUSTOM",
            major_parameter: major_parameter_from_path(&parsed_path),
            action: action_from_path(method, &parsed_path),
            body: payload_json.map(ToOwned::to_owned),
            body_bytes: None,
            content_type: payload_json.map(|_| "application/json".to_string()),
            audit_reason_header: audit_reason_header(reason),
            nonce: None,
        })
    }

    pub fn delete_message(
        &mut self,
        channel_id: &str,
        message_id: &str,
        reason: &str,
    ) -> RestOutcome {
        if !validate_snowflake(channel_id)
            || !validate_snowflake(message_id)
            || validate_reason(reason) != D7OK
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Delete,
            path: format!("/api/v10/channels/{}/messages/{}", channel_id, message_id),
            route_template: "DELETE /channels/{channel_id}/messages/{message_id}",
            major_parameter: channel_id.to_string(),
            action: RestAction::DeleteMessage,
            body: None,
            body_bytes: None,
            content_type: None,
            audit_reason_header: audit_reason_header(reason),
            nonce: None,
        })
    }

    pub fn timeout_member(
        &mut self,
        guild_id: &str,
        user_id: &str,
        seconds: u64,
        reason: &str,
        now_ms: u128,
    ) -> RestOutcome {
        if !validate_snowflake(guild_id)
            || !validate_snowflake(user_id)
            || !(1..=2_419_200).contains(&seconds)
            || validate_reason(reason) != D7OK
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        let Some(until_ms) = checked_timeout_until_ms(now_ms, seconds) else {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        };
        let body = format!(
            "{{\"communication_disabled_until\":\"{}\"}}",
            utc_rfc3339_millis(until_ms)
        );
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Patch,
            path: format!("/api/v10/guilds/{}/members/{}", guild_id, user_id),
            route_template: "PATCH /guilds/{guild_id}/members/{user_id}",
            major_parameter: guild_id.to_string(),
            action: RestAction::TimeoutMember,
            body: Some(body),
            body_bytes: None,
            content_type: Some("application/json".to_string()),
            audit_reason_header: audit_reason_header(reason),
            nonce: None,
        })
    }

    pub fn untimeout_member(&mut self, guild_id: &str, user_id: &str, reason: &str) -> RestOutcome {
        if !validate_snowflake(guild_id)
            || !validate_snowflake(user_id)
            || validate_reason(reason) != D7OK
        {
            return RestOutcome::not_sent(D7ARG, WHY_INVALID_FORM_BODY);
        }
        RestOutcome::planned(RestRequest {
            method: HttpMethod::Patch,
            path: format!("/api/v10/guilds/{}/members/{}", guild_id, user_id),
            route_template: "PATCH /guilds/{guild_id}/members/{user_id}",
            major_parameter: guild_id.to_string(),
            action: RestAction::UntimeoutMember,
            body: Some("{\"communication_disabled_until\":null}".to_string()),
            body_bytes: None,
            content_type: Some("application/json".to_string()),
            audit_reason_header: audit_reason_header(reason),
            nonce: None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct NonceGenerator {
    seed: u128,
    counter: u128,
}

impl Default for NonceGenerator {
    fn default() -> Self {
        Self {
            seed: nonce_seed(),
            counter: 0,
        }
    }
}

impl NonceGenerator {
    pub fn next_nonce(&mut self) -> String {
        self.counter = self.counter.wrapping_add(1);
        base36(self.seed.wrapping_add(self.counter))
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RestResponse {
    pub http_status: u16,
    pub discord_error_code: Option<i32>,
    pub body: String,
    pub headers: HashMap<String, String>,
}

impl RestResponse {
    pub fn ok() -> Self {
        Self {
            http_status: 200,
            discord_error_code: None,
            body: String::new(),
            headers: HashMap::new(),
        }
    }

    pub fn discord_error(http_status: u16, discord_error_code: i32) -> Self {
        Self {
            http_status,
            discord_error_code: Some(discord_error_code),
            body: format!("{{\"code\":{},\"message\":\"mock\"}}", discord_error_code),
            headers: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestTransportError {
    NetworkBeforeSend,
    TimeoutBeforeSend,
    TimeoutAfterSend,
    ConnectionResetAfterSend,
    EofAfterSend,
}

pub trait RestBackend {
    fn execute(&mut self, request: &RestRequest) -> Result<RestResponse, RestTransportError>;
}

#[derive(Debug)]
pub struct RestExecutor<B: RestBackend> {
    pub planner: RestPlanner,
    pub backend: B,
    pub invalid_budget: InvalidRequestBudget,
    pub route_limiter: RouteLimiter,
    pub global_limiter: RestGlobalLimiter,
    pub global_max_wait_ms: u64,
}

impl<B: RestBackend> RestExecutor<B> {
    pub fn new(backend: B, invalid_budget: InvalidRequestBudget) -> Self {
        Self {
            planner: RestPlanner::new(),
            backend,
            invalid_budget,
            route_limiter: RouteLimiter::default(),
            global_limiter: RestGlobalLimiter::new(),
            global_max_wait_ms: 10_000,
        }
    }

    pub fn with_global_max_wait_ms(mut self, global_max_wait_ms: u64) -> Self {
        self.global_max_wait_ms = global_max_wait_ms;
        self
    }

    pub fn execute_send_message(
        &mut self,
        channel_id: &str,
        content: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self.planner.send_message(channel_id, content);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_send_message_payload(
        &mut self,
        channel_id: &str,
        payload_json: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self.planner.send_message_payload(channel_id, payload_json);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_interaction_callback(
        &mut self,
        interaction_id: &str,
        interaction_token: &str,
        payload_json: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome =
            self.planner
                .interaction_callback(interaction_id, interaction_token, payload_json);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_typing_indicator(&mut self, channel_id: &str, now_ms: u64) -> RestOutcome {
        let outcome = self.planner.typing_indicator(channel_id);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_raw_json_request(
        &mut self,
        method: HttpMethod,
        path: &str,
        payload_json: Option<&str>,
        reason: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self
            .planner
            .raw_json_request(method, path, payload_json, reason);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_gateway_bot(&mut self, now_ms: u64) -> RestOutcome {
        let outcome = self.planner.get_gateway_bot();
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_current_user(&mut self, now_ms: u64) -> RestOutcome {
        let outcome = self.planner.get_current_user();
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_message(
        &mut self,
        channel_id: &str,
        message_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self.planner.get_message(channel_id, message_id);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_channel(&mut self, channel_id: &str, now_ms: u64) -> RestOutcome {
        let outcome = self.planner.get_channel(channel_id);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_thread_member(
        &mut self,
        channel_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self.planner.get_thread_member(channel_id, user_id);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_joined_private_archived_threads(
        &mut self,
        channel_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self.planner.get_joined_private_archived_threads(channel_id);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_archived_public_threads(
        &mut self,
        channel_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self.planner.get_archived_public_threads(channel_id);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_archived_private_threads(
        &mut self,
        channel_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self.planner.get_archived_private_threads(channel_id);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_archived_threads_page(
        &mut self,
        channel_id: &str,
        kind: ArchivedThreadKind,
        before: Option<&str>,
        limit: u16,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self
            .planner
            .get_archived_threads_page(channel_id, kind, before, limit);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_guild(&mut self, guild_id: &str, now_ms: u64) -> RestOutcome {
        let outcome = self.planner.get_guild(guild_id);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_guild_member(
        &mut self,
        guild_id: &str,
        user_id: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self.planner.get_guild_member(guild_id, user_id);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_get_guild_roles(&mut self, guild_id: &str, now_ms: u64) -> RestOutcome {
        let outcome = self.planner.get_guild_roles(guild_id);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_delete_message(
        &mut self,
        channel_id: &str,
        message_id: &str,
        reason: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self.planner.delete_message(channel_id, message_id, reason);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_timeout_member(
        &mut self,
        guild_id: &str,
        user_id: &str,
        seconds: u64,
        reason: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome =
            self.planner
                .timeout_member(guild_id, user_id, seconds, reason, u128::from(now_ms));
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_untimeout_member(
        &mut self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
        now_ms: u64,
    ) -> RestOutcome {
        let outcome = self.planner.untimeout_member(guild_id, user_id, reason);
        self.execute_planned(outcome, now_ms)
    }

    pub fn execute_planned(&mut self, planned: RestOutcome, now_ms: u64) -> RestOutcome {
        if planned.status != D7OK {
            return planned;
        }
        let Some(request) = planned.request else {
            return RestOutcome::not_sent(D7INT, "REST_REQUEST_MISSING");
        };
        if self.invalid_budget.is_stopped(now_ms) {
            return not_sent_with_request(D7INV, "INVALID_REQUEST_BUDGET", request);
        }
        let route_decision =
            self.route_limiter
                .try_acquire(&request, now_ms, self.global_max_wait_ms);
        if !route_decision.allowed {
            return not_sent_with_request(D7RTL, "RATE_LIMITED", request);
        }
        let global_now = now_ms.saturating_add(route_decision.required_wait_ms);
        let global_wait_budget = self
            .global_max_wait_ms
            .saturating_sub(route_decision.required_wait_ms);
        let global_decision = self
            .global_limiter
            .try_acquire(global_now, global_wait_budget);
        if !global_decision.allowed {
            return not_sent_with_request(D7RTL, "RATE_LIMITED", request);
        }
        let wait_ms = route_decision
            .required_wait_ms
            .saturating_add(global_decision.required_wait_ms);
        if wait_ms > 0 {
            std::thread::sleep(Duration::from_millis(wait_ms));
        }
        let response_now = global_now.saturating_add(global_decision.required_wait_ms);
        let started = std::time::Instant::now();
        let result = self.backend.execute(&request);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let response_now = response_now.saturating_add(elapsed_ms);
        match result {
            Ok(response) => {
                self.route_limiter
                    .observe(&request, &response, response_now);
                self.global_limiter
                    .observe_response(&response, response_now);
                self.invalid_budget.record(&response, response_now);
                if (200..=299).contains(&response.http_status) {
                    let created_message_id = created_message_id(&response);
                    let response_body =
                        if request.action == RestAction::ReadOnly || !response.body.is_empty() {
                            Some(response.body)
                        } else {
                            None
                        };
                    RestOutcome {
                        status: D7OK,
                        why: WHY_OK.to_string(),
                        commit_state: CommitState::Committed,
                        request: Some(request),
                        http_status: i32::from(response.http_status),
                        discord_error_code: response.discord_error_code.unwrap_or(0),
                        created_message_id,
                        response_body,
                    }
                } else {
                    let mut outcome = map_discord_error(
                        request.action,
                        response.http_status,
                        response.discord_error_code,
                    );
                    outcome.request = Some(request);
                    outcome
                }
            }
            Err(error) => transport_error_outcome(request, error),
        }
    }
}

fn not_sent_with_request(status: i32, why: &str, request: RestRequest) -> RestOutcome {
    RestOutcome {
        status,
        why: why.to_string(),
        commit_state: CommitState::NotSent,
        request: Some(request),
        http_status: 0,
        discord_error_code: 0,
        created_message_id: None,
        response_body: None,
    }
}

#[derive(Clone, Debug, Default)]
pub struct MockRestBackend {
    responses: VecDeque<Result<RestResponse, RestTransportError>>,
    requests: Vec<RestRequest>,
}

impl MockRestBackend {
    pub fn push_response(&mut self, response: RestResponse) {
        self.responses.push_back(Ok(response));
    }

    pub fn push_error(&mut self, error: RestTransportError) {
        self.responses.push_back(Err(error));
    }

    pub fn requests(&self) -> &[RestRequest] {
        &self.requests
    }
}

impl RestBackend for MockRestBackend {
    fn execute(&mut self, request: &RestRequest) -> Result<RestResponse, RestTransportError> {
        self.requests.push(request.clone());
        self.responses
            .pop_front()
            .unwrap_or_else(|| Ok(RestResponse::ok()))
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct HttpRestBackend {
    scheme: HttpRestScheme,
    host: String,
    port: u16,
    token: String,
    user_agent: String,
    timeout_ms: u64,
}

impl std::fmt::Debug for HttpRestBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpRestBackend")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &"[REDACTED]")
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HttpRestScheme {
    Http,
    Https,
}

impl HttpRestBackend {
    pub fn new(
        base_url: &str,
        token: impl Into<String>,
        user_agent: impl Into<String>,
        timeout_ms: u64,
    ) -> Result<Self, &'static str> {
        let (scheme, host, port) = parse_base_url(base_url)?;
        let token = token.into();
        let user_agent = user_agent.into();
        validate_bot_token_value(&token)?;
        validate_user_agent_header_value(&user_agent)?;
        Ok(Self {
            scheme,
            host,
            port,
            token,
            user_agent,
            timeout_ms: timeout_ms.max(1),
        })
    }

    fn request_bytes(&self, request: &RestRequest) -> Vec<u8> {
        let body = request.body_bytes();
        let mut out = String::new();
        out.push_str(method_text(request.method));
        out.push(' ');
        out.push_str(&request.path);
        out.push_str(" HTTP/1.1\r\n");
        out.push_str("Host: ");
        out.push_str(&self.host_header_value());
        out.push_str("\r\n");
        out.push_str("Authorization: Bot ");
        out.push_str(&self.token);
        out.push_str("\r\n");
        out.push_str("User-Agent: ");
        out.push_str(&self.user_agent);
        out.push_str("\r\n");
        out.push_str("Accept: application/json\r\n");
        if let Some(content_type) = &request.content_type {
            out.push_str("Content-Type: ");
            out.push_str(content_type);
            out.push_str("\r\n");
        }
        if let Some(reason) = &request.audit_reason_header {
            out.push_str("X-Audit-Log-Reason: ");
            out.push_str(reason);
            out.push_str("\r\n");
        }
        out.push_str("Content-Length: ");
        out.push_str(&body.len().to_string());
        out.push_str("\r\n");
        out.push_str("Connection: close\r\n\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    fn uses_default_port(&self) -> bool {
        matches!(
            (self.scheme, self.port),
            (HttpRestScheme::Http, 80) | (HttpRestScheme::Https, 443)
        )
    }

    fn host_header_value(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.uses_default_port() {
            host
        } else {
            format!("{}:{}", host, self.port)
        }
    }

    fn connect_tcp(&self) -> Result<NetworkStream, RestTransportError> {
        NetworkStream::connect(
            &self.host,
            self.port,
            Duration::from_millis(self.timeout_ms),
            self.scheme == HttpRestScheme::Http,
        )
        .map_err(|_| RestTransportError::NetworkBeforeSend)
    }
}

impl RestBackend for HttpRestBackend {
    fn execute(&mut self, request: &RestRequest) -> Result<RestResponse, RestTransportError> {
        if !validate_rest_path(&request.path)
            || request
                .content_type
                .as_deref()
                .is_some_and(|value| !valid_http_header_value(value))
            || request
                .audit_reason_header
                .as_deref()
                .is_some_and(|value| !valid_http_header_value(value))
        {
            return Err(RestTransportError::NetworkBeforeSend);
        }
        let stream = self.connect_tcp()?;
        let request_bytes = self.request_bytes(request);
        match self.scheme {
            HttpRestScheme::Http => {
                let mut stream = stream;
                execute_http_stream(&mut stream, &request_bytes)
            }
            HttpRestScheme::Https => {
                let connector =
                    TlsConnector::new().map_err(|_| RestTransportError::NetworkBeforeSend)?;
                let mut stream = connector
                    .connect(&self.host, stream)
                    .map_err(|_| RestTransportError::NetworkBeforeSend)?;
                execute_http_stream(&mut stream, &request_bytes)
            }
        }
    }
}

pub fn default_user_agent() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let repository = option_env!("CARGO_PKG_REPOSITORY").unwrap_or("");
    if repository.is_empty() {
        format!("dfor77/{version}")
    } else {
        format!("dfor77/{version} ({repository})")
    }
}

pub fn map_discord_error(
    action: RestAction,
    http_status: u16,
    discord_code: Option<i32>,
) -> RestOutcome {
    let (status, why) = match (http_status, discord_code) {
        (401, _) => (D7FOR, WHY_AUTH_INVALID),
        (400, Some(50035)) => (D7BAD, WHY_INVALID_FORM_BODY),
        (404, Some(10003)) => (D7NFD, "CHANNEL_NOT_FOUND"),
        (404, Some(10004)) => (D7NFD, "GUILD_NOT_FOUND"),
        (404, Some(10007)) => (D7NFD, "MEMBER_NOT_FOUND"),
        (404, Some(10008)) => (D7NFD, "MESSAGE_NOT_FOUND"),
        (404, Some(10013)) => (D7NFD, "USER_NOT_FOUND"),
        (403, Some(50001)) => (D7FOR, "MISSING_ACCESS"),
        (403, Some(50013)) => (D7FOR, "MISSING_PERMISSION"),
        (400 | 403, Some(50021)) => (D7FOR, "SYSTEM_MESSAGE_ACTION_FORBIDDEN"),
        (400, Some(50068)) => (D7FOR, "INVALID_MESSAGE_TYPE"),
        (400 | 403, Some(50083)) => (D7FOR, "THREAD_ARCHIVED"),
        (400 | 403, Some(160005)) => (D7FOR, "THREAD_LOCKED"),
        (429, _) | (400, Some(20016 | 20028 | 20029)) => (D7RTL, "RATE_LIMITED"),
        (400, Some(40058)) => (D7FOR, WHY_CANNOT_SEND_TO_FORUM_PARENT),
        (400, Some(50024)) => (D7FOR, WHY_CHANNEL_TYPE_UNSUPPORTED),
        (400, _) => (D7BAD, "UNKNOWN_BAD_REQUEST"),
        (403, _) => {
            if action == RestAction::ReadOnly {
                (D7FOR, "MISSING_ACCESS")
            } else {
                (D7FOR, "MISSING_PERMISSION")
            }
        }
        (404, _) => (D7NFD, fallback_not_found(action)),
        (500..=599, _) => {
            if action == RestAction::ReadOnly {
                (D7NET, "SNAPSHOT_INCOMPLETE")
            } else {
                (D7UNC, "UNKNOWN_COMMIT")
            }
        }
        _ => (D7BAD, "DISCORD_ERROR_UNMAPPED"),
    };
    RestOutcome {
        status,
        why: why.to_string(),
        commit_state: if (500..=599).contains(&http_status) && action != RestAction::ReadOnly {
            CommitState::Unknown
        } else {
            CommitState::Rejected
        },
        request: None,
        http_status: i32::from(http_status),
        discord_error_code: discord_code.unwrap_or(0),
        created_message_id: None,
        response_body: None,
    }
}

fn transport_error_outcome(request: RestRequest, error: RestTransportError) -> RestOutcome {
    let (status, why, commit_state) = match (request.action, error) {
        (_, RestTransportError::NetworkBeforeSend) => {
            (D7NET, "NETWORK_ERROR", CommitState::NotSent)
        }
        (_, RestTransportError::TimeoutBeforeSend) => (D7TIM, "TIMEOUT", CommitState::NotSent),
        (RestAction::ReadOnly, RestTransportError::TimeoutAfterSend) => {
            (D7TIM, "TIMEOUT", CommitState::Rejected)
        }
        (
            RestAction::ReadOnly,
            RestTransportError::ConnectionResetAfterSend | RestTransportError::EofAfterSend,
        ) => (D7NET, "NETWORK_ERROR", CommitState::Rejected),
        (
            _,
            RestTransportError::TimeoutAfterSend
            | RestTransportError::ConnectionResetAfterSend
            | RestTransportError::EofAfterSend,
        ) => (D7UNC, "UNKNOWN_COMMIT", CommitState::Unknown),
    };
    RestOutcome {
        status,
        why: why.to_string(),
        commit_state,
        request: Some(request),
        http_status: 0,
        discord_error_code: 0,
        created_message_id: None,
        response_body: None,
    }
}

pub fn common_headers(
    user_agent: &str,
    has_json_body: bool,
    audit_reason: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Authorization".to_string(), "Bot <redacted>".to_string()),
        ("User-Agent".to_string(), user_agent.to_string()),
    ];
    if has_json_body {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
    if let Some(reason) = audit_reason {
        headers.push(("X-Audit-Log-Reason".to_string(), reason.to_string()));
    }
    headers
}

fn execute_http_stream<S: Read + Write>(
    stream: &mut S,
    request_bytes: &[u8],
) -> Result<RestResponse, RestTransportError> {
    let mut written = 0;
    while written < request_bytes.len() {
        match stream.write(&request_bytes[written..]) {
            Ok(0) => {
                return Err(if written == 0 {
                    RestTransportError::NetworkBeforeSend
                } else {
                    RestTransportError::ConnectionResetAfterSend
                })
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                return Err(if written == 0 {
                    RestTransportError::NetworkBeforeSend
                } else {
                    RestTransportError::ConnectionResetAfterSend
                })
            }
        }
    }
    stream
        .flush()
        .map_err(|_| RestTransportError::ConnectionResetAfterSend)?;

    http::read_response(stream)
}

fn parse_base_url(value: &str) -> Result<(HttpRestScheme, String, u16), &'static str> {
    let origin = validate_rest_origin_for_token(value)?;
    let scheme = match origin.scheme {
        HttpOriginScheme::Http => HttpRestScheme::Http,
        HttpOriginScheme::Https => HttpRestScheme::Https,
    };
    let host = origin.host;
    let port = origin.port;
    Ok((scheme, host, port))
}

fn validate_user_agent_header_value(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("user agent must not be empty");
    }
    if value.bytes().any(|byte| byte < b' ' || byte == 0x7F) {
        return Err("user agent must not contain control characters");
    }
    Ok(())
}

fn method_text(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
    }
}

fn extract_discord_error_code(body: &str) -> Option<i32> {
    let body = parse_json(body).ok()?;
    match body.get("code")? {
        JsonValue::Number(text) | JsonValue::String(text) => text.parse::<i32>().ok(),
        _ => None,
    }
}

fn header<'a>(response: &'a RestResponse, key: &str) -> Option<&'a str> {
    header_from_map(&response.headers, key)
}

fn header_from_map<'a>(headers: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.as_str())
}

fn validate_json_payload(payload_json: &str) -> bool {
    !payload_json.is_empty()
        && payload_json.len() <= MAX_REST_JSON_BODY_BYTES
        && parse_json(payload_json).is_ok()
}

fn validate_json_object_payload(payload_json: &str) -> bool {
    if payload_json.is_empty() || payload_json.len() > MAX_REST_JSON_BODY_BYTES {
        return false;
    }
    matches!(parse_json(payload_json), Ok(JsonValue::Object(_)))
}

fn validate_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !matches!(value, "." | "..")
        && value.bytes().all(|byte| {
            (0x21..=0x7E).contains(&byte) && !matches!(byte, b'/' | b'?' | b'#' | b'\\' | b'%')
        })
}

fn validate_reaction_emoji(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && percent_encode(value).len() <= 512
        && !value.chars().any(|ch| ch.is_control())
}

pub(crate) fn validate_attachment_file_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| (0x20..=0x7E).contains(&byte) && !matches!(byte, b'"' | b'/' | b'\\'))
}

fn validate_poll_text(value: &str, max_chars: usize) -> bool {
    !value.is_empty() && value.chars().count() <= max_chars
}

fn sticker_message_payload_json(content: &str, sticker_ids: &[String], nonce: &str) -> String {
    let mut stickers = String::new();
    for (index, sticker_id) in sticker_ids.iter().enumerate() {
        if index > 0 {
            stickers.push(',');
        }
        stickers.push('"');
        stickers.push_str(sticker_id);
        stickers.push('"');
    }
    if content.is_empty() {
        format!(
            "{{\"allowed_mentions\":{{\"parse\":[]}},\"sticker_ids\":[{}],\"nonce\":\"{}\",\"enforce_nonce\":true}}",
            stickers, nonce
        )
    } else {
        format!(
            "{{\"content\":\"{}\",\"allowed_mentions\":{{\"parse\":[]}},\"sticker_ids\":[{}],\"nonce\":\"{}\",\"enforce_nonce\":true}}",
            json_escape(content),
            stickers,
            nonce
        )
    }
}

fn poll_message_payload_json(
    content: &str,
    question: &str,
    answers: &[String],
    duration_hours: u32,
    allow_multiselect: bool,
    nonce: &str,
) -> String {
    let mut answer_json = String::new();
    for (index, answer) in answers.iter().enumerate() {
        if index > 0 {
            answer_json.push(',');
        }
        answer_json.push_str(&format!(
            "{{\"poll_media\":{{\"text\":\"{}\"}}}}",
            json_escape(answer)
        ));
    }
    let duration_json = if duration_hours == 0 {
        String::new()
    } else {
        format!(",\"duration\":{}", duration_hours)
    };
    let poll = format!(
        "\"poll\":{{\"question\":{{\"text\":\"{}\"}},\"answers\":[{}]{}\
         ,\"allow_multiselect\":{},\"layout_type\":1}}",
        json_escape(question),
        answer_json,
        duration_json,
        if allow_multiselect { "true" } else { "false" }
    );
    if content.is_empty() {
        format!(
            "{{\"allowed_mentions\":{{\"parse\":[]}},{},\"nonce\":\"{}\",\"enforce_nonce\":true}}",
            poll, nonce
        )
    } else {
        format!(
            "{{\"content\":\"{}\",\"allowed_mentions\":{{\"parse\":[]}},{},\"nonce\":\"{}\",\"enforce_nonce\":true}}",
            json_escape(content),
            poll,
            nonce
        )
    }
}

fn attachment_payload_json(content: &str, files: &[AttachmentUpload], nonce: &str) -> String {
    let mut attachments_json = String::new();
    for (index, file) in files.iter().enumerate() {
        if index > 0 {
            attachments_json.push(',');
        }
        attachments_json.push_str(&format!(
            "{{\"id\":\"{}\",\"filename\":\"{}\"}}",
            index,
            json_escape(&file.file_name)
        ));
    }
    format!(
        "{{\"content\":\"{}\",\"allowed_mentions\":{{\"parse\":[]}},\"attachments\":[{}],\"nonce\":\"{}\",\"enforce_nonce\":true}}",
        json_escape(content),
        attachments_json,
        nonce
    )
}

fn multipart_message_body(
    boundary: &str,
    payload_json: &str,
    files: &[AttachmentUpload],
) -> Vec<u8> {
    let mut body = Vec::new();
    push_multipart_text(
        &mut body,
        boundary,
        "payload_json",
        "application/json",
        payload_json.as_bytes(),
    );
    for (index, file) in files.iter().enumerate() {
        let header = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"files[{}]\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
            boundary, index, file.file_name
        );
        body.extend_from_slice(header.as_bytes());
        body.extend_from_slice(&file.file_bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
    body
}

fn push_multipart_text(
    body: &mut Vec<u8>,
    boundary: &str,
    name: &str,
    content_type: &str,
    value: &[u8],
) {
    let header = format!(
        "--{}\r\nContent-Disposition: form-data; name=\"{}\"\r\nContent-Type: {}\r\n\r\n",
        boundary, name, content_type
    );
    body.extend_from_slice(header.as_bytes());
    body.extend_from_slice(value);
    body.extend_from_slice(b"\r\n");
}

fn validate_rest_path(path: &str) -> bool {
    path::RestPath::parse(path).is_some()
}

fn major_parameter_from_path(path: &path::RestPath) -> String {
    match path.resource_parts().as_slice() {
        ["channels" | "guilds" | "webhooks" | "applications" | "interactions", id, ..] => {
            path::canonical_id(id).unwrap_or_else(|| "global".to_string())
        }
        _ => "global".to_string(),
    }
}

fn action_from_path(method: HttpMethod, path: &path::RestPath) -> RestAction {
    if method == HttpMethod::Get {
        return RestAction::ReadOnly;
    }
    match path.resource_parts().as_slice() {
        ["interactions", _, _, "callback"] => RestAction::InteractionCallback,
        ["applications", _, "commands", ..] | ["applications", _, "guilds", _, "commands", ..] => {
            RestAction::ApplicationCommand
        }
        ["guilds", _, "roles", ..] => RestAction::RoleManagement,
        ["guilds", _, "members" | "bans", ..] => RestAction::MemberManagement,
        ["guilds", _, "voice-states", ..] => RestAction::GatewayVoice,
        ["guilds", _, ..] => RestAction::GuildManagement,
        ["channels", _, "typing"] => RestAction::TypingIndicator,
        ["channels", _, "messages", _, "reactions", ..] => RestAction::MessageReaction,
        ["channels", _, "messages", ..] => match method {
            HttpMethod::Post => RestAction::SendMessage,
            HttpMethod::Delete => RestAction::DeleteMessage,
            _ => RestAction::ChannelManagement,
        },
        ["channels", _, ..] => RestAction::ChannelManagement,
        _ => RestAction::LowLevel,
    }
}

fn audit_reason_header(reason: &str) -> Option<String> {
    if reason.is_empty() {
        None
    } else {
        Some(percent_encode(reason))
    }
}

fn fallback_not_found(action: RestAction) -> &'static str {
    match action {
        RestAction::SendMessage
        | RestAction::DeleteMessage
        | RestAction::MessageReaction
        | RestAction::TypingIndicator
        | RestAction::ChannelManagement => "CHANNEL_NOT_FOUND",
        RestAction::TimeoutMember
        | RestAction::UntimeoutMember
        | RestAction::MemberManagement
        | RestAction::GatewayVoice => "MEMBER_NOT_FOUND",
        RestAction::GuildManagement | RestAction::RoleManagement => "GUILD_NOT_FOUND",
        RestAction::InteractionCallback => "INTERACTION_NOT_FOUND",
        RestAction::ApplicationCommand => "APPLICATION_COMMAND_NOT_FOUND",
        RestAction::LowLevel => "RESOURCE_NOT_FOUND",
        RestAction::ReadOnly => "RESOURCE_NOT_FOUND",
    }
}

fn created_message_id(response: &RestResponse) -> Option<String> {
    parse_json(&response.body).ok().and_then(|value| {
        value
            .get("id")
            .and_then(|id| id.as_str())
            .filter(|id| validate_snowflake(id))
            .map(str::to_string)
    })
}

fn checked_timeout_until_ms(now_ms: u128, seconds: u64) -> Option<u128> {
    let delta_ms = u128::from(seconds).checked_mul(1000)?;
    let until_ms = now_ms.checked_add(delta_ms)?;
    let max_ms = (i64::MAX as u128).checked_mul(1000)?.checked_add(999)?;
    (until_ms <= max_ms).then_some(until_ms)
}

fn random_u128() -> Option<u128> {
    let mut file = File::open("/dev/urandom").ok()?;
    let mut bytes = [0_u8; 16];
    file.read_exact(&mut bytes).ok()?;
    Some(u128::from_be_bytes(bytes))
}

fn nonce_seed() -> u128 {
    random_u128().unwrap_or_else(fallback_nonce_seed)
}

fn fallback_nonce_seed() -> u128 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    now ^ (u128::from(std::process::id()) << 96)
}

fn base36(mut value: u128) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while value > 0 && out.len() < 25 {
        let digit = (value % 36) as usize;
        out.push(DIGITS[digit] as char);
        value /= 36;
    }
    out.iter().rev().collect()
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            ch if ch <= '\u{1F}' => out.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

fn utc_rfc3339_millis(ms_since_epoch: u128) -> String {
    let seconds = (ms_since_epoch / 1000) as i64;
    let millis = (ms_since_epoch % 1000) as u32;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_http_response(bytes: &[u8]) -> Result<RestResponse, RestTransportError> {
        http::read_response(&mut std::io::Cursor::new(bytes))
    }

    #[test]
    fn request_credentials_are_hidden_from_debug_output() {
        let backend = HttpRestBackend::new(
            "https://discord.com",
            "private-test-token",
            "test-agent",
            1000,
        )
        .unwrap();
        assert!(!format!("{backend:?}").contains("private-test-token"));
    }

    #[test]
    fn rejects_conflicting_or_oversized_response_framing() {
        for headers in [
            "Content-Length: 0\r\nTransfer-Encoding: chunked",
            "Transfer-Encoding: gzip, chunked",
            "Content-Length: +1",
            "Content-Length: 999999999",
            "Content-Length: 1\r\ncontent-length: 1",
        ] {
            let response = format!("HTTP/1.1 200 OK\r\n{headers}\r\n\r\n0\r\n\r\n");
            assert!(
                parse_http_response(response.as_bytes()).is_err(),
                "{headers}"
            );
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nX-Padding: {}\r\nContent-Length: 0\r\n\r\n",
            "a".repeat(MAX_HTTP_HEAD_BYTES)
        );
        assert!(parse_http_response(response.as_bytes()).is_err());
    }

    #[test]
    fn partial_request_write_is_never_classified_as_not_sent() {
        struct PartialWrite(bool);
        impl Read for PartialWrite {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Ok(0)
            }
        }
        impl Write for PartialWrite {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                if self.0 {
                    return Err(std::io::ErrorKind::BrokenPipe.into());
                }
                self.0 = true;
                Ok(1)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            execute_http_stream(&mut PartialWrite(false), b"POST /api/v10 HTTP/1.1\r\n\r\n")
                .unwrap_err(),
            RestTransportError::ConnectionResetAfterSend
        );
    }

    struct LargeResponseStream {
        prefix: Vec<u8>,
        offset: usize,
        remaining: usize,
        written: usize,
    }

    struct CompleteThenTimeoutStream {
        bytes: Vec<u8>,
        offset: usize,
        read_calls: usize,
        written: usize,
    }

    impl std::io::Read for LargeResponseStream {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.offset < self.prefix.len() {
                let read = buffer.len().min(self.prefix.len() - self.offset);
                buffer[..read].copy_from_slice(&self.prefix[self.offset..self.offset + read]);
                self.offset += read;
                return Ok(read);
            }
            if self.remaining == 0 {
                return Ok(0);
            }
            let read = buffer.len().min(self.remaining);
            buffer[..read].fill(b'a');
            self.remaining -= read;
            Ok(read)
        }
    }

    impl std::io::Read for CompleteThenTimeoutStream {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.read_calls += 1;
            if self.offset >= self.bytes.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "would block after complete response",
                ));
            }
            let read = buffer.len().min(self.bytes.len() - self.offset);
            buffer[..read].copy_from_slice(&self.bytes[self.offset..self.offset + read]);
            self.offset += read;
            Ok(read)
        }
    }

    impl std::io::Write for LargeResponseStream {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.written += buffer.len();
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl std::io::Write for CompleteThenTimeoutStream {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.written += buffer.len();
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn send_message_builds_required_json_body() {
        let mut planner = RestPlanner::new();
        let outcome = planner.send_message("100000000000000001", "hello @everyone");
        assert_eq!(outcome.status, D7OK);
        let request = outcome.request.unwrap();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.major_parameter, "100000000000000001");
        let body = request.body.unwrap();
        assert!(body.contains("\"allowed_mentions\":{\"parse\":[]}"));
        assert!(body.contains("\"enforce_nonce\":true"));
        assert!(body.contains("hello @everyone"));
        assert!(request.nonce.unwrap().len() <= 25);
    }

    #[test]
    fn rich_message_payload_and_attachment_builders_use_discord_message_route() {
        let mut planner = RestPlanner::new();
        let payload = r#"{"content":"hello","embeds":[{"title":"t"}],"components":[]}"#;
        let outcome = planner.send_message_payload("100000000000000001", payload);
        assert_eq!(outcome.status, D7OK);
        let request = outcome.request.unwrap();
        assert_eq!(
            request.path,
            "/api/v10/channels/100000000000000001/messages"
        );
        assert_eq!(request.content_type.as_deref(), Some("application/json"));
        assert_eq!(request.body.as_deref(), Some(payload));

        let upload = planner.send_message_attachment(
            "100000000000000001",
            "file content",
            "report.txt",
            b"abc\x00def".to_vec(),
        );
        assert_eq!(upload.status, D7OK);
        let request = upload.request.unwrap();
        assert!(request
            .content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("multipart/form-data; boundary=dfor77-")));
        let body = request.body_bytes();
        assert!(body
            .windows(b"name=\"payload_json\"".len())
            .any(|window| { window == b"name=\"payload_json\"" }));
        assert!(body
            .windows(b"filename=\"report.txt\"".len())
            .any(|window| { window == b"filename=\"report.txt\"" }));
        assert!(body
            .windows(b"abc\x00def".len())
            .any(|window| window == b"abc\x00def"));

        let multi = planner.send_message_attachments(
            "100000000000000001",
            "files",
            vec![
                AttachmentUpload {
                    file_name: "first.txt".to_string(),
                    file_bytes: b"one".to_vec(),
                },
                AttachmentUpload {
                    file_name: "second.bin".to_string(),
                    file_bytes: b"two".to_vec(),
                },
            ],
        );
        assert_eq!(multi.status, D7OK);
        let request = multi.request.unwrap();
        let payload_json = request.body.as_deref().unwrap();
        assert!(payload_json.contains(
            r#""attachments":[{"id":"0","filename":"first.txt"},{"id":"1","filename":"second.bin"}]"#
        ));
        let body = request.body_bytes();
        assert!(body
            .windows(b"name=\"files[0]\"; filename=\"first.txt\"".len())
            .any(|window| window == b"name=\"files[0]\"; filename=\"first.txt\""));
        assert!(body
            .windows(b"name=\"files[1]\"; filename=\"second.bin\"".len())
            .any(|window| window == b"name=\"files[1]\"; filename=\"second.bin\""));

        let stickers = planner.send_message_stickers(
            "100000000000000001",
            "sticker content",
            vec![
                "100000000000000002".to_string(),
                "100000000000000003".to_string(),
            ],
        );
        assert_eq!(stickers.status, D7OK);
        let request = stickers.request.unwrap();
        assert_eq!(
            request.path,
            "/api/v10/channels/100000000000000001/messages"
        );
        assert_eq!(request.content_type.as_deref(), Some("application/json"));
        let body = request.body.as_deref().unwrap();
        assert!(body.contains("\"content\":\"sticker content\""));
        assert!(body.contains("\"allowed_mentions\":{\"parse\":[]}"));
        assert!(body.contains(r#""sticker_ids":["100000000000000002","100000000000000003"]"#));
        assert!(body.contains("\"enforce_nonce\":true"));
        let invalid = planner.send_message_stickers(
            "100000000000000001",
            "",
            vec![
                "100000000000000002".to_string(),
                "100000000000000003".to_string(),
                "100000000000000004".to_string(),
                "100000000000000005".to_string(),
            ],
        );
        assert_eq!(invalid.status, D7ARG);

        let poll = planner.send_message_poll(
            "100000000000000001",
            "poll content",
            "Deploy now?",
            vec!["yes".to_string(), "no".to_string(), "later".to_string()],
            24,
            true,
        );
        assert_eq!(poll.status, D7OK);
        let request = poll.request.unwrap();
        assert_eq!(
            request.path,
            "/api/v10/channels/100000000000000001/messages"
        );
        assert_eq!(request.content_type.as_deref(), Some("application/json"));
        let body = request.body.as_deref().unwrap();
        assert!(body.contains("\"content\":\"poll content\""));
        assert!(body.contains("\"question\":{\"text\":\"Deploy now?\"}"));
        assert!(body.contains(
            r#""answers":[{"poll_media":{"text":"yes"}},{"poll_media":{"text":"no"}},{"poll_media":{"text":"later"}}]"#
        ));
        assert!(body.contains("\"duration\":24"));
        assert!(body.contains("\"allow_multiselect\":true"));
        assert!(body.contains("\"layout_type\":1"));

        let default_duration = planner.send_message_poll(
            "100000000000000001",
            "",
            "Deploy now?",
            vec!["yes".to_string(), "no".to_string()],
            0,
            false,
        );
        assert_eq!(default_duration.status, D7OK);
        let body = default_duration.request.unwrap().body.unwrap();
        assert!(!body.contains("\"duration\""));
        assert!(body.contains("\"allow_multiselect\":false"));

        let invalid_poll = planner.send_message_poll(
            "100000000000000001",
            "",
            "Deploy now?",
            vec!["yes".to_string()],
            24,
            false,
        );
        assert_eq!(invalid_poll.status, D7ARG);
    }

    #[test]
    fn raw_paths_cannot_escape_the_api_prefix_or_hide_separators_in_encoding() {
        let mut planner = RestPlanner::new();
        for path in [
            "/api/v10/../v9/guilds/10/roles/50",
            "/api/v10/%2e%2e/v9/guilds/10/roles/50",
            "/api/v10/%252e%252e/v9/guilds/10/roles/50",
            "/api/v10/guilds/10%2froles/50",
            "/api/v10/webhooks/50/a%2Fb/messages/30",
            "/api/v10/guilds/10/%5c..%5c../roles/50",
            "/api/v10/guilds/10/%0d%0aInjected/roles/50",
            "/api/v10/guilds/10/%00/roles/50",
            "/api/v10/guilds/10/roles/%",
            "/api/v10/guilds/10/roles/%GG",
        ] {
            let outcome = planner.raw_json_request(HttpMethod::Patch, path, Some("{}"), "");
            assert_eq!(outcome.status, D7ARG, "path={path}");
            assert!(outcome.request.is_none());
        }
    }

    #[test]
    fn encoded_resource_names_and_relative_segments_keep_the_same_action_and_major() {
        let mut planner = RestPlanner::new();
        for (path, action, major) in [
            (
                "/api/v10/%67uilds/%30%31%30/%72oles/050?filter=%2F..%2F",
                RestAction::RoleManagement,
                "10",
            ),
            (
                "/api/v10/channels/99/../%32%30/messages/030",
                RestAction::DeleteMessage,
                "20",
            ),
            (
                "/api/v10/%77ebhooks/050/%73ecret/messages/@original",
                RestAction::LowLevel,
                "50",
            ),
        ] {
            let outcome = planner.raw_json_request(HttpMethod::Delete, path, None, "");
            assert_eq!(outcome.status, D7OK);
            let request = outcome.request.unwrap();
            assert_eq!(request.path, path);
            assert_eq!(request.action, action, "path={path}");
            assert_eq!(request.major_parameter, major, "path={path}");
        }
    }

    #[test]
    fn interaction_command_and_raw_rest_builders_validate_paths_and_json() {
        let mut planner = RestPlanner::new();
        let callback = planner.interaction_callback(
            "100000000000000001",
            "token_abc",
            r#"{"type":4,"data":{"content":"ok"}}"#,
        );
        assert_eq!(callback.status, D7OK);
        assert_eq!(
            callback.request.unwrap().path,
            "/api/v10/interactions/100000000000000001/token_abc/callback"
        );

        let command = planner.application_command(
            "100000000000000010",
            Some("100000000000000011"),
            None,
            HttpMethod::Post,
            Some(r#"{"name":"ping","type":1,"description":"pong"}"#),
        );
        assert_eq!(command.status, D7OK);
        assert_eq!(
            command.request.unwrap().path,
            "/api/v10/applications/100000000000000010/guilds/100000000000000011/commands"
        );

        let raw = planner.raw_json_request(
            HttpMethod::Patch,
            "/api/v10/guilds/100000000000000011/roles/100000000000000012",
            Some(r#"{"name":"mod"}"#),
            "reason",
        );
        assert_eq!(raw.status, D7OK);
        let request = raw.request.unwrap();
        assert_eq!(request.action, RestAction::RoleManagement);
        assert_eq!(request.major_parameter, "100000000000000011");

        let raw_get = planner.raw_json_request(
            HttpMethod::Get,
            "/api/v10/guilds/100000000000000011/roles",
            None,
            "",
        );
        assert_eq!(raw_get.status, D7OK);
        assert_eq!(raw_get.request.unwrap().action, RestAction::ReadOnly);

        assert_eq!(
            planner
                .raw_json_request(
                    HttpMethod::Get,
                    "https://discord.com/api/v10/users/@me",
                    None,
                    ""
                )
                .status,
            D7ARG
        );
        assert_eq!(
            planner
                .send_message_payload("100000000000000001", r#"["not object"]"#)
                .status,
            D7ARG
        );
    }

    #[test]
    fn interaction_tokens_are_literal_segments_and_cannot_be_decoded_into_routes() {
        let mut planner = RestPlanner::new();
        for token in [
            ".", "..", "%2F", "%2f..%2f", "%252F", "%3F", "abc/def", "abc?def", "abc#def",
            "abc\\def", "abc def", "abc\n", "abc\0", "é",
        ] {
            let outcome = planner.interaction_callback("20", token, r#"{"type":5}"#);
            assert_eq!(outcome.status, D7ARG, "{token:?}");
            assert_eq!(outcome.commit_state, CommitState::NotSent);
            assert!(outcome.request.is_none());
        }
        for token in ["safe.token_-", "token_abc", "a..b", "@original"] {
            let outcome = planner.interaction_callback("20", token, r#"{"type":5}"#);
            assert_eq!(outcome.status, D7OK);
            assert_eq!(
                outcome.request.unwrap().path,
                format!("/api/v10/interactions/20/{token}/callback")
            );
        }
    }

    #[test]
    fn reaction_builder_encodes_emoji_and_selects_routes() {
        let mut planner = RestPlanner::new();
        let add = planner.message_reaction(
            "100000000000000001",
            "100000000000000002",
            "🔥",
            None,
            ReactionOperation::AddOwn,
        );
        assert_eq!(add.status, D7OK);
        let request = add.request.unwrap();
        assert_eq!(request.method, HttpMethod::Put);
        assert_eq!(request.action, RestAction::MessageReaction);
        assert_eq!(
            request.path,
            "/api/v10/channels/100000000000000001/messages/100000000000000002/reactions/%F0%9F%94%A5/@me"
        );

        let delete_user = planner.message_reaction(
            "100000000000000001",
            "100000000000000002",
            "party:100000000000000003",
            Some("100000000000000004"),
            ReactionOperation::DeleteUser,
        );
        assert_eq!(delete_user.status, D7OK);
        assert_eq!(
            delete_user.request.unwrap().path,
            "/api/v10/channels/100000000000000001/messages/100000000000000002/reactions/party%3A100000000000000003/100000000000000004"
        );

        let delete_all = planner.message_reaction(
            "100000000000000001",
            "100000000000000002",
            "",
            None,
            ReactionOperation::DeleteAll,
        );
        assert_eq!(delete_all.status, D7OK);
        assert_eq!(
            delete_all.request.unwrap().route_template,
            "DELETE /channels/{channel_id}/messages/{message_id}/reactions"
        );

        let invalid_user = planner.message_reaction(
            "100000000000000001",
            "100000000000000002",
            "🔥",
            Some("100000000000000004"),
            ReactionOperation::AddOwn,
        );
        assert_eq!(invalid_user.status, D7ARG);
    }

    #[test]
    fn send_message_nonce_is_unique_and_bounded() {
        let mut planner = RestPlanner::new();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..4096 {
            let outcome = planner.send_message("100000000000000001", "hello");
            assert_eq!(outcome.status, D7OK);
            let request = outcome.request.unwrap();
            let nonce = request.nonce.unwrap();
            assert!(nonce.len() <= 25, "{nonce}");
            assert!(
                nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase()),
                "{nonce}"
            );
            assert!(seen.insert(nonce));
        }
    }

    #[test]
    fn nonce_generator_uses_bounded_state() {
        assert!(std::mem::size_of::<NonceGenerator>() <= 32);
        let mut generator = NonceGenerator {
            seed: u128::MAX - 2,
            counter: 0,
        };
        let values = [
            generator.next_nonce(),
            generator.next_nonce(),
            generator.next_nonce(),
            generator.next_nonce(),
        ];
        let mut seen = std::collections::HashSet::new();
        for nonce in values {
            assert!(nonce.len() <= 25, "{nonce}");
            assert!(seen.insert(nonce));
        }
    }

    #[test]
    fn get_gateway_bot_builds_read_only_request() {
        let mut planner = RestPlanner::new();
        let outcome = planner.get_gateway_bot();
        assert_eq!(outcome.status, D7OK);
        let request = outcome.request.unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/api/v10/gateway/bot");
        assert_eq!(request.route_template, "GET /gateway/bot");
        assert_eq!(request.major_parameter, "global");
        assert_eq!(request.action, RestAction::ReadOnly);
        assert!(request.body.is_none());
        assert!(request.nonce.is_none());
    }

    #[test]
    fn get_current_user_builds_read_only_request() {
        let mut planner = RestPlanner::new();
        let outcome = planner.get_current_user();
        assert_eq!(outcome.status, D7OK);
        let request = outcome.request.unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/api/v10/users/@me");
        assert_eq!(request.route_template, "GET /users/@me");
        assert_eq!(request.major_parameter, "global");
        assert_eq!(request.action, RestAction::ReadOnly);
        assert!(request.body.is_none());
    }

    #[test]
    fn get_message_builds_read_only_request() {
        let mut planner = RestPlanner::new();
        let outcome = planner.get_message("100000000000000001", "100000000000000002");
        assert_eq!(outcome.status, D7OK);
        let request = outcome.request.unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(
            request.path,
            "/api/v10/channels/100000000000000001/messages/100000000000000002"
        );
        assert_eq!(
            request.route_template,
            "GET /channels/{channel_id}/messages/{message_id}"
        );
        assert_eq!(request.major_parameter, "100000000000000001");
        assert_eq!(request.action, RestAction::ReadOnly);
        assert!(request.body.is_none());
    }

    #[test]
    fn get_channel_builds_read_only_request() {
        let mut planner = RestPlanner::new();
        let outcome = planner.get_channel("100000000000000001");
        assert_eq!(outcome.status, D7OK);
        let request = outcome.request.unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/api/v10/channels/100000000000000001");
        assert_eq!(request.route_template, "GET /channels/{channel_id}");
        assert_eq!(request.major_parameter, "100000000000000001");
        assert_eq!(request.action, RestAction::ReadOnly);
        assert!(request.body.is_none());
    }

    #[test]
    fn get_thread_member_builds_read_only_request() {
        let mut planner = RestPlanner::new();
        let outcome = planner.get_thread_member("100000000000000001", "100000000000000002");
        assert_eq!(outcome.status, D7OK);
        let request = outcome.request.unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(
            request.path,
            "/api/v10/channels/100000000000000001/thread-members/100000000000000002"
        );
        assert_eq!(
            request.route_template,
            "GET /channels/{channel_id}/thread-members/{user_id}"
        );
        assert_eq!(request.major_parameter, "100000000000000001");
        assert_eq!(request.action, RestAction::ReadOnly);
        assert!(request.body.is_none());
    }

    #[test]
    fn get_archived_thread_routes_build_read_only_requests() {
        let mut planner = RestPlanner::new();
        let joined = planner
            .get_joined_private_archived_threads("100000000000000001")
            .request
            .unwrap();
        assert_eq!(
            joined.path,
            "/api/v10/channels/100000000000000001/users/@me/threads/archived/private"
        );
        assert_eq!(
            joined.route_template,
            "GET /channels/{channel_id}/users/@me/threads/archived/private"
        );
        assert_eq!(joined.major_parameter, "100000000000000001");
        assert_eq!(joined.action, RestAction::ReadOnly);

        let public = planner
            .get_archived_public_threads("100000000000000001")
            .request
            .unwrap();
        assert_eq!(
            public.path,
            "/api/v10/channels/100000000000000001/threads/archived/public"
        );
        assert_eq!(
            public.route_template,
            "GET /channels/{channel_id}/threads/archived/public"
        );
        assert_eq!(public.major_parameter, "100000000000000001");
        assert_eq!(public.action, RestAction::ReadOnly);

        let private = planner
            .get_archived_private_threads("100000000000000001")
            .request
            .unwrap();
        assert_eq!(
            private.path,
            "/api/v10/channels/100000000000000001/threads/archived/private"
        );
        assert_eq!(
            private.route_template,
            "GET /channels/{channel_id}/threads/archived/private"
        );
        assert_eq!(private.major_parameter, "100000000000000001");
        assert_eq!(private.action, RestAction::ReadOnly);
    }

    #[test]
    fn archived_thread_pages_validate_cursor_kinds_and_keep_the_route_bucket() {
        let mut planner = RestPlanner::new();
        let joined = planner
            .get_archived_threads_page("20", ArchivedThreadKind::JoinedPrivate, Some("40"), 100)
            .request
            .unwrap();
        assert_eq!(
            joined.path,
            "/api/v10/channels/20/users/@me/threads/archived/private?limit=100&before=40"
        );
        assert_eq!(
            joined.route_template,
            "GET /channels/{channel_id}/users/@me/threads/archived/private"
        );
        assert_eq!(joined.major_parameter, "20");
        assert_eq!(joined.action, RestAction::ReadOnly);
        for kind in [ArchivedThreadKind::Private, ArchivedThreadKind::Public] {
            let request = planner
                .get_archived_threads_page("20", kind, Some("2026-10-28T00:00:00.123456+09:00"), 50)
                .request
                .unwrap();
            let suffix = if kind == ArchivedThreadKind::Private {
                "private"
            } else {
                "public"
            };
            assert_eq!(request.path, format!("/api/v10/channels/20/threads/archived/{suffix}?limit=50&before=2026-10-28T00%3A00%3A00.123456%2B09%3A00"));
            assert_eq!(request.major_parameter, "20");
        }
        for (kind, cursor) in [
            (ArchivedThreadKind::JoinedPrivate, "0"),
            (ArchivedThreadKind::JoinedPrivate, "18446744073709551616"),
            (ArchivedThreadKind::JoinedPrivate, "40&limit=1"),
            (ArchivedThreadKind::Private, "40"),
            (ArchivedThreadKind::Private, "2025-02-29T00:00:00Z"),
            (
                ArchivedThreadKind::Public,
                "2026-10-28T00:00:00Z\r\nX-Injected: yes",
            ),
            (ArchivedThreadKind::Public, " 2026-10-28T00:00:00Z"),
        ] {
            let rejected = planner.get_archived_threads_page("20", kind, Some(cursor), 1);
            assert_eq!(rejected.status, D7ARG);
            assert!(rejected.request.is_none());
        }
        for limit in [0, 101, u16::MAX] {
            assert_eq!(
                planner
                    .get_archived_threads_page("20", ArchivedThreadKind::Private, None, limit)
                    .status,
                D7ARG
            );
        }
    }

    #[test]
    fn get_guild_member_builds_read_only_request() {
        let mut planner = RestPlanner::new();
        let outcome = planner.get_guild_member("100000000000000001", "100000000000000002");
        assert_eq!(outcome.status, D7OK);
        let request = outcome.request.unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(
            request.path,
            "/api/v10/guilds/100000000000000001/members/100000000000000002"
        );
        assert_eq!(
            request.route_template,
            "GET /guilds/{guild_id}/members/{user_id}"
        );
        assert_eq!(request.major_parameter, "100000000000000001");
        assert_eq!(request.action, RestAction::ReadOnly);
    }

    #[test]
    fn get_guild_roles_builds_read_only_request() {
        let mut planner = RestPlanner::new();
        let outcome = planner.get_guild_roles("100000000000000001");
        assert_eq!(outcome.status, D7OK);
        let request = outcome.request.unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/api/v10/guilds/100000000000000001/roles");
        assert_eq!(request.route_template, "GET /guilds/{guild_id}/roles");
        assert_eq!(request.major_parameter, "100000000000000001");
        assert_eq!(request.action, RestAction::ReadOnly);
    }

    #[test]
    fn http_rest_backend_accepts_https_discord_origin() {
        let mut planner = RestPlanner::new();
        let request = planner.get_gateway_bot().request.unwrap();
        let backend =
            HttpRestBackend::new("https://discord.com", "secret", "dfor77-test", 1000).unwrap();
        assert_eq!(backend.scheme, HttpRestScheme::Https);
        assert_eq!(backend.host, "discord.com");
        assert_eq!(backend.port, 443);
        let request_text = String::from_utf8(backend.request_bytes(&request)).unwrap();
        assert!(request_text.starts_with("GET /api/v10/gateway/bot HTTP/1.1"));
        assert!(request_text.contains("\r\nHost: discord.com\r\n"));
        assert!(request_text.contains("\r\nAuthorization: Bot secret\r\n"));
    }

    #[test]
    fn default_user_agent_includes_crate_version() {
        let value = default_user_agent();
        assert!(value.starts_with(&format!("dfor77/{}", env!("CARGO_PKG_VERSION"))));
        assert!(!value.contains('\n'));
        assert!(!value.contains('\r'));
    }

    #[test]
    fn http_rest_backend_preserves_non_default_http_port() {
        let mut planner = RestPlanner::new();
        let request = planner.get_gateway_bot().request.unwrap();
        let backend =
            HttpRestBackend::new("http://127.0.0.1:18080", "secret", "dfor77-test", 1000).unwrap();
        assert_eq!(backend.scheme, HttpRestScheme::Http);
        assert_eq!(backend.host, "127.0.0.1");
        assert_eq!(backend.port, 18080);
        let request_text = String::from_utf8(backend.request_bytes(&request)).unwrap();
        assert!(request_text.contains("\r\nHost: 127.0.0.1:18080\r\n"));

        let backend =
            HttpRestBackend::new("http://[::1]:18080", "secret", "dfor77-test", 1000).unwrap();
        assert_eq!(backend.host, "::1");
        assert_eq!(backend.port, 18080);
        let request_text = String::from_utf8(backend.request_bytes(&request)).unwrap();
        assert!(request_text.contains("\r\nHost: [::1]:18080\r\n"));

        let backend =
            HttpRestBackend::new("https://[2001:db8::1]", "secret", "dfor77-test", 1000).unwrap();
        assert_eq!(backend.host, "2001:db8::1");
        assert_eq!(backend.port, 443);
        let request_text = String::from_utf8(backend.request_bytes(&request)).unwrap();
        assert!(request_text.contains("\r\nHost: [2001:db8::1]\r\n"));
    }

    #[test]
    fn http_rest_backend_rejects_non_origin_base_urls() {
        for url in [
            "ftp://discord.com",
            "http://discord.com",
            "http://192.168.0.10:18080",
            "https://discord.com/api/v10",
            "https://discord.com?x=1",
            "https://user@discord.com",
            "https://discord.com:abc",
            "https://discord.com:0",
        ] {
            assert!(
                HttpRestBackend::new(url, "secret", "dfor77-test", 1000).is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn http_rest_backend_rejects_header_injection_values() {
        for token in ["", "secret token", "secret\nX-Bad: yes", "secret\rbad"] {
            assert!(
                HttpRestBackend::new("https://discord.com", token, "dfor77-test", 1000).is_err(),
                "{token:?}"
            );
        }
        for user_agent in ["", "dfor77\nX-Bad: yes", "dfor77\rbad"] {
            assert!(
                HttpRestBackend::new("https://discord.com", "secret", user_agent, 1000).is_err(),
                "{user_agent:?}"
            );
        }
    }

    #[test]
    fn action_builders_reject_bad_ids_before_send() {
        let mut planner = RestPlanner::new();
        let outcome = planner.delete_message("bad", "2", "");
        assert_eq!(outcome.status, D7ARG);
        assert_eq!(outcome.commit_state, CommitState::NotSent);
        assert!(outcome.request.is_none());
    }

    #[test]
    fn timeout_uses_utc_rfc3339_millis() {
        let mut planner = RestPlanner::new();
        let outcome = planner.timeout_member("1", "2", 60, "reason", 0);
        let body = outcome.request.unwrap().body.unwrap();
        assert_eq!(
            body,
            "{\"communication_disabled_until\":\"1970-01-01T00:01:00.000Z\"}"
        );
    }

    #[test]
    fn timeout_rejects_computed_until_overflow() {
        let mut planner = RestPlanner::new();
        let outcome = planner.timeout_member("1", "2", 60, "reason", u128::MAX);
        assert_eq!(outcome.status, D7ARG);
        assert_eq!(outcome.why, WHY_INVALID_FORM_BODY);
        assert!(outcome.request.is_none());
    }

    #[test]
    fn action_builders_handle_audit_reason_header_boundaries() {
        let mut planner = RestPlanner::new();
        let empty = planner.delete_message("1", "2", "").request.unwrap();
        assert_eq!(empty.audit_reason_header, None);

        let encoded = planner.delete_message("1", "2", "mod note / あ");
        assert_eq!(
            encoded.request.unwrap().audit_reason_header.as_deref(),
            Some("mod%20note%20%2F%20%E3%81%82")
        );

        let max_reason = "a".repeat(512);
        let max_request = planner
            .untimeout_member("1", "2", &max_reason)
            .request
            .unwrap();
        assert_eq!(
            max_request.audit_reason_header.as_deref(),
            Some(max_reason.as_str())
        );

        let too_long = "a".repeat(513);
        let rejected = planner.timeout_member("1", "2", 60, &too_long, 0);
        assert_eq!(rejected.status, D7ARG);
        assert_eq!(rejected.why, WHY_INVALID_FORM_BODY);
        assert!(rejected.request.is_none());
    }

    #[test]
    fn maps_discord_errors() {
        let outcome = map_discord_error(RestAction::DeleteMessage, 404, Some(10008));
        assert_eq!(outcome.status, D7NFD);
        assert_eq!(outcome.why, "MESSAGE_NOT_FOUND");
        let outcome = map_discord_error(RestAction::SendMessage, 500, None);
        assert_eq!(outcome.status, D7UNC);
        assert_eq!(outcome.commit_state, CommitState::Unknown);
        let outcome = map_discord_error(RestAction::ReadOnly, 500, None);
        assert_eq!(outcome.status, D7NET);
        assert_ne!(outcome.status, D7UNC);
        assert_eq!(outcome.commit_state, CommitState::Rejected);
    }

    #[test]
    fn read_only_transport_errors_do_not_become_unknown_commit() {
        let mut backend = MockRestBackend::default();
        backend.push_error(RestTransportError::TimeoutAfterSend);
        backend.push_error(RestTransportError::ConnectionResetAfterSend);
        let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let mut executor = RestExecutor::new(backend, budget);

        let timeout = executor.execute_get_gateway_bot(0);
        assert_eq!(timeout.status, D7TIM);
        assert_ne!(timeout.status, D7UNC);
        assert_eq!(timeout.commit_state, CommitState::Rejected);

        let network = executor.execute_get_current_user(0);
        assert_eq!(network.status, D7NET);
        assert_ne!(network.status, D7UNC);
        assert_eq!(network.commit_state, CommitState::Rejected);
    }

    #[test]
    fn parses_discord_error_code_from_json_body_only() {
        let response = parse_http_response(
            concat!(
                "HTTP/1.1 400 Bad Request\r\n",
                "Content-Type: application/json\r\n",
                "\r\n",
                r#"{"message":"quoted \"code\":50013","code":50035}"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(response.discord_error_code, Some(50035));

        let response = parse_http_response(
            concat!(
                "HTTP/1.1 400 Bad Request\r\n",
                "Content-Type: application/json\r\n",
                "\r\n",
                r#"{"message":"quoted \"code\":50013"}"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(response.discord_error_code, None);
    }

    #[test]
    fn rejects_invalid_utf8_http_response_body() {
        let error =
            parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n\xFF").unwrap_err();
        assert_eq!(error, RestTransportError::ConnectionResetAfterSend);
    }

    #[test]
    fn rejects_malformed_http_response_status_and_headers() {
        for response in [
            "HTTP/2 200 OK\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 99 Bad\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 OK\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nBad Header: yes\r\n\r\n",
            "HTTP/1.1 200 OK\r\nMissingColon\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: \u{7F}\r\n\r\n",
        ] {
            let error = parse_http_response(response.as_bytes()).unwrap_err();
            assert_eq!(error, RestTransportError::ConnectionResetAfterSend);
        }
    }

    #[test]
    fn parses_chunked_http_response_body() {
        let response = parse_http_response(
            concat!(
                "HTTP/1.1 403 Forbidden\r\n",
                "Content-Type: application/json\r\n",
                "Transfer-Encoding: chunked\r\n",
                "\r\n",
                "E;ignored=yes\r\n",
                "{\"code\":50013,",
                "\r\n",
                "20\r\n",
                "\"message\":\"Missing Permissions\"}",
                "\r\n",
                "0\r\n",
                "X-Trailer: ignored\r\n",
                "\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(response.http_status, 403);
        assert_eq!(response.discord_error_code, Some(50013));
        assert_eq!(
            response.body,
            r#"{"code":50013,"message":"Missing Permissions"}"#
        );
    }

    #[test]
    fn parses_content_length_response_without_reading_extra_bytes() {
        let response = parse_http_response(
            concat!(
                "HTTP/1.1 400 Bad Request\r\n",
                "Content-Type: application/json\r\n",
                "Content-Length: 14\r\n",
                "\r\n",
                "{\"code\":50035}",
                "ignored"
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(response.body, r#"{"code":50035}"#);
        assert_eq!(response.discord_error_code, Some(50035));
    }

    #[test]
    fn execute_http_stream_rejects_ambiguous_content_length() {
        let mut stream = CompleteThenTimeoutStream {
            bytes: concat!(
                "HTTP/1.1 200 OK\r\n",
                "content-length: 999\r\n",
                "Content-Length: 14\r\n",
                "x-ratelimit-bucket: old\r\n",
                "X-RateLimit-Bucket: new\r\n",
                "\r\n",
                "{\"id\":\"12345\"}",
                "ignored"
            )
            .as_bytes()
            .to_vec(),
            offset: 0,
            read_calls: 0,
            written: 0,
        };
        assert_eq!(
            execute_http_stream(&mut stream, b"GET / HTTP/1.1\r\n\r\n").unwrap_err(),
            RestTransportError::ConnectionResetAfterSend
        );
        assert_eq!(stream.read_calls, 1);
        assert!(stream.written > 0);
    }

    #[test]
    fn rejects_malformed_chunked_http_response_body() {
        let error = parse_http_response(
            concat!(
                "HTTP/1.1 400 Bad Request\r\n",
                "Transfer-Encoding: chunked\r\n",
                "\r\n",
                "10\r\n",
                "{\"code\":50035}"
            )
            .as_bytes(),
        )
        .unwrap_err();
        assert_eq!(error, RestTransportError::ConnectionResetAfterSend);
    }

    #[test]
    fn execute_http_stream_rejects_oversized_header() {
        let mut stream = LargeResponseStream {
            prefix: Vec::new(),
            offset: 0,
            remaining: MAX_HTTP_HEAD_BYTES + 1,
            written: 0,
        };
        let error = execute_http_stream(&mut stream, b"GET / HTTP/1.1\r\n\r\n").unwrap_err();
        assert_eq!(error, RestTransportError::ConnectionResetAfterSend);
        assert!(stream.written > 0);
    }

    #[test]
    fn execute_http_stream_rejects_oversized_response() {
        let mut stream = LargeResponseStream {
            prefix: format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_HTTP_RESPONSE_BYTES + 1
            )
            .into_bytes(),
            offset: 0,
            remaining: MAX_HTTP_RESPONSE_BYTES + 1,
            written: 0,
        };
        let error = execute_http_stream(&mut stream, b"GET / HTTP/1.1\r\n\r\n").unwrap_err();
        assert_eq!(error, RestTransportError::ConnectionResetAfterSend);
        assert!(stream.written > 0);
    }

    #[test]
    fn execute_http_stream_returns_after_complete_content_length_body() {
        let mut stream = CompleteThenTimeoutStream {
            bytes: concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Length: 14\r\n",
                "\r\n",
                "{\"id\":\"12345\"}"
            )
            .as_bytes()
            .to_vec(),
            offset: 0,
            read_calls: 0,
            written: 0,
        };
        let response = execute_http_stream(&mut stream, b"GET / HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(response.http_status, 200);
        assert_eq!(response.body, r#"{"id":"12345"}"#);
        assert_eq!(stream.read_calls, 1);
        assert!(stream.written > 0);
    }

    #[test]
    fn execute_http_stream_returns_after_complete_chunked_body() {
        let mut stream = CompleteThenTimeoutStream {
            bytes: concat!(
                "HTTP/1.1 403 ERR\r\n",
                "Transfer-Encoding: chunked\r\n",
                "\r\n",
                "E\r\n",
                "{\"code\":50013,",
                "\r\n",
                "20\r\n",
                "\"message\":\"Missing Permissions\"}",
                "\r\n",
                "0\r\n",
                "\r\n"
            )
            .as_bytes()
            .to_vec(),
            offset: 0,
            read_calls: 0,
            written: 0,
        };
        let response = execute_http_stream(&mut stream, b"GET / HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(response.http_status, 403);
        assert_eq!(response.discord_error_code, Some(50013));
        assert_eq!(stream.read_calls, 1);
        assert!(stream.written > 0);
    }

    #[test]
    fn invalid_budget_ignores_shared_429() {
        let mut budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let mut shared = RestResponse {
            http_status: 429,
            discord_error_code: None,
            body: String::new(),
            headers: HashMap::new(),
        };
        shared
            .headers
            .insert("X-RateLimit-Scope".to_string(), "shared".to_string());
        for now in 0..10 {
            budget.record(&shared, now);
        }
        assert!(!budget.should_warn(10));
        assert!(!budget.is_stopped(10));
    }

    #[test]
    fn invalid_budget_stops_and_recovers() {
        let mut budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let response = RestResponse {
            http_status: 403,
            discord_error_code: Some(50013),
            body: String::new(),
            headers: HashMap::new(),
        };
        for now in 0..4 {
            budget.record(&response, now);
        }
        assert!(budget.is_stopped(4));
        assert!(!budget.is_stopped(10 * 60 * 1000 + 10));
    }

    #[test]
    fn route_limiter_migrates_to_discord_bucket_key() {
        let mut planner = RestPlanner::new();
        let request = planner
            .send_message("100000000000000001", "hello")
            .request
            .unwrap();
        let mut response = RestResponse {
            http_status: 200,
            discord_error_code: None,
            body: String::new(),
            headers: HashMap::new(),
        };
        response
            .headers
            .insert("X-RateLimit-Bucket".to_string(), "abc".to_string());
        response
            .headers
            .insert("X-RateLimit-Reset-After".to_string(), "1.25".to_string());
        let mut limiter = RouteLimiter::default();
        limiter.observe(&request, &response, 100);
        assert_eq!(limiter.bucket_key_for(&request), "abc 100000000000000001");
        assert_eq!(retry_after_ms(&response, 100), Some(1250));
        assert_eq!(limiter.stop_until_ms_for(&request), None);
    }

    #[test]
    fn retry_after_uses_body_and_absolute_reset_headers() {
        let mut response = RestResponse {
            http_status: 429,
            discord_error_code: None,
            body: r#"{"retry_after":1.2}"#.to_string(),
            headers: HashMap::new(),
        };
        response
            .headers
            .insert("Retry-After".to_string(), "0.5".to_string());
        response
            .headers
            .insert("X-RateLimit-Reset-After".to_string(), "0.75".to_string());
        response
            .headers
            .insert("X-RateLimit-Reset".to_string(), "3.2501".to_string());
        assert_eq!(retry_after_ms(&response, 1000), Some(2251));
    }

    #[test]
    fn route_limiter_uses_body_retry_after_when_headers_are_missing() {
        let mut planner = RestPlanner::new();
        let request = planner
            .send_message("100000000000000001", "hello")
            .request
            .unwrap();
        let mut response = RestResponse {
            http_status: 429,
            discord_error_code: None,
            body: r#"{"retry_after":1.001}"#.to_string(),
            headers: HashMap::new(),
        };
        response
            .headers
            .insert("X-RateLimit-Bucket".to_string(), "abc".to_string());
        let mut limiter = RouteLimiter::default();
        limiter.observe(&request, &response, 100);
        assert_eq!(limiter.stop_until_ms_for(&request), Some(1101));
    }

    #[test]
    fn route_limiter_blocks_when_bucket_wait_exceeds_limit() {
        let mut planner = RestPlanner::new();
        let request = planner
            .send_message("100000000000000001", "hello")
            .request
            .unwrap();
        let mut response = RestResponse::ok();
        response
            .headers
            .insert("X-RateLimit-Bucket".to_string(), "abc".to_string());
        response
            .headers
            .insert("X-RateLimit-Remaining".to_string(), "0".to_string());
        response
            .headers
            .insert("X-RateLimit-Reset-After".to_string(), "2".to_string());
        let mut limiter = RouteLimiter::default();
        limiter.observe(&request, &response, 100);
        assert_eq!(limiter.stop_until_ms_for(&request), Some(2100));
        assert_eq!(
            limiter.try_acquire(&request, 500, 1000),
            RestAcquireDecision {
                allowed: false,
                required_wait_ms: 1600
            }
        );
        assert_eq!(
            limiter.try_acquire(&request, 500, 1600),
            RestAcquireDecision {
                allowed: true,
                required_wait_ms: 1600
            }
        );
    }

    #[test]
    fn executor_dates_relative_limits_from_response_receipt() {
        struct DelayedBackend;
        impl RestBackend for DelayedBackend {
            fn execute(&mut self, _: &RestRequest) -> Result<RestResponse, RestTransportError> {
                std::thread::sleep(Duration::from_millis(20));
                let mut response = RestResponse::ok();
                response.http_status = 429;
                response.body = r#"{"global":true,"retry_after":1}"#.into();
                Ok(response)
            }
        }
        let mut executor = RestExecutor::new(DelayedBackend, InvalidRequestBudget::default());
        let planned = executor.planner.get_current_user();
        let request = planned.request.as_ref().unwrap().clone();
        assert_eq!(executor.execute_planned(planned, 100).status, D7RTL);
        assert!(executor.route_limiter.stop_until_ms_for(&request).unwrap() >= 1120);
        assert!(executor.global_limiter.stop_until_ms().unwrap() >= 1120);
    }

    #[test]
    fn mock_executor_commits_2xx_response() {
        let mut backend = MockRestBackend::default();
        backend.push_response(RestResponse::ok());
        let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let mut executor = RestExecutor::new(backend, budget);
        let outcome = executor.execute_send_message("100000000000000001", "hello", 0);
        assert_eq!(outcome.status, D7OK);
        assert_eq!(outcome.commit_state, CommitState::Committed);
        assert_eq!(executor.backend.requests().len(), 1);
    }

    #[test]
    fn mock_executor_gets_gateway_bot_response_body() {
        let mut backend = MockRestBackend::default();
        let mut response = RestResponse::ok();
        response.body = r#"{"url":"wss://gateway.discord.gg","session_start_limit":{"total":1000,"remaining":1,"reset_after":60000,"max_concurrency":16}}"#.to_string();
        backend.push_response(response);
        let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let mut executor = RestExecutor::new(backend, budget);
        let outcome = executor.execute_get_gateway_bot(0);
        assert_eq!(outcome.status, D7OK);
        assert_eq!(outcome.commit_state, CommitState::Committed);
        assert!(outcome
            .response_body
            .as_deref()
            .is_some_and(|body| body.contains("session_start_limit")));
        let request = &executor.backend.requests()[0];
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/api/v10/gateway/bot");
    }

    #[test]
    fn mock_executor_raw_get_keeps_response_body_and_read_only_errors() {
        let mut backend = MockRestBackend::default();
        let mut response = RestResponse::ok();
        response.body = r#"{"items":[1,2]}"#.to_string();
        backend.push_response(response);
        backend.push_response(RestResponse::discord_error(500, 0));
        let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let mut executor = RestExecutor::new(backend, budget);

        let ok = executor.execute_raw_json_request(
            HttpMethod::Get,
            "/api/v10/custom/resource",
            None,
            "",
            0,
        );
        assert_eq!(ok.status, D7OK);
        assert_eq!(ok.response_body.as_deref(), Some(r#"{"items":[1,2]}"#));

        let failed = executor.execute_raw_json_request(
            HttpMethod::Get,
            "/api/v10/custom/resource",
            None,
            "",
            0,
        );
        assert_eq!(failed.status, D7NET);
        assert_ne!(failed.status, D7UNC);
        assert_eq!(failed.commit_state, CommitState::Rejected);
    }

    #[test]
    fn successful_mutations_keep_nonempty_response_bodies_without_changing_empty_responses() {
        for method in [
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Patch,
            HttpMethod::Delete,
        ] {
            let mut backend = MockRestBackend::default();
            let mut response = RestResponse::ok();
            response.body = r#"{"id":"10","name":"created-resource"}"#.into();
            backend.push_response(response);
            backend.push_response(RestResponse::ok());
            backend.push_response(RestResponse::discord_error(403, 50013));
            let mut executor = RestExecutor::new(backend, InvalidRequestBudget::default());
            let outcome =
                executor.execute_raw_json_request(method, "/api/v10/custom/resource", None, "", 0);
            assert_eq!(outcome.status, D7OK);
            assert_eq!(outcome.commit_state, CommitState::Committed);
            assert_eq!(
                outcome.response_body.as_deref(),
                Some(r#"{"id":"10","name":"created-resource"}"#)
            );
            let empty =
                executor.execute_raw_json_request(method, "/api/v10/custom/resource", None, "", 0);
            assert_eq!(empty.status, D7OK);
            assert_eq!(empty.response_body, None);
            let failed =
                executor.execute_raw_json_request(method, "/api/v10/custom/resource", None, "", 0);
            assert_eq!(failed.status, D7FOR);
            assert_eq!(failed.response_body, None);
        }
    }

    #[test]
    fn response_ids_are_validated_before_they_become_audit_metadata() {
        for value in [
            "",
            "0",
            "-1",
            "not-an-id",
            "18446744073709551616",
            &"x".repeat(100_000),
        ] {
            let mut response = RestResponse::ok();
            response.body = format!(r#"{{"id":"{value}"}}"#);
            assert_eq!(created_message_id(&response), None);
        }
        let mut response = RestResponse::ok();
        response.body = r#"{"id":"18446744073709551615"}"#.into();
        assert_eq!(
            created_message_id(&response).as_deref(),
            Some("18446744073709551615")
        );
    }

    #[test]
    fn send_message_success_extracts_created_message_id() {
        let mut backend = MockRestBackend::default();
        let mut response = RestResponse::ok();
        response.body = r#"{"id":"100000000000000099"}"#.to_string();
        backend.push_response(response);
        let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let mut executor = RestExecutor::new(backend, budget);
        let outcome = executor.execute_send_message("100000000000000001", "hello", 0);
        assert_eq!(outcome.status, D7OK);
        assert_eq!(
            outcome.created_message_id.as_deref(),
            Some("100000000000000099")
        );
    }

    #[test]
    fn mock_executor_maps_4xx_response() {
        let mut backend = MockRestBackend::default();
        backend.push_response(RestResponse::discord_error(403, 50013));
        let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let mut executor = RestExecutor::new(backend, budget);
        let outcome =
            executor.execute_delete_message("100000000000000001", "100000000000000002", "", 0);
        assert_eq!(outcome.status, D7FOR);
        assert_eq!(outcome.why, "MISSING_PERMISSION");
        assert_eq!(outcome.commit_state, CommitState::Rejected);
    }

    #[test]
    fn mock_executor_maps_after_send_timeout_to_unknown_commit() {
        let mut backend = MockRestBackend::default();
        backend.push_error(RestTransportError::TimeoutAfterSend);
        let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let mut executor = RestExecutor::new(backend, budget);
        let outcome = executor.execute_send_message("100000000000000001", "hello", 0);
        assert_eq!(outcome.status, D7UNC);
        assert_eq!(outcome.commit_state, CommitState::Unknown);
    }

    #[test]
    fn mock_executor_blocks_when_invalid_budget_is_stopped() {
        let mut backend = MockRestBackend::default();
        backend.push_response(RestResponse::discord_error(403, 50013));
        backend.push_response(RestResponse::discord_error(403, 50013));
        backend.push_response(RestResponse::discord_error(403, 50013));
        backend.push_response(RestResponse::discord_error(403, 50013));
        let budget = InvalidRequestBudget::new(1, 2, 3).unwrap();
        let mut executor = RestExecutor::new(backend, budget);
        for now in 0..3 {
            let _ = executor.execute_send_message("100000000000000001", "hello", now);
        }
        let outcome = executor.execute_send_message("100000000000000001", "hello", 3);
        assert_eq!(outcome.status, D7INV);
        assert_eq!(outcome.commit_state, CommitState::NotSent);
        assert_eq!(executor.backend.requests().len(), 3);
    }

    #[test]
    fn rest_global_limiter_allows_50_per_second_and_waits_within_limit() {
        let mut limiter = RestGlobalLimiter::new();
        for _ in 0..50 {
            assert_eq!(
                limiter.try_acquire(0, 0),
                RestAcquireDecision {
                    allowed: true,
                    required_wait_ms: 0
                }
            );
        }
        assert_eq!(
            limiter.try_acquire(0, 0),
            RestAcquireDecision {
                allowed: false,
                required_wait_ms: 20
            }
        );
        assert_eq!(
            limiter.try_acquire(0, 20),
            RestAcquireDecision {
                allowed: true,
                required_wait_ms: 20
            }
        );
    }

    #[test]
    fn rest_global_limiter_large_elapsed_refills_without_wrapping() {
        let mut limiter = RestGlobalLimiter::new();
        for _ in 0..50 {
            assert!(limiter.try_acquire(0, 0).allowed);
        }
        assert_eq!(
            limiter.try_acquire(u64::MAX, 0),
            RestAcquireDecision {
                allowed: true,
                required_wait_ms: 0
            }
        );
    }

    #[test]
    fn rest_global_limiter_observes_global_429() {
        let mut limiter = RestGlobalLimiter::new();
        let mut response = RestResponse {
            http_status: 429,
            discord_error_code: None,
            body: String::new(),
            headers: HashMap::new(),
        };
        response
            .headers
            .insert("X-RateLimit-Global".to_string(), "true".to_string());
        response
            .headers
            .insert("Retry-After".to_string(), "1.5".to_string());
        limiter.observe_response(&response, 100);
        assert_eq!(limiter.stop_until_ms(), Some(1600));
        assert_eq!(
            limiter.try_acquire(100, 1000),
            RestAcquireDecision {
                allowed: false,
                required_wait_ms: 1500
            }
        );
        assert!(limiter.try_acquire(100, 1500).allowed);
    }

    #[test]
    fn executor_does_not_send_when_global_wait_exceeds_limit() {
        let backend = MockRestBackend::default();
        let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let mut executor = RestExecutor::new(backend, budget).with_global_max_wait_ms(0);
        for _ in 0..50 {
            let outcome = executor.execute_send_message("100000000000000001", "hello", 0);
            assert_eq!(outcome.status, D7OK);
        }
        let outcome = executor.execute_send_message("100000000000000001", "hello", 0);
        assert_eq!(outcome.status, D7RTL);
        assert_eq!(outcome.commit_state, CommitState::NotSent);
        assert!(outcome
            .request
            .as_ref()
            .is_some_and(|request| request.nonce.is_some()));
        assert_eq!(executor.backend.requests().len(), 50);
    }

    #[test]
    fn executor_does_not_send_when_route_bucket_wait_exceeds_limit() {
        let mut backend = MockRestBackend::default();
        let mut response = RestResponse::ok();
        response
            .headers
            .insert("X-RateLimit-Bucket".to_string(), "abc".to_string());
        response
            .headers
            .insert("X-RateLimit-Remaining".to_string(), "0".to_string());
        response
            .headers
            .insert("X-RateLimit-Reset-After".to_string(), "2".to_string());
        backend.push_response(response);
        let budget = InvalidRequestBudget::new(2, 3, 4).unwrap();
        let mut executor = RestExecutor::new(backend, budget).with_global_max_wait_ms(1000);
        let first = executor.execute_send_message("100000000000000001", "hello", 0);
        assert_eq!(first.status, D7OK);
        let second = executor.execute_send_message("100000000000000001", "hello", 100);
        assert_eq!(second.status, D7RTL);
        assert_eq!(second.commit_state, CommitState::NotSent);
        assert_eq!(executor.backend.requests().len(), 1);
    }
}
