use std::collections::HashSet;

use super::ChannelSnapshot;
use crate::json::{parse_json, JsonValue};

pub(crate) struct ArchivedThreadPage {
    pub threads: Vec<ChannelSnapshot>,
    pub has_more: bool,
}

impl ArchivedThreadPage {
    pub fn parse(
        body: &str,
        guild_id: &str,
        parent_id: &str,
        bot_user_id: &str,
        joined_only: bool,
        now_ms: u64,
    ) -> Option<Self> {
        let root = parse_json(body).ok()?;
        let entries = root.get("threads")?.as_array()?;
        let members = root.get("members")?.as_array()?;
        let has_more = root.get("has_more")?.as_bool()?;
        if entries.len() > 100 || members.len() > entries.len() || has_more && entries.is_empty() {
            return None;
        }
        let mut ids = HashSet::new();
        let mut previous = None;
        let mut threads = Vec::with_capacity(entries.len());
        for entry in entries {
            let thread = super::channel::full(entry, guild_id, "", now_ms, false)?;
            let numeric_id = thread.channel_id.parse::<u64>().ok()?;
            if !thread.complete
                || thread.guild_id != guild_id
                || thread.parent_channel_id.as_deref() != Some(parent_id)
                || thread.channel_type != 12
                || !ids.insert(numeric_id)
                || joined_only && previous.is_some_and(|id| numeric_id >= id)
            {
                return None;
            }
            for key in ["member", "thread_member"] {
                if entry.get(key).is_some_and(|member| {
                    !thread_member_matches(member, &thread.channel_id, bot_user_id, false)
                }) {
                    return None;
                }
            }
            if joined_only && thread.current_user_thread_joined == Some(false) {
                return None;
            }
            previous = Some(numeric_id);
            threads.push(thread);
        }
        let mut joined_ids = HashSet::new();
        for member in members {
            let id = member
                .get("id")
                .or_else(|| member.get("thread_id"))?
                .as_str()?;
            let numeric_id = id.parse::<u64>().ok()?;
            if !ids.contains(&numeric_id)
                || !joined_ids.insert(numeric_id)
                || !thread_member_matches(member, id, bot_user_id, true)
            {
                return None;
            }
        }
        for thread in &mut threads {
            let joined = joined_only
                || joined_ids.contains(&thread.channel_id.parse::<u64>().ok()?)
                || thread.current_user_thread_joined == Some(true);
            thread.current_user_thread_joined = Some(joined);
            thread.current_user_thread_joined_at_ms = Some(now_ms);
        }
        Some(Self { threads, has_more })
    }

    pub fn find(&self, thread_id: &str) -> Option<ChannelSnapshot> {
        self.threads
            .iter()
            .find(|thread| thread.channel_id == thread_id)
            .cloned()
    }

    pub fn last_id(&self) -> Option<u64> {
        self.threads.last()?.channel_id.parse().ok()
    }
}

pub(crate) fn thread_member_matches(
    member: &JsonValue,
    thread_id: &str,
    user_id: &str,
    require_id: bool,
) -> bool {
    matches!(member, JsonValue::Object(_))
        && (!require_id || member.get("id").is_some() || member.get("thread_id").is_some())
        && ["id", "thread_id"].into_iter().all(|key| {
            member
                .get(key)
                .is_none_or(|value| value.as_str() == Some(thread_id))
        })
        && member
            .get("user_id")
            .is_none_or(|value| value.as_str() == Some(user_id))
        && member.get("member").is_none_or(|guild_member| {
            matches!(guild_member, JsonValue::Object(_))
                && guild_member
                    .get("user")
                    .is_none_or(|user| user.get("id").and_then(JsonValue::as_str) == Some(user_id))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str) -> String {
        format!(
            r#"{{"id":"{id}","guild_id":"10","parent_id":"20","type":12,"thread_metadata":{{"archived":true,"locked":false}}}}"#
        )
    }

    fn page(threads: &str, members: &str, more: bool) -> String {
        format!(r#"{{"threads":[{threads}],"members":[{members}],"has_more":{more}}}"#)
    }

    #[test]
    fn joined_pages_prove_membership_and_private_pages_use_current_user_members() {
        let body = page(
            &format!("{},{}", thread("80"), thread("50")),
            r#"{"id":"50","user_id":"99"}"#,
            true,
        );
        let joined = ArchivedThreadPage::parse(&body, "10", "20", "99", true, 7).unwrap();
        assert!(joined.has_more);
        assert_eq!(joined.last_id(), Some(50));
        for thread in &joined.threads {
            assert_eq!(thread.current_user_thread_joined, Some(true));
            assert_eq!(thread.current_user_thread_joined_at_ms, Some(7));
        }
        let all = ArchivedThreadPage::parse(&body, "10", "20", "99", false, 7).unwrap();
        assert_eq!(
            all.find("50").unwrap().current_user_thread_joined,
            Some(true)
        );
        assert_eq!(
            all.find("80").unwrap().current_user_thread_joined,
            Some(false)
        );
        assert!(all.find("30").is_none());
    }

    #[test]
    fn malformed_archive_pages_cannot_supply_membership_or_a_cursor() {
        let valid = thread("50");
        let cases = vec![
            "{}".to_string(),
            r#"{"threads":[],"members":[],"has_more":0}"#.into(),
            page("", "", true),
            page(&format!("{valid},{valid}"), "", false),
            page(
                &valid.replace(r#""parent_id":"20""#, r#""parent_id":"21""#),
                "",
                false,
            ),
            page(
                &valid.replace(r#""guild_id":"10""#, r#""guild_id":"11""#),
                "",
                false,
            ),
            page(&valid.replace(r#""type":12"#, r#""type":11"#), "", false),
            page(&valid.replace(r#","locked":false"#, ""), "", false),
            page(&valid, r#"{"id":"50","user_id":"98"}"#, false),
            page(&valid, r#"{"id":"51","user_id":"99"}"#, false),
            page(
                &valid,
                r#"{"id":"50","thread_id":"51","user_id":"99"}"#,
                false,
            ),
            page(
                &format!("{},{}", thread("80"), thread("50")),
                r#"{"id":"50"},{"id":"50"}"#,
                false,
            ),
            page(
                &valid.replace(
                    r#""type":12"#,
                    r#""type":12,"member":{"id":"50","user_id":"98"}"#,
                ),
                "",
                false,
            ),
            page(&format!("{},{}", thread("50"), thread("80")), "", false),
        ];
        for body in cases {
            assert!(
                ArchivedThreadPage::parse(&body, "10", "20", "99", true, 0).is_none(),
                "{body}"
            );
        }
    }

    #[test]
    fn oversized_pages_and_standalone_member_entries_are_rejected() {
        let entries = (1..=101)
            .rev()
            .map(|id| thread(&id.to_string()))
            .collect::<Vec<_>>();
        assert!(ArchivedThreadPage::parse(
            &page(&entries[..100].join(","), "", true),
            "10",
            "20",
            "99",
            true,
            0
        )
        .is_some());
        assert!(ArchivedThreadPage::parse(
            &page(&entries.join(","), "", true),
            "10",
            "20",
            "99",
            true,
            0
        )
        .is_none());
        assert!(ArchivedThreadPage::parse(
            &page("", r#"{"id":"50","user_id":"99"}"#, false),
            "10",
            "20",
            "99",
            true,
            0
        )
        .is_none());
    }
}
