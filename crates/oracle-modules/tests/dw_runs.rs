//! Real DW subprocess qualification. Requires a built module and the complete
//! normalized run-eligibility catalog; no network or Discord transport is used.
use oracle_core::{
    member_mutation::MemberMutationPolicy,
    member_read::{MemberContext, MemberReadPolicy},
    *,
};
use oracle_modules::{
    ConfigurationPolicy, DispatchPermit, ModuleCatalogEntry, ModuleManager, NotificationCheck,
    NotificationRequest, NotificationTransport, SharedCardService,
    runtime_settings::ModuleRuntimeSettings,
};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

const PREFIX: &str = "https://dandys-world-robloxhorror.fandom.com/index.php?oldid=";
const DAY: u64 = 86_400_000;
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn interaction() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    (((now() - 1_420_070_400_000 - 1000) << 22) + SEQ.fetch_add(1, Ordering::Relaxed)).to_string()
}
fn member(guild: &GuildId, user: &str) -> MemberContext {
    MemberContext {
        guild: guild.clone(),
        user: user.parse().unwrap(),
        channel: "700".into(),
        roles: BTreeSet::new(),
        observed_at: Instant::now(),
    }
}
struct Policy;
#[async_trait::async_trait]
impl ConfigurationPolicy for Policy {
    async fn validate(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        values: &Value,
    ) -> Result<()> {
        if values == &json!({}) || values == &json!({"limits":{"drafts_per_guild":0}}) {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        }
    }
    async fn validate_subscriptions(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        events: &[GuildEventKind],
    ) -> Result<()> {
        if events == [GuildEventKind::Maintenance] {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        }
    }
}
struct NoSend;
#[async_trait::async_trait]
impl NotificationTransport for NoSend {
    async fn send(
        &self,
        _: &NotificationRequest,
        _: &DispatchPermit,
        _: &dyn NotificationCheck,
        _: CancellationToken,
    ) -> Result<Value> {
        panic!("native run qualification must never send a Discord message")
    }
}
/// Record the host callback boundary without simulating Discord delivery. Every
/// enqueued value must already be present in the real module document store.
struct RecordedCards {
    storage: Arc<Storage>,
    intents: Mutex<BTreeMap<(ModuleId, GuildId, String), Value>>,
    enqueues: AtomicU64,
}
#[async_trait::async_trait]
impl SharedCardService for RecordedCards {
    async fn enqueue(&self, module: &ModuleId, guild: &GuildId, intent: Value) -> Result<Value> {
        let key = intent["key"].as_str().unwrap().to_owned();
        let saved = self
            .storage
            .document_get(module, guild, "runs", &key)
            .await?
            .unwrap();
        assert_eq!(saved.value["publication"], intent);
        assert_eq!(
            intent["desired_revision"],
            saved.value["run"]["desired_card_revision"]
        );
        let status = json!({"state":"pending","desired_revision":intent["desired_revision"],"confirmed_revision":null});
        self.intents
            .lock()
            .unwrap()
            .insert((module.clone(), guild.clone(), key), intent);
        self.enqueues.fetch_add(1, Ordering::Relaxed);
        Ok(status)
    }
    async fn status(&self, module: &ModuleId, guild: &GuildId, key: &str) -> Result<Value> {
        let intents = self.intents.lock().unwrap();
        let revision = intents
            .get(&(module.clone(), guild.clone(), key.into()))
            .map(|intent| intent["desired_revision"].clone())
            .unwrap_or(json!(0));
        Ok(json!({"state":"pending","desired_revision":revision,"confirmed_revision":null}))
    }
}
fn snapshot(directory: &Path, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    fs::write(directory.join(format!("{digest}.json")), bytes).unwrap();
    fs::write(directory.join("active"), digest).unwrap();
}
async fn call(
    manager: &ModuleManager,
    guild: &GuildId,
    binding: &ModuleCatalogEntry,
    user: &str,
    id: &str,
    op: &str,
    input: Value,
) -> Result<Value> {
    let started = Instant::now();
    let result = manager
        .invoke_member_mutation_bound(
            &member(guild, user),
            id,
            guild,
            &binding.module,
            op,
            input,
            &binding.session,
            binding.generation,
            binding.epoch,
        )
        .await
        .map(|reply| reply.value);
    let elapsed_us = started.elapsed().as_micros();
    if let Some(path) = std::env::var_os("DW_RUN_METRICS_FILE") {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({"operation":op,"elapsed_us":elapsed_us,"success":result.is_ok()})
        )
        .unwrap();
    }
    result
}
async fn change(
    manager: &ModuleManager,
    guild: &GuildId,
    binding: &ModuleCatalogEntry,
    user: &str,
    id: &str,
    command: Value,
) -> Value {
    call(
        manager,
        guild,
        binding,
        user,
        &interaction(),
        "run_change",
        json!({"id":id,"command":command}),
    )
    .await
    .unwrap()
}
async fn binding(manager: &ModuleManager, guild: &GuildId) -> ModuleCatalogEntry {
    manager
        .catalog(&PolicyContext::LocalOperator, guild)
        .await
        .unwrap()
        .remove(0)
}
fn control(reply: &Value, label: &str) -> Value {
    reply["reply"]["buttons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|button| button["label"] == label)
        .unwrap_or_else(|| panic!("missing {label} control in {reply}"))["input"]
        .clone()
}
async fn ui(
    manager: &ModuleManager,
    guild: &GuildId,
    binding: &ModuleCatalogEntry,
    user: &str,
    input: Value,
) -> Value {
    call(
        manager,
        guild,
        binding,
        user,
        &interaction(),
        "run_ui",
        input,
    )
    .await
    .unwrap()
}
async fn saved_run(
    manager: &ModuleManager,
    guild: &GuildId,
    binding: &ModuleCatalogEntry,
    user: &str,
    id: &str,
) -> Value {
    call(
        manager,
        guild,
        binding,
        user,
        &interaction(),
        "run_view",
        json!({"id":id}),
    )
    .await
    .unwrap()["run"]
        .clone()
}
/// Drive the actual module's private controls with ordinary members and the
/// complete supplied wiki catalog. Discord delivery is qualified separately.
async fn qualify_ui(
    manager: &ModuleManager,
    guild: &GuildId,
    binding: &ModuleCatalogEntry,
    data: &Path,
    catalog: &Value,
) -> Vec<(String, Value)> {
    let mut page = call(
        manager,
        guild,
        binding,
        "910",
        &interaction(),
        "run_create_organized",
        json!({"name":"Casual"}),
    )
    .await
    .unwrap();
    let id = page["result"]["run_id"].as_str().unwrap().to_owned();
    let options = &page["reply"]["buttons"][0]["prompt"]["select"]["choices"];
    assert_eq!(options.as_array().unwrap().len(), 25);
    let first_toon = options[0]["value"].as_str().unwrap().to_owned();
    // Opening and abandoning the combined Toon/count modal leaves the draft unchanged.
    let mut count_input = control(&page, "Add or edit Toon");
    count_input["toon"] = json!(first_toon);
    let before = saved_run(manager, guild, binding, "910", &id).await;
    page = call(
        manager,
        guild,
        binding,
        "910",
        &interaction(),
        "run_create_casual",
        json!({"name":"Replacement name"}),
    )
    .await
    .unwrap();
    assert_eq!(page["result"]["run_id"], id);
    assert_eq!(saved_run(manager, guild, binding, "910", &id).await, before);
    assert_eq!(before["mode"], "organized");
    assert_eq!(before["name"], "Casual");
    let mut set_first = count_input;
    set_first["count"] = json!("2");
    page = ui(manager, guild, binding, "910", set_first).await;
    // Publish a newly validated catalog while this native process and draft stay live.
    // The actual snapshot monitor must adopt it; restarting would not test this boundary.
    let pinned = saved_run(manager, guild, binding, "910", &id).await;
    let mut refreshed = catalog.clone();
    refreshed["coverage"]["warnings"]
        .as_array_mut()
        .unwrap()
        .push(json!("Native qualification refresh observation"));
    snapshot(data, &refreshed);
    let refreshed_id = fs::read_to_string(data.join("active")).unwrap();
    assert_ne!(pinned["eligibility"]["source_hash"], refreshed_id);
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let health = manager
                .invoke(
                    &PolicyContext::LocalOperator,
                    &binding.module,
                    guild,
                    "health",
                    json!({}),
                )
                .await
                .unwrap();
            if health["reply"]["text"]
                .as_str()
                .unwrap()
                .contains(&refreshed_id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("running snapshot monitor must adopt the new catalog");
    let fresh = call(
        manager,
        guild,
        binding,
        "919",
        &interaction(),
        "run_create_casual",
        json!({"name":"Refresh witness"}),
    )
    .await
    .unwrap();
    let fresh_id = fresh["result"]["run_id"].as_str().unwrap();
    assert_eq!(
        saved_run(manager, guild, binding, "919", fresh_id).await["eligibility"]["source_hash"],
        refreshed_id
    );
    assert_eq!(
        saved_run(manager, guild, binding, "910", &id).await,
        pinned,
        "catalog adoption must not rewrite an existing draft or its saved allocations"
    );
    let mut reachable = BTreeSet::new();
    loop {
        let choices = page["reply"]["buttons"][0]["prompt"]["select"]["choices"]
            .as_array()
            .unwrap();
        assert!(choices.len() <= 25);
        for choice in choices {
            assert!(reachable.insert(choice["value"].as_str().unwrap().to_owned()));
        }
        if !page["reply"]["buttons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|button| button["label"] == "Next Toons")
        {
            break;
        }
        page = ui(manager, guild, binding, "910", control(&page, "Next Toons")).await;
    }
    assert!(reachable.len() > 25);
    let all = before["eligibility"]["toons"].as_object().unwrap();
    assert_eq!(reachable, all.keys().cloned().collect());
    let last_toon = page["reply"]["buttons"][0]["prompt"]["select"]["choices"][0]["value"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(first_toon, last_toon);
    let mut set_last = control(&page, "Add or edit Toon");
    assert_eq!(set_last["page"], 1);
    let page_label = page["reply"]["buttons"][0]["prompt"]["select"]["label"].clone();
    set_last["toon"] = json!(last_toon);
    set_last["count"] = json!("6");
    page = ui(manager, guild, binding, "910", set_last).await;
    assert_eq!(control(&page, "Add or edit Toon")["page"], 1);
    assert_eq!(
        page["reply"]["buttons"][0]["prompt"]["select"]["label"],
        page_label
    );
    let host = ui(
        manager,
        guild,
        binding,
        "910",
        control(&page, "Choose my Toon"),
    )
    .await;
    let choice = host["reply"]["choices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|choice| choice["input"]["toon"] == last_toon)
        .unwrap()["input"]
        .clone();
    let review = ui(manager, guild, binding, "910", choice).await;
    let draft = saved_run(manager, guild, binding, "910", &id).await;
    assert_eq!(draft["allocations"][&first_toon], 2);
    assert_eq!(draft["allocations"][&last_toon], 6);
    assert_eq!(draft["host_toon"], last_toon);
    assert_eq!(draft["state"], "draft");
    let mut schedule = control(&review, "Set date & duration");
    schedule["starts_at"] = json!("2099-01-01 20:00");
    // A plain local date uses the configured UX default when timezone is omitted.
    schedule["duration"] = json!("1h 30m");
    let review = ui(manager, guild, binding, "910", schedule).await;
    assert!(
        review["reply"]["card"]["fields"]
            .to_string()
            .contains("<t:")
    );
    ui(manager, guild, binding, "910", control(&review, "Post run")).await;
    let posted = saved_run(manager, guild, binding, "910", &id).await;
    assert_eq!(posted["state"], "open");
    assert_eq!(posted["assignments"].as_object().unwrap().len(), 1);
    assert_eq!(posted["assignments"]["910"]["toon"], last_toon);
    // Each member independently opens the same public Join action and receives
    // their own current personal selector; no owner-held control is required.
    for user in ["911", "912"] {
        let join = ui(
            manager,
            guild,
            binding,
            user,
            json!({"action":"view","view":"join","id":id}),
        )
        .await;
        let choice = join["reply"]["choices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|choice| choice["input"]["toon"] == first_toon)
            .unwrap()["input"]
            .clone();
        ui(manager, guild, binding, user, choice).await;
    }
    let joined = saved_run(manager, guild, binding, "910", &id).await;
    assert_eq!(joined["assignments"].as_object().unwrap().len(), 3);
    for user in ["911", "912"] {
        assert_eq!(joined["assignments"][user]["toon"], first_toon);
    }
    ui(
        manager,
        guild,
        binding,
        "911",
        json!({"action":"switch","id":id,"toon":last_toon}),
    )
    .await;
    ui(
        manager,
        guild,
        binding,
        "912",
        json!({"action":"leave","id":id}),
    )
    .await;
    let changed = saved_run(manager, guild, binding, "910", &id).await;
    assert_eq!(changed["assignments"]["911"]["toon"], last_toon);
    assert!(changed["assignments"]["912"].is_null());
    ui(
        manager,
        guild,
        binding,
        "910",
        json!({"action":"lock","id":id,"expected_revision":changed["desired_card_revision"]}),
    )
    .await;
    let locked = saved_run(manager, guild, binding, "910", &id).await;
    assert_eq!(locked["state"], "locked");
    ui(
        manager,
        guild,
        binding,
        "912",
        json!({"action":"join","id":id,"toon":first_toon}),
    )
    .await;
    assert_eq!(saved_run(manager, guild, binding, "910", &id).await, locked);

    let casual = call(
        manager,
        guild,
        binding,
        "920",
        &interaction(),
        "run_create_casual",
        json!({"name":"Organized"}),
    )
    .await
    .unwrap();
    let casual_id = casual["result"]["run_id"].as_str().unwrap();
    assert!(!casual["reply"].to_string().contains("set_count"));
    let mut schedule = control(&casual, "Set date & duration");
    schedule["starts_at"] = json!("<t:4070908800:F>");
    // A Hammertime timestamp must work without any timezone input.
    schedule["duration"] = json!("90m");
    let casual = ui(manager, guild, binding, "920", schedule).await;
    ui(manager, guild, binding, "920", control(&casual, "Post run")).await;
    // Casual public Join opens personal controls with an explicit no-Toon path.
    for user in ["921", "922"] {
        let join = ui(
            manager,
            guild,
            binding,
            user,
            json!({"action":"view","view":"join","id":casual_id}),
        )
        .await;
        ui(
            manager,
            guild,
            binding,
            user,
            control(&join, "Join without a Toon"),
        )
        .await;
    }
    let no_toons = saved_run(manager, guild, binding, "920", casual_id).await;
    assert_eq!(no_toons["mode"], "casual");
    assert_eq!(no_toons["name"], "Organized");
    assert!(no_toons["allocations"].is_null());
    assert_eq!(no_toons["assignments"].as_object().unwrap().len(), 3);
    for user in ["920", "921", "922"] {
        assert!(no_toons["assignments"][user]["toon"].is_null());
    }
    for user in ["921", "922"] {
        ui(
            manager,
            guild,
            binding,
            user,
            json!({"action":"switch","id":casual_id,"toon":first_toon}),
        )
        .await;
    }
    let repeated = saved_run(manager, guild, binding, "920", casual_id).await;
    for user in ["921", "922"] {
        assert_eq!(repeated["assignments"][user]["toon"], first_toon);
    }
    assert!(repeated["allocations"].is_null());
    for user in ["923", "924", "925", "926", "927"] {
        ui(
            manager,
            guild,
            binding,
            user,
            json!({"action":"join","id":casual_id}),
        )
        .await;
    }
    let full = saved_run(manager, guild, binding, "920", casual_id).await;
    assert_eq!(full["assignments"].as_object().unwrap().len(), 8);
    let full_card = ui(
        manager,
        guild,
        binding,
        "928",
        json!({"action":"view","view":"join","id":casual_id}),
    )
    .await;
    assert!(
        full_card["reply"]["card"]["description"]
            .as_str()
            .unwrap()
            .contains("full")
    );
    assert!(full_card["reply"]["choices"].as_array().unwrap().is_empty());
    ui(
        manager,
        guild,
        binding,
        "928",
        json!({"action":"join","id":casual_id}),
    )
    .await;
    assert_eq!(
        saved_run(manager, guild, binding, "920", casual_id).await,
        full
    );

    let cancelled = call(
        manager,
        guild,
        binding,
        "930",
        &interaction(),
        "run_create_casual",
        json!({}),
    )
    .await
    .unwrap();
    let cancel_id = cancelled["result"]["run_id"].as_str().unwrap();
    let confirm = ui(
        manager,
        guild,
        binding,
        "930",
        control(&cancelled, "Cancel setup"),
    )
    .await;
    assert_eq!(
        saved_run(manager, guild, binding, "930", cancel_id).await["state"],
        "draft"
    );
    ui(
        manager,
        guild,
        binding,
        "930",
        control(&confirm, "Cancel run"),
    )
    .await;
    assert_eq!(
        saved_run(manager, guild, binding, "930", cancel_id).await["state"],
        "cancelled"
    );
    ui(
        manager,
        guild,
        binding,
        "910",
        json!({"action":"complete","id":id,"expected_revision":locked["desired_card_revision"]}),
    )
    .await;
    let completed = saved_run(manager, guild, binding, "910", &id).await;
    assert_eq!(completed["state"], "completed");
    vec![("910".into(), completed), ("920".into(), full)]
}
fn activation(module: &ModuleId, guild: &GuildId) -> DesiredActivation {
    DesiredActivation {
        module: module.clone(),
        guild: guild.clone(),
        active: true,
        grants: vec![
            "storage.own".into(),
            "config.own".into(),
            "events.guild".into(),
            "shared_cards.publish".into(),
        ],
        bindings: BTreeMap::new(),
    }
}
async fn configure(manager: &ModuleManager, guild: &GuildId, module: &ModuleId) {
    manager
        .configure_member_mutations(
            &PolicyContext::LocalOperator,
            guild,
            module,
            Some(MemberMutationPolicy {
                channels: BTreeSet::from(["700".into()]),
                permission_roles: BTreeMap::from([(
                    "manage_all_runs".into(),
                    BTreeSet::from(["800".into()]),
                )]),
                per_user_per_minute: 60,
                per_guild_per_minute: 200,
            }),
        )
        .await
        .unwrap();
    let plan = manager
        .configuration_plan(
            &PolicyContext::LocalOperator,
            guild,
            module,
            None,
            json!({}),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert_eq!(
        manager
            .configuration_apply(&PolicyContext::LocalOperator, guild, module, &plan.id)
            .await
            .unwrap()
            .state,
        "effective"
    );
}
#[tokio::test]
#[ignore = "requires DW_MODULE_BINARY and DW_RUN_CATALOG; run explicitly with --ignored"]
async fn native_runs_sqlite() {
    qualify(false).await;
}
#[tokio::test]
#[ignore = "requires DW_MODULE_BINARY, DW_RUN_CATALOG and isolated ORACLE_TEST_DW_RUNS_POSTGRES_URL"]
async fn native_runs_postgres() {
    qualify(true).await;
}
async fn qualify(postgres: bool) {
    // Wine's Z: drive exposes Unix /dev. Use the Windows temp drive so the
    // fixture cannot accidentally hide Windows-only filesystem assumptions.
    #[cfg(windows)]
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let temp = Temp(std::env::temp_dir().join(format!("oracle-dw-runs-{}", uuid::Uuid::new_v4())));
    #[cfg(unix)]
    fs::DirBuilder::new().mode(0o700).create(&temp.0).unwrap();
    #[cfg(windows)]
    oracle_local_ipc::create_private_directory_new(&temp.0).unwrap();
    let storage = Arc::new(
        Storage::open(if postgres {
            DatabaseConfig::Postgres {
                url: std::env::var("ORACLE_TEST_DW_RUNS_POSTGRES_URL")
                    .expect("isolated PostgreSQL database required"),
            }
        } else {
            DatabaseConfig::Sqlite {
                path: temp.0.join("state.sqlite"),
            }
        })
        .await
        .unwrap(),
    );
    let guild: GuildId = "123".parse().unwrap();
    let other: GuildId = "456".parse().unwrap();
    let module: ModuleId = "community.dandys-world".parse().unwrap();
    storage
        .initialize_guilds(&[guild.clone(), other.clone()])
        .await
        .unwrap();
    // Begin from Stage 5's empty module namespace, independently of the wiki catalog.
    let digest = "a".repeat(64);
    storage
        .begin_migration(&module, &guild, 0, 1, &digest)
        .await
        .unwrap();
    assert!(
        storage
            .migration_page(&module, &guild, 1)
            .await
            .unwrap()
            .documents
            .is_empty()
    );
    storage
        .commit_migration_page(&module, &guild, &digest, None, &[], None, true)
        .await
        .unwrap();
    let core = Arc::new(CoreService::new(
        storage.clone(),
        vec![
            GuildPolicy {
                guild: guild.clone(),
                operators: vec![],
            },
            GuildPolicy {
                guild: other.clone(),
                operators: vec![],
            },
        ],
    ));
    let manager = ModuleManager::new(storage.clone(), core, temp.0.join("artifacts")).unwrap();
    let cards = Arc::new(RecordedCards {
        storage: storage.clone(),
        intents: Mutex::new(BTreeMap::new()),
        enqueues: AtomicU64::new(0),
    });
    manager.set_shared_card_service(cards.clone()).unwrap();
    manager
        .set_configuration_services(storage.clone(), Arc::new(Policy))
        .unwrap();
    manager
        .set_event_services(BTreeSet::from(["guilds".into()]), Arc::new(NoSend))
        .unwrap();
    let package_dir = temp.0.join("package");
    fs::create_dir(&package_dir).unwrap();
    let bytes =
        fs::read(std::env::var_os("DW_MODULE_BINARY").expect("built DW module required")).unwrap();
    let executable = if cfg!(windows) {
        "module.exe"
    } else {
        "module"
    };
    fs::write(package_dir.join(executable), &bytes).unwrap();
    let mut manifest: ModuleManifest =
        serde_json::from_str(include_str!("../../../modules/dandys-world/manifest.json")).unwrap();
    if cfg!(windows) {
        manifest.target = "x86_64-pc-windows-gnu".into();
    }
    let package = ModulePackage {
        manifest,
        entrypoint: executable.into(),
        files: BTreeMap::from([(executable.into(), format!("{:x}", Sha256::digest(&bytes)))]),
        source_revision: "dw-run-native-test".into(),
        toolchain: "real DW SDK executable".into(),
        license: "test fixture".into(),
    };
    fs::write(
        package_dir.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    let installed = manager.install(&package_dir, true).await.unwrap();
    let data = temp.0.join("catalog");
    manager
        .configure_runtime_settings(
            &PolicyContext::LocalOperator,
            BTreeMap::from([(
                module.clone(),
                ModuleRuntimeSettings {
                    image_prefix: None,
                    data_directory: Some(data.clone()),
                    citation_prefix: Some(PREFIX.into()),
                },
            )]),
            &[temp.0.join("state.sqlite")],
            {
                #[cfg(unix)]
                {
                    fs::metadata(&temp.0).unwrap().uid()
                }
                #[cfg(windows)]
                {
                    0
                }
            },
        )
        .await
        .unwrap();
    let catalog: Value = serde_json::from_slice(
        &fs::read(
            std::env::var_os("DW_RUN_CATALOG").expect("complete normalized catalog required"),
        )
        .unwrap(),
    )
    .unwrap();
    snapshot(&data, &catalog);
    let previous = std::env::var_os("DW_PREVIOUS_MODULE_BINARY").map(|path| {
        let directory = temp.0.join("previous-package");
        fs::create_dir(&directory).unwrap();
        let bytes = fs::read(path).unwrap();
        fs::write(directory.join(executable), &bytes).unwrap();
        let mut previous = package.clone();
        previous.manifest.version = "0.8.3".into();
        previous
            .files
            .insert(executable.into(), format!("{:x}", Sha256::digest(&bytes)));
        fs::write(
            directory.join("package.json"),
            serde_json::to_vec(&previous).unwrap(),
        )
        .unwrap();
        directory
    });
    if let Some(directory) = &previous {
        let previous = manager.install(directory, true).await.unwrap();
        manager.load(&previous.digest).await.unwrap();
    } else {
        manager.load(&installed.digest).await.unwrap();
    }
    manager
        .activate(&PolicyContext::LocalOperator, activation(&module, &guild))
        .await
        .unwrap();
    manager
        .activate(&PolicyContext::LocalOperator, activation(&module, &other))
        .await
        .unwrap();
    assert_eq!(
        storage
            .migration_status(&module, &guild)
            .await
            .unwrap()
            .data_version,
        5
    );
    let bound = binding(&manager, &guild).await;
    assert!(
        call(
            &manager,
            &guild,
            &bound,
            "900",
            &interaction(),
            "run_create_casual",
            json!({})
        )
        .await
        .is_err(),
        "mutations need an explicit host opt-in"
    );
    configure(&manager, &guild, &module).await;
    configure(&manager, &other, &module).await;
    if previous.is_some() {
        manager
            .upgrade(&module, &installed.digest, Duration::from_secs(3))
            .await
            .unwrap();
    }
    let bound = binding(&manager, &guild).await;
    for input in [
        json!({"owner_id":"901"}),
        json!({"guild_id":"456"}),
        json!({"actor":{"user_id":"901"}}),
    ] {
        assert!(
            call(
                &manager,
                &guild,
                &bound,
                "900",
                &interaction(),
                "run_create_casual",
                input
            )
            .await
            .is_err()
        );
    }
    let create_id = interaction();
    let created = call(
        &manager,
        &guild,
        &bound,
        "900",
        &create_id,
        "run_create_casual",
        json!({"name":"Native run"}),
    )
    .await
    .unwrap();
    assert_eq!(
        created,
        call(
            &manager,
            &guild,
            &bound,
            "900",
            &create_id,
            "run_create_casual",
            json!({"name":"Native run"})
        )
        .await
        .unwrap()
    );
    let id = created["result"]["run_id"].as_str().unwrap();
    assert!(
        call(
            &manager,
            &guild,
            &bound,
            "901",
            &interaction(),
            "run_view",
            json!({"id":id})
        )
        .await
        .is_err()
    );
    assert!(
        call(
            &manager,
            &guild,
            &bound,
            "901",
            &interaction(),
            "run_change",
            json!({"id":id,"command":{"action":"publish"}})
        )
        .await
        .is_err()
    );
    let foreign = binding(&manager, &other).await;
    assert!(
        call(
            &manager,
            &other,
            &foreign,
            "900",
            &interaction(),
            "run_view",
            json!({"id":id})
        )
        .await
        .is_err()
    );
    assert!(
        manager
            .invoke_member_mutation_bound(
                &member(&guild, "900"),
                &interaction(),
                &other,
                &module,
                "run_view",
                json!({"id":id}),
                &bound.session,
                bound.generation,
                bound.epoch
            )
            .await
            .is_err()
    );
    change(&manager, &guild, &bound, "900", id, json!({"action":"set_schedule","schedule":{"starts_at":4070908800i64,"duration_minutes":90,"timezone":"UTC"}})).await;
    change(
        &manager,
        &guild,
        &bound,
        "900",
        id,
        json!({"action":"publish"}),
    )
    .await;
    let join_id = interaction();
    let joined = call(
        &manager,
        &guild,
        &bound,
        "901",
        &join_id,
        "run_signup",
        json!({"id":id}),
    )
    .await
    .unwrap();
    assert_eq!(joined["result"]["run_id"], id);
    let replayed_join = call(
        &manager,
        &guild,
        &bound,
        "901",
        &join_id,
        "run_signup",
        json!({"id":id}),
    )
    .await
    .unwrap();
    assert!(
        replayed_join["reply"]["card"]["description"]
            .as_str()
            .unwrap()
            .contains("already in this run")
    );
    let saved = call(
        &manager,
        &guild,
        &bound,
        "900",
        &interaction(),
        "run_view",
        json!({"id":id}),
    )
    .await
    .unwrap();
    assert_eq!(saved["run"]["owner_id"], "900");
    assert_eq!(saved["run"]["guild_id"], "123");
    assert_eq!(saved["run"]["assignments"].as_object().unwrap().len(), 2);
    assert!(saved["run"]["allocations"].is_null());
    assert_eq!(
        saved["publication"]["desired_revision"],
        saved["run"]["desired_card_revision"]
    );
    let ui_runs = qualify_ui(&manager, &guild, &bound, &data, &catalog).await;
    for (_, run) in &ui_runs {
        let id = run["id"].as_str().unwrap();
        let source = manager
            .shared_card_source(&guild, &module, id)
            .await
            .unwrap();
        assert_eq!(
            source.intent,
            cards.intents.lock().unwrap()[&(module.clone(), guild.clone(), id.into())]
        );
        if run["state"] == "completed" {
            assert!(source.intent["actions"].as_array().unwrap().is_empty());
        } else {
            assert_eq!(source.intent["actions"].as_array().unwrap().len(), 4);
        }
        let dispatch = manager
            .shared_card_dispatch(
                &guild,
                &module,
                &source.session,
                source.generation,
                source.epoch,
            )
            .await
            .unwrap();
        dispatch.dispatch(|| ()).unwrap();
    }
    manager
        .configure_member_reads(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            Some(MemberReadPolicy {
                channels: BTreeSet::new(),
                roles: BTreeSet::new(),
                per_user_per_minute: 20,
                per_guild_per_minute: 40,
            }),
        )
        .await
        .unwrap();
    assert!(
        manager
            .invoke_member_bound(
                &member(&guild, "900"),
                &guild,
                &module,
                "run_view",
                json!({"id":id}),
                &bound.session,
                bound.generation,
                bound.epoch
            )
            .await
            .is_err(),
        "MemberRead never gains run document callbacks"
    );
    assert!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &module,
                &guild,
                "maintenance",
                json!({})
            )
            .await
            .is_err()
    );
    // Exercise the installed-manifest compatibility gate against real v5 documents.
    // This deliberately uses the current executable: rejection must happen before
    // launching an artifact that declares only the older namespace readable.
    let old_dir = temp.0.join("older-package");
    fs::create_dir(&old_dir).unwrap();
    fs::write(old_dir.join(executable), &bytes).unwrap();
    let mut older = package.clone();
    older.manifest.version = "0.7.2".into();
    older.manifest.data_version = 3;
    older.manifest.readable_data_versions = vec![3];
    older
        .manifest
        .migrations
        .retain(|migration| migration.to <= 3);
    fs::write(
        old_dir.join("package.json"),
        serde_json::to_vec(&older).unwrap(),
    )
    .unwrap();
    let old = manager.install(&old_dir, true).await.unwrap();
    let health_before = manager.health().await;
    assert_eq!(
        manager
            .upgrade(&module, &old.digest, Duration::from_secs(3))
            .await
            .unwrap_err()
            .code,
        ErrorCode::DataVersionMismatch
    );
    assert_eq!(
        manager.health().await[&module]["host"]["pid"],
        health_before[&module]["host"]["pid"]
    );
    assert_eq!(binding(&manager, &guild).await.generation, bound.generation);
    for (owner, expected) in &ui_runs {
        assert_eq!(
            saved_run(
                &manager,
                &guild,
                &bound,
                owner,
                expected["id"].as_str().unwrap()
            )
            .await,
            *expected
        );
    }
    assert_eq!(
        storage
            .migration_status(&module, &guild)
            .await
            .unwrap()
            .data_version,
        5
    );
    assert!(
        storage
            .desired_modules()
            .await
            .unwrap()
            .iter()
            .any(|desired| desired.module == module
                && desired.digest == installed.digest
                && desired.loaded)
    );
    // Kill the actual module after acknowledged writes, then restart with stale
    // evidence. No graceful module shutdown participates in saving assignments.
    let health = manager.health().await;
    let pid = health[&module]["host"]["pid"]
        .as_u64()
        .expect("native subprocess PID");
    #[cfg(unix)]
    assert!(
        std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    #[cfg(windows)]
    assert!(
        std::process::Command::new("taskkill.exe")
            .args(["/PID", &pid.to_string(), "/F"])
            .status()
            .unwrap()
            .success()
    );
    manager
        .unload(&module, Duration::from_secs(3))
        .await
        .unwrap();
    let mut stale = catalog;
    for source in stale["sources"].as_array_mut().unwrap() {
        source["validated_at_ms"] = json!(1);
    }
    snapshot(&data, &stale);
    manager.load(&installed.digest).await.unwrap();
    let rebound = binding(&manager, &guild).await;
    assert_ne!(bound.generation, rebound.generation);
    assert!(
        manager
            .shared_card_dispatch(
                &guild,
                &module,
                &bound.session,
                bound.generation,
                bound.epoch
            )
            .await
            .is_err()
    );
    for (owner, expected) in &ui_runs {
        let id = expected["id"].as_str().unwrap();
        assert_eq!(
            saved_run(&manager, &guild, &rebound, owner, id).await,
            *expected
        );
        let recovered = ui(
            &manager,
            &guild,
            &rebound,
            owner,
            json!({"action":"view","id":id}),
        )
        .await;
        assert!(recovered["reply"]["card"]["title"].is_string());
    }
    assert_eq!(
        created["result"],
        call(
            &manager,
            &guild,
            &rebound,
            "900",
            &create_id,
            "run_create_casual",
            json!({"name":"Native run"})
        )
        .await
        .unwrap()["result"]
    );
    assert_eq!(
        saved,
        call(
            &manager,
            &guild,
            &rebound,
            "900",
            &interaction(),
            "run_view",
            json!({"id":id})
        )
        .await
        .unwrap()
    );
    assert!(
        call(
            &manager,
            &guild,
            &rebound,
            "902",
            &interaction(),
            "run_create_casual",
            json!({})
        )
        .await
        .is_err(),
        "stale catalog prevents new eligibility snapshots"
    );
    call(
        &manager,
        &guild,
        &rebound,
        "902",
        &interaction(),
        "run_signup",
        json!({"id":id}),
    )
    .await
    .unwrap();
    let leave_id = interaction();
    let left = call(
        &manager,
        &guild,
        &rebound,
        "901",
        &leave_id,
        "run_leave",
        json!({"id":id}),
    )
    .await
    .unwrap();
    assert_eq!(
        left,
        call(
            &manager,
            &guild,
            &rebound,
            "901",
            &leave_id,
            "run_leave",
            json!({"id":id})
        )
        .await
        .unwrap()
    );
    // A later restart with no wiki snapshot must still recover durable runs.
    manager
        .unload(&module, Duration::from_secs(3))
        .await
        .unwrap();
    fs::remove_file(data.join("active")).unwrap();
    manager.load(&installed.digest).await.unwrap();
    let rebound = binding(&manager, &guild).await;
    let recovered = call(
        &manager,
        &guild,
        &rebound,
        "900",
        &interaction(),
        "run_view",
        json!({"id":id}),
    )
    .await
    .unwrap();
    assert_eq!(recovered["run"]["eligibility"], saved["run"]["eligibility"]);
    change(
        &manager,
        &guild,
        &rebound,
        "900",
        id,
        json!({"action":"lock"}),
    )
    .await;
    change(
        &manager,
        &guild,
        &rebound,
        "900",
        id,
        json!({"action":"reopen"}),
    )
    .await;
    assert!(
        call(
            &manager,
            &guild,
            &rebound,
            "903",
            &interaction(),
            "run_create_casual",
            json!({})
        )
        .await
        .is_err()
    );
    // Age a never-published draft through the storage seam, then exercise the actual
    // authenticated host Maintenance delivery rather than calling cleanup directly.
    // Clone the existing pinned snapshot so no wiki refresh is needed for this fixture.
    let mut draft = saved.clone();
    draft.as_object_mut().unwrap().remove("reply");
    let expired_id = "23456789";
    draft["publication"] = Value::Null;
    draft["run"]["id"] = json!(expired_id);
    draft["run"]["owner_id"] = json!("903");
    draft["run"]["state"] = json!("draft");
    draft["run"]["assignments"] = json!({});
    // One instant: separate clock reads can make the last owner edit newer
    // than updated_at, producing corrupt input instead of an expired draft.
    let expired_at = now() - 2 * DAY;
    draft["run"]["created_at"] = json!(expired_at);
    draft["run"]["updated_at"] = json!(expired_at);
    draft["run"]["last_owner_edit_at"] = json!(expired_at);
    draft["run"]["desired_card_revision"] = json!(1);
    let live = storage
        .document_get(&module, &guild, "run_index", "live")
        .await
        .unwrap()
        .unwrap();
    let mut live_value = live.value;
    live_value[expired_id] = json!({"owner":"903","state":"draft","last_owner_edit_at":expired_at});
    storage
        .document_batch(
            &module,
            &guild,
            5,
            &[
                DocumentWrite {
                    collection: "runs".into(),
                    key: expired_id.into(),
                    expected_revision: None,
                    value: Some(draft),
                },
                DocumentWrite {
                    collection: "run_index".into(),
                    key: "live".into(),
                    expected_revision: Some(live.revision),
                    value: Some(live_value),
                },
            ],
        )
        .await
        .unwrap();
    // Reapply configuration after a new process, preserving explicit event admission.
    configure(&manager, &guild, &module).await;
    let held = manager
        .invoke_member_mutation_bound(
            &member(&guild, "900"),
            &interaction(),
            &guild,
            &module,
            "run_view",
            json!({"id":id}),
            &rebound.session,
            rebound.generation,
            rebound.epoch,
        )
        .await
        .unwrap();
    let plan = manager
        .configuration_plan(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            None,
            json!({"limits":{"drafts_per_guild":0}}),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert_eq!(
        manager
            .configuration_apply(&PolicyContext::LocalOperator, &guild, &module, &plan.id)
            .await
            .unwrap()
            .state,
        "effective"
    );
    assert!(
        held.policy.check().is_err(),
        "configuration changes fence earlier mutation authorization"
    );
    let enqueues_before_maintenance = cards.enqueues.load(Ordering::Relaxed);
    assert_eq!(
        manager
            .deliver_event(
                &guild,
                GuildEvent {
                    id: "run-cleanup".into(),
                    kind: GuildEventKind::Maintenance,
                    occurred_at_ms: now(),
                    origin: GuildEventOrigin::Unknown,
                    subject_id: None,
                    actor_id: None,
                    related_id: None
                }
            )
            .await
            .unwrap()
            .accepted,
        1
    );
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if storage
                .document_get(&module, &guild, "runs", expired_id)
                .await
                .unwrap()
                .is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Maintenance must clean old draft despite stale wiki");
    tokio::time::timeout(Duration::from_secs(8), async {
        while cards.enqueues.load(Ordering::Relaxed) <= enqueues_before_maintenance {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Maintenance must retry durable publication intents without a wiki snapshot");
    let final_run = storage
        .document_get(&module, &guild, "runs", id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        final_run.value["run"]["assignments"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    assert!(final_run.value["run"]["assignments"]["900"].is_object());
    assert!(final_run.value["run"]["assignments"]["902"].is_object());
    assert_eq!(
        final_run.value["run"]["eligibility"],
        saved["run"]["eligibility"]
    );
    manager.shutdown().await.unwrap();
    storage.close().await.unwrap();
}
