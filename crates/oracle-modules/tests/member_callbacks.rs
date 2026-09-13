//! Adversarial framed callbacks from a real native member-read operation.
use oracle_core::{
    member_read::{MemberContext, MemberReadPolicy},
    *,
};
use oracle_modules::{ModuleManager, Observation, SendTransport};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct Counts {
    gets: AtomicUsize,
    batches: AtomicUsize,
    operations: AtomicUsize,
    reservations: AtomicUsize,
    ready: AtomicUsize,
    sends: AtomicUsize,
}
impl Counts {
    fn snapshot(&self) -> [usize; 6] {
        [
            &self.gets,
            &self.batches,
            &self.operations,
            &self.reservations,
            &self.ready,
            &self.sends,
        ]
        .map(|c| c.load(Ordering::SeqCst))
    }
}
struct Instrumented {
    storage: Arc<Storage>,
    counts: Arc<Counts>,
}
#[async_trait::async_trait]
impl ModuleRepository for Instrumented {
    async fn installations(&self) -> Result<Vec<InstalledModule>> {
        self.storage.installations().await
    }
    async fn install_module(&self, installed: &InstalledModule) -> Result<()> {
        self.storage.install_module(installed).await
    }
    async fn remove_installation(&self, digest: &str) -> Result<()> {
        self.storage.remove_installation(digest).await
    }
    async fn desired_modules(&self) -> Result<Vec<DesiredModule>> {
        self.storage.desired_modules().await
    }
    async fn set_module_desired(&self, desired: &DesiredModule) -> Result<()> {
        self.storage.set_module_desired(desired).await
    }
    async fn desired_activations(&self) -> Result<Vec<DesiredActivation>> {
        self.storage.desired_activations().await
    }
    async fn set_activation_desired(&self, activation: &DesiredActivation) -> Result<()> {
        self.storage.set_activation_desired(activation).await
    }
    async fn document_get(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        collection: &str,
        key: &str,
    ) -> Result<Option<ModuleDocument>> {
        self.counts.gets.fetch_add(1, Ordering::SeqCst);
        self.storage
            .document_get(module, guild, collection, key)
            .await
    }
    async fn document_batch(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        data_version: u32,
        writes: &[DocumentWrite],
    ) -> Result<Vec<ModuleDocument>> {
        self.counts.batches.fetch_add(1, Ordering::SeqCst);
        self.storage
            .document_batch(module, guild, data_version, writes)
            .await
    }
    async fn migration_status(
        &self,
        module: &ModuleId,
        guild: &GuildId,
    ) -> Result<MigrationProgress> {
        self.storage.migration_status(module, guild).await
    }
    async fn begin_migration(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        from: u32,
        to: u32,
        digest: &str,
    ) -> Result<MigrationProgress> {
        self.storage
            .begin_migration(module, guild, from, to, digest)
            .await
    }
    async fn migration_page(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        limit: u32,
    ) -> Result<MigrationPage> {
        self.storage.migration_page(module, guild, limit).await
    }
    async fn commit_migration_page(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        digest: &str,
        expected_cursor: Option<&str>,
        writes: &[DocumentWrite],
        next_cursor: Option<&str>,
        complete: bool,
    ) -> Result<MigrationProgress> {
        self.storage
            .commit_migration_page(
                module,
                guild,
                digest,
                expected_cursor,
                writes,
                next_cursor,
                complete,
            )
            .await
    }
}
#[async_trait::async_trait]
impl Repository for Instrumented {
    async fn status(&self, guild: Option<&GuildId>) -> Result<Status> {
        self.storage.status(guild).await
    }
    async fn set_paused(
        &self,
        guild: &GuildId,
        paused: bool,
        revision: u64,
        actor: &str,
        operation: &OperationId,
    ) -> Result<ControlReceipt> {
        self.storage
            .set_paused(guild, paused, revision, actor, operation)
            .await
    }
    async fn begin_operation(&self, operation: &Operation) -> Result<()> {
        self.counts.operations.fetch_add(1, Ordering::SeqCst);
        self.storage.begin_operation(operation).await
    }
    async fn reserve_effect(&self, effect: &Effect) -> Result<Effect> {
        self.counts.reservations.fetch_add(1, Ordering::SeqCst);
        self.storage.reserve_effect(effect).await
    }
    async fn effect(&self, guild: &GuildId, id: &EffectId) -> Result<Effect> {
        self.storage.effect(guild, id).await
    }
    async fn transition_effect(
        &self,
        guild: &GuildId,
        id: &EffectId,
        revision: u64,
        next: EffectState,
        receipt: Option<Value>,
    ) -> Result<Effect> {
        self.storage
            .transition_effect(guild, id, revision, next, receipt)
            .await
    }
    async fn finish_operation(
        &self,
        guild: &GuildId,
        id: &OperationId,
        state: OperationState,
    ) -> Result<()> {
        self.storage.finish_operation(guild, id, state).await
    }
    async fn recovery(&self, guild: &GuildId, limit: u32) -> Result<Vec<Effect>> {
        self.storage.recovery(guild, limit).await
    }
}
struct CountedTransport(Arc<Counts>);
#[async_trait::async_trait]
impl SendTransport for CountedTransport {
    async fn ready(&self) -> Result<()> {
        self.0.ready.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn dispatch(&self, body: &Value) -> Result<Value> {
        self.0.sends.fetch_add(1, Ordering::SeqCst);
        Ok(body.clone())
    }
    async fn observe(&self, receipt: Value) -> Result<Observation> {
        Ok(Observation::Verified(receipt))
    }
}
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn package(root: &Path, binary: &Path) -> PathBuf {
    let path = root.join("package");
    fs::create_dir(&path).unwrap();
    let bytes = fs::read(binary).expect("build MEMBER_CALLBACK_PROBE_BINARY first");
    fs::write(path.join("module"), &bytes).unwrap();
    let manifest: ModuleManifest =
        serde_json::from_str(include_str!("fixtures/member-callbacks/manifest.json")).unwrap();
    let package = ModulePackage {
        manifest,
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "adversarial-member-callback-test".into(),
        toolchain: "separate native RPC fixture".into(),
        license: "test-only fixture".into(),
    };
    fs::write(
        path.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    path
}
async fn operator_probe(
    manager: &ModuleManager,
    module: &ModuleId,
    guild: &GuildId,
    operation: &str,
    method: &str,
    params: Value,
) -> Value {
    manager
        .invoke(
            &PolicyContext::LocalOperator,
            module,
            guild,
            operation,
            json!({"method":method,"params":params}),
        )
        .await
        .unwrap()
}
#[tokio::test]
#[ignore = "requires MEMBER_CALLBACK_PROBE_BINARY built from tests/fixtures/member-callbacks; run explicitly with --ignored"]
async fn member_callback_denial_precedes_document_access_and_effects_for_all_callers() {
    let binary = PathBuf::from(
        std::env::var_os("MEMBER_CALLBACK_PROBE_BINARY").expect("set MEMBER_CALLBACK_PROBE_BINARY"),
    );
    let temp = Temp(
        std::env::temp_dir().join(format!("oracle-member-callbacks-{}", uuid::Uuid::new_v4())),
    );
    fs::create_dir(&temp.0).unwrap();
    let storage = Arc::new(
        Storage::open(DatabaseConfig::Sqlite {
            path: temp.0.join("state.sqlite"),
        })
        .await
        .unwrap(),
    );
    let guild: GuildId = "123".parse().unwrap();
    let module: ModuleId = "fixture.member-callbacks".parse().unwrap();
    storage
        .initialize_guilds(std::slice::from_ref(&guild))
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let repository = Arc::new(Instrumented {
        storage: storage.clone(),
        counts: counts.clone(),
    });
    let core = Arc::new(CoreService::new(
        repository.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![],
        }],
    ));
    let manager = ModuleManager::with_transport(
        repository,
        core,
        temp.0.join("artifacts"),
        Arc::new(CountedTransport(counts.clone())),
    )
    .unwrap();
    let installed = manager
        .install(&package(&temp.0, &binary), true)
        .await
        .unwrap();
    manager.load(&installed.digest).await.unwrap();
    manager
        .activate(
            &PolicyContext::LocalOperator,
            DesiredActivation {
                module: module.clone(),
                guild: guild.clone(),
                active: true,
                grants: vec!["storage.own".into(), "host.echo".into()],
                bindings: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    // Privileged controls prove this exact process, host transport and storage path work.
    let written=operator_probe(&manager,&module,&guild,"operator_probe","host.document_batch",json!({"writes":[{"collection":"probes","key":"sentinel","expected_revision":null,"value":{"unchanged":true}}]})).await;
    assert_eq!(written["accepted"], true);
    assert_eq!(written["result"][0]["revision"], 1);
    let read = operator_probe(
        &manager,
        &module,
        &guild,
        "operator_probe",
        "host.document_get",
        json!({"collection":"probes","key":"sentinel"}),
    )
    .await;
    assert_eq!(read["accepted"], true);
    assert_eq!(read["result"]["value"], json!({"unchanged":true}));
    let echoed = operator_probe(
        &manager,
        &module,
        &guild,
        "operator_probe",
        "host.echo",
        json!({"purpose":"positive-control","body":{"echo":1}}),
    )
    .await;
    assert_eq!(echoed["accepted"], true);
    assert_eq!(counts.snapshot(), [1, 1, 1, 1, 1, 1]);
    manager
        .configure_member_reads(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            Some(MemberReadPolicy {
                channels: BTreeSet::new(),
                roles: BTreeSet::new(),
                per_user_per_minute: 60,
                per_guild_per_minute: 60,
            }),
        )
        .await
        .unwrap();
    let member = MemberContext {
        guild: guild.clone(),
        user: "900".parse().unwrap(),
        channel: "700".into(),
        roles: BTreeSet::new(),
        observed_at: Instant::now(),
    };
    let binding = manager
        .member_catalog(&member, &guild)
        .await
        .unwrap()
        .entries
        .remove(0);
    assert_eq!(binding.operations.len(), 1);
    assert_eq!(binding.operations[0].name, "member_probe");
    let calls = [
        (
            "host.document_get",
            json!({"collection":"probes","key":"sentinel"}),
        ),
        (
            "host.document_batch",
            json!({"writes":[{"collection":"probes","key":"sentinel","expected_revision":1,"value":null}]}),
        ),
        (
            "host.echo",
            json!({"purpose":"must-not-execute","body":{"escaped":true}}),
        ),
        (
            "host.notify",
            json!({"purpose":"must-not-notify","destination":"any","text":"must not send"}),
        ),
        ("host.health", json!({})),
        (
            "host.contract_invoke",
            json!({"contract":"arbitrary/v1","input":{}}),
        ),
        ("host.future_mutation", json!({"escaped":true})),
    ];
    let before = counts.snapshot();
    let mut attempts = 0;
    for (method, valid) in calls {
        for params in [valid, json!({"unexpected_callback_field":true})] {
            // Module-level and guild-level capabilities are present, but this operation has none.
            let result = operator_probe(
                &manager,
                &module,
                &guild,
                "member_probe",
                method,
                params.clone(),
            )
            .await;
            assert_eq!(
                result["accepted"], false,
                "operator member-route callback accepted: {method}"
            );
            assert!(
                result["error"]
                    .as_str()
                    .unwrap()
                    .contains("ForbiddenPermission"),
                "{method}: {result}"
            );
            assert_eq!(
                counts.snapshot(),
                before,
                "callback reached host service: {method}"
            );
            attempts += 1;
            let member = MemberContext {
                observed_at: Instant::now(),
                ..member.clone()
            };
            let result = manager
                .invoke_member_bound(
                    &member,
                    &guild,
                    &module,
                    "member_probe",
                    json!({"method":method,"params":params}),
                    &binding.session,
                    binding.generation,
                    binding.epoch,
                )
                .await
                .unwrap()
                .value;
            assert_eq!(
                result["accepted"], false,
                "ordinary member callback accepted: {method}"
            );
            assert!(
                result["error"]
                    .as_str()
                    .unwrap()
                    .contains("ForbiddenPermission"),
                "{method}: {result}"
            );
            assert_eq!(
                counts.snapshot(),
                before,
                "callback reached host service: {method}"
            );
            attempts += 1;
        }
    }
    assert_eq!(attempts, 28);
    let persisted = storage
        .document_get(&module, &guild, "probes", "sentinel")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.revision, 1);
    assert_eq!(persisted.value, json!({"unchanged":true}));
    // Malformed privileged callbacks do reach decoding; member versions above are denied earlier.
    for method in [
        "host.document_get",
        "host.document_batch",
        "host.echo",
        "host.notify",
        "host.health",
        "host.contract_invoke",
    ] {
        let malformed = operator_probe(
            &manager,
            &module,
            &guild,
            "operator_probe",
            method,
            json!({"unexpected_callback_field":true}),
        )
        .await;
        assert_eq!(malformed["accepted"], false);
        assert!(
            malformed["error"]
                .as_str()
                .unwrap()
                .contains("InvalidInput"),
            "{method}: {malformed}"
        );
    }
    assert_eq!(counts.snapshot(), before);
    manager
        .unload(&module, Duration::from_secs(3))
        .await
        .unwrap();
    manager.shutdown().await.unwrap();
    storage.close().await.unwrap();
}
