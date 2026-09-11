//! B02 crosses real discovery, configuration, receipts, and native module RPC.
//! Scripted choices prove host contracts, not live-model choice quality.
use super::*;
#[path = "../../../examples/modules/activity-log/src/configuration_fixture.rs"]
mod configuration_fixture;
use oracle_ai::provider::{
    Continuation, ModelProvider, ModelRequest, ModelTurn, PreparedTurn, ProviderError, StopReason,
    Usage,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

async fn install(root: &Path, host: &Arc<Host>, typed: bool) -> ModuleId {
    let variable = if typed {
        "ORACLE_ACTIVITY_LOG_TYPED_CONFIG"
    } else {
        "ORACLE_ACTIVITY_LOG_NO_CONFIG"
    };
    let bytes =
        std::fs::read(std::env::var_os(variable).expect("build the matching native fixture"))
            .unwrap();
    let source = root.join("b02-source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("module"), &bytes).unwrap();
    let manifest = serde_json::from_str(include_str!(
        "../../../examples/modules/activity-log/manifest.json"
    ))
    .unwrap();
    let manifest = serde_json::from_value(configuration_fixture::configuration_variant(
        manifest, typed,
    ))
    .unwrap();
    let package = ModulePackage {
        manifest,
        entrypoint: "module".into(),
        files: BTreeMap::from([("module".into(), format!("{:x}", Sha256::digest(&bytes)))]),
        source_revision: "b02-configuration-discovery".into(),
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
    host.modules
        .activate(
            &PolicyContext::LocalOperator,
            DesiredActivation {
                module: module.clone(),
                guild: GuildId::new("100").unwrap(),
                active: true,
                grants: installed.package.manifest.capabilities,
                bindings: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    module
}

struct Script {
    step: AtomicUsize,
    typed: bool,
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
            input_token_reservation: body.len() as u64,
            body,
            provider_metadata: None,
            output_token_reservation: 1024,
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
        let tools = input["tools"].as_array().unwrap();
        let proposal = if step == 0 {
            Some((
                "core_tools_search_v1",
                json!({"query":"community.activity-log configuration plan apply status probe"}),
            ))
        } else if !self.typed {
            assert!(
                !tools
                    .iter()
                    .any(|tool| tool["name"].as_str().unwrap().contains("_config_"))
            );
            None
        } else {
            match step {
                1 => {
                    let plan = tools
                        .iter()
                        .find(|tool| tool["name"] == "community_activity_log_config_plan_v1")
                        .unwrap();
                    let fields = plan["parameters"]["properties"]["values"]["properties"]
                        .as_object()
                        .unwrap();
                    // Every fixed choice comes from the discovered typed schema.
                    // A schema discriminator is not a fabricated named-preset request.
                    let mut values = serde_json::Map::new();
                    for (key, schema) in fields {
                        if let Some(choices) = schema["enum"].as_array() {
                            assert_eq!(choices.len(), 1);
                            values.insert(key.clone(), choices[0].clone());
                        }
                    }
                    assert_eq!(fields["destination"]["type"], "string");
                    values.insert("destination".into(), json!("456"));
                    Some((
                        "community_activity_log_config_plan_v1",
                        json!({"values":values}),
                    ))
                }
                2 => Some((
                    "community_activity_log_config_apply_v1",
                    json!({"reference":input["results"][0]["value"]["reference"]}),
                )),
                3 => Some((
                    "core_tools_search_v1",
                    json!({"query":"community.activity-log probe status verification"}),
                )),
                4 => Some(("community_activity_log_probe_v1", json!({}))),
                5 => Some(("community_activity_log_status_v1", json!({}))),
                _ => None,
            }
        };
        let calls = proposal
            .map(|(name, arguments)| {
                vec![ToolCall {
                    id: format!("b02-{step}"),
                    name: name.into(),
                    arguments,
                }]
            })
            .unwrap_or_default();
        Ok(ModelTurn {
            stop: if calls.is_empty() { StopReason::Completed } else { StopReason::ToolCalls }, calls,
            visible_text: Some(if self.typed { "Used the module's registered typed fields, preserving its documented privacy settings, and verified delivery." } else { "This module exposes neither a named preset nor a configuration operation. A compatible configuration capability must be registered before logging can be configured." }.into()),
            continuation: Continuation { profile: self.profile().id.clone(), opaque: "b02-fixture".into() },
            usage: Usage { total_tokens: Some(100), ..Default::default() }, model: Some("script".into()),
        })
    }
}

#[tokio::test]
#[ignore = "requires ORACLE_ACTIVITY_LOG_TYPED_CONFIG separately built with typed-config-fixture"]
async fn no_named_preset_uses_discovered_typed_configuration_and_verifies_delivery() {
    let (root, host, _) = super::tests::fixture().await;
    let (_, transport) = super::logging_tests::services(&host).await;
    let module = install(root.path(), &host, true).await;
    let guild = GuildId::new("100").unwrap();
    let snapshot = host
        .modules
        .ai_catalog_snapshot(&PolicyContext::LocalOperator, &guild)
        .await
        .unwrap();
    let configuration = snapshot
        .entries
        .iter()
        .find(|entry| entry.module == module)
        .unwrap()
        .configuration
        .as_ref()
        .unwrap();
    assert!(configuration.presets.is_empty());
    // The missing named preset cannot be fabricated, even though typed values work.
    assert!(
        host.execute(
            &PolicyContext::LocalOperator,
            &guild,
            OperationRequest::ConfigurationPlan {
                module: module.clone(),
                preset: Some("moderate/v1".into()),
                values: json!({"destination":"456"}),
            },
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    let run = super::logging_tests::coordinator_with_provider(&host, Arc::new(Script { step: AtomicUsize::new(0), typed: true }))
        .ask(&PolicyContext::LocalOperator, guild.clone(), "Use supported typed settings for moderate logging to staff destination 456 and verify delivery".into()).await.unwrap();
    assert_eq!(
        run.run.status,
        RunStatus::Succeeded,
        "{:?}",
        run.run.problem
    );
    let status = host
        .modules
        .configuration_inspect(&PolicyContext::LocalOperator, &guild, &module)
        .await
        .unwrap();
    assert_eq!(status.stored_revision, 1);
    assert_eq!(
        status.values.as_ref(),
        status.effective.as_ref().map(|effective| &effective.values)
    );
    let values = status.values.unwrap();
    assert_eq!(values["destination"], "456");
    assert_eq!(values["retain_message_content"], false);
    assert_eq!(values["retain_attachments"], false);
    assert_eq!(values["retention_days"], 14);
    assert_eq!(values["self_origin_exclusion"], true);
    assert_eq!(transport.messages.lock().unwrap().len(), 1);
    for field in ["retain_message_content", "invented_option"] {
        let mut unsupported = values.clone();
        unsupported[field] = json!(true);
        assert!(
            host.execute(
                &PolicyContext::LocalOperator,
                &guild,
                OperationRequest::ConfigurationPlan {
                    module: module.clone(),
                    preset: None,
                    values: unsupported,
                },
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
    }
    let unchanged = host
        .modules
        .configuration_inspect(&PolicyContext::LocalOperator, &guild, &module)
        .await
        .unwrap();
    assert_eq!(unchanged.stored_revision, 1);
    assert_eq!(unchanged.values.as_ref(), Some(&values));
    let calls = RunStore::new(host.core.clone(), host.storage.clone())
        .calls(&run)
        .await
        .unwrap();
    let plan = calls
        .iter()
        .find(|record| record.call.name == "community_activity_log_config_plan_v1")
        .unwrap();
    assert!(plan.call.arguments.get("preset").is_none());
    assert!(!plan.call.is_error);
    host.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires ORACLE_ACTIVITY_LOG_NO_CONFIG separately built with no-config-fixture"]
async fn no_configuration_capability_reports_missing_support_without_effects() {
    let (root, host, _) = super::tests::fixture().await;
    let (_, transport) = super::logging_tests::services(&host).await;
    let module = install(root.path(), &host, false).await;
    let guild = GuildId::new("100").unwrap();
    let run = super::logging_tests::coordinator_with_provider(
        &host,
        Arc::new(Script {
            step: AtomicUsize::new(0),
            typed: false,
        }),
    )
    .ask(
        &PolicyContext::LocalOperator,
        guild.clone(),
        "Set up moderate logging".into(),
    )
    .await
    .unwrap();
    assert_eq!(run.run.status, RunStatus::WaitingInput);
    assert!(
        run.run
            .unverified_model_message
            .as_ref()
            .unwrap()
            .contains("neither a named preset nor a configuration operation")
    );
    assert!(run.run.references.is_empty());
    let status = host
        .modules
        .configuration_inspect(&PolicyContext::LocalOperator, &guild, &module)
        .await
        .unwrap();
    assert_eq!(status.stored_revision, 0);
    assert!(status.values.is_none());
    assert!(status.effective.is_none());
    assert!(transport.messages.lock().unwrap().is_empty());
    let calls = RunStore::new(host.core.clone(), host.storage.clone())
        .calls(&run)
        .await
        .unwrap();
    assert!(
        calls
            .iter()
            .all(|record| record.call.name == "core_tools_search_v1")
    );
    host.close().await.unwrap();
}
