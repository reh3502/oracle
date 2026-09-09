//! Explicit, bounded Gateway + command smoke test. Never logs credentials.
use oracle_core::{CoreService, GuildId, GuildPolicy, PolicyContext};
use oracle_discord::{
    DiscordBootstrap, Token, cleanup_published_command, publish_guild_command,
    verify_published_command,
};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use serenity::all::{GuildId as DiscordGuildId, HttpBuilder};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

async fn run(report: &mut Value) -> Result<(), &'static str> {
    let token = Token::from_env("DISCORD_TOKEN").map_err(|_| "missing_token")?;
    let guild_text = std::env::var("GUILD_ID").map_err(|_| "missing_guild")?;
    let guild = GuildId::new(guild_text.clone()).map_err(|_| "invalid_guild")?;
    let discord_guild: DiscordGuildId = guild_text.parse().map_err(|_| "invalid_guild")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "clock")?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "oracle-stage1-live-{}-{nonce}.sqlite",
        std::process::id()
    ));
    let storage = Arc::new(
        Storage::open(DatabaseConfig::Sqlite { path: path.clone() })
            .await
            .map_err(|_| "storage_open")?,
    );
    let outcome = async {
        storage
            .initialize_guilds(std::slice::from_ref(&guild))
            .await
            .map_err(|_| "storage_initialize")?;
        let core = Arc::new(CoreService::new(
            storage.clone(),
            vec![GuildPolicy {
                guild: guild.clone(),
                operators: vec![],
            }],
        ));
        let status = core
            .status(&PolicyContext::LocalOperator, Some(&guild))
            .await
            .map_err(|_| "core_status")?;
        if status.ai_available || status.modules_loaded != 0 {
            return Err("unexpected_core_status");
        }
        report["sqlite_core_without_ai"] = json!(true);
        let http = HttpBuilder::new(token.clone()).build();
        let application =
            tokio::time::timeout(Duration::from_secs(15), http.get_current_application_info())
                .await
                .map_err(|_| "application_timeout")?
                .map_err(|_| "application_read")?;
        http.set_application_id(application.id);
        let before = tokio::time::timeout(
            Duration::from_secs(15),
            http.get_guild_commands(discord_guild),
        )
        .await
        .map_err(|_| "preflight_timeout")?
        .map_err(|_| "preflight_read")?;
        let mut before_ids: Vec<_> = before.iter().map(|command| command.id.get()).collect();
        before_ids.sort_unstable();
        report["commands_before"] = json!(before_ids);
        let adapter = Arc::new(DiscordBootstrap::new(core));
        let stop = CancellationToken::new();
        let mut task = tokio::spawn(adapter.clone().run_gateway(token, stop.clone()));
        let (ready, early_exit) = tokio::select! {
            ready = adapter.wait_ready(Duration::from_secs(45)) => (ready.is_ok(), None),
            result = &mut task => (false, Some(result)),
        };
        stop.cancel();
        let ended_before_ready = early_exit.is_some();
        let shutdown = match early_exit {
            Some(result) => Some(result),
            None => match tokio::time::timeout(Duration::from_secs(15), &mut task).await {
                Ok(result) => Some(result),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    None
                }
            },
        };
        let shutdown_ok = matches!(shutdown, Some(Ok(Ok(()))));
        let task_status = match &shutdown {
            Some(Ok(Ok(()))) => json!({"kind":"completed"}),
            Some(Ok(Err(error))) => json!({"kind":"adapter_error","code":format!("{error:?}")}),
            Some(Err(error)) if error.is_panic() => json!({"kind":"panic"}),
            Some(Err(_)) => json!({"kind":"cancelled"}),
            None => json!({"kind":"shutdown_timeout"}),
        };
        report["gateway_ready"] = json!(ready);
        report["gateway_shutdown"] = json!(shutdown_ok);
        report["gateway_task"] = task_status;
        report["gateway_ended_before_ready"] = json!(ended_before_ready);
        if !ready {
            return Err("gateway_ready");
        }
        if !shutdown_ok {
            return Err("gateway_shutdown");
        }
        let receipt = publish_guild_command(&http, discord_guild)
            .await
            .map_err(|_| "publish")?;
        report["command_id"] = json!(receipt.id().get());
        report["command_created"] = json!(receipt.created());
        let verified = verify_published_command(&http, &receipt).await;
        // Always attempt cleanup after obtaining a receipt, including failed verification.
        let cleanup = cleanup_published_command(&http, &receipt).await;
        report["command_verified"] = json!(verified.is_ok());
        report["owned_command_cleanup"] = json!(cleanup.is_ok());
        cleanup.map_err(|_| "cleanup_requires_review")?;
        verified.map_err(|_| "command_verification")?;
        let after = tokio::time::timeout(
            Duration::from_secs(15),
            http.get_guild_commands(discord_guild),
        )
        .await
        .map_err(|_| "postflight_timeout")?
        .map_err(|_| "postflight_read")?;
        let mut after_ids: Vec<_> = after.iter().map(|command| command.id.get()).collect();
        after_ids.sort_unstable();
        report["commands_after"] = json!(after_ids);
        if before_ids != after_ids {
            return Err("command_set_changed");
        }
        Ok(())
    }
    .await;
    let closed = storage.close().await;
    report["storage_closed"] = json!(closed.is_ok());
    report["database_path"] = json!(path);
    closed.map_err(|_| "storage_close")?;
    outcome
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    if std::env::args().nth(1).as_deref() != Some("--execute") {
        eprintln!("Usage: stage1_live --execute [report.json]; reads DISCORD_TOKEN and GUILD_ID.");
        std::process::exit(2);
    }
    let mut report = json!({"schema":1,"purpose":"bounded Stage 1 Gateway and command publication smoke test","human_slash_invocation_verified":false});
    let result = run(&mut report).await;
    report["passed"] = json!(result.is_ok());
    if let Err(error) = result {
        report["failure"] = json!(error);
    }
    let encoded = serde_json::to_string_pretty(&report).expect("JSON report serialization");
    if let Some(path) = std::env::args().nth(2)
        && std::fs::write(path, &encoded).is_err()
    {
        eprintln!("report_write_failed");
        std::process::exit(1);
    }
    println!("{encoded}");
    if result.is_err() {
        std::process::exit(1);
    }
}
