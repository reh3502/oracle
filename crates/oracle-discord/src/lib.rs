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
    /// Preserve the complete value. Concrete Discord replies attach large JSON results.
    async fn complete_result(&self, value: &serde_json::Value) -> Result<()> {
        self.complete(&serde_json::to_string_pretty(value).map_err(|_| Error::Transport)?)
            .await
    }
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
    async fn complete_result(&self, value: &serde_json::Value) -> Result<()> {
        match interaction_ops::render(value)? {
            interaction_ops::ExactResponse::Text(text) => self.complete(&text).await,
            interaction_ops::ExactResponse::Attachment { filename, bytes } => {
                api(self.interaction.edit_response(
                    self.http,
                    discord::EditInteractionResponse::new()
                        .content(
                            "The complete result is attached. Review it before applying changes.",
                        )
                        .allowed_mentions(discord::CreateAllowedMentions::new())
                        .new_attachment(discord::CreateAttachment::bytes(bytes, filename)),
                ))
                .await?;
                Ok(())
            }
        }
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
fn authenticated_identity(
    interaction: &discord::CommandInteraction,
) -> Result<(PolicyContext, GuildId)> {
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
    Ok((context, guild))
}
fn authenticated_request(
    interaction: &discord::CommandInteraction,
) -> Result<(PolicyContext, GuildId, Action)> {
    let (context, guild) = authenticated_identity(interaction)?;
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

struct GatewayRuntime {
    modules: Arc<oracle_modules::ModuleManager>,
    intents: std::collections::BTreeSet<String>,
    bot: AtomicU64,
    connected: std::sync::atomic::AtomicBool,
    member_role_gaps: AtomicU64,
    unknown_origins: AtomicU64,
}
pub struct DiscordBootstrap {
    core: Arc<CoreService>,
    ready: watch::Sender<bool>,
    failures: AtomicU64,
    operations: Option<Arc<dyn oracle_operations::ingress::HumanOperations>>,
    runtime: Option<GatewayRuntime>,
}
impl DiscordBootstrap {
    pub fn new(core: Arc<CoreService>) -> Self {
        let (ready, _) = watch::channel(false);
        Self {
            core,
            ready,
            failures: AtomicU64::new(0),
            operations: None,
            runtime: None,
        }
    }
    pub fn with_operations(
        core: Arc<CoreService>,
        operations: Arc<dyn oracle_operations::ingress::HumanOperations>,
    ) -> Self {
        let mut bootstrap = Self::new(core);
        bootstrap.operations = Some(operations);
        bootstrap
    }
    pub fn with_runtime(
        core: Arc<CoreService>,
        operations: Arc<dyn oracle_operations::ingress::HumanOperations>,
        modules: Arc<oracle_modules::ModuleManager>,
        intents: std::collections::BTreeSet<String>,
    ) -> oracle_core::Result<Self> {
        gateway_events::intents(&intents)?;
        modules.set_event_intents(std::collections::BTreeSet::new())?;
        let mut bootstrap = Self::with_operations(core, operations);
        bootstrap.runtime = Some(GatewayRuntime {
            modules,
            intents,
            bot: AtomicU64::new(0),
            connected: std::sync::atomic::AtomicBool::new(false),
            member_role_gaps: AtomicU64::new(0),
            unknown_origins: AtomicU64::new(0),
        });
        Ok(bootstrap)
    }
    fn set_gateway_coverage(&self, connected: bool) {
        if let Some(runtime) = &self.runtime {
            let intents = if connected {
                runtime.intents.clone()
            } else {
                std::collections::BTreeSet::new()
            };
            let valid = runtime.modules.set_event_intents(intents).is_ok();
            runtime
                .connected
                .store(connected && valid, Ordering::SeqCst);
            if !valid {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    pub fn event_coverage(&self) -> serde_json::Value {
        match &self.runtime {
            Some(runtime) => {
                serde_json::json!({"connected":runtime.connected.load(Ordering::SeqCst),"configured_intents":runtime.intents,"member_role_observation_gaps":runtime.member_role_gaps.load(Ordering::Relaxed),"unknown_origin_observations":runtime.unknown_origins.load(Ordering::Relaxed)})
            }
            None => {
                serde_json::json!({"connected":false,"configured_intents":[],"member_role_observation_gaps":0,"unknown_origin_observations":0})
            }
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

    /// Handles bootstrap and published module commands through the shared policy service.
    /// Actor, guild and permission facts come solely from the authenticated Gateway interaction.
    pub async fn handle_command(
        &self,
        interaction: &discord::CommandInteraction,
        responder: &dyn InteractionResponder,
    ) -> Result<bool> {
        if interaction.data.name.as_str() != "oracle" {
            if self.operations.is_none() || interaction.data.kind != discord::CommandType::ChatInput
            {
                return Ok(false);
            }
            return self
                .handle_operation(interaction, responder, Duration::from_secs(30))
                .await;
        }
        if interaction_ops::is_operation(interaction) {
            return self
                .handle_operation(interaction, responder, Duration::from_secs(30))
                .await;
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
                    if self
                        .operations
                        .as_ref()
                        .is_some_and(|operations| operations.ai_available())
                    {
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

    async fn handle_operation(
        &self,
        interaction: &discord::CommandInteraction,
        responder: &dyn InteractionResponder,
        timeout: Duration,
    ) -> Result<bool> {
        let (context, guild, request) = match interaction_ops::parse(interaction) {
            Ok(request) => request,
            Err(_) => {
                responder
                    .reject_ephemeral("Oracle requires a valid guild command and member identity.")
                    .await?;
                return Ok(true);
            }
        };
        responder.defer_ephemeral().await?;
        if let Err(error) = self.core.status(&context, Some(&guild)).await {
            responder
                .complete(&format!("Oracle refused this request: {:?}.", error.code))
                .await?;
            return Ok(true);
        }
        let Some(operations) = &self.operations else {
            responder
                .complete("These operations are unavailable in this host.")
                .await?;
            return Ok(true);
        };
        let cancel = CancellationToken::new();
        struct CancelOnDrop(CancellationToken);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _cancel = CancelOnDrop(cancel.clone());
        let result = tokio::time::timeout(
            timeout,
            operations.execute(&context, &guild, request, &cancel),
        )
        .await;
        match result {
            Ok(Ok(value)) => responder.complete_result(&value).await?,
            Ok(Err(error)) => {
                responder
                    .complete(&format!("Oracle refused this request: {:?}.", error.code))
                    .await?
            }
            Err(_) => {
                cancel.cancel();
                responder
                    .complete("This request timed out. Inspect its saved state before retrying.")
                    .await?;
            }
        }
        Ok(true)
    }

    fn observe_connection(&self, event: &discord::FullEvent) {
        match event {
            discord::FullEvent::Ready { data_about_bot, .. } => {
                if let Some(runtime) = &self.runtime {
                    runtime
                        .bot
                        .store(data_about_bot.user.id.get(), Ordering::SeqCst);
                }
                self.set_gateway_coverage(true);
                self.ready.send_replace(true);
            }
            discord::FullEvent::Resume { .. } => {
                self.set_gateway_coverage(true);
                self.ready.send_replace(true);
            }
            discord::FullEvent::ShardStageUpdate { event, .. }
                if event.new != discord::ConnectionStage::Connected =>
            {
                self.set_gateway_coverage(false);
                self.ready.send_replace(false);
            }
            _ => {}
        }
    }

    /// Connect only. Command publication is a separate explicit operator action.
    pub async fn run_gateway(self: Arc<Self>, token: Token, stop: CancellationToken) -> Result<()> {
        self.ready.send_replace(false);
        self.set_gateway_coverage(false);
        struct CoverageOnDrop<'a>(&'a DiscordBootstrap);
        impl Drop for CoverageOnDrop<'_> {
            fn drop(&mut self) {
                self.0.set_gateway_coverage(false);
                self.0.ready.send_replace(false);
            }
        }
        let _coverage = CoverageOnDrop(&self);
        if stop.is_cancelled() {
            return Ok(());
        }
        let intents = match &self.runtime {
            Some(runtime) => gateway_events::intents(&runtime.intents).map_err(Error::from)?,
            None => discord::GatewayIntents::GUILDS,
        };
        let mut client = tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            result = async { discord::Client::builder(token, intents)
                .event_handler(self.clone()).await } => result.map_err(|_| Error::Transport)?,
        };
        let shutdown = client.shard_manager.get_shutdown_trigger();
        let running = client.start();
        tokio::pin!(running);
        let result = tokio::select! {
            result = &mut running => result.map_err(|_| Error::Transport),
            _ = stop.cancelled() => {
                self.set_gateway_coverage(false);
                shutdown();
                tokio::time::timeout(Duration::from_secs(10), &mut running).await
                    .map_err(|_| Error::ShutdownTimeout).and_then(|result| result.map_err(|_| Error::Transport))
            }
        };
        self.ready.send_replace(false);
        self.set_gateway_coverage(false);
        result
    }
}
#[async_trait]
impl discord::EventHandler for DiscordBootstrap {
    async fn dispatch(&self, context: &discord::Context, event: &discord::FullEvent) {
        self.observe_connection(event);
        if let discord::FullEvent::InteractionCreate {
            interaction: discord::Interaction::Command(interaction),
            ..
        } = event
        {
            let responder = DiscordResponder {
                interaction,
                http: &context.http,
            };
            if self.handle_command(interaction, &responder).await.is_err() {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(runtime) = &self.runtime
            && runtime.connected.load(Ordering::SeqCst)
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            match gateway_events::normalize(event, runtime.bot.load(Ordering::SeqCst), now) {
                gateway_events::Normalized::Audit(guild, events) => {
                    if events
                        .first()
                        .is_some_and(|event| event.origin == oracle_core::GuildEventOrigin::Unknown)
                    {
                        runtime.unknown_origins.fetch_add(1, Ordering::Relaxed);
                    }
                    for event in events {
                        if runtime.modules.deliver_event(&guild, event).await.is_err() {
                            self.failures.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                gateway_events::Normalized::Event(guild, event) => {
                    if event.origin == oracle_core::GuildEventOrigin::Unknown {
                        runtime.unknown_origins.fetch_add(1, Ordering::Relaxed);
                    }
                    if runtime.modules.deliver_event(&guild, event).await.is_err() {
                        self.failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
                gateway_events::Normalized::MemberRolesGap(guild) => {
                    if self
                        .core
                        .status(&PolicyContext::LocalOperator, Some(&guild))
                        .await
                        .is_ok()
                    {
                        runtime.member_role_gaps.fetch_add(1, Ordering::Relaxed);
                    }
                }
                gateway_events::Normalized::Ignored => {}
            }
        }
    }
}

#[cfg(test)]
mod tests;

mod interaction_ops;
pub mod notification;
pub mod operations;
pub mod transport;

mod gateway_events;

pub mod bootstrap_commands;

mod command_presentation;
pub use command_presentation::{
    PublishedCommand, cleanup_published_command, oracle_command, publish_guild_command,
    verify_published_command,
};
