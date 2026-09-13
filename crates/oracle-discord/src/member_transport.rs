//! Fresh member authorization and fenced edits of the authenticated interaction reply.
use crate::{
    operations::{DiscordOperations, channel_from_json, member, read, sid, success},
    transport::FreshCheck,
};
use async_trait::async_trait;
use hyper::Method;
use oracle_core::{
    Error, ErrorCode, Result,
    member_read::{MemberContext, MemberReadPermit},
};
use oracle_operations::{
    executor::{DispatchFence, SendGuard, now},
    permissions::{self, Role},
    structure::ChannelKind,
};
use serde_json::json;
use serenity::all as discord;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const TTL: Duration = Duration::from_secs(10);
const USE_APPLICATION_COMMANDS: u64 = 1 << 31;
fn denied() -> Error {
    Error::new(ErrorCode::ForbiddenPermission)
}

pub(crate) fn reply_path(application_id: u64, token: &str) -> Result<String> {
    if application_id == 0
        || token.is_empty()
        || token.len() > 256
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || token == "."
        || token == ".."
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(format!(
        "/api/v10/webhooks/{application_id}/{token}/messages/@original"
    ))
}
fn require_identity(
    context: &MemberContext,
    guild: &str,
    user: &str,
    channel: &str,
    kind: ChannelKind,
    permissions: u64,
) -> Result<()> {
    if context.guild.as_str() != guild
        || context.user.as_str() != user
        || context.channel != channel
    {
        return Err(Error::new(ErrorCode::ForbiddenScope));
    }
    if kind != ChannelKind::Text
        || permissions & (permissions::VIEW_CHANNEL | USE_APPLICATION_COMMANDS)
            != permissions::VIEW_CHANNEL | USE_APPLICATION_COMMANDS
    {
        return Err(denied());
    }
    Ok(())
}
impl DiscordOperations {
    /// Fetch every identity and permission fact through Discord HTTP, never cache.
    pub async fn refresh_member(&self, context: &MemberContext) -> Result<MemberContext> {
        let observed_at = Instant::now();
        tokio::time::timeout(TTL, async {
            let guild_id = discord::GuildId::new(sid(context.guild.as_str())?);
            let user_id = discord::UserId::new(sid(context.user.as_str())?);
            let channel_id = discord::ChannelId::new(sid(&context.channel)?);
            let (guild, user, channel) = tokio::try_join!(
                read(self.http.get_guild(guild_id)),
                read(self.http.get_member(guild_id, user_id)),
                read(self.http.get_channel(channel_id.into()))
            )?;
            if user.guild_id != guild_id || guild.id != guild_id || user.user.id != user_id {
                return Err(Error::new(ErrorCode::ForbiddenScope));
            }
            let channel = channel.guild().ok_or_else(denied)?;
            let channel = channel_from_json(
                &serde_json::to_value(channel).map_err(|_| denied())?,
                &context.guild,
            )?;
            let roles = guild
                .roles
                .iter()
                .map(|role| Role {
                    id: role.id.to_string(),
                    position: i64::from(role.position),
                    permissions: role.permissions.bits(),
                    managed: role.managed(),
                })
                .collect::<Vec<_>>();
            let user = member(&user);
            let bits = permissions::channel_permissions(
                context.guild.as_str(),
                &guild.owner_id.to_string(),
                &roles,
                &user,
                &channel.overwrites,
            )?;
            require_identity(
                context,
                &guild.id.to_string(),
                &user.id,
                &channel.id,
                channel.kind,
                bits,
            )?;
            if observed_at.elapsed() >= TTL {
                return Err(denied());
            }
            Ok(MemberContext {
                guild: context.guild.clone(),
                user: context.user.clone(),
                channel: context.channel.clone(),
                roles: user.roles.into_iter().collect(),
                observed_at,
            })
        })
        .await
        .map_err(|_| denied())?
    }

    /// Edit only the trusted interaction's original ephemeral reply.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_member_reply(
        &self,
        application_id: u64,
        interaction_token: &str,
        text: &str,
        context: &MemberContext,
        policy: &MemberReadPermit,
        fence: Arc<dyn DispatchFence>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let path = reply_path(application_id, interaction_token)?;
        if text.trim().is_empty()
            || text.encode_utf16().count() > 2000
            || text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        // The caller supplies the host response fence (registry, then policy).
        // Re-wrapping the same permit would recursively acquire its policy lock.
        let guard = SendGuard::with_fence(cancel, now() + 30, fence);
        let fresh = ReplyFresh {
            operations: self,
            context,
            policy,
        };
        success(
            self.writer
                .execute(
                    Method::PATCH,
                    &path,
                    &json!({"content":text,"allowed_mentions":{"parse":[]},"flags":68}),
                    &guard,
                    &fresh,
                )
                .await?,
        )?;
        Ok(())
    }
}
struct ReplyFresh<'a> {
    operations: &'a DiscordOperations,
    context: &'a MemberContext,
    policy: &'a MemberReadPermit,
}
#[async_trait]
impl FreshCheck for ReplyFresh<'_> {
    async fn check(&self) -> Result<()> {
        self.policy.check()?;
        let fresh = self.operations.refresh_member(self.context).await?;
        if fresh.roles != self.context.roles
            || fresh.guild != self.context.guild
            || fresh.user != self.context.user
            || fresh.channel != self.context.channel
        {
            return Err(denied());
        }
        self.operations
            .core
            .authorize_member_read(&fresh, &fresh.guild)
            .await?;
        self.policy.check()?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trusted_token_path_rejects_path_and_query_injection() {
        assert_eq!(
            reply_path(123, "abc.DEF_-123").unwrap(),
            "/api/v10/webhooks/123/abc.DEF_-123/messages/@original"
        );
        for token in ["", ".", "..", "a/b", "a?b", "a%2fb", "a@b", "a\r\nb", "é"] {
            assert!(reply_path(123, token).is_err());
        }
        assert!(reply_path(0, "abc").is_err());
        assert!(reply_path(123, &"a".repeat(257)).is_err());
    }
    #[test]
    fn identity_and_channel_permissions_fail_closed() {
        let context = MemberContext {
            guild: oracle_core::GuildId::new("100").unwrap(),
            user: oracle_core::UserId::new("200").unwrap(),
            channel: "300".into(),
            roles: Default::default(),
            observed_at: Instant::now(),
        };
        let all = permissions::VIEW_CHANNEL | USE_APPLICATION_COMMANDS;
        assert!(require_identity(&context, "100", "200", "300", ChannelKind::Text, all).is_ok());
        for bits in [0, permissions::VIEW_CHANNEL, USE_APPLICATION_COMMANDS] {
            assert!(
                require_identity(&context, "100", "200", "300", ChannelKind::Text, bits).is_err()
            );
        }
        for kind in [
            ChannelKind::Voice,
            ChannelKind::Category,
            ChannelKind::Other(11),
        ] {
            assert!(require_identity(&context, "100", "200", "300", kind, all).is_err());
        }
        for (guild, user, channel) in [
            ("101", "200", "300"),
            ("100", "201", "300"),
            ("100", "200", "301"),
        ] {
            assert!(
                require_identity(&context, guild, user, channel, ChannelKind::Text, all).is_err()
            );
        }
    }
}
