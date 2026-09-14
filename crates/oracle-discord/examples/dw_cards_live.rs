//! Explicitly authorized test-guild transport canary, not human DW acceptance.
//! Reads credentials from process environment. Deletes only its own verified fixture.
use oracle_core::{CoreService, Error, ErrorCode, GuildId, GuildPolicy, ModuleId};
use oracle_discord::{
    Token,
    operations::DiscordOperations,
    shared_cards::{DiscordSharedCards, SharedCardAuthority},
};
use oracle_operations::{executor::DispatchFence, published::render_private_card, shared_cards::*};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use serenity::all::{ChannelType, Http, MessageId};
use std::{path::Path, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

struct Scope {
    guild: GuildId,
    module: ModuleId,
    run: String,
    target: SharedTarget,
}
struct Fence;
impl DispatchFence for Fence {
    fn dispatch(
        &self,
        send: &mut dyn FnMut() -> oracle_core::Result<()>,
    ) -> oracle_core::Result<()> {
        send()
    }
}
#[async_trait::async_trait]
impl SharedCardAuthority for Scope {
    async fn authorize(
        &self,
        effect: &SharedEffect,
    ) -> oracle_core::Result<Arc<dyn DispatchFence>> {
        if effect.guild != self.guild
            || effect.module != self.module
            || effect.run_id != self.run
            || effect.target != self.target
        {
            return Err(Error::new(ErrorCode::ForbiddenScope));
        }
        Ok(Arc::new(Fence))
    }
}
fn checkpoint(path: &Path, report: &Value) -> Result<(), &'static str> {
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(report).map_err(|_| "report_encode")?,
    )
    .map_err(|_| "report_write")?;
    std::fs::rename(tmp, path).map_err(|_| "report_rename")
}
async fn bounded<T>(
    f: impl std::future::Future<Output = Result<T, serenity::Error>>,
) -> Result<T, &'static str> {
    tokio::time::timeout(Duration::from_secs(25), f)
        .await
        .map_err(|_| "discord_timeout")?
        .map_err(|_| "discord_request")
}
fn intent(run: &str, revision: u64, complete: bool) -> SharedIntent {
    serde_json::from_value(json!({"key":run,"desired_revision":revision,"destination":"runs","created_at":1,"card":{"title":"Oracle DW Stage 7 transport test","description":if complete {"Completed • transport verification only. No human acceptance run performed."} else {"Open • transport verification only. Fixture controls are not connected to a running DW session."},"fields":[],"footer":"Synthetic test fixture"},"actions":if complete {json!([])} else {json!([{"name":"join","label":"Test Join","operation":"runs.ui","input":{}}])}})).unwrap()
}
fn effect(scope: &Scope, record: &SharedRecord) -> Result<SharedEffect, &'static str> {
    let effect_id = uuid::Uuid::new_v4().to_string();
    let marker = format!("oracle-shared:{effect_id}");
    let version = record
        .identity
        .as_ref()
        .map(|i| i.control_version.as_str())
        .unwrap_or(&effect_id);
    let mut card = render_private_card(
        &json!({"reply":{"card":record.desired.card,"choices":[],"buttons":[]}}),
    )
    .map_err(|_| "render")?;
    card.embed["footer"] = json!({"text":format!("Synthetic test fixture\n{marker}")});
    let mut buttons = Vec::new();
    for (index, action) in record.desired.actions.iter().enumerate() {
        buttons.push(json!({"type":2,"style":2,"label":action.label,"custom_id":record.control_id(&scope.guild, version, index).map_err(|_| "control")?}));
    }
    Ok(SharedEffect {
        effect_id,
        guild: scope.guild.clone(),
        module: scope.module.clone(),
        run_id: scope.run.clone(),
        target: scope.target.clone(),
        message_id: record.identity.as_ref().map(|i| i.message_id.clone()),
        desired_revision: record.desired.desired_revision,
        marker,
        payload: json!({"content":"","embeds":[card.embed],"components":if buttons.is_empty(){json!([])}else{json!([{"type":1,"components":buttons}])},"allowed_mentions":{"parse":[]},"attachments":[]}),
    })
}
async fn publish(
    journal: &SharedCardJournal,
    transport: &DiscordSharedCards,
    effect: &SharedEffect,
) -> Result<SharedRecord, &'static str> {
    journal
        .prepare(effect.clone())
        .await
        .map_err(|_| "prepare")?;
    let observed = transport
        .execute(effect, CancellationToken::new())
        .await
        .map_err(|_| "execute")?;
    let record = journal
        .settle(effect, observed)
        .await
        .map_err(|_| "settle")?;
    if record.phase != SharedPhase::Confirmed {
        return Err("unconfirmed_effect_no_retry");
    }
    Ok(record)
}
async fn run(path: &Path, report: &mut Value) -> Result<(), &'static str> {
    let token = Token::from_env("DISCORD_TOKEN").map_err(|_| "missing_token")?;
    let guild: GuildId = std::env::var("GUILD_ID")
        .map_err(|_| "missing_guild")?
        .parse()
        .map_err(|_| "invalid_guild")?;
    let run = format!("canary-{}", uuid::Uuid::new_v4());
    let db = path.with_extension("sqlite");
    let storage = Arc::new(
        Storage::open(DatabaseConfig::Sqlite { path: db.clone() })
            .await
            .map_err(|_| "storage")?,
    );
    storage
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .map_err(|_| "initialize")?;
    let core = Arc::new(CoreService::new(
        storage.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![],
        }],
    ));
    let adapter = Arc::new(DiscordOperations::new(token.clone(), core).map_err(|_| "adapter")?);
    let http = Http::new(token);
    let remote_guild = serenity::all::GuildId::new(guild.as_str().parse().map_err(|_| "guild_id")?);
    let channels = bounded(http.get_channels(remote_guild)).await?;
    let mut candidates: Vec<_> = channels
        .iter()
        .filter(|c| c.base.kind == ChannelType::Text)
        .collect();
    candidates.sort_by_key(|c| c.id.get());
    let mut target = None;
    for channel in candidates {
        if let Ok(found) = adapter
            .shared_target(&guild, &channel.id.to_string(), &CancellationToken::new())
            .await
        {
            target = Some(found);
            break;
        }
    }
    let scope = Arc::new(Scope {
        guild: guild.clone(),
        module: "dandys-world".parse().map_err(|_| "module_id")?,
        run,
        target: target.ok_or("no_eligible_channel")?,
    });
    let transport = DiscordSharedCards::new(adapter.clone(), scope.clone());
    let journal = SharedCardJournal::new(storage.clone());
    *report = json!({"kind":"synthetic transport canary; not human or mobile acceptance","guild_id":guild,"channel_id":scope.target.channel_id,"run_id":scope.run,"database":db,"phase":"before_create"});
    checkpoint(path, report)?;
    let record = journal
        .enqueue(
            &guild,
            &scope.module,
            &scope.run,
            intent(&scope.run, 1, false),
        )
        .await
        .map_err(|_| "enqueue")?;
    let create = effect(&scope, &record)?;
    let created = publish(&journal, &transport, &create).await?;
    let identity = created.identity.ok_or("identity")?;
    report["created_message_id"] = json!(identity.message_id);
    report["create_readback"] = json!(true);
    checkpoint(path, report)?;
    let record = journal
        .enqueue(
            &guild,
            &scope.module,
            &scope.run,
            intent(&scope.run, 2, true),
        )
        .await
        .map_err(|_| "enqueue_edit")?;
    let edit = effect(&scope, &record)?;
    let edited = publish(&journal, &transport, &edit).await?;
    if edited.identity.as_ref().is_none_or(|i| {
        i.message_id != identity.message_id || i.control_version != identity.control_version
    }) {
        return Err("edit_identity");
    }
    report["completed_edit_same_identity_no_actions"] = json!(true);
    checkpoint(path, report)?;
    // Release the exclusive database lease, then rebuild the host boundary from disk.
    storage.close().await.map_err(|_| "close_before_restart")?;
    let reopened = Arc::new(
        Storage::open(DatabaseConfig::Sqlite { path: db })
            .await
            .map_err(|_| "reopen")?,
    );
    let restarted_core = Arc::new(CoreService::new(
        reopened.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![],
        }],
    ));
    let restarted_adapter = Arc::new(
        DiscordOperations::new(
            Token::from_env("DISCORD_TOKEN").map_err(|_| "missing_token")?,
            restarted_core,
        )
        .map_err(|_| "restart_adapter")?,
    );
    let transport = DiscordSharedCards::new(restarted_adapter, scope.clone());
    let restarted = SharedCardJournal::new(reopened.clone());
    let (_, saved) = restarted
        .get(&guild, &scope.module, &scope.run)
        .await
        .map_err(|_| "reload")?
        .ok_or("missing_record")?;
    if saved.phase != SharedPhase::Confirmed || saved.confirmed_revision != Some(2) {
        return Err("restart_state");
    }
    if !matches!(
        transport
            .observe(&edit, CancellationToken::new())
            .await
            .map_err(|_| "restart_observe")?,
        SharedObservation::Confirmed { .. }
    ) {
        return Err("restart_remote");
    }
    report["restart_journal_readback"] = json!(true);
    checkpoint(path, report)?;
    // Only this exact freshly confirmed bot-owned message is deleted.
    let channel = serenity::all::GenericChannelId::new(
        scope.target.channel_id.parse().map_err(|_| "channel_id")?,
    );
    bounded(http.delete_message(
        channel,
        MessageId::new(identity.message_id.parse().map_err(|_| "message_id")?),
        Some("Oracle owned Stage 7 canary missing-message check"),
    ))
    .await?;
    if !matches!(
        transport
            .observe(&edit, CancellationToken::new())
            .await
            .map_err(|_| "missing_observe")?,
        SharedObservation::Missing
    ) {
        return Err("missing_detection");
    }
    restarted
        .mark_missing(&guild, &scope.module, &scope.run, &identity.message_id)
        .await
        .map_err(|_| "mark_missing")?;
    report["owned_fixture_deleted_missing_detected"] = json!(true);
    checkpoint(path, report)?;
    restarted
        .repost(&guild, &scope.module, &scope.run)
        .await
        .map_err(|_| "explicit_repost")?;
    let (_, record) = restarted
        .get(&guild, &scope.module, &scope.run)
        .await
        .map_err(|_| "repost_get")?
        .ok_or("repost_record")?;
    let repost = effect(&scope, &record)?;
    let final_record = publish(&restarted, &transport, &repost).await?;
    let final_identity = final_record.identity.ok_or("repost_identity")?;
    if final_identity.message_id == identity.message_id {
        return Err("repost_reused_identity");
    }
    report["reposted_message_id"] = json!(final_identity.message_id);
    report["explicit_repost_readback"] = json!(true);
    report["phase"] = json!("complete");
    checkpoint(path, report)?;
    reopened.close().await.map_err(|_| "close_reopened")?;

    Ok(())
}
#[tokio::main]
async fn main() {
    let root = Path::new("target/dw-stage7");
    std::fs::create_dir_all(root).expect("report directory");
    let path = root.join(format!("live-cards-{}.json", uuid::Uuid::new_v4()));
    let mut report = json!({"phase":"starting"});
    let result = tokio::time::timeout(Duration::from_secs(240), run(&path, &mut report))
        .await
        .unwrap_or(Err("overall_timeout_no_retry"));
    report["result"] = json!(
        result
            .as_ref()
            .map(|_| "passed")
            .unwrap_or_else(|error| error)
    );
    let _ = checkpoint(&path, &report);
    println!("{}", report);
    if result.is_err() {
        std::process::exit(1);
    }
}
