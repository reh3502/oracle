//! Explicitly authorized, bounded real shared-operations canary. No AI calls.
//! Deletion is a test-only teardown seam because StructureBackend has no delete.
use oracle_core::{CoreService, GuildId, GuildPolicy, PolicyContext};
use oracle_discord::{DiscordBootstrap, Token, operations::DiscordOperations};
use oracle_operations::{
    executor::{ChannelMutation, PlanState, SendGuard, StructureBackend, StructureExecutor},
    permissions::{CONNECT, Overwrite, OverwriteKind, SEND_MESSAGES, VIEW_CHANNEL},
    structure::{Change, Channel, ChannelKind, DesiredChannel, Snapshot, StructureRequest},
};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use serenity::all::{ChannelId, Http};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

// Checkpoints are atomic and synced. A completed command receipt is recorded
// immediately; a missing receipt after a checkpoint remains explicitly ambiguous.
struct Recovery {
    path: PathBuf,
    events: Mutex<Vec<Value>>,
}
impl Recovery {
    fn checkpoint(&self, report: &mut Value, phase: &str) -> Result<(), &'static str> {
        report["phase"] = json!(phase);
        report["recovery_events"] = json!(*self.events.lock().unwrap());
        atomic_report(&self.path, report)
    }
    fn event(&self, event: Value) -> Result<(), &'static str> {
        self.events.lock().unwrap().push(event);
        let mut report: Value =
            serde_json::from_slice(&std::fs::read(&self.path).map_err(|_| "checkpoint_read")?)
                .map_err(|_| "checkpoint_read")?;
        self.checkpoint(&mut report, "structure_mutation")
    }
}
fn atomic_report(path: &std::path::Path, report: &Value) -> Result<(), &'static str> {
    let temporary = path.with_extension(format!("checkpoint-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(report).map_err(|_| "checkpoint_encode")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|_| "checkpoint_write")?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "checkpoint_write")?;
    std::fs::rename(&temporary, path).map_err(|_| "checkpoint_rename")?;
    std::fs::File::open(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new(".")),
    )
    .and_then(|file| file.sync_all())
    .map_err(|_| "checkpoint_sync")
}
struct CanaryBackend {
    inner: DiscordOperations,
    http: Arc<Http>,
    recovery: Arc<Recovery>,
    baselines: Mutex<BTreeMap<String, Value>>,
}
#[async_trait::async_trait]
impl StructureBackend for CanaryBackend {
    async fn inspect(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
    ) -> oracle_core::Result<Snapshot> {
        self.inner.inspect(context, guild).await
    }
    async fn mutate(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        mutation: &ChannelMutation,
        guard: &SendGuard,
    ) -> oracle_core::Result<Channel> {
        let checkpoint_error = |_| oracle_core::Error::new(oracle_core::ErrorCode::Io);
        self.recovery
            .event(json!({"phase":"before_channel_write","name":mutation.desired.name}))
            .map_err(checkpoint_error)?;
        let result = self.inner.mutate(context, guild, mutation, guard).await;
        match &result {
            Ok(channel) => {
                self.recovery
                    .event(json!({"phase":"channel_write_receipt","channel_id":channel.id}))
                    .map_err(checkpoint_error)?;
                let id = ChannelId::new(
                    channel
                        .id
                        .parse()
                        .map_err(|_| oracle_core::Error::new(oracle_core::ErrorCode::Integrity))?,
                );
                if let Ok(full) = bounded(self.http.get_channel(id.into())).await
                    && let Ok(value) = serde_json::to_value(full)
                {
                    self.baselines
                        .lock()
                        .unwrap()
                        .insert(channel.id.clone(), value);
                }
            }
            Err(error) => self
                .recovery
                .event(json!({"phase":"channel_write_failed","error":error.code}))
                .map_err(checkpoint_error)?,
        }
        result
    }
}
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
    recovery: &Recovery,
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
    recovery.checkpoint(report, "before_command_create")?;
    let created = bounded(http.create_guild_command(guild, &definition))
        .await
        .map_err(|_| "command_create_ambiguity_requires_review")?;
    report["owned_command_id"] = json!(created.id.get());
    recovery.checkpoint(report, "command_create_receipt")?;
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
        recovery.checkpoint(report, "before_command_delete")?;
        bounded(http.delete_guild_command(guild, created.id))
            .await
            .map_err(|_| "command_cleanup_requires_review")?;
    }
    report["owned_command_cleanup"] = json!(true);
    recovery.checkpoint(report, "command_deleted")?;
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
async fn run(report: &mut Value, recovery: Arc<Recovery>) -> Result<(), &'static str> {
    let nonce = nonce()?;
    report["nonce"] = json!(nonce);
    recovery.checkpoint(report, "nonce_selected")?;
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
    recovery.checkpoint(report, "ledger_created")?;
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
        discord_smoke(core.clone(), token.clone(), &guild, &nonce, report, &recovery).await?;
        let http = Arc::new(Http::new(token.clone()));
        let backend = Arc::new(CanaryBackend {
            inner: DiscordOperations::new(Token::from_env("DISCORD_TOKEN").map_err(|_| "missing_token")?, core.clone()).map_err(|_| "adapter_create")?,
            http: http.clone(), recovery: recovery.clone(), baselines: Mutex::new(BTreeMap::new()),
        });
        let executor = StructureExecutor::new(core, storage.clone(), backend.clone());
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
                overwrites: Some(vec![Overwrite { id: guild.to_string(), kind: OverwriteKind::Role, allow: 0, deny: VIEW_CHANNEL | SEND_MESSAGES | CONNECT }]),
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
                .any(|s| s.change != Change::Create)
        {
            return Err("unexpected_plan");
        }
        report["create_plan"] = json!(true);
        report["cleanup_protection"] = json!("private fixtures; full channel metadata and message checks; concurrent administrator activity cannot be excluded atomically");
        if plan.steps.iter().any(|s| s.approval_required) {
            executor.approve(&context, &guild, &plan.id, &plan.hash).await.map_err(|_| "fixture_approval")?;
        }
        recovery.checkpoint(report, "before_structure_apply")?;
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
            recovery.checkpoint(report, "structure_apply_returned")?;
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
            recovery.checkpoint(report, "before_repeat_apply")?;
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
            recovery.checkpoint(report, "repeat_complete")?;
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
        recovery.checkpoint(report, "structure_receipts_recovered")?;
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
            // Compare the complete REST DTO, including fields outside the shared
            // structure projection. Missing baseline/read access fails closed.
            let full = bounded(http.get_channel(id.into())).await;
            let baseline = backend.baselines.lock().unwrap().get(&channel.id).cloned();
            if baseline.is_none() || full.ok().and_then(|c| serde_json::to_value(c).ok()) != baseline {
                cleanup_ok = false;
                continue;
            }
            if channel.kind != ChannelKind::Category
                && !bounded(http.get_messages(id.into(), None, Some(1u8.try_into().unwrap()))).await.is_ok_and(|messages| messages.is_empty()) {
                cleanup_ok = false;
                continue;
            }
            report["cleanup_channel_id"] = json!(channel.id);
            recovery.checkpoint(report, "before_channel_delete")?;
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
            recovery.checkpoint(report, "channel_delete_returned")?;
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
    let report_path = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "oracle-stage5-checkpoint-{}.json",
                std::process::id()
            ))
        });
    report["report_path"] = json!(report_path);
    report["passed"] = json!(false);
    let recovery = Arc::new(Recovery {
        path: report_path,
        events: Mutex::new(Vec::new()),
    });
    let result = match recovery.checkpoint(&mut report, "starting") {
        Ok(()) => run(&mut report, recovery.clone()).await,
        Err(error) => Err(error),
    };
    report["passed"] = json!(result.is_ok());
    if let Err(error) = result {
        report["failure"] = json!(error);
    }
    if recovery.checkpoint(&mut report, "finished").is_err() {
        eprintln!("report_write_failed");
        std::process::exit(1);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("report serialization")
    );
    if result.is_err() {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn checkpoint_retains_write_receipts_across_outer_phase_updates() {
        let root = std::env::temp_dir().join(format!(
            "oracle-checkpoint-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let recovery = Recovery {
            path: root.join("report.json"),
            events: Mutex::new(Vec::new()),
        };
        let mut report =
            json!({"passed": false, "database_path": "retained-ledger", "nonce": "fixture123"});
        recovery
            .checkpoint(&mut report, "before_structure_apply")
            .unwrap();
        recovery
            .event(json!({"phase":"channel_write_receipt","channel_id":"123"}))
            .unwrap();
        // Simulate interruption before the executor returns to its caller.
        let saved: Value = serde_json::from_slice(&std::fs::read(&recovery.path).unwrap()).unwrap();
        assert_eq!(saved["phase"], "structure_mutation");
        assert_eq!(saved["recovery_events"][0]["channel_id"], "123");
        assert_eq!(saved["database_path"], "retained-ledger");
        assert_eq!(
            std::fs::metadata(&recovery.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        recovery.checkpoint(&mut report, "cleanup").unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(&recovery.path).unwrap()).unwrap();
        assert_eq!(saved["recovery_events"][0]["channel_id"], "123");
        // A failed next checkpoint leaves the previous complete report intact.
        let original = std::fs::read(&recovery.path).unwrap();
        std::fs::create_dir(
            recovery
                .path
                .with_extension(format!("checkpoint-{}", std::process::id())),
        )
        .unwrap();
        assert!(recovery.checkpoint(&mut report, "uncommitted").is_err());
        assert_eq!(std::fs::read(&recovery.path).unwrap(), original);
        std::fs::remove_dir_all(root).unwrap();
    }
}
