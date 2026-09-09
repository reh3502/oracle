//! Discord bootstrap presentation over the shared, host-owned policy service.
#![forbid(unsafe_code)]

use async_trait::async_trait;
use oracle_core::{CoreService, ErrorCode, GuildId, PolicyContext, UserId};
use serenity::all as discord;
use std::{
    fmt,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub use serenity::all::Token;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidInteraction,
    Core(ErrorCode),
    Transport,
    CommandConflict,
    ShutdownTimeout,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
impl From<oracle_core::Error> for Error {
    fn from(error: oracle_core::Error) -> Self {
        Self::Core(error.code)
    }
}
async fn api<T>(future: impl Future<Output = serenity::Result<T>>) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .map_err(|_| Error::Transport)?
        .map_err(|_| Error::Transport)
}

/// A narrow response seam. The concrete implementation always uses ephemeral replies.
#[async_trait]
pub trait InteractionResponder: Send + Sync {
    async fn defer_ephemeral(&self) -> Result<()>;
    async fn complete(&self, message: &str) -> Result<()>;
    async fn reject_ephemeral(&self, message: &str) -> Result<()>;
}
struct DiscordResponder<'a> {
    interaction: &'a discord::CommandInteraction,
    http: &'a discord::Http,
}
#[async_trait]
impl InteractionResponder for DiscordResponder<'_> {
    async fn defer_ephemeral(&self) -> Result<()> {
        api(self.interaction.defer_ephemeral(self.http)).await
    }
    async fn complete(&self, message: &str) -> Result<()> {
        api(self.interaction.edit_response(
            self.http,
            discord::EditInteractionResponse::new()
                .content(message)
                .allowed_mentions(discord::CreateAllowedMentions::new()),
        ))
        .await?;
        Ok(())
    }
    async fn reject_ephemeral(&self, message: &str) -> Result<()> {
        api(self.interaction.create_response(
            self.http,
            discord::CreateInteractionResponse::Message(
                discord::CreateInteractionResponseMessage::new()
                    .content(message)
                    .ephemeral(true)
                    .allowed_mentions(discord::CreateAllowedMentions::new()),
            ),
        ))
        .await
    }
}

#[derive(Clone, Copy)]
enum Action {
    Status,
    SetPaused(bool),
}
fn authenticated_request(
    interaction: &discord::CommandInteraction,
) -> Result<(PolicyContext, GuildId, Action)> {
    let guild = interaction.guild_id.ok_or(Error::InvalidInteraction)?;
    let member = interaction
        .member
        .as_ref()
        .ok_or(Error::InvalidInteraction)?;
    if member.guild_id != guild
        || member.user.id != interaction.user.id
        || interaction.data.kind != discord::CommandType::ChatInput
    {
        return Err(Error::InvalidInteraction);
    }
    let permissions = member.permissions.ok_or(Error::InvalidInteraction)?;
    let guild = GuildId::new(guild.to_string())?;
    let user = UserId::new(member.user.id.to_string())?;
    let context = PolicyContext::Discord {
        guild: guild.clone(),
        user,
        manage_guild: permissions
            .intersects(discord::Permissions::MANAGE_GUILD | discord::Permissions::ADMINISTRATOR),
    };
    let [subcommand] = interaction.data.options.as_ref() else {
        return Err(Error::InvalidInteraction);
    };
    let discord::CommandDataOptionValue::SubCommand(options) = &subcommand.value else {
        return Err(Error::InvalidInteraction);
    };
    let action = match (subcommand.name.as_str(), options.as_ref()) {
        ("status", []) => Action::Status,
        ("control", [option]) if option.name.as_str() == "action" => match &option.value {
            discord::CommandDataOptionValue::String(value) if value.as_str() == "pause" => {
                Action::SetPaused(true)
            }
            discord::CommandDataOptionValue::String(value) if value.as_str() == "resume" => {
                Action::SetPaused(false)
            }
            _ => return Err(Error::InvalidInteraction),
        },
        _ => return Err(Error::InvalidInteraction),
    };
    Ok((context, guild, action))
}

