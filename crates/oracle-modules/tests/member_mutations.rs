//! Adversarial callbacks from a real protocol-1.2 native module.
use oracle_core::{
    member_mutation::MemberMutationPolicy,
    member_read::{MemberContext, MemberReadPolicy},
    *,
};
use oracle_modules::ModuleManager;
use oracle_storage::{DatabaseConfig, Storage};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn interaction() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    ((ms - 1_420_070_400_000 - 1000) << 22).to_string()
}
#[tokio::test]
#[ignore = "requires MEMBER_MUTATION_PROBE_BINARY; run explicitly with --ignored"]
async fn authenticated_mutations_are_opt_in_scoped_and_fenced() {
    let temp =
        Temp(std::env::temp_dir().join(format!("oracle-mutations-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&temp.0).unwrap();
    let storage = Arc::new(
        Storage::open(DatabaseConfig::Sqlite {
            path: temp.0.join("state.sqlite"),
        })
        .await
        .unwrap(),
    );
    let guild: GuildId = "123".parse().unwrap();
    let module: ModuleId = "fixture.member-mutations".parse().unwrap();
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
        ModuleManager::new(storage.clone(), core.clone(), temp.0.join("artifacts")).unwrap();
    let package_dir = temp.0.join("package");
    std::fs::create_dir(&package_dir).unwrap();
    let bytes = std::fs::read(std::env::var_os("MEMBER_MUTATION_PROBE_BINARY").unwrap()).unwrap();
    std::fs::write(package_dir.join("module"), &bytes).unwrap();
    let manifest: ModuleManifest =
        serde_json::from_str(include_str!("fixtures/member-mutations/manifest.json")).unwrap();
    let package = ModulePackage {
        manifest,
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(&bytes)))]),
        source_revision: "mutation-test".into(),
        toolchain: "native probe".into(),
        license: "test-only".into(),
    };
    std::fs::write(
        package_dir.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    let installed = manager.install(&package_dir, true).await.unwrap();
    manager.load(&installed.digest).await.unwrap();
    manager
        .activate(
            &PolicyContext::LocalOperator,
            DesiredActivation {
                module: module.clone(),
                guild: guild.clone(),
                active: true,
                grants: vec!["storage.own".into()],
                bindings: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    let binding = manager
        .catalog(&PolicyContext::LocalOperator, &guild)
        .await
        .unwrap()
        .remove(0);
    let mut actor = MemberContext {
        guild: guild.clone(),
        user: "900".parse().unwrap(),
        channel: "700".into(),
        roles: BTreeSet::from(["222".into()]),
        observed_at: Instant::now(),
    };
    let input =
        json!({"method":"host.document_get","params":{"collection":"probes","key":"sentinel"}});
    macro_rules! call {
        ($op:expr,$input:expr) => {
            manager
                .invoke_member_mutation_bound(
                    &actor,
                    &interaction(),
                    &guild,
                    &module,
                    $op,
                    $input,
                    &binding.session,
                    binding.generation,
                    binding.epoch,
                )
                .await
        };
    }
    assert!(call!("mutation_probe", input.clone()).is_err());
    manager
        .configure_member_mutations(
            &PolicyContext::LocalOperator,
            &guild,
            &module,
            Some(MemberMutationPolicy {
                permission_roles: BTreeMap::from([(
                    "manage_all_runs".into(),
                    BTreeSet::from(["222".into()]),
                )]),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let reflection=call!("actor_probe",json!({"method":"reflect","params":{"actor":{"user_id":"999","permissions":["forged"]},"guild":"999"}})).unwrap().value;
    assert_eq!(reflection["actor"]["user_id"], "900");
    assert_eq!(reflection["guild"], "123");
    assert_eq!(
        reflection["actor"]["permissions"],
        json!(["manage_all_runs"])
    );
    assert!(
        call!(
            "actor_probe",
            json!({"method":"reflect","params":{},"actor":{"user_id":"999"}})
        )
        .is_err()
    );
    assert!(
        manager
            .invoke(
                &PolicyContext::LocalOperator,
                &module,
                &guild,
                "mutation_probe",
                input.clone()
            )
            .await
            .is_err()
    );
    let write = json!({"method":"host.document_batch","params":{"writes":[{"collection":"probes","key":"sentinel","expected_revision":null,"value":{"owner":"900"}}]}});
    assert_eq!(
        call!("read_probe", write.clone()).unwrap().value["accepted"],
        false
    );
    assert_eq!(
        call!("mutation_probe", write).unwrap().value["accepted"],
        true
    );
    assert_eq!(
        call!("read_probe", input.clone()).unwrap().value["result"]["value"]["owner"],
        "900"
    );
    for (method, params) in [
        (
            "host.document_get",
            json!({"collection":"secret","key":"sentinel"}),
        ),
        (
            "host.document_batch",
            json!({"writes":[{"collection":"secret","key":"sentinel","expected_revision":null,"value":{}}]}),
        ),
        ("host.health", json!({})),
        ("host.notify", json!({})),
        ("host.echo", json!({})),
        ("host.contract_invoke", json!({})),
        ("host.unknown", json!({})),
    ] {
        assert_eq!(
            call!("mutation_probe", json!({"method":method,"params":params}))
                .unwrap()
                .value["accepted"],
            false,
            "{method}"
        );
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
                per_guild_per_minute: 200,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        manager
            .invoke_member_bound(
                &actor,
                &guild,
                &module,
                "member_probe",
                input.clone(),
                &binding.session,
                binding.generation,
                binding.epoch
            )
            .await
            .unwrap()
            .value["accepted"],
        false
    );
    let pending = call!("read_probe", input.clone()).unwrap();
    manager.invalidate_member_mutations(&guild);
    assert!(pending.policy.check().is_err());
    assert!(call!("read_probe", input.clone()).is_err());
    actor.roles.clear();
    actor.observed_at = Instant::now();
    assert_eq!(
        call!("actor_probe", json!({"method":"reflect","params":{}}))
            .unwrap()
            .value["actor"]["permissions"],
        json!([])
    );
    manager
        .configure_member_mutations(&PolicyContext::LocalOperator, &guild, &module, None)
        .await
        .unwrap();
    assert!(call!("read_probe", input).is_err());
    assert!(
        storage
            .document_get(&module, &guild, "secret", "sentinel")
            .await
            .unwrap()
            .is_none()
    );
    manager.shutdown().await.unwrap();
}
