//! Concrete Discord HTTP adapter. Serenity types stay on this side of the boundary.
use crate::transport::{DiscordWriteClient, FreshCheck, WriteResponse};
use async_trait::async_trait;
use hyper::Method;
use oracle_core::{CoreService, Error, ErrorCode, GuildId, PolicyContext, Result};
use oracle_operations::{
    commands::{CommandBackend, PublishedCommand, canonical_definition},
    executor::{ChannelMutation, SendGuard, StructureBackend, now},
    permissions::{self, Member, Overwrite, OverwriteKind, Role},
    structure::{Channel, ChannelKind, Snapshot},
};
use serde_json::{Value, json};
use serenity::all as discord;
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::OnceCell;

const MANAGE_GUILD: u64 = 1 << 5;
fn error(code: ErrorCode) -> Error {
    Error::new(code)
}
pub(crate) fn sid(id: &str) -> Result<u64> {
    oracle_core::UserId::new(id).map_err(|_| error(ErrorCode::InvalidInput))?;
    id.parse().map_err(|_| error(ErrorCode::InvalidInput))
}
pub(crate) async fn read<T>(
    future: impl Future<Output = std::result::Result<T, serenity::Error>>,
) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .map_err(|_| error(ErrorCode::Io))?
        .map_err(|_| error(ErrorCode::Io))
}

