//! Explicit opt-in real Discord canary: lost local acknowledgement, restart,
//! confirmed edit and receipt-owned deletion. No user interactions are forged.
//! A successful HTTP response is deliberately discarded; this is not packet loss.
use oracle_core::{CoreService, Error, ErrorCode, GuildId, GuildPolicy, ModuleId};
use oracle_discord::{
    Token,
    operations::DiscordOperations,
    shared_cards::{DiscordSharedCards, SharedCardAuthority},
};
use oracle_operations::{executor::DispatchFence, shared_cards::*};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::json;
use serenity::all::{GenericChannelId, Http, MessageId};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

type Outcome<T> = Result<T, Box<dyn std::error::Error>>;
struct Scope(Mutex<SharedEffect>);
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
        let expected = self.0.lock().unwrap();
        let mut candidate = effect.clone();
        // The adapter fills the server-returned ID for its readback after a create.
        // No destination, payload, marker, revision or existing message may change.
        if expected.message_id.is_none()
            && candidate.message_id.as_deref().is_some_and(|id| {
                id.parse::<u64>()
                    .is_ok_and(|value| value > 0 && value.to_string() == id)
            })
        {
            candidate.message_id = None;
        }
        if *expected != candidate {
            return Err(Error::new(ErrorCode::ForbiddenScope));
        }
        Ok(Arc::new(Fence))
    }
}
fn checkpoint(
    root: &Path,
    phase: &str,
    effect: &SharedEffect,
    message: Option<&str>,
) -> Outcome<()> {
    let mut history = fs::read(root.join("checkpoint.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["phases"].as_array().cloned())
        .unwrap_or_default();
    history.push(json!({"phase":phase,"at_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()}));
    let bytes = serde_json::to_vec_pretty(
        &json!({"phase":phase,"phases":history,"effect":effect,"confirmed_message":message,"simulation":"fresh mode deliberately discards a confirmed response; recovery mode observes an existing uncertain create without resending"}),
    )?;
    let tmp = root.join("checkpoint.tmp");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(tmp, root.join("checkpoint.json"))?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}
async fn measured(
    root: &Path,
    phase: &str,
    operation: impl std::future::Future<Output = oracle_core::Result<SharedObservation>>,
) -> Outcome<SharedObservation> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(root.join("transport-timings.jsonl"))?;
    writeln!(file, "{}", json!({"phase":phase,"state":"started"}))?;
    file.sync_all()?;
    let started = std::time::Instant::now();
    let result = operation.await;
    let elapsed_ms = started.elapsed().as_millis();
    let category = match &result {
        Ok(SharedObservation::Confirmed { .. }) => "confirmed".to_owned(),
        Ok(SharedObservation::Missing) => "missing".to_owned(),
        Ok(SharedObservation::Unknown) => "unknown".to_owned(),
        Ok(SharedObservation::Rejected) => "rejected".to_owned(),
        Ok(SharedObservation::NotSent { reason }) => format!("not_sent:{reason:?}"),
        Err(error) => format!("error:{:?}", error.code),
    };
    writeln!(
        file,
        "{}",
        json!({"phase":phase,"state":"finished","elapsed_ms":elapsed_ms,"category":category,"boundary":"Discord transport including fresh permissions and readback; not interaction acknowledgement latency"})
    )?;
    file.sync_all()?;
    Ok(result?)
}
fn intent(revision: u64, created: u64) -> SharedIntent {
    serde_json::from_value(json!({"key":"DWQUAL8","desired_revision":revision,"destination":"runs","created_at":created,"card":{"title":"Oracle run qualification","description":"Temporary test","fields":[],"footer":""},"actions":[]})).unwrap()
}
async fn run() -> Outcome<()> {
    if std::env::var("ORACLE_DW_SHARED_LIVE").as_deref() != Ok("I_AUTHORIZE_ONE_TEST_CARD") {
        return Err("explicit opt-in required".into());
    }
    let root = std::path::PathBuf::from(std::env::var("DW_SHARED_LIVE_DIR")?);
    let recover = match std::env::var("ORACLE_DW_SHARED_RECOVER") {
        Ok(value) if value == "OBSERVE_EXISTING_CREATE" => true,
        Err(std::env::VarError::NotPresent) => false,
        _ => return Err("invalid recovery opt-in".into()),
    };
    // Fresh mode never reuses a ledger. Recovery is explicitly readback-only for create.
    if recover {
        if !root.join("journal.sqlite").is_file() || !root.join("checkpoint.json").is_file() {
            return Err("existing recovery artifacts required".into());
        }
    } else {
        fs::DirBuilder::new().mode(0o700).create(&root)?;
    }
    let guild = GuildId::new(std::env::var("GUILD_ID")?)?;
    let channel = std::env::var("DW_SHARED_LIVE_CHANNEL")?;
    let token = Token::from_env("DISCORD_TOKEN")?;
    let http = Http::new(token.clone());
    let application = http.get_current_application_info().await?;
    let bot = http.get_current_user().await?;
    let mut timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut marker = format!("oracle-dw-stage8-{}-{timestamp}", std::process::id());
    let mut effect = SharedEffect {
        effect_id: format!("{marker}-create"),
        guild: guild.clone(),
        module: ModuleId::new("test.dw-qualification")?,
        run_id: "DWQUAL8".into(),
        target: SharedTarget {
            channel_id: channel.clone(),
            application_id: application.id.to_string(),
            bot_id: bot.id.to_string(),
        },
        message_id: None,
        desired_revision: 1,
        marker: marker.clone(),
        payload: json!({"content":"","embeds":[{"title":"Oracle run qualification · temporary test","description":"Testing recovery after a discarded acknowledgement.","fields":[{"name":"Starts","value":format!("<t:{}:F>\n<t:{}:R>",timestamp+3600,timestamp+3600),"inline":false},{"name":"Estimated duration","value":"1 h 30 min","inline":false}],"footer":{"text":marker}}],"components":[],"allowed_mentions":{"parse":[]}}),
    };
    if recover {
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("checkpoint.json"))?)?;
        if !matches!(
            saved["phase"].as_str(),
            Some("before_create" | "acknowledgement_discarded")
        ) {
            return Err("recovery only accepts an unresolved initial create".into());
        }
        let prior: SharedEffect = serde_json::from_value(saved["effect"].clone())?;
        if prior.guild != guild
            || prior.target != effect.target
            || prior.module != effect.module
            || prior.run_id != effect.run_id
            || prior.desired_revision != 1
            || prior.message_id.is_some()
            || !prior.marker.starts_with("oracle-dw-stage8-")
        {
            return Err("recovery checkpoint target mismatch".into());
        }
        effect = prior;
        marker = effect.marker.clone();
    } else {
        checkpoint(&root, "before_create", &effect, None)?;
    }
    let config = DatabaseConfig::Sqlite {
        path: root.join("journal.sqlite"),
    };
    let store = Arc::new(Storage::open(config.clone()).await?);
    store
        .initialize_guilds(std::slice::from_ref(&guild))
        .await?;
    let core = Arc::new(CoreService::new(
        store.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![],
        }],
    ));
    let scope = Arc::new(Scope(Mutex::new(effect.clone())));
    let adapter = DiscordSharedCards::new(
        Arc::new(DiscordOperations::new(token, core)?),
        scope.clone(),
    );
    let journal = SharedCardJournal::new(store.clone());
    if recover {
        let saved = journal
            .get(&guild, &effect.module, &effect.run_id)
            .await?
            .ok_or("recovery journal missing")?
            .1;
        if saved.effect.as_ref() != Some(&effect)
            || saved.identity.is_some()
            || saved.confirmed_revision.is_some()
            || !matches!(saved.phase, SharedPhase::Pending | SharedPhase::Unknown)
            || saved.desired.desired_revision != 1
        {
            return Err("recovery frozen effect does not match durable journal".into());
        }
        timestamp = saved.desired.created_at;
        // A second recovery attempt needs explicit review; it must never repeat an edit.
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(root.join("recovery.claim"))?
            .sync_all()?;
        checkpoint(&root, "recovering_existing_create_no_resend", &effect, None)?;
    } else {
        journal
            .enqueue(&guild, &effect.module, &effect.run_id, intent(1, timestamp))
            .await?;
        journal.prepare(effect.clone()).await?;
        // Exactly one create attempt. On error the prepared effect stays recoverable.
        let acknowledged = measured(
            &root,
            "create",
            adapter.execute(&effect, CancellationToken::new()),
        )
        .await?;
        if !matches!(acknowledged, SharedObservation::Confirmed { .. }) {
            return Err("create was not confirmed; inspect checkpoint, do not resend".into());
        }
        drop(acknowledged);
    }
    let unknown = journal.settle(&effect, SharedObservation::Unknown).await?;
    assert_eq!(unknown.phase, SharedPhase::Unknown);
    checkpoint(
        &root,
        if recover {
            "existing_create_pending_readback"
        } else {
            "acknowledgement_discarded"
        },
        &effect,
        None,
    )?;
    drop(journal);
    drop(adapter);
    store.close().await?;
    let reopened = Arc::new(Storage::open(config).await?);
    let core = Arc::new(CoreService::new(
        reopened.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![],
        }],
    ));
    let adapter = DiscordSharedCards::new(
        Arc::new(DiscordOperations::new(
            Token::from_env("DISCORD_TOKEN")?,
            core,
        )?),
        scope.clone(),
    );
    let journal = SharedCardJournal::new(reopened.clone());
    let saved = journal
        .get(&guild, &effect.module, &effect.run_id)
        .await?
        .ok_or("journal missing")?
        .1;
    assert_eq!(saved.phase, SharedPhase::Unknown);
    let recovered = measured(
        &root,
        "recover_create",
        adapter.observe(&effect, CancellationToken::new()),
    )
    .await?;
    let SharedObservation::Confirmed { message_id } = &recovered else {
        return Err("create readback unresolved; do not resend".into());
    };
    let message_id = message_id.clone();
    journal.settle(&effect, recovered).await?;
    checkpoint(&root, "recovered_same_message", &effect, Some(&message_id))?;
    effect.effect_id = format!("{marker}-edit");
    effect.message_id = Some(message_id.clone());
    effect.desired_revision = 2;
    effect.payload["embeds"][0]["description"] =
        json!("Recovery confirmed. Testing an edit of the same message.");
    *scope.0.lock().unwrap() = effect.clone();
    journal
        .enqueue(&guild, &effect.module, &effect.run_id, intent(2, timestamp))
        .await?;
    journal.prepare(effect.clone()).await?;
    checkpoint(&root, "before_edit", &effect, Some(&message_id))?;
    let edited = measured(
        &root,
        "edit",
        adapter.execute(&effect, CancellationToken::new()),
    )
    .await?;
    if !matches!(&edited,SharedObservation::Confirmed {message_id:id} if id == &message_id) {
        return Err("edit unresolved; inspect checkpoint".into());
    }
    journal.settle(&effect, edited).await?;
    assert!(
        matches!(measured(&root, "confirm_edit", adapter.observe(&effect,CancellationToken::new())).await?,SharedObservation::Confirmed {message_id:id} if id==message_id)
    );
    checkpoint(
        &root,
        "before_receipt_owned_delete",
        &effect,
        Some(&message_id),
    )?;
    // Exact confirmed bot-owned message, with independent author/marker readback.
    let channel_id = GenericChannelId::new(channel.parse()?);
    let id = MessageId::new(message_id.parse()?);
    let existing = http.get_message(channel_id, id).await?;
    if existing.author.id != bot.id
        || !existing.embeds.iter().any(|embed| {
            embed
                .footer
                .as_ref()
                .is_some_and(|footer| footer.text.contains(&marker))
        })
    {
        return Err("cleanup identity mismatch".into());
    }
    http.delete_message(
        channel_id,
        id,
        Some("Oracle receipt-owned qualification cleanup"),
    )
    .await?;
    assert!(matches!(
        measured(
            &root,
            "confirm_deleted",
            adapter.observe(&effect, CancellationToken::new())
        )
        .await?,
        SharedObservation::Missing
    ));
    journal
        .mark_missing(&guild, &effect.module, &effect.run_id, &message_id)
        .await?;
    checkpoint(
        &root,
        "passed_deleted_message_observed_missing",
        &effect,
        Some(&message_id),
    )?;
    reopened.close().await?;
    Ok(())
}
#[tokio::main]
async fn main() {
    if !matches!(
        tokio::time::timeout(std::time::Duration::from_secs(180), run()).await,
        Ok(Ok(()))
    ) {
        eprintln!(
            "Qualification stopped. Inspect the private checkpoint; never blindly resend an interrupted create."
        );
        std::process::exit(1);
    }
    println!("Shared-card qualification passed; test message deleted. See private checkpoint.");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn test_authority_allows_create_readback_only_and_rejects_other_changes() {
        let effect = SharedEffect {
            effect_id: "qualification-create".into(),
            guild: GuildId::new("123").unwrap(),
            module: ModuleId::new("test.dw-qualification").unwrap(),
            run_id: "DWQUAL8".into(),
            target: SharedTarget {
                channel_id: "456".into(),
                application_id: "789".into(),
                bot_id: "789".into(),
            },
            message_id: None,
            desired_revision: 1,
            marker: "unique-test".into(),
            payload: json!({}),
        };
        let scope = Scope(Mutex::new(effect.clone()));
        scope.authorize(&effect).await.unwrap();
        let mut foreign = effect.clone();
        foreign.target.channel_id = "457".into();
        assert!(scope.authorize(&foreign).await.is_err());
        foreign = effect.clone();
        foreign.guild = GuildId::new("124").unwrap();
        assert!(scope.authorize(&foreign).await.is_err());
        foreign = effect.clone();
        foreign.message_id = Some("999".into());
        scope.authorize(&foreign).await.unwrap();
        foreign.payload = json!({"content":"unapproved"});
        assert!(scope.authorize(&foreign).await.is_err());
        for invalid in ["0", "0999", "-1", "not-an-id"] {
            foreign = effect.clone();
            foreign.message_id = Some(invalid.into());
            assert!(scope.authorize(&foreign).await.is_err());
        }
        let mut editing = effect.clone();
        editing.message_id = Some("999".into());
        let edit_scope = Scope(Mutex::new(editing.clone()));
        edit_scope.authorize(&editing).await.unwrap();
        editing.message_id = Some("1000".into());
        assert!(edit_scope.authorize(&editing).await.is_err());
        foreign = effect.clone();
        foreign.payload = json!({"content":"unapproved"});
        assert!(scope.authorize(&foreign).await.is_err());
    }
}
