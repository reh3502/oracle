//! Scoped run reminders. The caller journals an immutable request before sending;
//! an uncertain send is recovered by observation, never by repeating the POST.
use crate::{
    operations::{DiscordOperations, read, sid},
    transport::FreshCheck,
};
use async_trait::async_trait;
use hyper::Method;
use oracle_core::{Error, ErrorCode, GuildId, PolicyContext, Result};
use oracle_modules::DispatchPermit;
use oracle_operations::executor::{DispatchFence, SendGuard, now};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::all as discord;
use std::{collections::BTreeSet, future::Future, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

const EMOJI: &str = "%E2%9C%85";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunReminderMessage {
    pub guild: GuildId,
    pub channel: String,
    pub key: String,
    pub text: String,
    pub users: Vec<String>,
    pub role: Option<String>,
    pub attendance: bool,
}

#[async_trait]
pub trait RunReminderAuthority: Send + Sync {
    /// Re-read the persisted intent, destination, recipients and active generation.
    async fn validate(&self, request: &RunReminderMessage) -> Result<()>;
}

pub struct DiscordRunReminders {
    adapter: Arc<DiscordOperations>,
    authority: Arc<dyn RunReminderAuthority>,
}
struct Fence(DispatchPermit);
impl DispatchFence for Fence {
    fn dispatch(&self, send: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        self.0.dispatch(send)?
    }
}
struct Fresh<'a> {
    transport: &'a DiscordRunReminders,
    request: &'a RunReminderMessage,
    seed: bool,
}
#[async_trait]
impl FreshCheck for Fresh<'_> {
    async fn check(&self) -> Result<()> {
        self.transport.fresh(self.request, self.seed).await
    }
}
fn invalid() -> Error {
    Error::new(ErrorCode::InvalidInput)
}
fn unknown() -> Error {
    Error::new(ErrorCode::UnknownOutcome)
}
async fn bounded_write<T>(
    guard: &SendGuard,
    deadline: tokio::time::Instant,
    write: impl Future<Output = Result<T>>,
) -> Result<T> {
    let result = if deadline <= tokio::time::Instant::now() {
        Err(Error::new(ErrorCode::Cancelled))
    } else {
        tokio::time::timeout_at(deadline, write)
            .await
            .unwrap_or_else(|_| Err(Error::new(ErrorCode::Cancelled)))
    };
    if result.is_err() {
        // Freeze the evidence before inspecting it: a delayed connection driver
        // must not submit bytes after a timeout was classified as unsent.
        guard.revoke();
    }
    result.map_err(|error| {
        if guard.request_started() {
            unknown()
        } else {
            error
        }
    })
}
fn rejected_status(status: u16) -> Error {
    Error::new(match status {
        400 | 405 | 413 => ErrorCode::InvalidInput,
        401 | 403 => ErrorCode::ForbiddenPermission,
        404 => ErrorCode::NotFound,
        429 => ErrorCode::QuotaExceeded,
        _ => ErrorCode::UnknownOutcome,
    })
}

fn body(request: &RunReminderMessage) -> Result<Value> {
    sid(&request.channel)?;
    if request.key.is_empty()
        || request.key.len() > 128
        || !request
            .key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:".contains(&b))
        || request.text.is_empty()
        || request.users.len() > 9
        || (request.users.is_empty() && request.role.is_none())
        || request.users.iter().collect::<BTreeSet<_>>().len() != request.users.len()
        || (request.attendance && request.role.is_some())
    {
        return Err(invalid());
    }
    let mut mentions = Vec::new();
    for user in &request.users {
        sid(user)?;
        mentions.push(format!("<@{user}>"));
    }
    if let Some(role) = &request.role {
        sid(role)?;
        if role == request.guild.as_str() {
            return Err(invalid());
        }
        mentions.push(format!("<@&{role}>"));
    }
    let content = format!(
        "{}\n{}\n-# oracle-run-reminder:{}",
        mentions.join(" "),
        request.text,
        request.key
    );
    if content.chars().count() > 2000 {
        return Err(invalid());
    }
    Ok(
        json!({"content":content,"allowed_mentions":{"parse":[],"users":request.users,"roles":request.role.iter().collect::<Vec<_>>(),"replied_user":false}}),
    )
}

