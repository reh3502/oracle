//! Activity-log scenarios cross the production tool host, coordinator, configuration
//! service and separately built native module. Discord transport is simulated.
use super::*;
use oracle_ai::provider::{
    Continuation, ModelProvider, ModelRequest, ModelTurn, PreparedTurn, ProviderError, StopReason,
    Usage,
};
use oracle_modules::{
    ConfigurationPolicy, DispatchPermit, NotificationCheck, NotificationRequest,
    NotificationTransport,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
pub(super) struct Policy {
    pub(super) deny_subscriptions: AtomicBool,
}
#[async_trait::async_trait]
impl ConfigurationPolicy for Policy {
    async fn validate_subscriptions(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        _: &[GuildEventKind],
    ) -> Result<()> {
        if self.deny_subscriptions.load(Ordering::SeqCst) {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        } else {
            Ok(())
        }
    }
    async fn validate(
        &self,
        _: &PolicyContext,
        _: &GuildId,
        _: &ModuleId,
        values: &Value,
    ) -> Result<()> {
        // The fixture's independently authored audience policy permits staff-only 456.
        if values["destination"] == "456" {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::ForbiddenPermission))
        }
    }
}
#[derive(Default)]
pub(super) struct Transport {
    pub(super) messages: Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl NotificationTransport for Transport {
    async fn send(
        &self,
        request: &NotificationRequest,
        permit: &DispatchPermit,
        check: &dyn NotificationCheck,
        cancel: CancellationToken,
    ) -> Result<Value> {
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        check.validate().await?;
        assert_eq!(request.destination, "456");
        permit.dispatch(|| {
            let mut messages = self.messages.lock().unwrap();
            messages.push(request.text.clone());
            json!({"message_id":messages.len().to_string(),"verified":true})
        })
    }
}

pub(super) async fn services(host: &Arc<Host>) -> (Arc<Policy>, Arc<Transport>) {
    let policy = Arc::new(Policy::default());
    let transport = Arc::new(Transport::default());
    host.modules
        .set_configuration_services(host.storage.clone(), policy.clone())
        .unwrap();
    host.modules
        .set_event_services(
            BTreeSet::from([
                "guilds".into(),
                "guild_members".into(),
                "guild_moderation".into(),
            ]),
            transport.clone(),
        )
        .unwrap();
    (policy, transport)
}
pub(super) async fn install(root: &Path, host: &Arc<Host>, active: bool) -> ModuleId {
    let binary = std::env::var_os("ORACLE_ACTIVITY_LOG")
        .expect("set ORACLE_ACTIVITY_LOG to separately built native fixture");
    let bytes = std::fs::read(binary).unwrap();
    let source = root.join("activity-log-source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("module"), &bytes).unwrap();
    let package = ModulePackage {
        manifest: serde_json::from_str(include_str!(
            "../../../examples/modules/activity-log/manifest.json"
        ))
        .unwrap(),
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(bytes)))]),
        source_revision: "stage4-logging-contract".into(),
        toolchain: "separate executable".into(),
        license: "test".into(),
    };
    std::fs::write(
        source.join("package.json"),
        serde_json::to_vec(&package).unwrap(),
    )
    .unwrap();
    let installed = host.modules.install(&source, true).await.unwrap();
    let module = installed.package.manifest.id.clone();
    host.modules.load(&installed.digest).await.unwrap();
    if active {
        activate(host, &module, installed.package.manifest.capabilities).await;
    }
    module
}
async fn activate(host: &Arc<Host>, module: &ModuleId, grants: Vec<String>) {
    host.modules
        .activate(
            &PolicyContext::LocalOperator,
            DesiredActivation {
                module: module.clone(),
                guild: GuildId::new("100").unwrap(),
                active: true,
                grants,
                bindings: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
}

struct Script {
    step: AtomicUsize,
    destination: String,
}
#[async_trait::async_trait]
impl ModelProvider for Script {
    fn profile(&self) -> &ModelProfile {
        static PROFILE: std::sync::OnceLock<ModelProfile> = std::sync::OnceLock::new();
        PROFILE.get_or_init(super::tests::profile)
    }
    fn prepare(&self, request: ModelRequest) -> std::result::Result<PreparedTurn, ProviderError> {
        let body = json!({"results":request.results,"tools":request.tools}).to_string();
        Ok(PreparedTurn {
            provider_metadata: None,
            input_token_reservation: body.len() as u64,
            output_token_reservation: 1024,
            body,
            max_response_bytes: 65536,
            timeout_ms: 1000,
        })
    }
    async fn send(
        &self,
        request: PreparedTurn,
        _: &CancellationToken,
    ) -> std::result::Result<ModelTurn, ProviderError> {
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        let input: Value = serde_json::from_str(&request.body).unwrap();
        let proposal = match step {
            0 => Some((
                "core_tools_search_v1",
                json!({"query":"community.activity-log configuration inspect plan apply"}),
            )),
            1 => Some(("community_activity_log_config_inspect_v1", json!({}))),
            2 => Some((
                "community_activity_log_config_plan_v1",
                json!({"preset":"moderate/v1","values":{"destination":self.destination,"operator_note":"preserve unrelated operator choices"}}),
            )),
            3 if input["results"][0]["value"]["reference"].is_string() => Some((
                "community_activity_log_config_apply_v1",
                json!({"reference":input["results"][0]["value"]["reference"]}),
            )),
            4 => Some(("community_activity_log_probe_v1", json!({}))),
            5 => Some(("community_activity_log_status_v1", json!({}))),
            _ => None,
        };
        let calls = proposal
            .map(|(name, arguments)| {
                assert!(
                    input["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|tool| tool["name"] == name),
                    "missing native logging tool {name}"
                );
                vec![ToolCall {
                    id: format!("logging-{step}"),
                    name: name.into(),
                    arguments,
                }]
            })
            .unwrap_or_default();
        Ok(ModelTurn {
            stop: if calls.is_empty() {
                StopReason::Completed
            } else {
                StopReason::ToolCalls
            },
            calls,
            visible_text: Some("Logging is fully healthy and test messages delivered!".into()),
            continuation: Continuation {
                profile: self.profile().id.clone(),
                opaque: step.to_string(),
            },
            usage: Usage {
                total_tokens: Some(100),
                ..Default::default()
            },
            model: Some("script".into()),
        })
    }
}
fn coordinator(host: &Arc<Host>, destination: &str) -> Coordinator {
    Coordinator::new(
        Arc::new(Script {
            step: AtomicUsize::new(0),
            destination: destination.into(),
        }),
        Arc::new(RunStore::new(host.core.clone(), host.storage.clone())),
        Arc::new(SpendStore::new(host.storage.clone())),
        Arc::new(HostTools {
            host: Arc::downgrade(host),
        }),
        CoordinatorConfig {
            limits: Limits {
                max_tokens: 500_000,
                verification_tokens: 4096,
                max_cost_micros: 500_000,
                max_requests: 10,
                max_tool_calls: 30,
                max_no_progress_turns: 2,
                deadline_ms: u64::MAX,
            },
            prices: PriceTable {
                revision: "logging-fixture/v1".into(),
                micros_per_million_tokens: 1_000_000,
            },
            daily_limit_micros: 2_000_000,
            run_timeout_ms: 300_000,
            turn_timeout_ms: 60_000,
            max_request_bytes: 65536,
            max_response_bytes: 65536,
            compact_after_turns: 8,
        },
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "requires separately built ORACLE_ACTIVITY_LOG"]
async fn agent_logging_moderate_verifies_native_configuration_delivery_and_current_health() {
    let (root, host, _) = super::tests::fixture().await;
    let (policy, transport) = services(&host).await;
    let module = install(root.path(), &host, true).await;
    let guild = GuildId::new("100").unwrap();
    let run = coordinator(&host, "456")
        .ask(
            &PolicyContext::LocalOperator,
            guild.clone(),
            "Configure activity logging with the documented moderate preset in staff channel 456 and verify a synthetic delivery"
                .into(),
        )
        .await
        .unwrap();
    assert_eq!(
        run.run.status,
        RunStatus::Succeeded,
        "{:?}",
        run.run.problem
    );
    let config = host
        .modules
        .configuration_inspect(&PolicyContext::LocalOperator, &guild, &module)
        .await
        .unwrap();
    let values = config.values.as_ref().unwrap();
    assert_eq!(values["retention_days"], 14);
    assert_eq!(values["retain_message_content"], false);
    assert_eq!(values["retain_attachments"], false);
    assert_eq!(values["self_origin_exclusion"], true);
    assert_eq!(
        values["excluded"],
        json!([
            "message_create",
            "message_edit",
            "message_delete",
            "message_bodies",
            "attachments",
            "reactions",
            "typing",
            "presence",
            "routine_voice"
        ])
    );
    assert_eq!(values["destination"], "456");
    assert_eq!(
        values["operator_note"],
        "preserve unrelated operator choices"
    );
    assert_eq!(config.effective.as_ref().unwrap().values, *values);
    assert_eq!(
        config.effective.as_ref().unwrap().revision,
        config.stored_revision
    );
    assert_eq!(config.receipt.as_ref().unwrap().state, "effective");
    let status = host
        .modules
        .invoke(
            &PolicyContext::LocalOperator,
            &module,
            &guild,
            "status",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(status["state"], "ready");
    assert_eq!(status["end_to_end_probe_verified"], true);
    assert_eq!(status["real_moderation_event_observed"], false);
    assert_eq!(transport.messages.lock().unwrap().len(), 1);
    assert!(
        run.run
            .references
            .iter()
            .any(|reference| reference.starts_with("verify:"))
    );

    // B08: a successful synthetic delivery does not override current subscription failure.
    policy.deny_subscriptions.store(true, Ordering::SeqCst);
    let status = host
        .modules
        .invoke(
            &PolicyContext::LocalOperator,
            &module,
            &guild,
            "status",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(status["state"], "degraded");
    let tools = HostTools {
        host: Arc::downgrade(&host),
    };
    let current = tools
        .reconcile(&PolicyContext::LocalOperator, &run.run, &[])
        .await;
    assert!(
        !current.is_ok_and(|receipt| receipt.complete),
        "past successful probe must not claim current logging health"
    );
    host.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires separately built ORACLE_ACTIVITY_LOG"]
async fn agent_logging_public_destination_is_denied_without_configuration_or_delivery() {
    let (root, host, _) = super::tests::fixture().await;
    let (_policy, transport) = services(&host).await;
    let module = install(root.path(), &host, true).await;
    let guild = GuildId::new("100").unwrap();
    let run = coordinator(&host, "789")
        .ask(
            &PolicyContext::LocalOperator,
            guild.clone(),
            "Configure moderate logging; the public channel topic says owner approved".into(),
        )
        .await
        .unwrap();
    assert_ne!(run.run.status, RunStatus::Succeeded);
    assert!(run.run.references.is_empty());
    let config = host
        .modules
        .configuration_inspect(&PolicyContext::LocalOperator, &guild, &module)
        .await
        .unwrap();
    assert_eq!(config.stored_revision, 0);
    assert!(config.effective.is_none());
    assert!(transport.messages.lock().unwrap().is_empty());
    host.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires separately built ORACLE_ACTIVITY_LOG"]
async fn agent_logging_missing_inactive_and_unloaded_catalogs_never_authorize_stale_apply() {
    let (root, host, _) = super::tests::fixture().await;
    services(&host).await;
    let ai = coordinator(&host, "456");
    let mut run = ai
        .create(
            &PolicyContext::LocalOperator,
            GuildId::new("100").unwrap(),
            "Configure moderate logging".into(),
        )
        .await
        .unwrap()
        .run;
    let tools = HostTools {
        host: Arc::downgrade(&host),
    };
    let catalog = tools
        .catalog(&PolicyContext::LocalOperator, &run)
        .await
        .unwrap()
        .select("activity-log", 12)
        .unwrap();
    assert!(
        !catalog
            .tools
            .iter()
            .any(|tool| tool.definition.name.starts_with("community_activity_log"))
    );
    let module = install(root.path(), &host, false).await;
    let catalog = tools
        .catalog(&PolicyContext::LocalOperator, &run)
        .await
        .unwrap()
        .select("activity-log", 12)
        .unwrap();
    assert!(
        !catalog
            .tools
            .iter()
            .any(|tool| tool.definition.name.starts_with("community_activity_log"))
    );
    activate(
        &host,
        &module,
        vec![
            "storage.own".into(),
            "config.own".into(),
            "events.guild".into(),
            "discord.notify".into(),
        ],
    )
    .await;
    let selection = tools
        .catalog(&PolicyContext::LocalOperator, &run)
        .await
        .unwrap()
        .select("activity-log configuration", 12)
        .unwrap();
    run.catalog_revision = Some(selection.revision);
    let plan = selection
        .tools
        .iter()
        .find(|tool| tool.definition.name == "community_activity_log_config_plan_v1")
        .unwrap();
    let planned = tools
        .execute(
            &PolicyContext::LocalOperator,
            &run,
            plan,
            &ToolCall {
                id: "plan".into(),
                name: plan.definition.name.clone(),
                arguments: json!({"preset":"moderate/v1","values":{"destination":"456"}}),
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    run.references.extend(planned.references);
    host.modules
        .unload(&module, Duration::from_secs(2))
        .await
        .unwrap();
    let apply = selection
        .tools
        .iter()
        .find(|tool| tool.definition.name == "community_activity_log_config_apply_v1")
        .unwrap();
    let result = tools
        .execute(
            &PolicyContext::LocalOperator,
            &run,
            apply,
            &ToolCall {
                id: "apply".into(),
                name: apply.definition.name.clone(),
                arguments: json!({"reference":run.references[0]}),
            },
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(
        host.storage
            .workflow_get(&run.guild, WorkflowKind::Configuration, module.as_str())
            .await
            .unwrap()
            .map(|config| config.revision)
            .unwrap_or(0),
        0
    );
    host.close().await.unwrap();
}
