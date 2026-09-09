use async_trait::async_trait;
use oracle_core::*;
use oracle_operations::{
    commands::*,
    executor::{SendGuard, now},
};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
#[derive(Default)]
struct State {
    commands: Vec<PublishedCommand>,
    writes: usize,
    lost_ack: bool,
    mismatch: bool,
    cancel: Option<CancellationToken>,
    revoke_before_write: Option<Arc<std::sync::atomic::AtomicBool>>,
}
#[derive(Default)]
struct Backend(Mutex<State>);
#[async_trait]
impl CommandBackend for Backend {
    async fn list(&self, _: &GuildId) -> Result<Vec<PublishedCommand>> {
        Ok(self.0.lock().unwrap().commands.clone())
    }
    async fn create(
        &self,
        _: &GuildId,
        definition: &Value,
        guard: &SendGuard,
    ) -> Result<PublishedCommand> {
        if let Some(active) = self.0.lock().unwrap().revoke_before_write.take() {
            active.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        guard.dispatch(|| {
            let mut state = self.0.lock().unwrap();
            state.writes += 1;
            let mut command = PublishedCommand {
                id: format!("{}", 100 + state.writes),
                definition: definition.clone(),
            };
            if state.mismatch {
                command.definition["description"] = json!("wrong");
            }
            state.commands.push(command.clone());
            if let Some(cancel) = &state.cancel {
                cancel.cancel();
            }
            if state.lost_ack {
                return Err(Error::new(ErrorCode::UnknownOutcome));
            }
            Ok(command)
        })
    }
    async fn edit(
        &self,
        _: &GuildId,
        id: &str,
        definition: &Value,
        guard: &SendGuard,
    ) -> Result<PublishedCommand> {
        guard.dispatch(|| {
            let mut state = self.0.lock().unwrap();
            state.writes += 1;
            let command = state.commands.iter_mut().find(|c| c.id == id).unwrap();
            command.definition = definition.clone();
            Ok(command.clone())
        })
    }
    async fn delete(&self, _: &GuildId, id: &str, guard: &SendGuard) -> Result<()> {
        guard.dispatch(|| {
            let mut state = self.0.lock().unwrap();
            state.writes += 1;
            state.commands.retain(|c| c.id != id);
            Ok(())
        })
    }
}
fn desired(name: &str) -> DesiredCommand {
    DesiredCommand {
        owner: ModuleId::new("logger").unwrap(),
        definition: json!({"name":name,"description":"description","type":1}),
        route: Some(CommandRoute {
            session: "session".into(),
            generation: 1,
            epoch: 2,
        }),
    }
}
async fn setup() -> (tempfile::TempDir, Arc<Storage>, Arc<Backend>, GuildId) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Storage::open(DatabaseConfig::Sqlite {
            path: root.path().join("db"),
        })
        .await
        .unwrap(),
    );
    let guild = GuildId::new("123").unwrap();
    store
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    (root, store, Arc::new(Backend::default()), guild)
}
#[tokio::test]
async fn noop_add_remove_edit_preserve_ids_and_unrelated() {
    let (_root, store, backend, guild) = setup().await;
    backend.0.lock().unwrap().commands.push(PublishedCommand {
        id: "50".into(),
        definition: desired("unrelated").definition,
    });
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    let cancel = CancellationToken::new();
    let one = desired("one");
    let two = desired("two");
    assert_eq!(
        reconciler
            .reconcile(&guild, std::slice::from_ref(&one), &cancel, now() + 60)
            .await
            .unwrap()
            .created,
        1
    );
    let first = backend.0.lock().unwrap().commands.clone();
    assert_eq!(
        reconciler
            .reconcile(&guild, std::slice::from_ref(&one), &cancel, now() + 60)
            .await
            .unwrap()
            .unchanged,
        1
    );
    assert_eq!(backend.0.lock().unwrap().writes, 1);
    assert_eq!(
        reconciler
            .reconcile(&guild, &[one.clone(), two.clone()], &cancel, now() + 60)
            .await
            .unwrap()
            .created,
        1
    );
    assert_eq!(
        reconciler
            .reconcile(&guild, std::slice::from_ref(&one), &cancel, now() + 60)
            .await
            .unwrap()
            .deleted,
        1
    );
    assert_eq!(backend.0.lock().unwrap().commands, first);
    let mut edit = one.clone();
    edit.definition["description"] = json!("updated");
    assert_eq!(
        reconciler
            .reconcile(&guild, &[edit], &cancel, now() + 60)
            .await
            .unwrap()
            .edited,
        1
    );
    assert_eq!(backend.0.lock().unwrap().commands[1].id, first[1].id);
    store.close().await.unwrap();
}
#[tokio::test]
async fn drift_and_unowned_collision_never_write() {
    let (_root, store, backend, guild) = setup().await;
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    let cancel = CancellationToken::new();
    let one = desired("one");
    reconciler
        .reconcile(&guild, std::slice::from_ref(&one), &cancel, now() + 60)
        .await
        .unwrap();
    backend.0.lock().unwrap().commands[0].definition["description"] = json!("external");
    assert_eq!(
        reconciler
            .reconcile(&guild, std::slice::from_ref(&one), &cancel, now() + 60)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        reconciler
            .reconcile(&guild, &[], &cancel, now() + 60)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let other = desired("other");
    backend.0.lock().unwrap().commands.push(PublishedCommand {
        id: "300".into(),
        definition: other.definition.clone(),
    });
    assert!(
        reconciler
            .reconcile(&guild, &[other], &cancel, now() + 60)
            .await
            .is_err()
    );
    assert_eq!(backend.0.lock().unwrap().writes, 1);
    store.close().await.unwrap();
}
#[tokio::test]
async fn ambiguous_create_survives_reopen_and_never_retries() {
    let (root, store, backend, guild) = setup().await;
    backend.0.lock().unwrap().lost_ack = true;
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    let cancel = CancellationToken::new();
    let one = desired("one");
    assert_eq!(
        reconciler
            .reconcile(&guild, std::slice::from_ref(&one), &cancel, now() + 60)
            .await
            .unwrap_err()
            .code,
        ErrorCode::UnknownOutcome
    );
    assert!(reconciler.bindings(&guild).await.unwrap().is_empty());
    drop(reconciler);
    store.close().await.unwrap();
    drop(store);
    let reopened = Arc::new(
        Storage::open(DatabaseConfig::Sqlite {
            path: root.path().join("db"),
        })
        .await
        .unwrap(),
    );
    let reconciler = CommandReconciler::new(reopened.clone(), backend.clone());
    assert_eq!(
        reconciler
            .reconcile(&guild, &[one], &cancel, now() + 60)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RecoveryRequired
    );
    assert_eq!(backend.0.lock().unwrap().writes, 1);
    reopened.close().await.unwrap();
}
#[tokio::test]
async fn cancellation_fences_remaining_commands_and_keeps_pending() {
    let (_root, store, backend, guild) = setup().await;
    let cancel = CancellationToken::new();
    backend.0.lock().unwrap().cancel = Some(cancel.clone());
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    assert_eq!(
        reconciler
            .reconcile(
                &guild,
                &[desired("one"), desired("two")],
                &cancel,
                now() + 60
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::Cancelled
    );
    assert_eq!(backend.0.lock().unwrap().writes, 1);
    assert!(reconciler.bindings(&guild).await.unwrap().is_empty());
    store.close().await.unwrap();
}
#[tokio::test]
async fn wrong_readback_retains_pending() {
    let (_root, store, backend, guild) = setup().await;
    backend.0.lock().unwrap().mismatch = true;
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    let cancel = CancellationToken::new();
    assert_eq!(
        reconciler
            .reconcile(&guild, &[desired("one")], &cancel, now() + 60)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Integrity
    );
    assert!(reconciler.bindings(&guild).await.unwrap().is_empty());
    store.close().await.unwrap();
}
#[test]
fn canonical_server_fields_and_empty_localizations() {
    assert_eq!(canonical_definition(&json!({"id":"1","application_id":"2","guild_id":"3","version":"4","name":"one","description":"x","name_localizations":{},"description_localizations":null})).unwrap(),json!({"name":"one","description":"x","type":1}));
    assert!(canonical_definition(&json!({"name":"BAD","type":1})).is_err());
}

#[tokio::test]
async fn registry_revocation_at_backend_send_blocks_publication() {
    use oracle_operations::executor::DispatchFence;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct RegistryFence(Arc<AtomicBool>);
    impl DispatchFence for RegistryFence {
        fn dispatch(&self, send: &mut dyn FnMut() -> Result<()>) -> Result<()> {
            if !self.0.load(Ordering::SeqCst) {
                return Err(Error::new(ErrorCode::ModuleUnavailable));
            }
            send()
        }
    }
    let (_root, store, backend, guild) = setup().await;
    let active = Arc::new(AtomicBool::new(true));
    backend.0.lock().unwrap().revoke_before_write = Some(active.clone());
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    let result = reconciler
        .reconcile_fenced(
            &guild,
            &[desired("one")],
            &CancellationToken::new(),
            now() + 60,
            Arc::new(RegistryFence(active)),
        )
        .await;
    assert_eq!(result.unwrap_err().code, ErrorCode::ModuleUnavailable);
    assert_eq!(backend.0.lock().unwrap().writes, 0);
    assert!(backend.0.lock().unwrap().commands.is_empty());
    let bindings = reconciler.bindings(&guild).await.unwrap();
    assert!(
        bindings.is_empty(),
        "pending publication must not become a live route"
    );
    let records = store
        .workflow_list(&guild, WorkflowKind::CommandBinding, None, 100)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    let pending: CommandBinding = serde_json::from_value(records[0].value.clone()).unwrap();
    assert_eq!(pending.pending, Some(PendingCommand::Create));
    store.close().await.unwrap();
}

fn bootstrap_definitions() -> (Value, Value) {
    let legacy = json!({"name":"oracle","description":"Oracle framework administration","type":1,"default_member_permissions":"32","options":[{"name":"status","description":"Show framework status","type":1},{"name":"control","description":"Pause or resume this server","type":1,"options":[{"name":"action","description":"Requested control","type":3,"required":true,"choices":[{"name":"pause","value":"pause"},{"name":"resume","value":"resume"}]}]}]});
    let mut current = legacy.clone();
    current["options"].as_array_mut().unwrap().push(json!({"name":"structure","description":"Server structure","type":2,"options":[{"name":"inspect","description":"Inspect structure","type":1}]}));
    (legacy, current)
}
#[tokio::test]
async fn known_legacy_bootstrap_is_adopted_edited_and_restart_is_noop() {
    let (_root, store, backend, guild) = setup().await;
    let (legacy, current) = bootstrap_definitions();
    let owner = ModuleId::new("oracle.bootstrap").unwrap();
    let unrelated = PublishedCommand {
        id: "999".into(),
        definition: desired("unrelated").definition,
    };
    backend.0.lock().unwrap().commands = vec![
        PublishedCommand {
            id: "404".into(),
            definition: legacy.clone(),
        },
        unrelated.clone(),
    ];
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    assert!(
        reconciler
            .adopt_known(&guild, &owner, &[legacy.clone(), current.clone()])
            .await
            .unwrap()
    );
    assert_eq!(backend.0.lock().unwrap().writes, 0);
    let adopted = reconciler.bindings(&guild).await.unwrap();
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].id.as_deref(), Some("404"));
    assert_eq!(adopted[0].owner, owner);
    let desired = DesiredCommand {
        owner: owner.clone(),
        definition: current.clone(),
        route: None,
    };
    let report = reconciler
        .reconcile(
            &guild,
            std::slice::from_ref(&desired),
            &CancellationToken::new(),
            now() + 60,
        )
        .await
        .unwrap();
    assert_eq!(report.edited, 1);
    assert_eq!(report.created, 0);
    assert_eq!(report.deleted, 0);
    assert_eq!(backend.0.lock().unwrap().commands[0].id, "404");
    assert_eq!(backend.0.lock().unwrap().commands[1], unrelated);
    drop(reconciler);
    let restarted = CommandReconciler::new(store.clone(), backend.clone());
    assert!(
        !restarted
            .adopt_known(&guild, &owner, &[legacy, current])
            .await
            .unwrap()
    );
    assert_eq!(
        restarted
            .reconcile(&guild, &[desired], &CancellationToken::new(), now() + 60)
            .await
            .unwrap()
            .unchanged,
        1
    );
    assert_eq!(backend.0.lock().unwrap().writes, 1);
    store.close().await.unwrap();
}
#[tokio::test]
async fn unknown_bootstrap_drift_is_not_adopted_or_overwritten() {
    let (_root, store, backend, guild) = setup().await;
    let (legacy, current) = bootstrap_definitions();
    let mut drift = legacy.clone();
    drift["options"][0]["description"] = json!("external change");
    backend.0.lock().unwrap().commands.push(PublishedCommand {
        id: "404".into(),
        definition: drift,
    });
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    assert_eq!(
        reconciler
            .adopt_known(
                &guild,
                &ModuleId::new("oracle.bootstrap").unwrap(),
                &[legacy, current]
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(backend.0.lock().unwrap().writes, 0);
    assert!(reconciler.bindings(&guild).await.unwrap().is_empty());
    assert!(
        store
            .workflow_list(&guild, WorkflowKind::CommandBinding, None, 100)
            .await
            .unwrap()
            .is_empty()
    );
    store.close().await.unwrap();
}
#[tokio::test]
async fn bootstrap_adoption_preserves_existing_owner_and_pending_record() {
    let (_root, store, backend, guild) = setup().await;
    let (legacy, current) = bootstrap_definitions();
    let owner = ModuleId::new("oracle.bootstrap").unwrap();
    let binding = CommandBinding {
        owner: owner.clone(),
        id: Some("404".into()),
        definition: legacy.clone(),
        route: None,
        pending: Some(PendingCommand::Edit),
        target: Some(current.clone()),
        deleted: false,
    };
    let saved = store
        .workflow_put(
            &guild,
            WorkflowKind::CommandBinding,
            "1:oracle",
            None,
            &serde_json::to_value(&binding).unwrap(),
        )
        .await
        .unwrap();
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    assert!(
        !reconciler
            .adopt_known(&guild, &owner, &[legacy.clone(), current.clone()])
            .await
            .unwrap()
    );
    assert_eq!(
        reconciler
            .adopt_known(
                &guild,
                &ModuleId::new("other-owner").unwrap(),
                &[legacy, current]
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let retained = store
        .workflow_get(&guild, WorkflowKind::CommandBinding, "1:oracle")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.revision, saved.revision);
    assert_eq!(retained.value, saved.value);
    assert_eq!(backend.0.lock().unwrap().writes, 0);
    store.close().await.unwrap();
}
#[tokio::test]
async fn absent_bootstrap_uses_normal_create_and_arbitrary_commands_cannot_be_adopted() {
    let (_root, store, backend, guild) = setup().await;
    let (legacy, current) = bootstrap_definitions();
    let owner = ModuleId::new("oracle.bootstrap").unwrap();
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    assert!(
        !reconciler
            .adopt_known(&guild, &owner, &[legacy, current.clone()])
            .await
            .unwrap()
    );
    assert!(
        reconciler
            .adopt_known(&guild, &owner, &[desired("unrelated").definition])
            .await
            .is_err()
    );
    assert_eq!(
        reconciler
            .reconcile(
                &guild,
                &[DesiredCommand {
                    owner,
                    definition: current,
                    route: None
                }],
                &CancellationToken::new(),
                now() + 60
            )
            .await
            .unwrap()
            .created,
        1
    );
    store.close().await.unwrap();
}

#[test]
fn canonical_option_defaults_do_not_hide_real_constraint_changes() {
    let baseline = json!({"name":"oracle","description":"test","options":[{"type":3,"name":"input","description":"JSON"}]});
    let mut defaults = baseline.clone();
    let option = defaults["options"][0].as_object_mut().unwrap();
    for (key, value) in [
        ("required", json!(false)),
        ("autocomplete", json!(false)),
        ("choices", json!([])),
        ("file_types", json!([])),
        ("channel_types", json!([])),
        ("min_length", Value::Null),
        ("max_value", Value::Null),
    ] {
        option.insert(key.into(), value);
    }
    assert_eq!(
        canonical_definition(&baseline).unwrap(),
        canonical_definition(&defaults).unwrap()
    );
    for (key, value) in [
        ("required", json!(true)),
        ("autocomplete", json!(true)),
        ("min_value", json!(0)),
        ("max_length", json!(10)),
        ("channel_types", json!([0])),
        ("file_types", json!(["image/png"])),
    ] {
        let mut changed = baseline.clone();
        changed["options"][0][key] = value;
        assert_ne!(
            canonical_definition(&baseline).unwrap(),
            canonical_definition(&changed).unwrap(),
            "changed {key} must remain visible"
        );
    }
}
