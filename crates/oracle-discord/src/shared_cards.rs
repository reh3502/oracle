//! Durable bot-message effects use the existing fenced, non-replaying write lane.
use crate::{
    operations::{DiscordOperations, read, sid},
    transport::FreshCheck,
};
use async_trait::async_trait;
use hyper::Method;
use oracle_core::{Error, ErrorCode, PolicyContext, Result};
use oracle_operations::{
    executor::{DispatchFence, SendGuard, now},
    shared_cards::{SharedCardTransport, SharedEffect, SharedObservation},
};
use serde_json::{Value, json};
use serenity::all as discord;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait SharedCardAuthority: Send + Sync {
    /// Return a fence bound to this exact current module, guild and configured target.
    async fn authorize(&self, effect: &SharedEffect) -> Result<Arc<dyn DispatchFence>>;
}
pub struct DiscordSharedCards {
    adapter: Arc<DiscordOperations>,
    authority: Arc<dyn SharedCardAuthority>,
}
impl DiscordSharedCards {
    pub fn new(adapter: Arc<DiscordOperations>, authority: Arc<dyn SharedCardAuthority>) -> Self {
        Self { adapter, authority }
    }
    async fn fresh(&self, effect: &SharedEffect) -> Result<()> {
        self.authority
            .authorize(effect)
            .await?
            .dispatch(&mut || Ok(()))?;
        let app = read(self.adapter.http.get_current_application_info()).await?;
        if app.id.to_string() != effect.target.application_id
            || self.adapter.bot_id().await?.to_string() != effect.target.bot_id
        {
            return Err(Error::new(ErrorCode::ForbiddenScope));
        }
        // Re-fetch the bot's complete channel permission facts together.
        let snapshot = self
            .adapter
            .channel_mutation_authority(
                &PolicyContext::LocalOperator,
                &effect.guild,
                &effect.target.channel_id,
            )
            .await?;
        let channel = snapshot
            .channels
            .iter()
            .find(|c| c.id == effect.target.channel_id)
            .ok_or_else(|| Error::new(ErrorCode::ForbiddenScope))?;
        let bits = oracle_operations::permissions::channel_permissions(
            effect.guild.as_str(),
            &snapshot.owner,
            &snapshot.roles,
            &snapshot.bot,
            &channel.overwrites,
        )?;
        let required = discord::Permissions::VIEW_CHANNEL.bits()
            | discord::Permissions::SEND_MESSAGES.bits()
            | discord::Permissions::EMBED_LINKS.bits()
            | discord::Permissions::READ_MESSAGE_HISTORY.bits();
        if channel.kind != oracle_operations::structure::ChannelKind::Text
            || bits & required != required
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        Ok(())
    }
    async fn observe_inner(&self, effect: &SharedEffect) -> Result<SharedObservation> {
        self.fresh(effect).await?;
        let channel = discord::GenericChannelId::new(sid(&effect.target.channel_id)?);
        if let Some(id) = &effect.message_id {
            let fetched = tokio::time::timeout(
                Duration::from_secs(10),
                self.adapter
                    .http
                    .get_message(channel, discord::MessageId::new(sid(id)?)),
            )
            .await;
            return match fetched {
                Ok(Ok(message)) => Ok(
                    if confirms(
                        effect,
                        &serde_json::to_value(message)
                            .map_err(|_| Error::new(ErrorCode::InvalidInput))?,
                    ) {
                        SharedObservation::Confirmed {
                            message_id: id.clone(),
                        }
                    } else {
                        SharedObservation::Unknown
                    },
                ),
                Ok(Err(failure)) if is_not_found(&failure) => {
                    // A permission change can hide a message behind 404. Require a
                    // second fresh channel/authority check before reporting absence.
                    if self.fresh(effect).await.is_ok() {
                        Ok(SharedObservation::Missing)
                    } else {
                        Ok(SharedObservation::Unknown)
                    }
                }
                _ => Ok(SharedObservation::Unknown),
            };
        }
        let messages = read(
            self.adapter.http.get_messages(
                channel,
                None,
                Some(
                    100u8
                        .try_into()
                        .map_err(|_| Error::new(ErrorCode::InvalidInput))?,
                ),
            ),
        )
        .await?;
        let mut matches = Vec::new();
        for message in messages {
            let id = message.id.to_string();
            if confirms(
                effect,
                &serde_json::to_value(message).map_err(|_| Error::new(ErrorCode::InvalidInput))?,
            ) {
                matches.push(id);
            }
        }
        Ok(match matches.as_slice() {
            [id] => SharedObservation::Confirmed {
                message_id: id.clone(),
            },
            _ => SharedObservation::Unknown,
        })
    }
}
struct SharedFresh<'a> {
    transport: &'a DiscordSharedCards,
    effect: &'a SharedEffect,
}
#[async_trait]
impl FreshCheck for SharedFresh<'_> {
    async fn check(&self) -> Result<()> {
        self.transport.fresh(self.effect).await
    }
}
fn is_not_found(error: &serenity::Error) -> bool {
    matches!(error,serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) if response.status_code.as_u16()==404)
}
fn validate_effect(effect: &SharedEffect) -> Result<()> {
    sid(&effect.target.channel_id)?;
    sid(&effect.target.application_id)?;
    sid(&effect.target.bot_id)?;
    if let Some(id) = &effect.message_id {
        sid(id)?;
    }
    let payload = &effect.payload;
    if effect.marker.is_empty()
        || effect.marker.len() > 128
        || effect.marker.chars().any(char::is_control)
        || payload.as_object().is_none_or(|o| {
            o.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "content" | "embeds" | "components" | "allowed_mentions" | "attachments"
                )
            })
        })
        || payload["allowed_mentions"] != json!({"parse":[]})
        || payload.get("attachments").is_some_and(|v| v != &json!([]))
        || payload
            .get("content")
            .is_some_and(|v| v.as_str().is_none_or(|s| s.encode_utf16().count() > 2000))
        || payload["embeds"].as_array().is_none_or(|v| v.len() != 1)
        || payload["components"].as_array().is_none_or(|v| v.len() > 5)
        || !payload["embeds"][0]["footer"]["text"]
            .as_str()
            .is_some_and(|s| s.contains(&effect.marker))
        || serde_json::to_vec(payload)
            .map_err(|_| Error::new(ErrorCode::InvalidInput))?
            .len()
            > 64 * 1024
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}
// Compare semantic payload fields only. Discord adds embed type/proxy dimensions,
// component IDs and default false booleans; those are not publication mutations.
fn canonical(value: &Value, component: bool) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.iter().map(|v| canonical(v, component)).collect())
        }
        Value::Object(object) => {
            let mut out = serde_json::Map::new();
            for (key, value) in object {
                if matches!(
                    key.as_str(),
                    "proxy_url" | "proxy_icon_url" | "height" | "width" | "id"
                ) || (!component && matches!(key.as_str(), "type" | "content_scan_version"))
                    || (matches!(key.as_str(), "disabled" | "default")
                        && value == &Value::Bool(false))
                    || (!component && key == "fields" && value == &json!([]))
                    || value.is_null()
                {
                    continue;
                }
                out.insert(key.clone(), canonical(value, component));
            }
            Value::Object(out)
        }
        value => value.clone(),
    }
}
fn confirms(effect: &SharedEffect, message: &Value) -> bool {
    if message["channel_id"] != effect.target.channel_id
        || message["guild_id"]
            .as_str()
            .is_some_and(|id| id != effect.guild.as_str())
        || message["author"]["id"] != effect.target.bot_id
        || message["author"]["bot"] != true
        || message["webhook_id"].is_string()
        || message["mention_everyone"] == true
        || message
            .get("mentions")
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        || message
            .get("mention_roles")
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        || message["application_id"]
            .as_str()
            .is_some_and(|id| id != effect.target.application_id)
        || message["id"].as_str().is_none_or(|id| sid(id).is_err())
        || effect
            .message_id
            .as_ref()
            .is_some_and(|id| message["id"] != *id)
    {
        return false;
    }
    if message
        .get("attachments")
        .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
    {
        return false;
    }
    message["content"].as_str().unwrap_or("") == effect.payload["content"].as_str().unwrap_or("")
        && canonical(&message["embeds"], false) == canonical(&effect.payload["embeds"], false)
        && canonical(&message["components"], true) == canonical(&effect.payload["components"], true)
}
#[async_trait]
impl SharedCardTransport for DiscordSharedCards {
    async fn execute(
        &self,
        effect: &SharedEffect,
        cancel: CancellationToken,
    ) -> Result<SharedObservation> {
        validate_effect(effect)?;
        let fence = match self.authority.authorize(effect).await {
            Ok(fence) => fence,
            Err(error) => {
                return Ok(SharedObservation::NotSent { reason: error.code });
            }
        };
        let guard = SendGuard::with_fence(cancel.clone(), now() + 60, fence);
        let fresh = SharedFresh {
            transport: self,
            effect,
        };
        let (method, path) = match &effect.message_id {
            Some(id) => (
                Method::PATCH,
                format!(
                    "/api/v10/channels/{}/messages/{id}",
                    effect.target.channel_id
                ),
            ),
            None => (
                Method::POST,
                format!("/api/v10/channels/{}/messages", effect.target.channel_id),
            ),
        };
        let outcome = self
            .adapter
            .writer
            .execute(method, &path, &effect.payload, &guard, &fresh)
            .await;
        match outcome {
            Ok(response) if (200..300).contains(&response.status) => {
                if let Some(id) = response.body["id"].as_str().filter(|id| sid(id).is_ok()) {
                    if effect
                        .message_id
                        .as_ref()
                        .is_some_and(|expected| expected != id)
                    {
                        return Ok(SharedObservation::Unknown);
                    }
                    let mut readback = effect.clone();
                    readback.message_id = Some(id.into());
                    let observation = self.observe(&readback, cancel).await?;
                    return Ok(
                        if effect.message_id.is_none()
                            && matches!(observation, SharedObservation::Missing)
                        {
                            SharedObservation::Unknown
                        } else {
                            observation
                        },
                    );
                }
                Ok(SharedObservation::Unknown)
            }
            Ok(response) if matches!(response.status, 400 | 401 | 403 | 404 | 405 | 413 | 429) => {
                Ok(SharedObservation::Rejected)
            }
            Err(error) if !guard.request_started() => {
                Ok(SharedObservation::NotSent { reason: error.code })
            }
            // A lost ACK or a 5xx may have committed. Never execute the effect again.
            _ => self.observe(effect, cancel).await,
        }
    }
    async fn observe(
        &self,
        effect: &SharedEffect,
        cancel: CancellationToken,
    ) -> Result<SharedObservation> {
        validate_effect(effect)?;
        tokio::select! {biased; _=cancel.cancelled()=>Ok(SharedObservation::Unknown),outcome=tokio::time::timeout(Duration::from_secs(20),self.observe_inner(effect))=>match outcome {Ok(value)=>value,Err(_)=>Err(Error::new(ErrorCode::UnknownOutcome))}}
    }
}

