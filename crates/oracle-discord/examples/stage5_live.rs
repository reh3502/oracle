//! Explicitly authorized, bounded real shared-operations canary. No AI calls.
//! Deletion is a test-only teardown seam because StructureBackend has no delete.
use oracle_core::{CoreService, GuildId, GuildPolicy, PolicyContext};
use oracle_discord::{DiscordBootstrap, Token, operations::DiscordOperations};
use oracle_operations::{
    executor::{PlanState, StructureExecutor},
    structure::{Change, ChannelKind, DesiredChannel, StructureRequest},
};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use serenity::all::{ChannelId, Http};
use std::{
    collections::BTreeSet,
    os::unix::fs::OpenOptionsExt,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

fn nonce() -> Result<String, &'static str> {
    let value = std::env::var("ORACLE_STAGE5_NONCE").unwrap_or_else(|_| {
        format!(
            "{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        )
    });
    if !(8..=32).contains(&value.len())
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err("invalid_nonce");
    }
    Ok(value)
}
async fn discord_smoke(
    core: Arc<CoreService>,
    token: Token,
    guild: &GuildId,
    nonce: &str,
    report: &mut Value,
) -> Result<(), &'static str> {
    let adapter = Arc::new(DiscordBootstrap::new(core));
    let stop = CancellationToken::new();
    let mut task = tokio::spawn(adapter.clone().run_gateway(token.clone(), stop.clone()));
    let (ready, ended) = tokio::select! {
        ready = adapter.wait_ready(Duration::from_secs(45)) => (ready.is_ok(), None),
        result = &mut task => (false, Some(result)),
    };
    stop.cancel();
    let stopped = match ended {
        Some(result) => matches!(result, Ok(Ok(()))),
        None => match tokio::time::timeout(Duration::from_secs(15), &mut task).await {
            Ok(result) => matches!(result, Ok(Ok(()))),
            Err(_) => {
                task.abort();
                let _ = task.await;
                false
            }
        },
    };
    report["gateway_ready"] = json!(ready);
    report["gateway_shutdown"] = json!(stopped);
    if !ready || !stopped {
        return Err("gateway");
    }
    let http = Http::new(token);
    let guild = serenity::all::GuildId::new(guild.as_str().parse().map_err(|_| "invalid_guild")?);
    let application = bounded(http.get_current_application_info()).await?;
    http.set_application_id(application.id);
    let before = bounded(http.get_guild_commands(guild)).await?;
    let name = format!("oracle-s5-{}", &nonce[..nonce.len().min(16)]);
    report["fixture_command_name"] = json!(name);
    if before.iter().any(|command| command.name.as_str() == name) {
        return Err("command_nonce_collision");
    }
    let definition = serenity::all::CreateCommand::new(name.clone())
        .description("Oracle release canary fixture")
        .kind(serenity::all::CommandType::ChatInput)
        .default_member_permissions(serenity::all::Permissions::MANAGE_GUILD);
    let created = bounded(http.create_guild_command(guild, &definition))
        .await
        .map_err(|_| "command_create_ambiguity_requires_review")?;
    report["owned_command_id"] = json!(created.id.get());
    if before.iter().any(|command| command.id == created.id) {
        return Err("command_ownership_requires_review");
    }
    let current = bounded(http.get_guild_commands(guild)).await;
    let exact = current.as_ref().is_ok_and(|commands| {
        commands
            .iter()
            .find(|command| command.id == created.id)
            .is_some_and(|command| {
                command.name.as_str() == name
                    && command.description.as_str() == "Oracle release canary fixture"
                    && command.kind == serenity::all::CommandType::ChatInput
                    && command.options.is_empty()
                    && command.default_member_permissions
                        == Some(serenity::all::Permissions::MANAGE_GUILD)
                    && serde_json::to_value(command).ok() == serde_json::to_value(&created).ok()
            })
    });
    report["command_verified"] = json!(exact);
    // Always attempt fresh cleanup verification, even if the first read failed.
    // Preserve human edits. A lost create response never establishes ownership.
    let cleanup_state = bounded(http.get_guild_commands(guild))
        .await
        .map_err(|_| "command_cleanup_read_requires_review")?;
    if let Some(command) = cleanup_state
        .iter()
        .find(|command| command.id == created.id)
    {
        if serde_json::to_value(command).ok() != serde_json::to_value(&created).ok() {
            return Err("command_changed_requires_review");
        }
        bounded(http.delete_guild_command(guild, created.id))
            .await
            .map_err(|_| "command_cleanup_requires_review")?;
    }
    report["owned_command_cleanup"] = json!(true);
    let after = bounded(http.get_guild_commands(guild)).await?;
    let before_values: std::collections::BTreeMap<_, _> = before
        .iter()
        .map(|c| (c.id.get(), serde_json::to_value(c).ok()))
        .collect();
    let after_values: std::collections::BTreeMap<_, _> = after
        .iter()
        .map(|c| (c.id.get(), serde_json::to_value(c).ok()))
        .collect();
    let unchanged = before_values == after_values;
    report["commands_unchanged"] = json!(unchanged);
    if !unchanged {
        return Err("command_set_changed_requires_review");
    }
    if !exact {
        return Err("command_readback");
    }
    Ok(())
}
async fn bounded<T>(
    future: impl std::future::Future<Output = Result<T, serenity::Error>>,
) -> Result<T, &'static str> {
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .map_err(|_| "discord_timeout")?
        .map_err(|_| "discord_request")
}
async fn run(report: &mut Value) -> Result<(), &'static str> {
    let nonce = nonce()?;
    report["nonce"] = json!(nonce);
    let token = Token::from_env("DISCORD_TOKEN").map_err(|_| "missing_token")?;
    let guild = GuildId::new(std::env::var("GUILD_ID").map_err(|_| "missing_guild")?)
        .map_err(|_| "invalid_guild")?;
    let path = std::env::temp_dir().join(format!(
        "oracle-stage5-live-{}-{nonce}.sqlite",
        std::process::id()
    ));
    // Retain the local operation ledger for ambiguity/recovery review; never reuse it.
    std::fs::OpenOptions::new()
        .create_new(true)
        .mode(0o600)
        .write(true)
        .open(&path)
        .map_err(|_| "ledger_create")?;
    report["database_path"] = json!(path);
    let storage = Arc::new(
        Storage::open(DatabaseConfig::Sqlite { path })
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
        discord_smoke(core.clone(), token.clone(), &guild, &nonce, report).await?;
        let backend = Arc::new(
            DiscordOperations::new(token.clone(), core.clone()).map_err(|_| "adapter_create")?,
        );
        let executor = StructureExecutor::new(core, storage.clone(), backend);
        let context = PolicyContext::LocalOperator;
        let before = executor
            .inspect(&context, &guild)
            .await
            .map_err(|_| "preflight_inspect")?;
        if !before.complete {
            return Err("incomplete_inventory");
        }
        report["complete_inventory"] = json!(true);
        let request = StructureRequest {
            channels: [
                ("category", ChannelKind::Category),
                ("text", ChannelKind::Text),
                ("voice", ChannelKind::Voice),
            ]
            .into_iter()
            .map(|(suffix, kind)| DesiredChannel {
                key: format!("stage5.{nonce}.{suffix}"),
                name: format!("oracle-s5-{nonce}-{suffix}"),
                kind,
                parent: (kind != ChannelKind::Category).then(|| format!("stage5.{nonce}.category")),
                existing_id: None,
                overwrites: None,
            })
            .collect(),
        };
        if before
            .channels
            .iter()
            .any(|c| request.channels.iter().any(|d| d.name == c.name))
        {
            return Err("nonce_collision");
        }
        report["nonce_preflight"] = json!(true);
        let before_ids: BTreeSet<_> = before.channels.iter().map(|c| c.id.clone()).collect();
        let plan = executor
            .plan(&context, &guild, &request)
            .await
            .map_err(|_| "plan")?;
        report["plan_id"] = json!(plan.id);
        if plan.steps.len() != 3
            || plan
                .steps
                .iter()
                .any(|s| s.change != Change::Create || s.approval_required)
        {
            return Err("unexpected_plan");
        }
        report["create_plan"] = json!(true);
        let cancel = CancellationToken::new();
        let test_result = async {
            let applied = match tokio::time::timeout(
                Duration::from_secs(180),
                executor.apply(&context, &guild, &plan.id, &cancel),
            )
            .await
            {
                Ok(result) => result.map_err(|_| "apply")?,
                Err(_) => {
                    cancel.cancel();
                    return Err("apply_timeout_ambiguity_requires_review");
                }
            };
            if !matches!(applied.state, PlanState::Complete) || applied.receipts.len() != 3 {
                return Err("partial_apply_requires_review");
            }
            report["apply_complete"] = json!(true);
            let snapshot = executor
                .inspect(&context, &guild)
                .await
                .map_err(|_| "readback")?;
            if !snapshot.complete
                || applied.receipts.iter().any(|r| {
                    !snapshot.channels.contains(&r.channel)
                        || r.change != Change::Create
                        || before_ids.contains(&r.channel.id)
                })
            {
                return Err("receipt_readback");
            }
            report["receipt_readback"] = json!(true);
            let repeat = executor
                .plan(&context, &guild, &request)
                .await
                .map_err(|_| "repeat_plan")?;
            if repeat.steps.len() != 3 || repeat.steps.iter().any(|s| s.change != Change::Reuse) {
                return Err("repeat_not_noop");
            }
            let repeated = tokio::time::timeout(
                Duration::from_secs(60),
                executor.apply(&context, &guild, &repeat.id, &CancellationToken::new()),
            )
            .await
            .map_err(|_| "repeat_timeout")?
            .map_err(|_| "repeat_apply")?;
            if !matches!(repeated.state, PlanState::Complete)
                || repeated.receipts.iter().any(|r| r.change != Change::Reuse)
            {
                return Err("repeat_incomplete");
            }
            report["repeat_noop"] = json!(true);
            Ok(())
        }
        .await;
        // Always recover durable receipts after applying, even on partial results or
        // timeout. Never infer ownership from a matching name after a lost response.
        let saved = executor
            .read_plan(&context, &guild, &plan.id)
            .await
            .map_err(|_| "receipt_recovery_requires_review")?;
        report["owned_channel_ids"] = json!(
            saved
                .receipts
                .iter()
                .filter(|r| r.change == Change::Create)
                .map(|r| &r.channel.id)
                .collect::<Vec<_>>()
        );
        let http = Http::new(token);
        let mut cleanup_ok = true;
        for receipt in saved.receipts.iter().rev() {
            if receipt.change != Change::Create || before_ids.contains(&receipt.channel.id) {
                cleanup_ok = false;
                continue;
            }
            let fresh = match executor.inspect(&context, &guild).await {
                Ok(s) if s.complete => s,
                _ => {
                    cleanup_ok = false;
                    continue;
                }
            };
            let Some(channel) = fresh.channels.iter().find(|c| c.id == receipt.channel.id) else {
                continue;
            };
            // Preserve resources changed by a human and categories with unowned children.
            if channel != &receipt.channel
                || !request
                    .channels
                    .iter()
                    .any(|d| d.name == channel.name && d.kind == channel.kind)
                || fresh
                    .channels
                    .iter()
                    .any(|c| c.parent.as_ref() == Some(&channel.id))
            {
                cleanup_ok = false;
                continue;
            }
            let id = ChannelId::new(channel.id.parse().map_err(|_| "receipt_id")?);
            if !matches!(
                tokio::time::timeout(
                    Duration::from_secs(20),
                    http.delete_channel(
                        id.into(),
                        Some("Oracle Stage 5 receipt-owned canary cleanup")
                    )
                )
                .await,
                Ok(Ok(_))
            ) {
                cleanup_ok = false;
            }
        }
        report["owned_cleanup"] = json!(cleanup_ok);
        let after = executor
            .inspect(&context, &guild)
            .await
            .map_err(|_| "postflight_inspect")?;
        let after_ids: BTreeSet<_> = after.channels.iter().map(|c| c.id.clone()).collect();
        let unchanged = after.complete
            && after_ids == before_ids
            && before
                .channels
                .iter()
                .all(|channel| after.channels.contains(channel));
        report["postflight_unchanged"] = json!(unchanged);
        if !cleanup_ok || !unchanged {
            return Err("cleanup_or_ambiguous_resource_requires_review");
        }
        test_result
    }
    .await;
    let closed = storage.close().await;
    report["storage_closed"] = json!(closed.is_ok());
    closed.map_err(|_| "storage_close")?;
    outcome
}
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    std::panic::set_hook(Box::new(|_| eprintln!("canary_panic_payload_redacted")));
    if std::env::args().nth(1).as_deref() != Some("--execute") {
        eprintln!(
            "Usage: stage5_live --execute [report.json]; requires DISCORD_TOKEN and GUILD_ID"
        );
        std::process::exit(2);
    }
    let mut report = json!({"schema":1,"purpose":"stage5_structure_live","human_slash_invocation_verified":false});
    let result = run(&mut report).await;
    report["passed"] = json!(result.is_ok());
    if let Err(error) = result {
        report["failure"] = json!(error);
    }
    let encoded = serde_json::to_string_pretty(&report).expect("report serialization");
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
