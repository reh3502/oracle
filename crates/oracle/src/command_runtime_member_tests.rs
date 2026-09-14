//! Authenticated member options through durable publication and a real SDK process.
use super::*;
use crate::command_runtime::{invoke_published_options, published_uses_member_identity};
use oracle_core::member_read::{MemberContext, MemberReadPolicy};
use oracle_modules::runtime_settings::ModuleRuntimeSettings;
use oracle_operations::ingress::{PublishedReply, PublishedRequest};
use std::{os::unix::fs::MetadataExt, path::Path, time::Instant};

pub(super) fn identity(
    guild: &GuildId,
    user: &str,
    operator: bool,
) -> (PolicyContext, MemberContext) {
    let user: UserId = user.parse().unwrap();
    (
        PolicyContext::Discord {
            guild: guild.clone(),
            user: user.clone(),
            manage_guild: operator,
        },
        MemberContext {
            guild: guild.clone(),
            user,
            channel: "44".into(),
            roles: BTreeSet::from(["55".into()]),
            observed_at: Instant::now(),
        },
    )
}
fn options(id: &str, route: &str, value: Value) -> PublishedRequest {
    PublishedRequest {
        interaction_id: None,
        private_action: None,
        expected_binding: None,
        member_only: false,
        command_id: id.into(),
        command_name: "dw".into(),
        route: route.into(),
        options: value.as_object().unwrap().clone(),
    }
}
fn policy() -> MemberReadPolicy {
    MemberReadPolicy {
        channels: BTreeSet::from(["44".into()]),
        roles: BTreeSet::from(["55".into()]),
        per_user_per_minute: 60,
        per_guild_per_minute: 600,
    }
}
fn dispatch(reply: &PublishedReply) -> Result<()> {
    reply
        .fence
        .as_ref()
        .expect("publication reply must carry a response fence")
        .dispatch(&mut || Ok(()))
}
fn assert_revoked(reply: &PublishedReply) {
    let mut sent = false;
    let result = reply.fence.as_ref().unwrap().dispatch(&mut || {
        sent = true;
        Ok(())
    });
    assert!(result.is_err());
    assert!(
        !sent,
        "revocation must prevent the transport callback, not only report an error afterward"
    );
}
fn writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                writable(&entry.path());
            }
        }
    }
}
fn synthetic_snapshot(directory: &Path) {
    let source_origin = "https://dandys-world-robloxhorror.fandom.com";
    let sources = json!([{"id":"page:1","page_id":1,"title":"Fixture Rock","url":format!("{source_origin}/wiki/Fixture_Rock"),"revision_id":2,"revision_timestamp":"2026-09-12T00:00:00Z","validated_at_ms":now()*1000,"content_sha256":"a".repeat(64),"license":"CC BY-SA 3.0","license_url":"https://creativecommons.org/licenses/by-sa/3.0/"}]);
    let entities = json!([{"id":"toon:fixture","kind":"toon","name":"Fixture Rock","aliases":["Rock"],"availability":"supported","warnings":[],"facts":[{"id":"fixture.health","key":"health","text":"Two fixture hearts","value":2,"unit":"hearts","conditions":["base"],"state":"supported","citations":[{"source_id":"page:1","section":"Fixture stats","quote":"Two fixture hearts"}]}],"relationships":[]}]);
    let value = json!({"schema_version":1,"adapter_version":"synthetic-command-runtime-fixture","source_origin":source_origin,"crawl_started_at":"2026-09-12T00:00:00Z","crawl_completed_at":"2026-09-12T00:01:00Z","sources":sources,"entities":entities,
        "coverage":{"discovered_pages":1,"imported_pages":1,"namespace_counts":{"articles":1},"nonredirect_articles":1,"redirects":0,"entities_by_kind":{"toon":1},"excluded":[],"unresolved_redirects":[],"warnings":[]}});
    let bytes = serde_json::to_vec(&value).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    std::fs::write(directory.join(format!("{digest}.json")), bytes).unwrap();
    std::fs::write(directory.join("active"), digest).unwrap();
}

