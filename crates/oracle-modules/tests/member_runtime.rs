//! Explicit qualification with the real, separately built DW executable.
//! Both tests are ignored until their binary/database prerequisites are supplied.
use oracle_core::{
    member_read::{MemberContext, MemberReadPolicy},
    *,
};
use oracle_modules::{ModuleCatalogEntry, ModuleManager, runtime_settings::ModuleRuntimeSettings};
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PREFIX: &str = "https://dandys-world-robloxhorror.fandom.com/index.php?oldid=";
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("oracle-member-runtime-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn package(
    root: &Path,
    binary: &Path,
    name: &str,
    mutate: impl FnOnce(&mut ModuleManifest),
) -> PathBuf {
    let directory = root.join(name);
    fs::create_dir(&directory).unwrap();
    let bytes = fs::read(binary).expect("DW_MODULE_BINARY must name a built DW executable");
    fs::write(directory.join("module"), &bytes).unwrap();
    let mut manifest: ModuleManifest =
        serde_json::from_str(include_str!("../../../modules/dandys-world/manifest.json")).unwrap();
    mutate(&mut manifest);
    let package = ModulePackage {
        manifest,
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "dw-member-runtime-fixture".into(),
        toolchain: "separately built DW SDK executable".into(),
        license: "synthetic test data; source licenses unchanged".into(),
    };
    fs::write(
        directory.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    directory
}
fn snapshot(directory: &Path) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let candidate = json!({
        "schema_version":1,"adapter_version":"synthetic-member-runtime-fixture","source_origin":"https://dandys-world-robloxhorror.fandom.com","crawl_started_at":"2026-09-12T00:00:00Z","crawl_completed_at":"2026-09-12T00:01:00Z",
        "sources":[{"id":"page:1","page_id":1,"title":"Fixture Rock","url":"https://dandys-world-robloxhorror.fandom.com/wiki/Fixture_Rock","revision_id":2,"revision_timestamp":"2026-09-12T00:00:00Z","validated_at_ms":now,"content_sha256":"a".repeat(64),"license":"CC BY-SA 3.0","license_url":"https://creativecommons.org/licenses/by-sa/3.0/"}],
        "entities":[{"id":"toon:fixture","kind":"toon","name":"Fixture Rock","aliases":["Rock"],"availability":"supported","warnings":[],"facts":[{"id":"fixture.health","key":"health","text":"Two synthetic fixture hearts","value":2,"unit":"hearts","conditions":["base"],"state":"supported","citations":[{"source_id":"page:1","section":"Fixture stats","quote":"Two synthetic fixture hearts"}]}],"relationships":[]}],
        "coverage":{"discovered_pages":1,"imported_pages":1,"namespace_counts":{"articles":1},"nonredirect_articles":1,"redirects":0,"entities_by_kind":{"toon":1},"excluded":[],"unresolved_redirects":[],"warnings":[]}
    });
    let bytes = serde_json::to_vec(&candidate).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    fs::write(directory.join(format!("{digest}.json")), bytes).unwrap();
    fs::write(directory.join("active"), digest).unwrap();
}
fn member(guild: &GuildId) -> MemberContext {
    MemberContext {
        guild: guild.clone(),
        user: "900".parse().unwrap(),
        channel: "700".into(),
        roles: BTreeSet::from(["800".into()]),
        observed_at: Instant::now(),
    }
}
fn policy(limit: u32) -> MemberReadPolicy {
    MemberReadPolicy {
        channels: BTreeSet::from(["700".into()]),
        roles: BTreeSet::from(["800".into()]),
        per_user_per_minute: limit,
        per_guild_per_minute: limit,
    }
}
async fn invoke(
    manager: &ModuleManager,
    actor: &MemberContext,
    guild: &GuildId,
    binding: &ModuleCatalogEntry,
    operation: &str,
    input: Value,
) -> Result<oracle_modules::MemberInvocation> {
    manager
        .invoke_member_bound(
            actor,
            guild,
            &binding.module,
            operation,
            input,
            &binding.session,
            binding.generation,
            binding.epoch,
        )
        .await
}
fn activation(module: &ModuleId, guild: &GuildId) -> DesiredActivation {
    DesiredActivation {
        module: module.clone(),
        guild: guild.clone(),
        active: true,
        grants: vec![],
        bindings: BTreeMap::new(),
    }
}

#[tokio::test]
#[ignore = "requires DW_MODULE_BINARY; run explicitly with --ignored"]
async fn member_runtime_sqlite() {
    run(false).await;
}
#[tokio::test]
#[ignore = "requires DW_MODULE_BINARY and an isolated ORACLE_TEST_MODULE_POSTGRES_URL; run explicitly with --ignored"]
async fn member_runtime_postgres() {
    run(true).await;
}