pub struct DiscordOperations {
    pub(crate) core: Arc<CoreService>,
    pub(crate) http: Arc<discord::Http>,
    pub(crate) writer: Arc<DiscordWriteClient>,
    bot: OnceCell<discord::UserId>,
    application: OnceCell<discord::ApplicationId>,
}
impl DiscordOperations {
    pub fn new(token: discord::Token, core: Arc<CoreService>) -> Result<Self> {
        let secret = token
            .expose_secret()
            .strip_prefix("Bot ")
            .ok_or_else(|| error(ErrorCode::InvalidInput))?;
        let writer = Arc::new(DiscordWriteClient::new(secret.to_owned())?);
        Ok(Self {
            core,
            http: Arc::new(discord::Http::new(token)),
            writer,
            bot: OnceCell::new(),
            application: OnceCell::new(),
        })
    }
    pub(crate) async fn bot_id(&self) -> Result<discord::UserId> {
        self.bot
            .get_or_try_init(|| async { Ok(read(self.http.get_current_user()).await?.id) })
            .await
            .copied()
    }
    async fn application_id(&self) -> Result<discord::ApplicationId> {
        self.application
            .get_or_try_init(|| async {
                let id = read(self.http.get_current_application_info()).await?.id;
                self.http.set_application_id(id);
                Ok(id)
            })
            .await
            .copied()
    }
    async fn fresh_snapshot(&self, context: &PolicyContext, guild: &GuildId) -> Result<Snapshot> {
        self.core.status(context, Some(guild)).await?;
        let guild_id = discord::GuildId::new(sid(guild.as_str())?);
        let bot_id = self.bot_id().await?;
        let actor_id = match context {
            PolicyContext::LocalOperator => bot_id,
            PolicyContext::Discord {
                guild: scope, user, ..
            } => {
                if scope != guild {
                    return Err(error(ErrorCode::ForbiddenScope));
                }
                discord::UserId::new(sid(user.as_str())?)
            }
        };
        let (details, channels, actor, bot) = tokio::try_join!(
            read(self.http.get_guild(guild_id)),
            read(self.http.get_channels(guild_id)),
            read(self.http.get_member(guild_id, actor_id)),
            read(self.http.get_member(guild_id, bot_id))
        )?;
        if details.id != guild_id
            || actor.user.id != actor_id
            || bot.user.id != bot_id
            || actor.guild_id != guild_id
            || bot.guild_id != guild_id
        {
            return Err(error(ErrorCode::ForbiddenScope));
        }
        let roles = details
            .roles
            .iter()
            .map(|role| Role {
                id: role.id.to_string(),
                position: i64::from(role.position),
                permissions: role.permissions.bits(),
                managed: role.managed(),
            })
            .collect::<Vec<_>>();
        let actor = member(&actor);
        let bot = member(&bot);
        let complete = permissions::guild_permissions(
            guild.as_str(),
            &details.owner_id.to_string(),
            &roles,
            &bot,
        )? & permissions::ADMINISTRATOR
            != 0;
        let channels = channels
            .iter()
            .map(|channel| {
                let value =
                    serde_json::to_value(channel).map_err(|_| error(ErrorCode::InvalidInput))?;
                channel_from_json(&value, guild)
            })
            .collect::<Result<Vec<_>>>()?;
        Snapshot {
            guild: guild.clone(),
            owner: details.owner_id.to_string(),
            actor,
            bot,
            roles,
            channels,
            complete,
            observed_at: now(),
        }
        .visible()
    }
    pub(crate) async fn mutation_authority(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
    ) -> Result<Snapshot> {
        self.core.authorize_module(context, guild).await?;
        let snapshot = self.fresh_snapshot(context, guild).await?;
        if matches!(context, PolicyContext::Discord { .. })
            && permissions::guild_permissions(
                guild.as_str(),
                &snapshot.owner,
                &snapshot.roles,
                &snapshot.actor,
            )? & MANAGE_GUILD
                == 0
        {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
        Ok(snapshot)
    }
    /// Inspect a logging destination and its current audience, without sending a message.
    pub async fn validate_destination(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        id: &str,
        staff_only: bool,
    ) -> Result<()> {
        let snapshot = self.mutation_authority(context, guild).await?;
        let channel = snapshot
            .channels
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| error(ErrorCode::ForbiddenPermission))?;
        if channel.kind != ChannelKind::Text {
            return Err(error(ErrorCode::InvalidInput));
        }
        for member in [&snapshot.actor, &snapshot.bot] {
            let bits = permissions::channel_permissions(
                guild.as_str(),
                &snapshot.owner,
                &snapshot.roles,
                member,
                &channel.overwrites,
            )?;
            let required = permissions::VIEW_CHANNEL
                | permissions::SEND_MESSAGES
                | if member.id == snapshot.bot.id {
                    permissions::READ_MESSAGE_HISTORY
                } else {
                    0
                };
            // The bot also needs history permission for independent delivery readback.
            if bits & required != required {
                return Err(error(ErrorCode::ForbiddenPermission));
            }
        }
        if staff_only {
            require_private_roles(&snapshot, channel)?;
            for overwrite in &channel.overwrites {
                if overwrite.kind == OverwriteKind::Member
                    && overwrite.allow & permissions::VIEW_CHANNEL != 0
                {
                    let subject = read(self.http.get_member(
                        discord::GuildId::new(sid(guild.as_str())?),
                        discord::UserId::new(sid(&overwrite.id)?),
                    ))
                    .await?;
                    if subject.guild_id.to_string() != guild.as_str()
                        || subject.user.id.to_string() != overwrite.id
                    {
                        return Err(error(ErrorCode::ForbiddenScope));
                    }
                    let bits = permissions::guild_permissions(
                        guild.as_str(),
                        &snapshot.owner,
                        &snapshot.roles,
                        &member(&subject),
                    )?;
                    if bits
                        & (permissions::ADMINISTRATOR
                            | MANAGE_GUILD
                            | permissions::MANAGE_ROLES
                            | permissions::MANAGE_CHANNELS)
                        == 0
                    {
                        return Err(error(ErrorCode::ForbiddenPermission));
                    }
                }
            }
        }
        Ok(())
    }
}
fn require_private_roles(snapshot: &Snapshot, channel: &Channel) -> Result<()> {
    let subject_id = (1..=u64::MAX)
        .rev()
        .map(|id| id.to_string())
        .find(|id| {
            id != &snapshot.owner
                && !channel
                    .overwrites
                    .iter()
                    .any(|o| o.kind == OverwriteKind::Member && &o.id == id)
        })
        .ok_or_else(|| error(ErrorCode::InvalidInput))?;
    // Ordinary members may combine roles. Any visible non-staff role is rejected.
    for role in &snapshot.roles {
        if role.permissions
            & (permissions::ADMINISTRATOR
                | MANAGE_GUILD
                | permissions::MANAGE_ROLES
                | permissions::MANAGE_CHANNELS)
            != 0
        {
            continue;
        }
        let subject = Member {
            id: subject_id.clone(),
            roles: if role.id == snapshot.guild.as_str() {
                vec![]
            } else {
                vec![role.id.clone()]
            },
            timed_out: false,
        };
        if permissions::channel_permissions(
            snapshot.guild.as_str(),
            &snapshot.owner,
            &snapshot.roles,
            &subject,
            &channel.overwrites,
        )? & permissions::VIEW_CHANNEL
            != 0
        {
            return Err(error(ErrorCode::ForbiddenPermission));
        }
    }
    Ok(())
}
fn member(value: &discord::Member) -> Member {
    Member {
        id: value.user.id.to_string(),
        roles: value.roles.iter().map(ToString::to_string).collect(),
        timed_out: value
            .communication_disabled_until
            .is_some_and(|until| until.unix_timestamp() > now() as i64),
    }
}
fn string_id(value: &Value, key: &str) -> Result<String> {
    let id = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| error(ErrorCode::InvalidInput))?;
    sid(id)?;
    Ok(id.into())
}
fn bits(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| error(ErrorCode::InvalidInput))
}
pub(crate) fn channel_from_json(value: &Value, guild: &GuildId) -> Result<Channel> {
    let id = string_id(value, "id")?;
    if string_id(value, "guild_id")? != guild.as_str() {
        return Err(error(ErrorCode::ForbiddenScope));
    }
    let flags = value.get("flags").and_then(Value::as_u64).unwrap_or(0);
    if flags & (1 << 17) != 0 {
        return Err(error(ErrorCode::ForbiddenPermission));
    }
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error(ErrorCode::ForbiddenPermission))?
        .to_owned();
    let raw = value
        .get("type")
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(|| error(ErrorCode::InvalidInput))?;
    let kind = match raw {
        0 => ChannelKind::Text,
        2 => ChannelKind::Voice,
        4 => ChannelKind::Category,
        other => ChannelKind::Other(other),
    };
    let parent = match value.get("parent_id") {
        None | Some(Value::Null) => None,
        Some(_) => Some(string_id(value, "parent_id")?),
    };
    let overwrites = value
        .get("permission_overwrites")
        .and_then(Value::as_array)
        .ok_or_else(|| error(ErrorCode::InvalidInput))?
        .iter()
        .map(|o| {
            Ok(Overwrite {
                id: string_id(o, "id")?,
                kind: match o.get("type").and_then(Value::as_u64) {
                    Some(0) => OverwriteKind::Role,
                    Some(1) => OverwriteKind::Member,
                    _ => return Err(error(ErrorCode::InvalidInput)),
                },
                allow: bits(o, "allow")?,
                deny: bits(o, "deny")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Channel {
        id,
        guild: guild.clone(),
        parent,
        kind,
        name,
        overwrites,
    })
}
fn mutation_body(mutation: &ChannelMutation) -> Result<Value> {
    let type_id = match mutation.desired.kind {
        ChannelKind::Text => 0,
        ChannelKind::Voice => 2,
        ChannelKind::Category => 4,
        ChannelKind::Other(_) => return Err(error(ErrorCode::InvalidInput)),
    };
    let access=mutation.desired.overwrites.iter().map(|o|json!({"id":o.id,"type":if o.kind==OverwriteKind::Role{0}else{1},"allow":o.allow.to_string(),"deny":o.deny.to_string()})).collect::<Vec<_>>();
    let mut body = json!({"name":mutation.desired.name,"parent_id":mutation.desired.parent,"permission_overwrites":access});
    if mutation.before.is_none() {
        body["type"] = Value::from(type_id);
    }
    Ok(body)
}
pub(crate) fn success(response: WriteResponse) -> Result<Value> {
    match response.status {
        200..=299 => Ok(response.body),
        401 | 403 => Err(error(ErrorCode::ForbiddenPermission)),
        404 => Err(error(ErrorCode::NotFound)),
        400 | 409 => Err(error(ErrorCode::Conflict)),
        429 => Err(error(ErrorCode::QuotaExceeded)),
        _ => Err(error(ErrorCode::UnknownOutcome)),
    }
}
struct StructureFresh<'a> {
    adapter: &'a DiscordOperations,
    context: &'a PolicyContext,
    guild: &'a GuildId,
    expected: &'a str,
}
#[async_trait]
impl FreshCheck for StructureFresh<'_> {
    async fn check(&self) -> Result<()> {
        if self
            .adapter
            .mutation_authority(self.context, self.guild)
            .await?
            .fingerprint()?
            != self.expected
        {
            return Err(error(ErrorCode::Conflict));
        }
        Ok(())
    }
}
#[async_trait]
impl StructureBackend for DiscordOperations {
    async fn inspect(&self, context: &PolicyContext, guild: &GuildId) -> Result<Snapshot> {
        self.fresh_snapshot(context, guild).await
    }
    async fn mutate(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        mutation: &ChannelMutation,
        guard: &SendGuard,
    ) -> Result<Channel> {
        if mutation.desired.guild != *guild
            || mutation
                .before
                .as_ref()
                .is_some_and(|before| before.guild != *guild || before.id != mutation.desired.id)
        {
            return Err(error(ErrorCode::ForbiddenScope));
        }
        let (method, path) = if let Some(before) = &mutation.before {
            sid(&before.id)?;
            (Method::PATCH, format!("/api/v10/channels/{}", before.id))
        } else {
            (Method::POST, format!("/api/v10/guilds/{guild}/channels"))
        };
        let body = mutation_body(mutation)?;
        let fresh = StructureFresh {
            adapter: self,
            context,
            guild,
            expected: &mutation.expected_fingerprint,
        };
        let value = success(
            self.writer
                .execute(method, &path, &body, guard, &fresh)
                .await?,
        )?;
        let channel = channel_from_json(&value, guild)?;
        if mutation
            .before
            .as_ref()
            .is_some_and(|before| before.id != channel.id)
        {
            return Err(error(ErrorCode::UnknownOutcome));
        }
        Ok(channel)
    }
}
fn command_from_json(value: Value) -> Result<PublishedCommand> {
    Ok(PublishedCommand {
        id: string_id(&value, "id")?,
        definition: canonical_definition(&value)?,
    })
}
struct CommandsFresh<'a> {
    adapter: &'a DiscordOperations,
    guild: &'a GuildId,
    baseline: Vec<PublishedCommand>,
}
#[async_trait]
impl FreshCheck for CommandsFresh<'_> {
    async fn check(&self) -> Result<()> {
        self.adapter
            .core
            .status(&PolicyContext::LocalOperator, Some(self.guild))
            .await?;
        if self.adapter.list(self.guild).await? != self.baseline {
            return Err(error(ErrorCode::Conflict));
        }
        Ok(())
    }
}
#[async_trait]
impl CommandBackend for DiscordOperations {
    async fn list(&self, guild: &GuildId) -> Result<Vec<PublishedCommand>> {
        self.core
            .status(&PolicyContext::LocalOperator, Some(guild))
            .await?;
        self.application_id().await?;
        let commands = read(
            self.http
                .get_guild_commands(discord::GuildId::new(sid(guild.as_str())?)),
        )
        .await?;
        let mut commands = commands
            .into_iter()
            .map(|c| {
                command_from_json(
                    serde_json::to_value(c).map_err(|_| error(ErrorCode::InvalidInput))?,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        commands.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(commands)
    }
    async fn create(
        &self,
        guild: &GuildId,
        definition: &Value,
        guard: &SendGuard,
    ) -> Result<PublishedCommand> {
        let definition = canonical_definition(definition)?;
        let baseline = self.list(guild).await?;
        if baseline.iter().any(|c| {
            c.definition.get("name") == definition.get("name")
                && c.definition.get("type") == definition.get("type")
        }) {
            return Err(error(ErrorCode::Conflict));
        }
        let path = format!(
            "/api/v10/applications/{}/guilds/{guild}/commands",
            self.application_id().await?
        );
        let fresh = CommandsFresh {
            adapter: self,
            guild,
            baseline,
        };
        command_from_json(success(
            self.writer
                .execute(Method::POST, &path, &definition, guard, &fresh)
                .await?,
        )?)
    }
    async fn edit(
        &self,
        guild: &GuildId,
        id: &str,
        definition: &Value,
        guard: &SendGuard,
    ) -> Result<PublishedCommand> {
        sid(id)?;
        let definition = canonical_definition(definition)?;
        let baseline = self.list(guild).await?;
        if !baseline.iter().any(|c| c.id == id) {
            return Err(error(ErrorCode::NotFound));
        }
        let path = format!(
            "/api/v10/applications/{}/guilds/{guild}/commands/{id}",
            self.application_id().await?
        );
        let fresh = CommandsFresh {
            adapter: self,
            guild,
            baseline,
        };
        let result = command_from_json(success(
            self.writer
                .execute(Method::PATCH, &path, &definition, guard, &fresh)
                .await?,
        )?)?;
        if result.id != id {
            return Err(error(ErrorCode::UnknownOutcome));
        }
        Ok(result)
    }
    async fn delete(&self, guild: &GuildId, id: &str, guard: &SendGuard) -> Result<()> {
        sid(id)?;
        let baseline = self.list(guild).await?;
        if !baseline.iter().any(|c| c.id == id) {
            return Err(error(ErrorCode::NotFound));
        }
        let path = format!(
            "/api/v10/applications/{}/guilds/{guild}/commands/{id}",
            self.application_id().await?
        );
        let fresh = CommandsFresh {
            adapter: self,
            guild,
            baseline,
        };
        success(
            self.writer
                .execute(Method::DELETE, &path, &Value::Null, guard, &fresh)
                .await?,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn guild() -> GuildId {
        GuildId::new("100").unwrap()
    }
    fn fixture() -> Value {
        json!({"id":"200","guild_id":"100","type":0,"name":"minecraft-info","parent_id":"300","flags":0,"permission_overwrites":[{"id":"100","type":0,"allow":"1024","deny":"2048"}],"future_field":"ignored"})
    }
    #[test]
    fn channel_fixture_roundtrips_serenity_without_boundary_types() {
        let channel: discord::GuildChannel = serde_json::from_value(fixture()).unwrap();
        let wire = serde_json::to_value(channel).unwrap();
        let mapped = channel_from_json(&wire, &guild()).unwrap();
        assert_eq!(mapped.id, "200");
        assert_eq!(mapped.parent.as_deref(), Some("300"));
        assert_eq!(mapped.kind, ChannelKind::Text);
        assert_eq!(mapped.overwrites[0].allow, permissions::VIEW_CHANNEL);
        assert_eq!(mapped.overwrites[0].deny, permissions::SEND_MESSAGES);
    }
    #[test]
    fn obfuscated_missing_and_cross_guild_channels_fail_closed() {
        let mut value = fixture();
        value["flags"] = Value::from(1 << 17);
        assert_eq!(
            channel_from_json(&value, &guild()).unwrap_err().code,
            ErrorCode::ForbiddenPermission
        );
        value = fixture();
        value.as_object_mut().unwrap().remove("name");
        assert!(channel_from_json(&value, &guild()).is_err());
        value = fixture();
        value["guild_id"] = Value::from("101");
        assert_eq!(
            channel_from_json(&value, &guild()).unwrap_err().code,
            ErrorCode::ForbiddenScope
        );
        value = fixture();
        value["permission_overwrites"][0]["type"] = Value::from(99);
        assert!(channel_from_json(&value, &guild()).is_err());
        value = fixture();
        value["permission_overwrites"][0]["allow"] = Value::from("18446744073709551616");
        assert!(channel_from_json(&value, &guild()).is_err());
    }
    #[test]
    fn unknown_channel_kind_is_inspectable_but_not_mutable() {
        let mut value = fixture();
        value["type"] = Value::from(99);
        let desired = channel_from_json(&value, &guild()).unwrap();
        assert_eq!(desired.kind, ChannelKind::Other(99));
        assert!(
            mutation_body(&ChannelMutation {
                before: None,
                desired,
                expected_fingerprint: "test".into()
            })
            .is_err()
        );
    }
    #[test]
    fn mutations_create_with_type_and_preserve_exact_string_permissions() {
        let desired = channel_from_json(&fixture(), &guild()).unwrap();
        let mut mutation = ChannelMutation {
            before: None,
            desired: desired.clone(),
            expected_fingerprint: "test".into(),
        };
        let create = mutation_body(&mutation).unwrap();
        assert_eq!(
            create,
            json!({"name":"minecraft-info","parent_id":"300","type":0,"permission_overwrites":[{"id":"100","type":0,"allow":"1024","deny":"2048"}]})
        );
        mutation.before = Some(desired);
        mutation.desired.parent = None;
        let update = mutation_body(&mutation).unwrap();
        assert!(update.get("type").is_none());
        assert_eq!(update["parent_id"], Value::Null);
    }
    #[test]
    fn command_response_identity_and_server_defaults_are_normalized() {
        let mapped=command_from_json(json!({"id":"201","application_id":"300","guild_id":"100","version":"500","name":"oracle-counter","description":"Counter","type":1,"options":[],"default_member_permissions":null,"name_localizations":null,"description_localizations":{},"nsfw":false})).unwrap();
        assert_eq!(mapped.id, "201");
        assert_eq!(
            mapped.definition,
            json!({"name":"oracle-counter","description":"Counter","type":1})
        );
        assert!(command_from_json(json!({"id":"../bad","name":"x","description":"test"})).is_err());
    }
    #[test]
    fn http_statuses_preserve_permission_conflict_and_uncertain_outcomes() {
        for (status, code) in [
            (403, ErrorCode::ForbiddenPermission),
            (404, ErrorCode::NotFound),
            (409, ErrorCode::Conflict),
            (429, ErrorCode::QuotaExceeded),
            (503, ErrorCode::UnknownOutcome),
        ] {
            assert_eq!(
                success(WriteResponse {
                    status,
                    body: Value::Null
                })
                .unwrap_err()
                .code,
                code
            );
        }
        assert_eq!(
            success(WriteResponse {
                status: 204,
                body: Value::Null
            })
            .unwrap(),
            Value::Null
        );
    }
    #[test]
    fn staff_destinations_reject_everyone_and_ordinary_role_visibility() {
        let snapshot = Snapshot {
            guild: guild(),
            owner: "99".into(),
            actor: Member {
                id: "50".into(),
                roles: vec!["101".into()],
                timed_out: false,
            },
            bot: Member {
                id: "60".into(),
                roles: vec!["101".into()],
                timed_out: false,
            },
            roles: vec![
                Role {
                    id: "100".into(),
                    position: 0,
                    permissions: permissions::VIEW_CHANNEL,
                    managed: false,
                },
                Role {
                    id: "101".into(),
                    position: 1,
                    permissions: permissions::ADMINISTRATOR,
                    managed: false,
                },
                Role {
                    id: "102".into(),
                    position: 2,
                    permissions: 0,
                    managed: false,
                },
            ],
            channels: vec![],
            complete: true,
            observed_at: now(),
        };
        let mut channel = channel_from_json(&fixture(), &guild()).unwrap();
        channel.overwrites.clear();
        assert!(require_private_roles(&snapshot, &channel).is_err());
        channel.overwrites.push(Overwrite {
            id: "100".into(),
            kind: OverwriteKind::Role,
            allow: 0,
            deny: permissions::VIEW_CHANNEL,
        });
        assert!(require_private_roles(&snapshot, &channel).is_ok());
        channel.overwrites.push(Overwrite {
            id: "102".into(),
            kind: OverwriteKind::Role,
            allow: permissions::VIEW_CHANNEL,
            deny: 0,
        });
        assert!(require_private_roles(&snapshot, &channel).is_err());
    }
}