impl DiscordOperations {
    pub async fn shared_target(
        &self,
        guild: &oracle_core::GuildId,
        channel: &str,
        cancel: &CancellationToken,
    ) -> Result<oracle_operations::shared_cards::SharedTarget> {
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        let app = read(self.http.get_current_application_info()).await?;
        let bot = self.bot_id().await?;
        let snapshot = self
            .channel_mutation_authority(&PolicyContext::LocalOperator, guild, channel)
            .await?;
        let item = snapshot
            .channels
            .iter()
            .find(|item| item.id == channel)
            .ok_or_else(|| Error::new(ErrorCode::ForbiddenScope))?;
        let bits = oracle_operations::permissions::channel_permissions(
            guild.as_str(),
            &snapshot.owner,
            &snapshot.roles,
            &snapshot.bot,
            &item.overwrites,
        )?;
        let required = discord::Permissions::VIEW_CHANNEL.bits()
            | discord::Permissions::SEND_MESSAGES.bits()
            | discord::Permissions::EMBED_LINKS.bits()
            | discord::Permissions::READ_MESSAGE_HISTORY.bits();
        if item.kind != oracle_operations::structure::ChannelKind::Text
            || bits & required != required
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        Ok(oracle_operations::shared_cards::SharedTarget {
            channel_id: channel.into(),
            application_id: app.id.to_string(),
            bot_id: bot.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn effect() -> SharedEffect {
        SharedEffect {
            effect_id: "effect-one".into(),
            guild: "111".parse().unwrap(),
            module: "sample.runs".parse().unwrap(),
            run_id: "ABCD1234".into(),
            target: oracle_operations::shared_cards::SharedTarget {
                channel_id: "222".into(),
                application_id: "333".into(),
                bot_id: "444".into(),
            },
            message_id: None,
            desired_revision: 7,
            marker: "oracle:effect-one".into(),
            payload: json!({"content":"","embeds":[{"title":"Run","description":"Host name","fields":[],"footer":{"text":"oracle:effect-one"},"color":42}],"components":[{"type":1,"components":[{"type":2,"style":2,"label":"Join","custom_id":"os:opaque:join:tag"}]}],"allowed_mentions":{"parse":[]},"attachments":[]}),
        }
    }
    fn message(effect: &SharedEffect) -> Value {
        let mut v = effect.payload.clone();
        v["id"] = json!("555");
        v["channel_id"] = json!("222");
        v["author"] = json!({"id":"444","bot":true});
        v["application_id"] = json!("333");
        v["mention_everyone"] = json!(false);
        v["mentions"] = json!([]);
        v["mention_roles"] = json!([]);
        v.as_object_mut().unwrap().remove("allowed_mentions");
        v
    }

    #[test]
    fn real_discord_embed_and_component_roundtrip_preserves_frozen_meaning() {
        let e = effect();
        let mut m = message(&e);
        let embeds: Vec<discord::Embed> =
            serde_json::from_str(&e.payload["embeds"].to_string()).unwrap();
        let components: Vec<discord::Component> =
            serde_json::from_str(&e.payload["components"].to_string()).unwrap();
        m["embeds"] = serde_json::to_value(embeds).unwrap();
        m["components"] = serde_json::to_value(components).unwrap();
        assert!(confirms(&e, &m), "{m}");
    }
    #[test]
    fn exact_observation_ignores_only_discord_metadata() {
        let e = effect();
        validate_effect(&e).unwrap();
        let mut m = message(&e);
        m["embeds"][0]["type"] = json!("rich");
        m["embeds"][0]["content_scan_version"] = json!(0);
        m["components"][0]["id"] = json!(1);
        m["components"][0]["components"][0]["disabled"] = json!(false);
        assert!(confirms(&e, &m));
        m["components"][0]["components"][0]["disabled"] = json!(true);
        assert!(!confirms(&e, &m));
    }
    #[test]
    fn wrong_identity_marker_or_payload_never_confirms() {
        let e = effect();
        for (pointer, value) in [
            ("/channel_id", json!("999")),
            ("/author/id", json!("999")),
            ("/author/bot", json!(false)),
            ("/application_id", json!("999")),
            ("/embeds/0/footer/text", json!("different")),
            ("/embeds/0/title", json!("Changed")),
            ("/components/0/components/0/custom_id", json!("forged")),
            ("/mention_everyone", json!(true)),
        ] {
            let mut m = message(&e);
            *m.pointer_mut(pointer).unwrap() = value;
            assert!(!confirms(&e, &m), "{pointer}");
        }
        let mut m = message(&e);
        m["webhook_id"] = json!("999");
        assert!(!confirms(&e, &m));
        let mut bound = e.clone();
        bound.message_id = Some("777".into());
        assert!(!confirms(&bound, &message(&e)));
    }
    #[test]
    fn frozen_effect_cannot_enable_mentions_or_pick_unchecked_endpoints() {
        let mut e = effect();
        e.target.channel_id = "222/messages".into();
        assert!(validate_effect(&e).is_err());
        let mut e = effect();
        e.payload["allowed_mentions"] = json!({"parse":["everyone"]});
        assert!(validate_effect(&e).is_err());
        let mut e = effect();
        e.payload["flags"] = json!(64);
        assert!(validate_effect(&e).is_err());
        let mut e = effect();
        e.marker = "absent".into();
        assert!(validate_effect(&e).is_err());
    }
}