async fn run(postgres: bool) {
    let binary = PathBuf::from(
        std::env::var_os("DW_MODULE_BINARY")
            .expect("set DW_MODULE_BINARY to the separately built DW executable"),
    );
    let temp = Temp::new();
    let storage = Arc::new(
        Storage::open(if postgres {
            DatabaseConfig::Postgres {
                url: std::env::var("ORACLE_TEST_MODULE_POSTGRES_URL").expect(
                    "set isolated ORACLE_TEST_MODULE_POSTGRES_URL; no implicit SQLite fallback",
                ),
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
    let manager =
        ModuleManager::new(storage.clone(), core.clone(), temp.0.join("artifacts")).unwrap();
    // Install verifies the real v2 DW schema. A descriptor/schema mismatch fails before spawning.
    let bad = package(&temp.0, &binary, "bad-package", |m| {
        let ModuleCommandInput::Typed { options } = m.commands.as_mut().unwrap().routes[0]
            .input
            .as_mut()
            .unwrap()
        else {
            panic!("expected typed")
        };
        let ModuleCommandOptionType::String { max_length, .. } = &mut options[0].value_type else {
            panic!("expected string")
        };
        *max_length += 1;
    });
    assert!(manager.install(&bad, true).await.is_err());
    assert!(manager.health().await.is_empty());
    let good = package(&temp.0, &binary, "good-package", |_| {});
    let installed = manager
        .install(&good, true)
        .await
        .expect("actual DW manifest must pass production package validation");
    assert!(manager.health().await.is_empty());
    assert!(
        manager.load(&installed.digest).await.is_err(),
        "required runtime path must prevent load"
    );
    assert!(manager.health().await.is_empty());
    let directory = temp.0.join("dw-data");
    manager
        .configure_runtime_settings(
            &PolicyContext::LocalOperator,
            BTreeMap::from([(
                module.clone(),
                ModuleRuntimeSettings {
                    image_prefix: None,
                    data_directory: Some(directory.clone()),
                    citation_prefix: Some(PREFIX.into()),
                },
            )]),
            &[temp.0.join("state.sqlite")],
            fs::metadata(&temp.0).unwrap().uid(),
        )
        .await
        .unwrap();
    assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o7777, 0o700);
    assert!(
        manager.load(&installed.digest).await.is_err(),
        "empty snapshot directory must fail initialization"
    );
    snapshot(&directory);
    manager.load(&installed.digest).await.unwrap();
    for g in [&guild, &other] {
        manager
            .activate(&PolicyContext::LocalOperator, activation(&module, g))
            .await
            .unwrap();
    }
    let actor = member(&guild);
    let ordinary = PolicyContext::Discord {
        guild: guild.clone(),
        user: actor.user.clone(),
        manage_guild: false,
    };
    assert!(
        manager.catalog_snapshot(&ordinary, &guild).await.is_err(),
        "member must not gain operator catalog access"
    );
    assert!(
        manager
            .member_catalog(&actor, &guild)
            .await
            .unwrap()
            .entries
            .is_empty(),
        "installation/activation alone must not grant member reads"
    );
    let admin_catalog = manager
        .catalog_snapshot(&PolicyContext::LocalOperator, &guild)
        .await
        .unwrap();
    let binding = admin_catalog
        .entries
        .iter()
        .find(|e| e.module == module)
        .unwrap()
        .clone();
    assert!(
        invoke(&manager, &actor, &guild, &binding, "status", json!({}))
            .await
            .is_err()
    );
    assert!(
        manager
            .configure_member_reads(&ordinary, &guild, &module, Some(policy(60)))
            .await
            .is_err()
    );
    manager
        .configure_member_reads(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            Some(policy(60)),
        )
        .await
        .unwrap();
    let actor = member(&guild);
    let public = manager.member_catalog(&actor, &guild).await.unwrap();
    assert_eq!(public.entries.len(), 1);
    let binding = public.entries[0].clone();
    assert_eq!(binding.commands.routes.len(), 6);
    assert!(
        binding
            .operations
            .iter()
            .all(|o| o.audience == ModuleAudience::MemberRead && o.capabilities.is_empty())
    );
    assert!(!binding.operations.iter().any(|o| o.name == "health"));
    let answer = invoke(
        &manager,
        &actor,
        &guild,
        &binding,
        "lookup",
        json!({"name":"Rock","kind":"toon","field":"health"}),
    )
    .await
    .unwrap();
    assert!(
        answer.value["reply"]["text"]
            .as_str()
            .unwrap()
            .contains("Value: 2")
    );
    assert_eq!(answer.value["reply"]["citations"][0]["revision"], 2);
    answer.policy.check().unwrap();
    answer.registry.dispatch(|| ()).unwrap();
    assert!(
        invoke(&manager, &actor, &guild, &binding, "health", json!({}))
            .await
            .is_err(),
        "member cannot call private operator operation"
    );
    manager
        .invoke(
            &PolicyContext::LocalOperator,
            &module,
            &guild,
            "health",
            json!({}),
        )
        .await
        .unwrap();
    for input in [
        json!({"name":42}),
        json!({"name":"Rock","extra":true}),
        json!({"name":"Rock","offset":-1}),
        json!({"name":"Rock","kind":"invented"}),
    ] {
        assert!(
            invoke(&manager, &actor, &guild, &binding, "lookup", input)
                .await
                .is_err()
        );
    }
    assert!(
        invoke(&manager, &actor, &other, &binding, "status", json!({}))
            .await
            .is_err()
    );
    for bad in [
        ModuleCatalogEntry {
            session: "wrong".into(),
            ..binding.clone()
        },
        ModuleCatalogEntry {
            generation: binding.generation + 1,
            ..binding.clone()
        },
        ModuleCatalogEntry {
            epoch: binding.epoch + 1,
            ..binding.clone()
        },
    ] {
        assert!(
            invoke(&manager, &actor, &guild, &bad, "status", json!({}))
                .await
                .is_err()
        );
    }
    for bad in [
        MemberContext {
            channel: "701".into(),
            ..member(&guild)
        },
        MemberContext {
            roles: BTreeSet::new(),
            ..member(&guild)
        },
        MemberContext {
            observed_at: Instant::now() - Duration::from_secs(11),
            ..member(&guild)
        },
    ] {
        assert!(
            manager
                .member_catalog(&bad, &guild)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(
            invoke(&manager, &bad, &guild, &binding, "status", json!({}))
                .await
                .is_err()
        );
    }
    assert!(
        manager
            .member_catalog(&member(&other), &other)
            .await
            .unwrap()
            .entries
            .is_empty(),
        "policy must remain guild-scoped"
    );
    // Revocation fences already-created final-response permits as well as future calls.
    manager
        .configure_member_reads(&PolicyContext::LocalOperator, &guild, &module, None)
        .await
        .unwrap();
    assert!(answer.policy.check().is_err());
    assert!(answer.registry.dispatch(|| ()).is_err());
    assert!(
        invoke(
            &manager,
            &member(&guild),
            &guild,
            &binding,
            "status",
            json!({})
        )
        .await
        .is_err()
    );
    manager
        .configure_member_reads(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            Some(policy(1)),
        )
        .await
        .unwrap();
    let limited = member(&guild);
    let first = invoke(&manager, &limited, &guild, &binding, "status", json!({}))
        .await
        .unwrap();
    let exhausted = invoke(&manager, &limited, &guild, &binding, "status", json!({})).await;
    assert!(matches!(
        exhausted,
        Err(Error {
            code: ErrorCode::QuotaExceeded,
            ..
        })
    ));
    let other_user = MemberContext {
        user: "901".parse().unwrap(),
        ..member(&guild)
    };
    assert!(
        matches!(
            invoke(&manager, &other_user, &guild, &binding, "status", json!({})).await,
            Err(Error {
                code: ErrorCode::QuotaExceeded,
                ..
            })
        ),
        "guild quota must include different users"
    );
    manager.invalidate_member_reads(&guild);
    assert!(first.policy.check().is_err());
    assert!(
        invoke(&manager, &limited, &guild, &binding, "status", json!({}))
            .await
            .is_err()
    );
    manager
        .configure_member_reads(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            Some(policy(60)),
        )
        .await
        .unwrap();
    let revision = core
        .status(&PolicyContext::LocalOperator, Some(&guild))
        .await
        .unwrap()
        .guilds[0]
        .revision;
    let receipt = core
        .control(&PolicyContext::LocalOperator, &guild, true, revision)
        .await
        .unwrap();
    assert!(
        manager
            .member_catalog(&member(&guild), &guild)
            .await
            .is_err()
    );
    assert!(
        invoke(
            &manager,
            &member(&guild),
            &guild,
            &binding,
            "status",
            json!({})
        )
        .await
        .is_err()
    );
    core.control(
        &PolicyContext::LocalOperator,
        &guild,
        false,
        receipt.guild.revision,
    )
    .await
    .unwrap();
    manager
        .deactivate(
            &PolicyContext::LocalOperator,
            &module,
            &guild,
            Duration::from_secs(3),
        )
        .await
        .unwrap();
    assert!(
        invoke(
            &manager,
            &member(&guild),
            &guild,
            &binding,
            "status",
            json!({})
        )
        .await
        .is_err()
    );
    manager
        .activate(&PolicyContext::LocalOperator, activation(&module, &guild))
        .await
        .unwrap();
    let rebound = manager
        .member_catalog(&member(&guild), &guild)
        .await
        .unwrap()
        .entries
        .remove(0);
    assert_ne!(rebound.epoch, binding.epoch);
    assert!(
        invoke(
            &manager,
            &member(&guild),
            &guild,
            &binding,
            "status",
            json!({})
        )
        .await
        .is_err()
    );
    let final_answer = invoke(
        &manager,
        &member(&guild),
        &guild,
        &rebound,
        "status",
        json!({}),
    )
    .await
    .unwrap();
    let stopped = manager
        .unload(&module, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(stopped.is_some());
    assert!(final_answer.registry.dispatch(|| ()).is_err());
    assert!(
        manager
            .member_catalog(&member(&guild), &guild)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(
        invoke(
            &manager,
            &member(&guild),
            &guild,
            &rebound,
            "status",
            json!({})
        )
        .await
        .is_err()
    );
    assert!(manager.health().await.is_empty());
    manager.shutdown().await.unwrap();
    storage.close().await.unwrap();
}