fn confirms(request: &RunReminderMessage, expected: &Value, bot: &str, message: &Value) -> bool {
    let ids = |field: &str, objects: bool| -> Option<BTreeSet<String>> {
        message
            .get(field)?
            .as_array()?
            .iter()
            .map(|v| {
                if objects {
                    v.get("id")?.as_str()
                } else {
                    v.as_str()
                }
                .map(str::to_owned)
            })
            .collect()
    };
    message["author"]["id"] == bot
        && message["author"]["bot"] == true
        && message["channel_id"] == request.channel
        && message
            .get("guild_id")
            .is_none_or(|v| v.is_null() || v == request.guild.as_str())
        && !message["webhook_id"].is_string()
        && message["content"] == expected["content"]
        && message["mention_everyone"] == false
        && ids("mentions", true) == Some(request.users.iter().cloned().collect())
        && ids("mention_roles", false) == Some(request.role.iter().cloned().collect())
}

impl DiscordRunReminders {
    pub fn new(adapter: Arc<DiscordOperations>, authority: Arc<dyn RunReminderAuthority>) -> Self {
        Self { adapter, authority }
    }
    async fn fresh(&self, request: &RunReminderMessage, seed: bool) -> Result<()> {
        body(request)?;
        self.authority.validate(request).await?;
        let snapshot = self
            .adapter
            .channel_mutation_authority(
                &PolicyContext::LocalOperator,
                &request.guild,
                &request.channel,
            )
            .await?;
        let channel = snapshot
            .channels
            .iter()
            .find(|c| c.id == request.channel)
            .ok_or_else(|| Error::new(ErrorCode::ForbiddenScope))?;
        let bits = oracle_operations::permissions::channel_permissions(
            request.guild.as_str(),
            &snapshot.owner,
            &snapshot.roles,
            &snapshot.bot,
            &channel.overwrites,
        )?;
        let required = discord::Permissions::VIEW_CHANNEL.bits()
            | discord::Permissions::SEND_MESSAGES.bits()
            | discord::Permissions::READ_MESSAGE_HISTORY.bits()
            | if seed {
                discord::Permissions::ADD_REACTIONS.bits()
            } else {
                0
            };
        if channel.kind != oracle_operations::structure::ChannelKind::Text
            || bits & required != required
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        if let Some(role) = &request.role {
            let roles = read(
                self.adapter
                    .http
                    .get_guild_roles(discord::GuildId::new(sid(request.guild.as_str())?)),
            )
            .await?;
            let role = roles
                .get(&discord::RoleId::new(sid(role)?))
                .ok_or_else(|| Error::new(ErrorCode::ForbiddenScope))?;
            if !role.mentionable() && bits & discord::Permissions::MENTION_EVERYONE.bits() == 0 {
                return Err(Error::new(ErrorCode::ForbiddenPermission));
            }
        }
        self.authority.validate(request).await
    }
    async fn verified_message(
        &self,
        request: &RunReminderMessage,
        id: &str,
    ) -> Result<discord::Message> {
        self.fresh(request, false).await?;
        let message = read(self.adapter.http.get_message(
            discord::GenericChannelId::new(sid(&request.channel)?),
            discord::MessageId::new(sid(id)?),
        ))
        .await?;
        let value = serde_json::to_value(&message).map_err(|_| invalid())?;
        if message.id.to_string() != id
            || !confirms(
                request,
                &body(request)?,
                &self.adapter.bot_id().await?.to_string(),
                &value,
            )
        {
            return Err(unknown());
        }
        self.authority.validate(request).await?;
        Ok(message)
    }
    /// Caller must durably mark submission possible before invoking this method.
    /// UnknownOutcome requires recovery, never a repeated POST. Other errors prove
    /// no send occurred or a definite rejection; the host may retry those later.
    pub async fn send(
        &self,
        request: &RunReminderMessage,
        permit: &DispatchPermit,
        cancel: CancellationToken,
    ) -> Result<String> {
        self.send_before(
            request,
            permit,
            cancel,
            tokio::time::Instant::now() + Duration::from_secs(15),
        )
        .await
    }
    /// The enclosing worker supplies an absolute deadline with enough remaining
    /// time to persist a known non-send result. Preparation time consumes this
    /// budget too; a slow preflight cannot outlive the worker's classifier.
    pub async fn send_before(
        &self,
        request: &RunReminderMessage,
        permit: &DispatchPermit,
        cancel: CancellationToken,
        deadline: tokio::time::Instant,
    ) -> Result<String> {
        let payload = body(request)?;
        let deadline = deadline.min(tokio::time::Instant::now() + Duration::from_secs(15));
        let guard = SendGuard::with_fence(cancel, now() + 15, Arc::new(Fence(permit.clone())));
        let fresh = Fresh {
            transport: self,
            request,
            seed: false,
        };
        let response = bounded_write(
            &guard,
            deadline,
            self.adapter.writer.execute(
                Method::POST,
                &format!("/api/v10/channels/{}/messages", request.channel),
                &payload,
                &guard,
                &fresh,
            ),
        )
        .await?;
        if !(200..300).contains(&response.status) {
            return Err(rejected_status(response.status));
        }
        let id = response.body["id"].as_str().ok_or_else(unknown)?;
        self.verified_message(request, id)
            .await
            .map_err(|_| unknown())?;
        Ok(id.to_owned())
    }
    /// Bounded history observation. None means unknown, never evidence to resend.
    pub async fn recover(&self, request: &RunReminderMessage) -> Result<Option<String>> {
        self.fresh(request, false).await?;
        let expected = body(request)?;
        let bot = self.adapter.bot_id().await?.to_string();
        let mut before = None;
        let mut found = BTreeSet::new();
        for _ in 0..10 {
            let messages = read(self.adapter.http.get_messages(
                discord::GenericChannelId::new(sid(&request.channel)?),
                before.map(discord::MessagePagination::Before),
                Some(100u8.try_into().map_err(|_| invalid())?),
            ))
            .await?;
            let count = messages.len();
            for message in &messages {
                if confirms(
                    request,
                    &expected,
                    &bot,
                    &serde_json::to_value(message).map_err(|_| invalid())?,
                ) {
                    found.insert(message.id.to_string());
                }
            }
            if count < 100 {
                break;
            }
            let next = messages.iter().map(|m| m.id).min().ok_or_else(unknown)?;
            if before.is_some_and(|last| next >= last) {
                return Err(unknown());
            }
            before = Some(next);
        }
        self.authority.validate(request).await?;
        Ok(if found.len() == 1 {
            found.into_iter().next()
        } else {
            None
        })
    }
    /// Idempotent PUT, scoped to an independently verified confirmed message.
    pub async fn seed_attendance(
        &self,
        request: &RunReminderMessage,
        id: &str,
        permit: &DispatchPermit,
        cancel: CancellationToken,
    ) -> Result<()> {
        if !request.attendance {
            return Err(invalid());
        }
        self.verified_message(request, id).await?;
        let guard = SendGuard::with_fence(cancel, now() + 60, Arc::new(Fence(permit.clone())));
        let fresh = Fresh {
            transport: self,
            request,
            seed: true,
        };
        let response = self
            .adapter
            .writer
            .execute(
                Method::PUT,
                &format!(
                    "/api/v10/channels/{}/messages/{id}/reactions/{EMOJI}/@me",
                    request.channel
                ),
                &json!({}),
                &guard,
                &fresh,
            )
            .await?;
        if response.status != 204 {
            return Err(unknown());
        }
        let message = self.verified_message(request, id).await?;
        let value = serde_json::to_value(message).map_err(|_| invalid())?;
        if !value["reactions"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["emoji"]["name"] == "✅" && row["me"] == true)
        }) {
            return Err(unknown());
        }
        Ok(())
    }
    /// Only a fully paginated successful read returns a set. Failures never mean
    /// nobody confirmed. REST observations do not establish reaction timestamps.
    pub async fn attendance(
        &self,
        request: &RunReminderMessage,
        id: &str,
    ) -> Result<BTreeSet<String>> {
        if !request.attendance {
            return Err(invalid());
        }
        self.verified_message(request, id).await?;
        let mut users = BTreeSet::new();
        for kind in [
            discord::ReactionTypes::Normal,
            discord::ReactionTypes::Burst,
        ] {
            let mut after = None;
            let mut complete = false;
            for _ in 0..100 {
                self.authority.validate(request).await?;
                let page = read(self.adapter.http.get_reaction_users(
                    discord::GenericChannelId::new(sid(&request.channel)?),
                    discord::MessageId::new(sid(id)?),
                    EMOJI,
                    Some(kind),
                    Some(100u8.try_into().map_err(|_| invalid())?),
                    after,
                ))
                .await?;
                let count = page.len();
                let next = page.iter().map(|u| u.id).max();
                for user in page {
                    if !user.bot() {
                        users.insert(user.id.to_string());
                    }
                }
                if count < 100 {
                    complete = true;
                    break;
                }
                let next = next.ok_or_else(unknown)?;
                if after.is_some_and(|last| next <= last) {
                    return Err(unknown());
                }
                after = Some(next);
            }
            if !complete {
                return Err(Error::new(ErrorCode::QuotaExceeded));
            }
        }
        self.verified_message(request, id).await?;
        Ok(users)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct WaitingFresh(std::sync::atomic::AtomicBool);
    #[async_trait]
    impl FreshCheck for WaitingFresh {
        async fn check(&self) -> Result<()> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::pending().await
        }
    }
    #[tokio::test]
    async fn pre_send_freshness_deadline_is_known_unsent_and_retryable() {
        let guard = SendGuard::new(CancellationToken::new(), now() + 15);
        let fresh = WaitingFresh(false.into());
        let result = bounded_write(
            &guard,
            tokio::time::Instant::now() + Duration::from_millis(20),
            async {
                fresh.check().await?;
                guard.mark_request_started()?;
                Ok(())
            },
        )
        .await;
        assert!(fresh.0.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!guard.request_started());
        assert_eq!(result.unwrap_err().code, ErrorCode::Cancelled);
        assert!(guard.mark_request_started().is_err());
    }
    #[tokio::test]
    async fn post_submission_deadline_remains_unknown_and_cannot_retry() {
        let guard = SendGuard::new(CancellationToken::new(), now() + 15);
        let result: Result<()> = bounded_write(
            &guard,
            tokio::time::Instant::now() + Duration::from_millis(20),
            async {
                guard.mark_request_started()?;
                std::future::pending().await
            },
        )
        .await;
        assert!(guard.request_started());
        assert_eq!(result.unwrap_err().code, ErrorCode::UnknownOutcome);
    }
    #[tokio::test]
    async fn exhausted_worker_budget_never_polls_the_write() {
        let guard = SendGuard::new(CancellationToken::new(), now() + 15);
        let result: Result<()> = bounded_write(&guard, tokio::time::Instant::now(), async {
            panic!("already expired worker must not start another write")
        })
        .await;
        assert_eq!(result.unwrap_err().code, ErrorCode::Cancelled);
        assert!(!guard.request_started());
    }
    fn request() -> RunReminderMessage {
        RunReminderMessage {
            guild: GuildId::new("100").unwrap(),
            channel: "200".into(),
            key: "RUN:check:1".into(),
            text: "React to confirm. @everyone <@999>".into(),
            users: vec!["300".into(), "400".into()],
            role: None,
            attendance: true,
        }
    }
    #[test]
    fn ambiguous_delivery_cannot_be_mistaken_for_a_retryable_rejection() {
        for status in [408, 500, 502, 503, 504] {
            assert_eq!(rejected_status(status).code, ErrorCode::UnknownOutcome);
        }
        for status in [400, 401, 403, 404, 405, 413, 429] {
            assert_ne!(rejected_status(status).code, ErrorCode::UnknownOutcome);
        }
    }
    #[test]
    fn only_explicit_roster_can_be_pinged() {
        let b = body(&request()).unwrap();
        assert_eq!(
            b["allowed_mentions"],
            json!({"parse":[],"users":["300","400"],"roles":[],"replied_user":false})
        );
        assert!(
            b["content"]
                .as_str()
                .unwrap()
                .starts_with("<@300> <@400>\n")
        );
    }
    #[test]
    fn invalid_scope_duplicates_and_oversize_are_rejected() {
        let mut r = request();
        r.role = Some("500".into());
        assert!(body(&r).is_err());
        r.attendance = false;
        assert!(body(&r).is_ok());
        r.role = Some("100".into());
        assert!(body(&r).is_err());
        r.role = None;
        r.users.push("300".into());
        assert!(body(&r).is_err());
        r.users = (300..310).map(|v| v.to_string()).collect();
        assert!(body(&r).is_err());
        r.users.clear();
        r.text = "é".repeat(2000);
        assert!(body(&r).is_err());
        r.text = "ok".into();
        r.key = "bad\nmarker".into();
        assert!(body(&r).is_err());
    }
    #[test]
    fn readback_must_match_message_identity_and_exact_mentions() {
        let r = request();
        let b = body(&r).unwrap();
        let message = json!({"author":{"id":"500","bot":true},"channel_id":"200","content":b["content"],"mention_everyone":false,"mentions":[{"id":"400"},{"id":"300"}],"mention_roles":[]});
        assert!(confirms(&r, &b, "500", &message));
        for (field, value) in [
            ("channel_id", json!("201")),
            ("mention_everyone", json!(true)),
            ("mentions", json!([])),
            ("mention_roles", json!(["600"])),
            ("webhook_id", json!("700")),
            ("guild_id", json!("101")),
        ] {
            let mut bad = message.clone();
            bad[field] = value;
            assert!(!confirms(&r, &b, "500", &bad), "{field}");
        }
    }
}