pub struct DiscordBootstrap {
    core: Arc<CoreService>,
    ready: watch::Sender<bool>,
    failures: AtomicU64,
}
impl DiscordBootstrap {
    pub fn new(core: Arc<CoreService>) -> Self {
        let (ready, _) = watch::channel(false);
        Self {
            core,
            ready,
            failures: AtomicU64::new(0),
        }
    }
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }
    pub async fn wait_ready(&self, duration: Duration) -> Result<()> {
        let mut receiver = self.ready.subscribe();
        tokio::time::timeout(duration, async {
            while !*receiver.borrow_and_update() {
                receiver.changed().await.map_err(|_| Error::Transport)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| Error::Transport)?
    }

    /// Handles only `/oracle`; authorization stays in CoreService before repository access.
    /// Actor, guild and permission facts come solely from the authenticated Gateway interaction.
    pub async fn handle_command(
        &self,
        interaction: &discord::CommandInteraction,
        responder: &dyn InteractionResponder,
    ) -> Result<bool> {
        if interaction.data.name.as_str() != "oracle" {
            return Ok(false);
        }
        let (context, guild, action) = match authenticated_request(interaction) {
            Ok(request) => request,
            Err(_) => {
                responder
                    .reject_ephemeral("Oracle requires a valid guild command and member identity.")
                    .await?;
                return Ok(true);
            }
        };
        // Acknowledge before database work; no provider is required or consulted.
        responder.defer_ephemeral().await?;
        let result: oracle_core::Result<String> = async {
            let status = self.core.status(&context, Some(&guild)).await?;
            let state = status
                .guilds
                .iter()
                .find(|state| state.guild == guild)
                .ok_or_else(|| oracle_core::Error::new(ErrorCode::NotFound))?;
            match action {
                Action::Status => Ok(format!(
                    "Oracle: {}. Modules loaded: {}. Recovery required: {}. AI: {}.",
                    if state.paused { "paused" } else { "running" },
                    status.modules_loaded,
                    status.recovery_required,
                    if status.ai_available {
                        "available"
                    } else {
                        "unavailable"
                    }
                )),
                Action::SetPaused(paused) => {
                    let receipt = self
                        .core
                        .control(&context, &guild, paused, state.revision)
                        .await?;
                    Ok(format!(
                        "Oracle is {} for this server (revision {}).",
                        if receipt.guild.paused {
                            "paused"
                        } else {
                            "running"
                        },
                        receipt.guild.revision
                    ))
                }
            }
        }
        .await;
        let message = match result {
            Ok(message) => message,
            Err(error) => format!("Oracle refused this request: {:?}.", error.code),
        };
        responder.complete(&message).await?;
        Ok(true)
    }

    /// Connect only. Command publication is a separate explicit operator action.
    pub async fn run_gateway(self: Arc<Self>, token: Token, stop: CancellationToken) -> Result<()> {
        self.ready.send_replace(false);
        if stop.is_cancelled() {
            return Ok(());
        }
        let mut client = tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            result = async { discord::Client::builder(token, discord::GatewayIntents::GUILDS)
                .event_handler(self.clone()).await } => result.map_err(|_| Error::Transport)?,
        };
        let shutdown = client.shard_manager.get_shutdown_trigger();
        let running = client.start();
        tokio::pin!(running);
        let result = tokio::select! {
            result = &mut running => result.map_err(|_| Error::Transport),
            _ = stop.cancelled() => {
                shutdown();
                tokio::time::timeout(Duration::from_secs(10), &mut running).await
                    .map_err(|_| Error::ShutdownTimeout).and_then(|result| result.map_err(|_| Error::Transport))
            }
        };
        self.ready.send_replace(false);
        result
    }
}
#[async_trait]
impl discord::EventHandler for DiscordBootstrap {
    async fn dispatch(&self, context: &discord::Context, event: &discord::FullEvent) {
        match event {
            discord::FullEvent::Ready { .. } => {
                self.ready.send_replace(true);
            }
            discord::FullEvent::InteractionCreate {
                interaction: discord::Interaction::Command(interaction),
                ..
            } => {
                let responder = DiscordResponder {
                    interaction,
                    http: &context.http,
                };
                if self.handle_command(interaction, &responder).await.is_err() {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }
}

/// Only guild routes publish this descriptor; there is no global/bulk replacement API here.
pub fn oracle_command() -> discord::CreateCommand<'static> {
    discord::CreateCommand::new("oracle")
        .description("Oracle framework administration")
        .kind(discord::CommandType::ChatInput)
        .default_member_permissions(discord::Permissions::MANAGE_GUILD)
        .add_option(discord::CreateCommandOption::new(
            discord::CommandOptionType::SubCommand,
            "status",
            "Show framework status",
        ))
        .add_option(
            discord::CreateCommandOption::new(
                discord::CommandOptionType::SubCommand,
                "control",
                "Pause or resume this server",
            )
            .add_sub_option(
                discord::CreateCommandOption::new(
                    discord::CommandOptionType::String,
                    "action",
                    "Requested control",
                )
                .required(true)
                .add_string_choice("pause", "pause")
                .add_string_choice("resume", "resume"),
            ),
        )
}
fn command_matches(command: &discord::Command) -> bool {
    let Ok(expected) = serde_json::to_value(oracle_command()) else {
        return false;
    };
    let Ok(mut actual) = serde_json::to_value(command) else {
        return false;
    };
    // Discord omits empty root localization maps in REST responses. The model
    // represents omission as None/null, whereas EditCommand emits empty maps.
    // Normalize only those optional maps, preserving all behavioral fields.
    for key in ["name_localizations", "description_localizations"] {
        if actual.get(key).is_some_and(serde_json::Value::is_null) {
            actual[key] = serde_json::json!({});
        }
    }
    described_fields_match(&expected, &actual)
}
fn described_fields_match(expected: &serde_json::Value, actual: &serde_json::Value) -> bool {
    match (expected, actual) {
        (serde_json::Value::Object(expected), serde_json::Value::Object(actual)) => {
            expected.iter().all(|(key, value)| {
                actual
                    .get(key)
                    .is_some_and(|actual| described_fields_match(value, actual))
            })
        }
        (serde_json::Value::Array(expected), serde_json::Value::Array(actual)) => {
            expected.len() == actual.len()
                && expected
                    .iter()
                    .zip(actual)
                    .all(|(expected, actual)| described_fields_match(expected, actual))
        }
        _ => expected == actual,
    }
}
pub struct PublishedCommand {
    guild: discord::GuildId,
    id: discord::CommandId,
    created: bool,
    original: Option<serde_json::Value>,
}
impl PublishedCommand {
    pub fn id(&self) -> discord::CommandId {
        self.id
    }
    pub fn created(&self) -> bool {
        self.created
    }
}
pub async fn publish_guild_command(
    http: &discord::Http,
    guild: discord::GuildId,
) -> Result<PublishedCommand> {
    let before = api(http.get_guild_commands(guild)).await?;
    if let Some(existing) = before
        .iter()
        .find(|command| command.name.as_str() == "oracle")
    {
        if !command_matches(existing) {
            return Err(Error::CommandConflict);
        }
        return Ok(PublishedCommand {
            guild,
            id: existing.id,
            created: false,
            original: serde_json::to_value(existing).ok(),
        });
    }
    let created = api(http.create_guild_command(guild, &oracle_command())).await?;
    Ok(PublishedCommand {
        guild,
        id: created.id,
        created: true,
        original: serde_json::to_value(&created).ok(),
    })
}
/// Separate readback preserves the caller's cleanup receipt if observation fails.
pub async fn verify_published_command(
    http: &discord::Http,
    receipt: &PublishedCommand,
) -> Result<()> {
    let current = api(http.get_guild_commands(receipt.guild)).await?;
    if current
        .iter()
        .any(|command| command.id == receipt.id && command_matches(command))
    {
        Ok(())
    } else {
        Err(Error::CommandConflict)
    }
}
/// Bounded smoke-test cleanup only removes a command created by this publication call.
pub async fn cleanup_published_command(
    http: &discord::Http,
    receipt: &PublishedCommand,
) -> Result<()> {
    if !receipt.created {
        return Ok(());
    }
    let current = api(http.get_guild_commands(receipt.guild)).await?;
    let Some(command) = current.iter().find(|command| command.id == receipt.id) else {
        return Ok(());
    };
    if !command_matches(command)
        || receipt
            .original
            .as_ref()
            .is_none_or(|original| serde_json::to_value(command).ok().as_ref() != Some(original))
    {
        return Err(Error::CommandConflict);
    }
    api(http.delete_guild_command(receipt.guild, receipt.id)).await?;
    if api(http.get_guild_commands(receipt.guild))
        .await?
        .iter()
        .any(|command| command.id == receipt.id)
    {
        return Err(Error::CommandConflict);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
