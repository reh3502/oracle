//! Actual subprocess + SQLite + durable command publication boundary qualification.
use super::{compile_catalog, invoke_published};
use async_trait::async_trait;
use oracle_core::*;
use oracle_modules::{
    DispatchPermit, ModuleManager, NotificationCheck, NotificationRequest, NotificationTransport,
};
use oracle_operations::{
    commands::{CommandBackend, CommandReconciler, PublishedCommand},
    executor::{SendGuard, now},
    ingress::OperationRequest,
};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
#[derive(Default)]
struct Backend {
    commands: Mutex<Vec<PublishedCommand>>,
    writes: Mutex<usize>,
}
#[async_trait]
impl CommandBackend for Backend {
    async fn list(&self, _: &GuildId) -> Result<Vec<PublishedCommand>> {
        Ok(self.commands.lock().unwrap().clone())
    }
    async fn create(
        &self,
        _: &GuildId,
        definition: &Value,
        guard: &SendGuard,
    ) -> Result<PublishedCommand> {
        guard.dispatch(|| {
            let mut writes = self.writes.lock().unwrap();
            *writes += 1;
            let command = PublishedCommand {
                id: format!("{}", 1000 + *writes),
                definition: definition.clone(),
            };
            self.commands.lock().unwrap().push(command.clone());
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
            *self.writes.lock().unwrap() += 1;
            let mut commands = self.commands.lock().unwrap();
            let command = commands
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
            command.definition = definition.clone();
            Ok(command.clone())
        })
    }
    async fn delete(&self, _: &GuildId, id: &str, guard: &SendGuard) -> Result<()> {
        guard.dispatch(|| {
            *self.writes.lock().unwrap() += 1;
            self.commands.lock().unwrap().retain(|c| c.id != id);
            Ok(())
        })
    }
}
struct Policy;
#[async_trait]
impl oracle_modules::ConfigurationPolicy for Policy {
    async fn validate(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        _: &Value,
    ) -> Result<()> {
        Ok(())
    }
}
struct NoNotification;
#[async_trait]
impl NotificationTransport for NoNotification {
    async fn send(
        &self,
        _: &NotificationRequest,
        _: &DispatchPermit,
        _: &dyn NotificationCheck,
        _: CancellationToken,
    ) -> Result<Value> {
        panic!("command status must not dispatch a notification")
    }
}
fn request(id: &str, name: &str, route: &str) -> OperationRequest {
    OperationRequest::InvokePublished {
        command_id: id.into(),
        command_name: name.into(),
        route: route.into(),
        input: json!({}),
    }
}
const ACTOR: PolicyContext = PolicyContext::LocalOperator;
#[tokio::test]
#[ignore = "requires ORACLE_EVENT_PROBE freshly built oracle-fixture-configuration-probe --features events"]
async fn published_command_identity_and_explicit_grants_survive_lifecycle_changes() {
    let scratch = tempfile::tempdir().unwrap();
    let storage = Arc::new(
        Storage::open(DatabaseConfig::Sqlite {
            path: scratch.path().join("state.sqlite"),
        })
        .await
        .unwrap(),
    );
    let guild: GuildId = "123".parse().unwrap();
    storage
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    let core = Arc::new(CoreService::new(
        storage.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![],
        }],
    ));
    let manager =
        ModuleManager::new(storage.clone(), core, scratch.path().join("artifacts")).unwrap();
    manager
        .set_configuration_services(storage.clone(), Arc::new(Policy))
        .unwrap();
    manager
        .set_event_services(
            BTreeSet::from(["guilds".into(), "guild_members".into()]),
            Arc::new(NoNotification),
        )
        .unwrap();
    let backend = Arc::new(Backend::default());
    let unrelated = PublishedCommand {
        id: "999".into(),
        definition: json!({"name":"unrelated","type":1,"description":"Other command owner"}),
    };
    backend.commands.lock().unwrap().push(unrelated.clone());
    let reconciler = Arc::new(CommandReconciler::new(storage.clone(), backend.clone()));
    let source = scratch.path().join("package");
    std::fs::create_dir(&source).unwrap();
    let binary = std::env::var_os("ORACLE_EVENT_PROBE").expect("set ORACLE_EVENT_PROBE");
    let bytes = std::fs::read(binary).unwrap();
    std::fs::write(source.join("module"), &bytes).unwrap();
    let package = ModulePackage {
        manifest: serde_json::from_str(include_str!(
            "../../../examples/modules/configuration-probe/manifest-events.json"
        ))
        .unwrap(),
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "command-runtime-fixture".into(),
        toolchain: "separate events-feature executable".into(),
        license: "test-only".into(),
    };
    std::fs::write(
        source.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    let worker = manager.clone();
    let result = tokio::spawn(async move {
        let installed = worker.install(&source, true).await.unwrap();
        let module = installed.package.manifest.id;
        worker.load(&installed.digest).await.unwrap();
        let activation = DesiredActivation {
            module: module.clone(),
            guild: guild.clone(),
            active: true,
            grants: vec![
                "config.own".into(),
                "events.guild".into(),
                "discord.notify".into(),
            ],
            bindings: BTreeMap::new(),
        };
        worker.activate(&ACTOR, activation.clone()).await.unwrap();
        let catalog = worker.catalog(&ACTOR, &guild).await.unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog[0]
                .commands
                .routes
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["status"]
        );
        assert_eq!(
            catalog[0]
                .operations
                .iter()
                .map(|op| op.name.as_str())
                .collect::<Vec<_>>(),
            ["status"]
        );
        let desired = compile_catalog(&catalog).unwrap();
        assert_eq!(
            desired[0].definition["options"].as_array().unwrap().len(),
            1
        );
        assert!(!desired[0].definition.to_string().contains("ungranted"));
        let cancel = CancellationToken::new();
        assert_eq!(
            reconciler
                .reconcile(&guild, &desired, &cancel, now() + 60)
                .await
                .unwrap()
                .created,
            1
        );
        let bindings = reconciler.bindings(&guild).await.unwrap();
        assert_eq!(bindings.len(), 1);
        let id = bindings[0].id.clone().unwrap();
        let name = bindings[0].definition["name"].as_str().unwrap().to_owned();
        let original = bindings[0].route.clone().unwrap();
        assert_eq!(original.session, catalog[0].session);
        assert_eq!(
            invoke_published(
                &worker,
                &reconciler,
                &ACTOR,
                &guild,
                request(&id, &name, "status")
            )
            .await
            .unwrap()["epoch"],
            original.epoch
        );
        assert_eq!(
            invoke_published(
                &worker,
                &reconciler,
                &ACTOR,
                &guild,
                request("not-the-published-id", &name, "status")
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::ModuleUnavailable
        );
        assert_eq!(
            invoke_published(
                &worker,
                &reconciler,
                &ACTOR,
                &guild,
                request(&id, "different-namespace", "status")
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::ForbiddenScope
        );
        for route in ["ungranted", "configuration.apply", "not-published"] {
            assert_eq!(
                invoke_published(
                    &worker,
                    &reconciler,
                    &ACTOR,
                    &guild,
                    request(&id, &name, route)
                )
                .await
                .unwrap_err()
                .code,
                ErrorCode::InvalidInput
            );
        }
        worker
            .deactivate(&ACTOR, &module, &guild, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            invoke_published(
                &worker,
                &reconciler,
                &ACTOR,
                &guild,
                request(&id, &name, "status")
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::ModuleUnavailable
        );
        worker.activate(&ACTOR, activation).await.unwrap();
        let active = worker.catalog(&ACTOR, &guild).await.unwrap();
        assert_eq!(active[0].session, original.session);
        assert_ne!(active[0].epoch, original.epoch);
        assert_eq!(
            invoke_published(
                &worker,
                &reconciler,
                &ACTOR,
                &guild,
                request(&id, &name, "status")
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::ModuleUnavailable,
            "persisted old epoch admitted before publication refreshed"
        );
        let desired = compile_catalog(&active).unwrap();
        assert_eq!(
            reconciler
                .reconcile(&guild, &desired, &cancel, now() + 60)
                .await
                .unwrap()
                .unchanged,
            1
        );
        let refreshed = reconciler.bindings(&guild).await.unwrap().remove(0);
        assert_eq!(refreshed.id.as_deref(), Some(id.as_str()));
        assert_eq!(refreshed.route.as_ref().unwrap().epoch, active[0].epoch);
        assert_eq!(
            invoke_published(
                &worker,
                &reconciler,
                &ACTOR,
                &guild,
                request(&id, &name, "status")
            )
            .await
            .unwrap()["epoch"],
            active[0].epoch
        );
        worker
            .unload(&module, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            invoke_published(
                &worker,
                &reconciler,
                &ACTOR,
                &guild,
                request(&id, &name, "status")
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::ModuleUnavailable
        );
        worker.load(&installed.digest).await.unwrap();
        let loaded = worker.catalog(&ACTOR, &guild).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_ne!(loaded[0].session, active[0].session);
        assert_ne!(loaded[0].generation, active[0].generation);
        assert_eq!(
            invoke_published(
                &worker,
                &reconciler,
                &ACTOR,
                &guild,
                request(&id, &name, "status")
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::ModuleUnavailable,
            "persisted old process session admitted before publication refreshed"
        );
        reconciler
            .reconcile(
                &guild,
                &compile_catalog(&loaded).unwrap(),
                &cancel,
                now() + 60,
            )
            .await
            .unwrap();
        assert_eq!(
            reconciler.bindings(&guild).await.unwrap()[0].id.as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            invoke_published(
                &worker,
                &reconciler,
                &ACTOR,
                &guild,
                request(&id, &name, "status")
            )
            .await
            .unwrap()["epoch"],
            loaded[0].epoch
        );
        assert_eq!(
            *backend.writes.lock().unwrap(),
            1,
            "identity refresh replaced or edited unchanged Discord definition"
        );
        assert!(backend.commands.lock().unwrap().contains(&unrelated));
    })
    .await;
    let stopped = manager.shutdown().await;
    let closed = storage.close().await;
    fn writable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        if path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_dir())
        {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    writable(&entry.path());
                }
            }
        }
    }
    writable(scratch.path());
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
    stopped.unwrap();
    closed.unwrap();
}
