//! Configured notifications use the same fenced mutation transport as server operations.
use crate::{
    operations::{DiscordOperations, read, sid, success},
    transport::FreshCheck,
};
use async_trait::async_trait;
use hyper::Method;
use oracle_core::{Error, ErrorCode, GuildId, ModuleId, PolicyContext, Result};
use oracle_modules::{
    ConfigurationPolicy, DispatchPermit, NotificationCheck, NotificationRequest,
    NotificationTransport,
};
use oracle_operations::executor::{DispatchFence, SendGuard, now};
use serde_json::{Value, json};
use serenity::all as discord;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct ModuleFence(DispatchPermit);
impl DispatchFence for ModuleFence {
    fn dispatch(&self, send: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        self.0.dispatch(send)?
    }
}
#[async_trait]
impl ConfigurationPolicy for DiscordOperations {
    async fn validate(
        &self,
        actor: &PolicyContext,
        guild: &GuildId,
        _module: &ModuleId,
        values: &Value,
    ) -> Result<()> {
        self.mutation_authority(actor, guild).await?;
        if let Some(destination) = values.get("destination") {
            let destination = destination
                .as_str()
                .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
            // Destinations for administrative event metadata must remain staff-only.
            self.validate_destination(actor, guild, destination, true)
                .await?;
        }
        Ok(())
    }
}
struct NotificationFresh<'a> {
    adapter: &'a DiscordOperations,
    request: &'a NotificationRequest,
    check: &'a dyn NotificationCheck,
}
#[async_trait]
impl FreshCheck for NotificationFresh<'_> {
    async fn check(&self) -> Result<()> {
        self.check.validate().await?;
        self.adapter
            .validate_destination(
                &self.request.actor,
                &self.request.guild,
                &self.request.destination,
                true,
            )
            .await
    }
}
fn message_body(text: &str) -> Result<Value> {
    if text.is_empty() || text.chars().count() > 2000 {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(json!({"content":text,"allowed_mentions":{"parse":[]}}))
}
#[async_trait]
impl NotificationTransport for DiscordOperations {
    async fn send(
        &self,
        request: &NotificationRequest,
        permit: &DispatchPermit,
        check: &dyn NotificationCheck,
        cancel: CancellationToken,
    ) -> Result<Value> {
        sid(&request.destination)?;
        let body = message_body(&request.text)?;
        let guard =
            SendGuard::with_fence(cancel, now() + 60, Arc::new(ModuleFence(permit.clone())));
        let fresh = NotificationFresh {
            adapter: self,
            request,
            check,
        };
        let path = format!("/api/v10/channels/{}/messages", request.destination);
        let response = success(
            self.writer
                .execute(Method::POST, &path, &body, &guard, &fresh)
                .await?,
        )?;
        let id = response
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::new(ErrorCode::UnknownOutcome))?;
        let message_id = discord::MessageId::new(sid(id)?);
        let channel_id = discord::ChannelId::new(sid(&request.destination)?);
        // Independent GET proves remote content; POST success alone is insufficient.
        let message = read(self.http.get_message(channel_id.into(), message_id)).await?;
        if message.id != message_id
            || message.channel_id.get() != channel_id.get()
            || message.author.id != self.bot_id().await?
            || message.content != request.text
            || message.mention_everyone()
        {
            return Err(Error::new(ErrorCode::UnknownOutcome));
        }
        Ok(json!({"message_id":id,"channel_id":request.destination,"verified":true}))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_mentions_are_disabled_and_size_is_unicode_bounded() {
        let body = message_body("@everyone <@123> hello").unwrap();
        assert_eq!(body["allowed_mentions"], json!({"parse":[]}));
        assert!(message_body(&"é".repeat(2000)).is_ok());
        assert!(message_body(&"é".repeat(2001)).is_err());
        assert!(message_body("").is_err());
    }
}
