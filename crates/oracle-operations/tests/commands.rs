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
    lost_delete_after: Option<usize>,
    mismatch: bool,
    cancel: Option<CancellationToken>,
    revoke_before_write: Option<Arc<std::sync::atomic::AtomicBool>>,
    inject_collision_after_write: Option<(usize, PublishedCommand)>,
    hold_create: Option<(String, Arc<Hold>)>,
    hold_list: Option<(usize, usize, Arc<Hold>)>,
}
#[derive(Default)]
struct Hold {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[derive(Default)]
struct Backend(Mutex<State>);
#[async_trait]
impl CommandBackend for Backend {
    async fn list(&self, _: &GuildId) -> Result<Vec<PublishedCommand>> {
        let hold = {
            let mut state = self.0.lock().unwrap();
            let writes = state.writes;
            state.hold_list.as_mut().and_then(|(at, remaining, hold)| {
                if *at != writes {
                    return None;
                }
                if *remaining > 0 {
                    *remaining -= 1;
                    None
                } else {
                    Some(hold.clone())
                }
            })
        };
        if let Some(hold) = hold {
            hold.entered.notify_one();
            hold.release.notified().await;
        }
        Ok(self.0.lock().unwrap().commands.clone())
    }
    async fn create(
        &self,
        _: &GuildId,
        definition: &Value,
        guard: &SendGuard,
    ) -> Result<PublishedCommand> {
        let hold = self
            .0
            .lock()
            .unwrap()
            .hold_create
            .as_ref()
            .filter(|(name, _)| definition["name"] == *name)
            .map(|(_, hold)| hold.clone());
        if let Some(hold) = hold {
            hold.entered.notify_one();
            hold.release.notified().await;
        }
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
            if state
                .inject_collision_after_write
                .as_ref()
                .is_some_and(|(after, _)| *after == state.writes)
            {
                let (_, collision) = state.inject_collision_after_write.take().unwrap();
                state.commands.push(collision);
            }
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
            if state.lost_delete_after == Some(state.writes) {
                if let Some(cancel) = &state.cancel {
                    cancel.cancel();
                }
                return Err(Error::new(ErrorCode::UnknownOutcome));
            }
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
async fn one_owner_can_publish_aliases_and_roll_back_only_its_aliases() {
    let (_root, store, backend, guild) = setup().await;
    let baseline = desired("catalog");
    let mut framework = desired("oracle");
    framework.owner = ModuleId::new("oracle.framework").unwrap();
    backend.0.lock().unwrap().commands.push(PublishedCommand {
        id: "50".into(),
        definition: desired("unrelated").definition,
    });
    let reconciler = CommandReconciler::new(store, backend.clone());
    let cancel = CancellationToken::new();
    let old = [baseline.clone(), framework.clone()];
    reconciler
        .reconcile(&guild, &old, &cancel, now() + 60)
        .await
        .unwrap();
    let original = reconciler.bindings(&guild).await.unwrap();
    let aliases = [baseline, framework, desired("hostrun"), desired("signup")];
    let added = reconciler
        .reconcile(&guild, &aliases, &cancel, now() + 60)
        .await
        .unwrap();
    assert_eq!(added.created, 2);
    assert_eq!(added.unchanged, 2);
    let rolled_back = reconciler
        .reconcile(&guild, &old, &cancel, now() + 60)
        .await
        .unwrap();
    assert_eq!(rolled_back.deleted, 2);
    assert_eq!(reconciler.bindings(&guild).await.unwrap(), original);
    assert_eq!(backend.0.lock().unwrap().commands.len(), 3);
    assert!(
        backend
            .0
            .lock()
            .unwrap()
            .commands
            .iter()
            .any(|c| c.id == "50")
    );
}

#[tokio::test]
async fn alias_collision_preflight_preserves_the_entire_plan_without_writes() {
    let (_root, store, backend, guild) = setup().await;
    backend.0.lock().unwrap().commands.push(PublishedCommand {
        id: "50".into(),
        definition: desired("signup").definition,
    });
    let reconciler = CommandReconciler::new(store, backend.clone());
    let result = reconciler
        .reconcile(
            &guild,
            &[desired("catalog"), desired("hostrun"), desired("signup")],
            &CancellationToken::new(),
            now() + 60,
        )
        .await
        .unwrap_err();
    assert_eq!(result.code, ErrorCode::Conflict);
    // The entire plan is checked before any member of an alias group is sent.
    assert_eq!(backend.0.lock().unwrap().writes, 0);
    assert!(reconciler.bindings(&guild).await.unwrap().is_empty());
    reconciler
        .reconcile(&guild, &[], &CancellationToken::new(), now() + 60)
        .await
        .unwrap();
    assert!(reconciler.bindings(&guild).await.unwrap().is_empty());
    let state = backend.0.lock().unwrap();
    assert_eq!(state.commands.len(), 1);
    assert_eq!(state.commands[0].id, "50");
    assert_eq!(state.commands[0].definition["name"], "signup");
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

#[tokio::test]
async fn collision_after_preflight_compensates_only_confirmed_ids_and_restores_all_prior_bindings()
{
    let (_root, store, backend, guild) = setup().await;
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    let cancel = CancellationToken::new();
    let mut oracle = desired("oracle");
    oracle.owner = ModuleId::new("oracle.bootstrap").unwrap();
    oracle.route = None;
    let mut unrelated = desired("other");
    unrelated.owner = ModuleId::new("other.module").unwrap();
    let old = vec![desired("catalog"), oracle, unrelated];
    reconciler
        .reconcile(&guild, &old, &cancel, now() + 60)
        .await
        .unwrap();
    let before = reconciler.bindings(&guild).await.unwrap();
    let writes = backend.0.lock().unwrap().writes;
    backend.0.lock().unwrap().inject_collision_after_write = Some((
        writes + 1,
        PublishedCommand {
            id: "external".into(),
            definition: desired("signup").definition,
        },
    ));
    let mut target = old.clone();
    target.extend([desired("hostrun"), desired("signup")]);
    assert_eq!(
        reconciler
            .reconcile(&guild, &target, &cancel, now() + 60)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(reconciler.bindings(&guild).await.unwrap(), before);
    {
        let state = backend.0.lock().unwrap();
        assert_eq!(state.writes, writes + 2);
        assert_eq!(state.commands.len(), old.len() + 1);
        assert!(state.commands.iter().any(|c| c.id == "external"));
        assert!(
            !state
                .commands
                .iter()
                .any(|c| c.definition["name"] == "hostrun")
        );
    }
    store.close().await.unwrap();
}
#[tokio::test]
async fn alias_group_is_fenced_until_all_members_are_confirmed_but_unrelated_routes_stay_live() {
    let (_root, store, backend, guild) = setup().await;
    let reconciler = Arc::new(CommandReconciler::new(store.clone(), backend.clone()));
    let cancel = CancellationToken::new();
    let mut unrelated = desired("other");
    unrelated.owner = ModuleId::new("other.module").unwrap();
    let old = vec![desired("catalog"), unrelated];
    reconciler
        .reconcile(&guild, &old, &cancel, now() + 60)
        .await
        .unwrap();
    let hold = Arc::new(Hold::default());
    backend.0.lock().unwrap().hold_create = Some(("signup".into(), hold.clone()));
    let mut target = old.clone();
    target.extend([desired("hostrun"), desired("signup")]);
    let running = {
        let reconciler = reconciler.clone();
        let guild = guild.clone();
        tokio::spawn(async move {
            reconciler
                .reconcile(&guild, &target, &CancellationToken::new(), now() + 60)
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), hold.entered.notified())
        .await
        .unwrap();
    // hostrun is already a confirmed owned remote command, but none of its
    // owner's group (including the old catalog route) is callable yet.
    assert!(
        backend
            .0
            .lock()
            .unwrap()
            .commands
            .iter()
            .any(|c| c.definition["name"] == "hostrun")
    );
    let visible = reconciler.bindings(&guild).await.unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].owner.as_str(), "other.module");
    let restarted = CommandReconciler::new(store.clone(), backend.clone());
    assert_eq!(restarted.bindings(&guild).await.unwrap(), visible);
    let writes = backend.0.lock().unwrap().writes;
    assert_eq!(
        restarted
            .reconcile(&guild, &old, &CancellationToken::new(), now() + 60)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RecoveryRequired
    );
    assert_eq!(
        backend.0.lock().unwrap().writes,
        writes,
        "another reconciler must not roll back a live publisher"
    );
    assert_eq!(
        restarted.publication_status(&guild).await.unwrap().unwrap()["active"],
        true
    );
    hold.release.notify_one();
    running.await.unwrap().unwrap();
    assert_eq!(reconciler.bindings(&guild).await.unwrap().len(), 4);
    store.close().await.unwrap();
}

#[tokio::test]
async fn restart_rolls_back_confirmed_partial_group_before_accepting_another_plan() {
    let (_root, store, backend, guild) = setup().await;
    let reconciler = Arc::new(CommandReconciler::new(store.clone(), backend.clone()));
    let old = vec![desired("catalog")];
    reconciler
        .reconcile(&guild, &old, &CancellationToken::new(), now() + 60)
        .await
        .unwrap();
    let previous = reconciler.bindings(&guild).await.unwrap();
    let writes = backend.0.lock().unwrap().writes;
    let hold = Arc::new(Hold::default());
    backend.0.lock().unwrap().hold_list = Some((writes + 1, 1, hold.clone()));
    let task = {
        let reconciler = reconciler.clone();
        let guild = guild.clone();
        tokio::spawn(async move {
            reconciler
                .reconcile(
                    &guild,
                    &[desired("catalog"), desired("hostrun"), desired("signup")],
                    &CancellationToken::new(),
                    now() + 60,
                )
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), hold.entered.notified())
        .await
        .unwrap();
    assert!(reconciler.bindings(&guild).await.unwrap().is_empty());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    backend.0.lock().unwrap().hold_list = None;
    let row = store
        .workflow_get(&guild, WorkflowKind::CommandGroup, "publication")
        .await
        .unwrap()
        .unwrap();
    let mut journal = row.value;
    journal["expires_at"] = json!(now() - 1);
    store
        .workflow_put(
            &guild,
            WorkflowKind::CommandGroup,
            "publication",
            Some(row.revision),
            &journal,
        )
        .await
        .unwrap();
    let restarted = CommandReconciler::new(store.clone(), backend.clone());
    assert_eq!(
        restarted
            .reconcile(&guild, &old, &CancellationToken::new(), now() + 60)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RecoveryRequired
    );
    assert_eq!(restarted.bindings(&guild).await.unwrap(), previous);
    assert_eq!(backend.0.lock().unwrap().writes, writes + 2);
    assert_eq!(
        restarted
            .reconcile(&guild, &old, &CancellationToken::new(), now() + 60)
            .await
            .unwrap()
            .unchanged,
        1
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn interrupted_deletion_restores_aliases_with_confirmed_new_ids() {
    let (_root, store, backend, guild) = setup().await;
    let reconciler = CommandReconciler::new(store.clone(), backend.clone());
    let old = [desired("dw"), desired("hostrun"), desired("signup")];
    reconciler
        .reconcile(&guild, &old, &CancellationToken::new(), now() + 60)
        .await
        .unwrap();
    let previous = reconciler.bindings(&guild).await.unwrap();
    let interrupted = CancellationToken::new();
    backend.0.lock().unwrap().lost_delete_after = Some(6);
    backend.0.lock().unwrap().cancel = Some(interrupted.clone());
    assert_eq!(
        reconciler
            .reconcile(&guild, &[], &interrupted, now() + 60)
            .await
            .unwrap_err()
            .code,
        ErrorCode::UnknownOutcome
    );
    let row = store
        .workflow_get(&guild, WorkflowKind::CommandGroup, "publication")
        .await
        .unwrap()
        .unwrap();
    let mut journal = row.value;
    journal["expires_at"] = json!(now() - 1);
    store
        .workflow_put(
            &guild,
            WorkflowKind::CommandGroup,
            "publication",
            Some(row.revision),
            &journal,
        )
        .await
        .unwrap();
    backend.0.lock().unwrap().cancel = None;
    let restarted = CommandReconciler::new(store.clone(), backend.clone());
    assert_eq!(
        restarted
            .reconcile(&guild, &old, &CancellationToken::new(), now() + 60)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RecoveryRequired
    );
    assert_eq!(
        restarted
            .reconcile(&guild, &old, &CancellationToken::new(), now() + 60)
            .await
            .unwrap()
            .unchanged,
        3
    );
    let restored = restarted.bindings(&guild).await.unwrap();
    for (before, after) in previous.iter().zip(&restored) {
        assert_ne!(before.id, after.id);
        assert_eq!(before.owner, after.owner);
        assert_eq!(before.definition, after.definition);
        assert_eq!(before.route, after.route);
    }
    assert_eq!(backend.0.lock().unwrap().commands.len(), 3);
}
