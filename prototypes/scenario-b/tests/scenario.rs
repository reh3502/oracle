use oracle_scenario_b::*;
use serde_json::json;
use std::{path::Path, time::Duration};
fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_activity-log"))
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_load_exact_preset_and_independent_readback() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(&dir.path().join("host.sqlite")).unwrap();
    assert!(matches!(host.discover().await, Err(Error::Generation)));
    assert_eq!(host.runtime.loaded_count(), 0);
    let host_pid = std::process::id();
    let installed = dir.path().join("logging-module");
    std::fs::copy(binary(), &installed).unwrap();
    host.load(&installed).await.unwrap();
    let description = host.discover().await.unwrap();
    assert_eq!(description["module"], "community.activity-log");
    assert!(
        description["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("logging_config_apply_v1"))
    );
    let plan = host.plan("moderate/v1").await.unwrap();
    let receipt = host.apply(&plan, false, false).await.unwrap();
    assert!(receipt.complete);
    assert_eq!(receipt.stored_revision, 1);
    assert_eq!(receipt.effective_revision, Some(1));
    assert_eq!(host.delivery.count(), 1);
    let (_, config) = host.stored().unwrap();
    let config = config.unwrap();
    assert_eq!(
        config.enabled,
        [
            "moderation_audit",
            "channel_changes",
            "role_access_changes",
            "member_role_changes",
            "bans_unbans",
            "membership_summary"
        ]
    );
    assert_eq!(
        config.excluded,
        [
            "message_create",
            "message_edit",
            "message_delete",
            "message_bodies",
            "attachments",
            "reactions",
            "typing",
            "presence",
            "routine_voice"
        ]
    );
    assert_eq!(config.membership_summary_minutes, 15);
    assert_eq!(config.retention_days, 14);
    assert_eq!(config.destination, "staff-logs");
    assert!(!config.retain_message_content);
    assert!(!config.retain_attachments);
    assert!(config.self_origin_exclusion);
    assert!(config.coalesce);
    assert_eq!(config.queue_limit, 128);
    assert!(config.dropped_event_summary);
    assert_eq!(host.host_tracing, "info,oracle=debug");
    assert_eq!(std::process::id(), host_pid);
    let again = host.apply(&plan, false, false).await.unwrap();
    assert!(again.complete);
    assert_eq!(host.delivery.count(), 1);
    assert_eq!(host.stored().unwrap().0, 1);
    host.unload().await.unwrap();
    assert_eq!(host.runtime.loaded_count(), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_before_ack_preserves_desired_and_recovers_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("host.sqlite");
    let mut host = Host::open(&db).unwrap();
    host.load(binary()).await.unwrap();
    let old = host.module.as_ref().unwrap().clone();
    let plan = host.plan("moderate/v1").await.unwrap();
    let partial = host.apply(&plan, true, false).await.unwrap();
    assert!(!partial.complete);
    assert_eq!(partial.problem.as_deref(), Some("activation_unknown"));
    assert_eq!(partial.effective_revision, None);
    assert_eq!(host.stored().unwrap().0, 1);
    assert_eq!(host.delivery.count(), 0);
    old.wait_stopped(Duration::from_secs(5)).await.unwrap();
    assert!(!Path::new(&format!("/proc/{}", old.pid())).exists());
    assert!(
        old.invoke("health", json!({}), SCOPE, Duration::from_secs(1))
            .await
            .is_err()
    );
    host.unload().await.unwrap();
    drop(host);
    let mut reopened = Host::open(&db).unwrap();
    assert!(!reopened.receipt(&plan.id).unwrap().complete);
    reopened.load(binary()).await.unwrap();
    assert!(matches!(
        reopened.apply(&plan, false, false).await,
        Err(Error::Generation)
    ));
    let recovered = reopened.recover(&plan.id).await.unwrap();
    assert!(recovered.complete);
    assert_eq!(recovered.stored_revision, 1);
    assert_eq!(recovered.effective_revision, Some(1));
    assert_eq!(reopened.delivery.count(), 1);
    reopened.unload().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_generation_and_cas_conflict_cannot_overwrite_human_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(&dir.path().join("host.sqlite")).unwrap();
    host.load(binary()).await.unwrap();
    let first = host.plan("moderate/v1").await.unwrap();
    host.apply(&first, false, false).await.unwrap();
    let stale = host.plan("moderate/v1").await.unwrap();
    host.set_operator_note("preserve this human setting")
        .unwrap();
    assert!(matches!(
        host.apply(&stale, false, false).await,
        Err(Error::Conflict)
    ));
    assert_eq!(host.stored().unwrap().0, 2);
    let refreshed = host.plan("moderate/v1").await.unwrap();
    assert_eq!(
        refreshed.desired.operator_note,
        "preserve this human setting"
    );
    host.apply(&refreshed, false, false).await.unwrap();
    let old_plan = host.plan("moderate/v1").await.unwrap();
    let old = host.module.as_ref().unwrap().clone();
    host.unload().await.unwrap();
    host.load(binary()).await.unwrap();
    assert!(matches!(
        host.apply(&old_plan, false, false).await,
        Err(Error::Generation)
    ));
    assert!(
        old.invoke("health", json!({}), SCOPE, Duration::from_secs(1))
            .await
            .is_err()
    );
    assert_eq!(host.stored().unwrap().0, 3);
    host.unload().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_intent_unknown_preset_unsafe_destination_and_scope_are_denied() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(&dir.path().join("host.sqlite")).unwrap();
    host.load(binary()).await.unwrap();
    assert!(matches!(host.plan("invented/v1").await, Err(Error::Preset)));
    host.capabilities.guild_members = false;
    assert!(matches!(
        host.plan("moderate/v1").await,
        Err(Error::Capability)
    ));
    host.capabilities.guild_members = true;
    host.destination.readers.insert("everyone".into());
    assert!(matches!(host.plan("moderate/v1").await, Err(Error::Denied)));
    host.destination = Destination::staff();
    host.destination.scope = "guild:999".into();
    assert!(matches!(host.plan("moderate/v1").await, Err(Error::Denied)));
    host.destination = Destination::staff();
    host.destination.actor_can_view = false;
    assert!(matches!(host.plan("moderate/v1").await, Err(Error::Denied)));
    host.destination = Destination::staff();
    let plan = host.plan("moderate/v1").await.unwrap();
    host.capabilities.guild_members = false;
    assert!(matches!(
        host.apply(&plan, false, false).await,
        Err(Error::Capability)
    ));
    assert_eq!(host.stored().unwrap().0, 0);
    assert_eq!(host.delivery.count(), 0);
    host.unload().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_failure_and_unhealthy_subscriptions_are_partial() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(&dir.path().join("host.sqlite")).unwrap();
    host.load(binary()).await.unwrap();
    let plan = host.plan("moderate/v1").await.unwrap();
    host.delivery.fail_send = true;
    let receipt = host.apply(&plan, false, false).await.unwrap();
    assert!(!receipt.complete);
    assert_eq!(receipt.effective_revision, Some(1));
    assert_eq!(receipt.problem.as_deref(), Some("delivery_failed"));
    assert_eq!(host.delivery.count(), 0);
    host.delivery.fail_send = false;
    host.delivery.fail_readback = true;
    let unverified = host.recover(&receipt.id).await.unwrap();
    assert!(!unverified.complete);
    assert_eq!(unverified.problem.as_deref(), Some("delivery_unverified"));
    assert_eq!(host.delivery.count(), 1);
    host.delivery.fail_readback = false;
    assert!(host.recover(&receipt.id).await.unwrap().complete);
    assert_eq!(host.delivery.count(), 1);
    let plan = host.plan("moderate/v1").await.unwrap();
    let unhealthy = host.apply(&plan, false, true).await.unwrap();
    assert!(!unhealthy.complete);
    assert_eq!(
        unhealthy.problem.as_deref(),
        Some("subscriptions_unhealthy")
    );
    assert_eq!(host.delivery.count(), 1);
    host.unload().await.unwrap();
}