#[tokio::test]
#[ignore = "requires ORACLE_DW_MODULE freshly built modules/dandys-world dw-module"]
async fn typed_members_keep_operator_policy_citations_and_lifecycle_response_fences() {
    let scratch = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
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
            operators: vec!["42".parse().unwrap()],
        }],
    ));
    let manager =
        ModuleManager::new(storage.clone(), core, scratch.path().join("artifacts")).unwrap();
    let backend = Arc::new(Backend::default());
    let reconciler = Arc::new(CommandReconciler::new(storage.clone(), backend.clone()));
    let module: ModuleId = "community.dandys-world".parse().unwrap();
    let directory = scratch.path().join("data");
    manager
        .configure_runtime_settings(
            &ACTOR,
            BTreeMap::from([(
                module.clone(),
                ModuleRuntimeSettings {
                    image_prefix: None,
                    data_directory: Some(directory.clone()),
                    citation_prefix: Some(
                        "https://dandys-world-robloxhorror.fandom.com/index.php?oldid=".into(),
                    ),
                },
            )]),
            &[scratch.path().join("state.sqlite")],
            std::fs::metadata(scratch.path()).unwrap().uid(),
        )
        .await
        .unwrap();
    synthetic_snapshot(&directory);
    let source = scratch.path().join("package");
    std::fs::create_dir(&source).unwrap();
    let binary = std::env::var_os("ORACLE_DW_MODULE").expect("set ORACLE_DW_MODULE");
    let bytes = std::fs::read(binary).unwrap();
    std::fs::write(source.join("module"), &bytes).unwrap();
    let package = ModulePackage {
        manifest: serde_json::from_str(include_str!("../../../modules/dandys-world/manifest.json"))
            .unwrap(),
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "synthetic-member-command-fixture".into(),
        toolchain: "fresh dw-module executable".into(),
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
        worker.load(&installed.digest).await.unwrap();
        let activation = DesiredActivation {
            module: module.clone(),
            guild: guild.clone(),
            active: true,
            grants: vec![],
            bindings: BTreeMap::new(),
        };
        worker.activate(&ACTOR, activation.clone()).await.unwrap();
        worker
            .configure_member_reads(&ACTOR, &guild, &module, Some(policy()))
            .await
            .unwrap();
        let (member_actor, member) = identity(&guild, "7", false);
        let (admin_actor, admin) = identity(&guild, "42", true);
        assert_eq!(
            worker
                .catalog(&member_actor, &guild)
                .await
                .unwrap_err()
                .code,
            ErrorCode::ForbiddenPermission
        );
        let member_catalog = worker.member_catalog(&member, &guild).await.unwrap();
        assert_eq!(member_catalog.entries.len(), 1);
        assert!(
            member_catalog.entries[0]
                .operations
                .iter()
                .all(|o| o.audience == ModuleAudience::MemberRead)
        );
        assert!(
            !member_catalog.entries[0]
                .operations
                .iter()
                .any(|o| o.name == "health")
        );
        let catalog = worker.catalog(&admin_actor, &guild).await.unwrap();
        let desired = compile_catalog(&catalog).unwrap();
        assert!(desired[0].definition["default_member_permissions"].is_null());
        let lookup = desired[0].definition["options"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "lookup")
            .unwrap();
        assert_eq!(lookup["options"][0]["name"], "name");
        assert_eq!(lookup["options"][0]["type"], 3);
        assert_eq!(lookup["options"][3]["type"], 4);
        assert!(
            !lookup["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["name"] == "input")
        );
        let cancel = CancellationToken::new();
        reconciler
            .reconcile(&guild, &desired, &cancel, now() + 60)
            .await
            .unwrap();
        let binding = reconciler.bindings(&guild).await.unwrap().remove(0);
        let id = binding.id.unwrap();
        let query = json!({"name":"Rock","kind":"toon","field":"health"});
        assert!(
            published_uses_member_identity(
                &worker,
                &reconciler,
                &member_actor,
                &member,
                &guild,
                &options(&id, "lookup", query.clone())
            )
            .await
            .unwrap()
        );
        assert!(
            published_uses_member_identity(
                &worker,
                &reconciler,
                &admin_actor,
                &admin,
                &guild,
                &options(&id, "lookup", query.clone())
            )
            .await
            .unwrap()
        );
        let reply = invoke_published_options(
            &worker,
            &reconciler,
            &member_actor,
            &member,
            &guild,
            options(&id, "lookup", query.clone()),
        )
        .await
        .unwrap();
        let text = &reply
            .card
            .as_ref()
            .expect("member reply uses a card")
            .embed
            .to_string();
        assert!(text.contains("Two fixture hearts"), "{text}");
        assert!(text.contains("https://dandys-world-robloxhorror.fandom.com/index.php?oldid=2"));
        assert!(!text.contains("\"reply\""));
        assert!(reply.policy.is_some());
        dispatch(&reply).unwrap();
        let mut followup = options(&id, "lookup", query.clone());
        followup.member_only = true;
        followup.expected_binding = reply.binding.clone();
        let followed = invoke_published_options(
            &worker,
            &reconciler,
            &member_actor,
            &member,
            &guild,
            followup.clone(),
        )
        .await
        .unwrap();
        assert!(followed.card.is_some());
        // A forged or stale card identity is rejected before module execution.
        followup.expected_binding = Some("other-module/old-session/0/0".into());
        assert_eq!(
            invoke_published_options(
                &worker,
                &reconciler,
                &member_actor,
                &member,
                &guild,
                followup.clone()
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::ModuleUnavailable
        );
        followup.expected_binding = None;
        assert_eq!(
            invoke_published_options(&worker, &reconciler, &admin_actor, &admin, &guild, followup)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::InvalidInput
        );
        for (route, input) in [
            ("lookup", json!({})),
            ("lookup", json!({"name":2})),
            ("lookup", json!({"name":"Rock","kind":"unknown"})),
            ("lookup", json!({"name":"Rock","offset":-1})),
            ("lookup", json!({"name":"Rock","destination":"44"})),
            ("search", json!({"query":"Rock","limit":"1"})),
            ("search", json!({"query":"Rock","limit":11})),
        ] {
            let result = invoke_published_options(
                &worker,
                &reconciler,
                &member_actor,
                &member,
                &guild,
                options(&id, route, input),
            )
            .await;
            assert_eq!(result.err().unwrap().code, ErrorCode::InvalidInput);
        }
        assert_eq!(
            invoke_published(
                &worker,
                &reconciler,
                &member_actor,
                &guild,
                request(&id, "dw", "status")
            )
            .await
            .unwrap_err()
            .code,
            ErrorCode::ForbiddenPermission
        );
        assert_eq!(
            worker
                .invoke(&member_actor, &module, &guild, "health", json!({}))
                .await
                .unwrap_err()
                .code,
            ErrorCode::ForbiddenPermission
        );
        worker
            .invoke(&admin_actor, &module, &guild, "health", json!({}))
            .await
            .unwrap();
        assert!(
            invoke_published_options(
                &worker,
                &reconciler,
                &member_actor,
                &member,
                &guild,
                options(&id, "health", json!({}))
            )
            .await
            .is_err()
        );
        let mut wrong = member.clone();
        wrong.channel = "99".into();
        assert!(
            invoke_published_options(
                &worker,
                &reconciler,
                &member_actor,
                &wrong,
                &guild,
                options(&id, "lookup", query.clone())
            )
            .await
            .is_err()
        );
        let mut wrong = member.clone();
        wrong.roles.clear();
        assert!(
            invoke_published_options(
                &worker,
                &reconciler,
                &member_actor,
                &wrong,
                &guild,
                options(&id, "lookup", query.clone())
            )
            .await
            .is_err()
        );
        let mut wrong = member.clone();
        wrong.user = "8".parse().unwrap();
        assert_eq!(
            invoke_published_options(
                &worker,
                &reconciler,
                &member_actor,
                &wrong,
                &guild,
                options(&id, "lookup", query.clone())
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::ForbiddenScope
        );
        assert_eq!(
            invoke_published_options(
                &worker,
                &reconciler,
                &member_actor,
                &member,
                &guild,
                options("wrong-id", "lookup", query.clone())
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::ModuleUnavailable
        );
        let mut wrong = options(&id, "lookup", query.clone());
        wrong.command_name = "different".into();
        assert_eq!(
            invoke_published_options(&worker, &reconciler, &member_actor, &member, &guild, wrong)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::ForbiddenScope
        );
        worker
            .configure_member_reads(&ACTOR, &guild, &module, None)
            .await
            .unwrap();
        assert_revoked(&reply);
        // Admins invoking a member route retain member restrictions; direct operator health remains valid.
        assert!(
            invoke_published_options(
                &worker,
                &reconciler,
                &admin_actor,
                &admin,
                &guild,
                options(&id, "lookup", query.clone())
            )
            .await
            .is_err()
        );
        worker
            .invoke(&admin_actor, &module, &guild, "health", json!({}))
            .await
            .unwrap();
        worker
            .configure_member_reads(&ACTOR, &guild, &module, Some(policy()))
            .await
            .unwrap();
        let reply = invoke_published_options(
            &worker,
            &reconciler,
            &admin_actor,
            &admin,
            &guild,
            options(&id, "lookup", query.clone()),
        )
        .await
        .unwrap();
        assert!(reply.policy.is_some());
        dispatch(&reply).unwrap();
        worker
            .deactivate(&ACTOR, &module, &guild, Duration::from_secs(1))
            .await
            .unwrap();
        assert_revoked(&reply);
        worker.activate(&ACTOR, activation).await.unwrap();
        assert_eq!(
            invoke_published_options(
                &worker,
                &reconciler,
                &member_actor,
                &member,
                &guild,
                options(&id, "lookup", query.clone())
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::ModuleUnavailable
        );
        let refreshed = compile_catalog(&worker.catalog(&ACTOR, &guild).await.unwrap()).unwrap();
        reconciler
            .reconcile(&guild, &refreshed, &cancel, now() + 60)
            .await
            .unwrap();
        assert_eq!(
            reconciler.bindings(&guild).await.unwrap()[0].id.as_deref(),
            Some(id.as_str())
        );
        let reply = invoke_published_options(
            &worker,
            &reconciler,
            &member_actor,
            &member,
            &guild,
            options(&id, "lookup", query.clone()),
        )
        .await
        .unwrap();
        dispatch(&reply).unwrap();
        worker
            .unload(&module, Duration::from_secs(1))
            .await
            .unwrap();
        assert_revoked(&reply);
        worker.load(&installed.digest).await.unwrap();
        assert_eq!(
            invoke_published_options(
                &worker,
                &reconciler,
                &member_actor,
                &member,
                &guild,
                options(&id, "lookup", query.clone())
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::ModuleUnavailable
        );
        reconciler
            .reconcile(
                &guild,
                &compile_catalog(&worker.catalog(&ACTOR, &guild).await.unwrap()).unwrap(),
                &cancel,
                now() + 60,
            )
            .await
            .unwrap();
        let (_, fresh_member) = identity(&guild, "7", false);
        let reply = invoke_published_options(
            &worker,
            &reconciler,
            &member_actor,
            &fresh_member,
            &guild,
            options(&id, "lookup", query),
        )
        .await
        .unwrap();
        dispatch(&reply).unwrap();
        worker.invalidate_member_reads(&guild);
        assert_revoked(&reply);
        assert_eq!(
            *backend.writes.lock().unwrap(),
            1,
            "epoch/session refresh must preserve unchanged Discord definitions"
        );
    })
    .await;
    let stopped = manager.shutdown().await;
    let closed = storage.close().await;
    writable(scratch.path());
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
    stopped.unwrap();
    closed.unwrap();
}
