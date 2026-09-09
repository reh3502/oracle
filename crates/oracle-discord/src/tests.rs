use super::*;
use oracle_core as core;
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct FakeRepository {
    calls: AtomicUsize,
    writes: AtomicUsize,
    state: Mutex<core::GuildState>,
}
impl FakeRepository {
    fn count(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl core::Repository for FakeRepository {
    async fn status(&self, guild: Option<&core::GuildId>) -> core::Result<core::Status> {
        self.count();
        let state = self.state.lock().unwrap().clone();
        if guild != Some(&state.guild) {
            return Err(core::Error::new(core::ErrorCode::ForbiddenScope));
        }
        Ok(core::Status {
            deployment: core::DeploymentId::generate(),
            guilds: vec![state],
            modules_loaded: 0,
            recovery_required: 0,
            ai_available: false,
        })
    }
    async fn set_paused(
        &self,
        guild: &core::GuildId,
        paused: bool,
        expected_revision: u64,
        _actor: &str,
        operation: &core::OperationId,
    ) -> core::Result<core::ControlReceipt> {
        self.count();
        self.writes.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        if &state.guild != guild {
            return Err(core::Error::new(core::ErrorCode::ForbiddenScope));
        }
        if state.revision != expected_revision {
            return Err(core::Error::new(core::ErrorCode::Conflict));
        }
        state.paused = paused;
        state.revision += 1;
        Ok(core::ControlReceipt {
            operation: operation.clone(),
            guild: state.clone(),
        })
    }
    async fn begin_operation(&self, _: &core::Operation) -> core::Result<()> {
        self.count();
        Ok(())
    }
    async fn reserve_effect(&self, effect: &core::Effect) -> core::Result<core::Effect> {
        self.count();
        Ok(effect.clone())
    }
    async fn effect(&self, _: &core::GuildId, _: &core::EffectId) -> core::Result<core::Effect> {
        self.count();
        Err(core::Error::new(core::ErrorCode::NotFound))
    }
    async fn transition_effect(
        &self,
        _: &core::GuildId,
        _: &core::EffectId,
        _: u64,
        _: core::EffectState,
        _: Option<Value>,
    ) -> core::Result<core::Effect> {
        self.count();
        Err(core::Error::new(core::ErrorCode::NotFound))
    }
    async fn finish_operation(
        &self,
        _: &core::GuildId,
        _: &core::OperationId,
        _: core::OperationState,
    ) -> core::Result<()> {
        self.count();
        Ok(())
    }
    async fn recovery(&self, _: &core::GuildId, _: u32) -> core::Result<Vec<core::Effect>> {
        self.count();
        Ok(vec![])
    }
}
#[derive(Default)]
struct Responder {
    events: Mutex<Vec<String>>,
    fail_ack: bool,
}
#[async_trait]
impl InteractionResponder for Responder {
    async fn defer_ephemeral(&self) -> Result<()> {
        if self.fail_ack {
            return Err(Error::Transport);
        }
        self.events
            .lock()
            .unwrap()
            .push("deferred-ephemeral".into());
        Ok(())
    }
    async fn complete(&self, message: &str) -> Result<()> {
        self.events.lock().unwrap().push(message.into());
        Ok(())
    }
    async fn reject_ephemeral(&self, message: &str) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(format!("rejected-ephemeral:{message}"));
        Ok(())
    }
}
fn setup() -> (DiscordBootstrap, Arc<FakeRepository>) {
    let guild = core::GuildId::new("101").unwrap();
    let repository = Arc::new(FakeRepository {
        calls: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
        state: Mutex::new(core::GuildState {
            guild: guild.clone(),
            paused: false,
            revision: 0,
        }),
    });
    let service = CoreService::new(
        repository.clone(),
        vec![core::GuildPolicy {
            guild,
            operators: vec![core::UserId::new("303").unwrap()],
        }],
    );
    (DiscordBootstrap::new(Arc::new(service)), repository)
}
fn interaction(
    guild: &str,
    user: &str,
    permissions: &str,
    action: Option<&str>,
) -> discord::CommandInteraction {
    let option = match action {
        None => json!({"name":"status","type":1,"options":[]}),
        Some(action) => {
            json!({"name":"control","type":1,"options":[{"name":"action","type":3,"value":action}]})
        }
    };
    let raw = json!({"d":{
        "id":"707","application_id":"505","type":2,"guild_id":guild,"channel_id":"202",
        "data":{"id":"404","name":"oracle","type":1,"options":[option]},
        "channel":{"id":"202","type":0,"name":"fixture","guild_id":guild},
        "member":{"user":{"id":user,"username":"operator","discriminator":"0","avatar":null},"roles":[],"joined_at":null,"deaf":false,"mute":false,"flags":0,"permissions":permissions},
        "token":"invented-interaction-token","version":1,"app_permissions":"0","locale":"en-US","entitlements":[],"attachment_size_limit":10485760
    },"op":0,"s":1,"t":"INTERACTION_CREATE"});
    let gateway: discord::GatewayEvent = serde_json::from_str(&raw.to_string()).unwrap();
    let discord::GatewayEvent::Dispatch { event, .. } = gateway else {
        panic!("not dispatch")
    };
    let full =
        discord::FullEvent::from_event(event.into_event(), &mut None, &discord::Cache::default());
    let discord::FullEvent::InteractionCreate {
        interaction: discord::Interaction::Command(command),
        ..
    } = full
    else {
        panic!("not command interaction")
    };
    command
}

#[tokio::test]
async fn real_dispatch_status_pause_resume_share_core_without_ai() {
    let (adapter, repository) = setup();
    let responder = Responder::default();
    assert!(
        adapter
            .handle_command(&interaction("101", "303", "32", None), &responder)
            .await
            .unwrap()
    );
    adapter
        .handle_command(&interaction("101", "303", "32", Some("pause")), &responder)
        .await
        .unwrap();
    assert!(repository.state.lock().unwrap().paused);
    adapter
        .handle_command(&interaction("101", "303", "32", Some("resume")), &responder)
        .await
        .unwrap();
    assert!(!repository.state.lock().unwrap().paused);
    let events = responder.events.lock().unwrap();
    assert_eq!(events.len(), 6);
    assert_eq!(events[0], "deferred-ephemeral");
    assert!(events[1].contains("Modules loaded: 0"));
    assert!(events[1].contains("AI: unavailable"));
    assert!(events[3].contains("paused"));
    assert!(events[5].contains("running"));
    assert!(!events.join(" ").contains("invented-interaction-token"));
}

#[tokio::test]
async fn scope_denials_never_reach_repository_and_permission_denials_never_write() {
    for (guild, user, permissions) in [
        ("999", "303", "32"),
        ("101", "999", "32"),
        ("101", "303", "0"),
    ] {
        let (adapter, repository) = setup();
        let responder = Responder::default();
        adapter
            .handle_command(
                &interaction(guild, user, permissions, Some("pause")),
                &responder,
            )
            .await
            .unwrap();
        if guild == "999" {
            assert_eq!(repository.calls.load(Ordering::SeqCst), 0);
        }
        assert_eq!(repository.writes.load(Ordering::SeqCst), 0);
        assert!(!repository.state.lock().unwrap().paused);
        assert!(
            responder
                .events
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .contains("refused")
        );
    }
}

#[tokio::test]
async fn malformed_member_or_command_and_failed_ack_never_reach_repository() {
    let (adapter, repository) = setup();
    let mut command = interaction("101", "303", "32", None);
    command.member = None;
    adapter
        .handle_command(&command, &Responder::default())
        .await
        .unwrap();
    let mut mismatch = interaction("101", "303", "32", None);
    mismatch.member.as_mut().unwrap().guild_id = discord::GuildId::new(999);
    adapter
        .handle_command(&mismatch, &Responder::default())
        .await
        .unwrap();
    adapter
        .handle_command(
            &interaction("101", "303", "32", Some("delete-everything")),
            &Responder::default(),
        )
        .await
        .unwrap();
    let responder = Responder {
        fail_ack: true,
        ..Responder::default()
    };
    assert_eq!(
        adapter
            .handle_command(&interaction("101", "303", "32", None), &responder)
            .await,
        Err(Error::Transport)
    );
    assert_eq!(repository.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn descriptor_has_bootstrap_controls_and_scoped_operation_groups() {
    let definition = serde_json::to_value(oracle_command()).unwrap();
    assert_eq!(definition["name"], "oracle");
    assert_eq!(definition["default_member_permissions"], "32");
    assert_eq!(definition["options"].as_array().unwrap().len(), 4);
    assert_eq!(
        definition["options"][1]["options"][0]["choices"],
        json!([{"name":"pause","value":"pause"},{"name":"resume","value":"resume"}])
    );
}

#[test]
fn published_command_matcher_accepts_real_model_and_refuses_drift() {
    let mut payload = serde_json::to_value(oracle_command()).unwrap();
    payload["id"] = json!("404");
    payload["application_id"] = json!("505");
    payload["guild_id"] = json!("101");
    payload["version"] = json!("606");
    let command: discord::Command = serde_json::from_value(payload.clone()).unwrap();
    assert!(command_matches(&command));
    payload["description"] = json!("A different registrar owns this command");
    let changed: discord::Command = serde_json::from_value(payload).unwrap();
    assert!(!command_matches(&changed));
}

#[tokio::test]
async fn already_cancelled_gateway_never_contacts_discord() {
    let (adapter, _) = setup();
    let stop = CancellationToken::new();
    stop.cancel();
    Arc::new(adapter)
        .run_gateway("invented.never.sent".parse().unwrap(), stop)
        .await
        .unwrap();
}

#[test]
fn gateway_tls_provider_is_unambiguous() {
    // Tokio-tungstenite uses this implicit provider selection on Gateway startup.
    // SQLx and reqwest must not enable conflicting rustls providers together.
    let _builder = rustls::ClientConfig::builder();
}

#[test]
fn published_command_matches_discord_omitted_empty_localizations() {
    // REST omits empty localization maps; serde restores these optional fields as null.
    let mut payload = json!({
        "id":"404", "application_id":"505", "guild_id":"101", "version":"606",
        "default_member_permissions":"32", "type":1, "name":"oracle",
        "description":"Oracle framework administration", "nsfw":false,
        "options":[
            {"type":1,"name":"status","description":"Show framework status"},
            {"type":1,"name":"control","description":"Pause or resume this server",
             "options":[{"type":3,"name":"action","description":"Requested control",
                "required":true,"choices":[{"name":"pause","value":"pause"},
                    {"name":"resume","value":"resume"}]}]}
        ]
    });
    let groups = interaction_ops::descriptors()
        .into_iter()
        .map(|g| serde_json::to_value(g).unwrap());
    payload["options"].as_array_mut().unwrap().extend(groups);
    fn omit_localizations(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("name_localizations");
                map.remove("description_localizations");
                for value in map.values_mut() {
                    omit_localizations(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    omit_localizations(value);
                }
            }
            _ => {}
        }
    }
    omit_localizations(&mut payload);
    let command: discord::Command = serde_json::from_value(payload.clone()).unwrap();
    assert!(command_matches(&command));
    payload["default_member_permissions"] = json!("0");
    let changed: discord::Command = serde_json::from_value(payload).unwrap();
    assert!(!command_matches(&changed));
}

#[derive(Default)]
struct OperationsCapture {
    requests: Mutex<
        Vec<(
            PolicyContext,
            GuildId,
            oracle_operations::ingress::OperationRequest,
        )>,
    >,
    token: Mutex<Option<CancellationToken>>,
    events: Arc<Mutex<Vec<String>>>,
    block: bool,
    result: Value,
}
#[async_trait]
impl oracle_operations::ingress::HumanOperations for OperationsCapture {
    async fn execute(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        request: oracle_operations::ingress::OperationRequest,
        cancel: &CancellationToken,
    ) -> core::Result<Value> {
        assert_eq!(
            self.events.lock().unwrap().first().map(String::as_str),
            Some("defer")
        );
        self.requests
            .lock()
            .unwrap()
            .push((context.clone(), guild.clone(), request));
        *self.token.lock().unwrap() = Some(cancel.clone());
        if self.block {
            cancel.cancelled().await;
            return Err(core::Error::new(ErrorCode::Cancelled));
        }
        Ok(self.result.clone())
    }
}
struct ExactResponder {
    events: Arc<Mutex<Vec<String>>>,
    result: Mutex<Option<Value>>,
    attachment: Mutex<Option<Vec<u8>>>,
}
#[async_trait]
impl InteractionResponder for ExactResponder {
    async fn defer_ephemeral(&self) -> Result<()> {
        self.events.lock().unwrap().push("defer".into());
        Ok(())
    }
    async fn complete(&self, message: &str) -> Result<()> {
        self.events.lock().unwrap().push(message.into());
        Ok(())
    }
    async fn reject_ephemeral(&self, message: &str) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(format!("reject:{message}"));
        Ok(())
    }
    async fn complete_result(&self, value: &Value) -> Result<()> {
        *self.result.lock().unwrap() = Some(value.clone());
        match interaction_ops::render(value)? {
            interaction_ops::ExactResponse::Text(text) => self.events.lock().unwrap().push(text),
            interaction_ops::ExactResponse::Attachment { filename, bytes } => {
                assert!(matches!(filename, "plan.json" | "result.json"));
                *self.attachment.lock().unwrap() = Some(bytes);
            }
        }
        Ok(())
    }
}
fn operation_interaction(group: &str, command: &str, args: Value) -> discord::CommandInteraction {
    let base = interaction("101", "303", "32", None);
    let mut value = serde_json::to_value(base).unwrap();
    value["data"]["options"] =
        json!([{"name":group,"type":2,"options":[{"name":command,"type":1,"options":args}]}]);
    // Deserialize the concrete Serenity command again, retaining authenticated member data.
    serde_json::from_str(&value.to_string()).unwrap()
}
fn operations_setup(
    result: Value,
    block: bool,
) -> (DiscordBootstrap, Arc<OperationsCapture>, ExactResponder) {
    let (base, _) = setup();
    let events = Arc::new(Mutex::new(vec![]));
    let operations = Arc::new(OperationsCapture {
        events: events.clone(),
        result,
        block,
        ..Default::default()
    });
    let bootstrap = DiscordBootstrap::with_operations(base.core, operations.clone());
    (
        bootstrap,
        operations,
        ExactResponder {
            events,
            result: Mutex::new(None),
            attachment: Mutex::new(None),
        },
    )
}
#[tokio::test]
async fn operations_authenticated_route_defer_and_exact_large_plan() {
    let expected = json!({"hash":"exact-plan-hash","steps":[{"description":"x".repeat(3000)}]});
    let (bootstrap, operations, responder) = operations_setup(expected.clone(), false);
    let interaction = operation_interaction(
        "structure",
        "approve",
        json!([{"name":"plan","type":3,"value":"plan123"},{"name":"hash","type":3,"value":"exact-plan-hash"}]),
    );
    bootstrap
        .handle_command(&interaction, &responder)
        .await
        .unwrap();
    let requests = operations.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        matches!(&requests[0].0,PolicyContext::Discord{guild,user,manage_guild:true} if guild.as_str()=="101"&&user.as_str()=="303")
    );
    assert!(
        matches!(&requests[0].2,oracle_operations::ingress::OperationRequest::Approve{plan,hash} if plan=="plan123"&&hash=="exact-plan-hash")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(responder.attachment.lock().unwrap().as_ref().unwrap())
            .unwrap(),
        expected
    );
}
#[tokio::test]
async fn operations_malformed_and_crossguild_never_execute() {
    for malformed in 0..3 {
        let (bootstrap, operations, responder) = operations_setup(json!({}), false);
        let mut command = operation_interaction("structure", "inspect", json!([]));
        match malformed {
            0 => command.member.as_mut().unwrap().guild_id = discord::GuildId::new(999),
            1 => command.guild_id = Some(discord::GuildId::new(999)),
            _ => {
                command = operation_interaction(
                    "module-config",
                    "plan",
                    json!([{"name":"module","type":3,"value":"logger"},{"name":"values","type":3,"value":"not json"}]),
                );
            }
        }
        bootstrap
            .handle_command(&command, &responder)
            .await
            .unwrap();
        assert!(operations.requests.lock().unwrap().is_empty());
        assert!(responder.events.lock().unwrap()[0].starts_with("reject:"));
    }
}
#[tokio::test]
async fn operations_timeout_cancels_authority() {
    let (bootstrap, operations, responder) = operations_setup(json!({}), true);
    bootstrap
        .handle_operation(
            &operation_interaction("structure", "inspect", json!([])),
            &responder,
            Duration::from_millis(5),
        )
        .await
        .unwrap();
    assert!(
        operations
            .token
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    assert!(
        responder
            .events
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("timed out")
    );
}
#[test]
fn operations_descriptor_and_config_parser() {
    let descriptor = serde_json::to_value(oracle_command()).unwrap();
    let groups = descriptor["options"].as_array().unwrap();
    assert!(groups.iter().any(|v| v["name"] == "structure"));
    assert!(groups.iter().any(|v| v["name"] == "module-config"));
    let command = operation_interaction(
        "module-config",
        "plan",
        json!([{"name":"module","type":3,"value":"community.activity-log"},{"name":"preset","type":3,"value":"moderate/v1"},{"name":"values","type":3,"value":"{\"destination\":\"123\"}"}]),
    );
    assert!(
        matches!(interaction_ops::parse(&command).unwrap().2,oracle_operations::ingress::OperationRequest::ConfigurationPlan{module,preset:Some(preset),values} if module.as_str()=="community.activity-log"&&preset=="moderate/v1"&&values==json!({"destination":"123"}))
    );
}

#[tokio::test]
async fn operations_configured_scope_blocks_other_authenticated_guild() {
    let (bootstrap, operations, responder) = operations_setup(json!({}), false);
    let mut command = operation_interaction("structure", "inspect", json!([]));
    command.guild_id = Some(discord::GuildId::new(999));
    command.member.as_mut().unwrap().guild_id = discord::GuildId::new(999);
    bootstrap
        .handle_command(&command, &responder)
        .await
        .unwrap();
    assert!(operations.requests.lock().unwrap().is_empty());
    let events = responder.events.lock().unwrap();
    assert_eq!(events[0], "defer");
    assert!(events[1].contains("ForbiddenScope"));
}

struct NoNotification;
#[async_trait]
impl oracle_modules::NotificationTransport for NoNotification {
    async fn send(
        &self,
        _: &oracle_modules::NotificationRequest,
        _: &oracle_modules::DispatchPermit,
        _: &dyn oracle_modules::NotificationCheck,
        _: CancellationToken,
    ) -> core::Result<Value> {
        Err(core::Error::new(ErrorCode::InvalidInput))
    }
}
#[tokio::test]
async fn gateway_coverage_is_unavailable_until_ready_and_clears_on_disconnect_and_shutdown() {
    let root = std::env::temp_dir().join(format!(
        "oracle-gateway-test-{}",
        core::OperationId::generate()
    ));
    std::fs::create_dir(&root).unwrap();
    let storage = Arc::new(
        oracle_storage::Storage::open(oracle_storage::DatabaseConfig::Sqlite {
            path: root.join("db"),
        })
        .await
        .unwrap(),
    );
    let (base, _) = setup();
    let modules = oracle_modules::ModuleManager::new(
        storage.clone(),
        base.core.clone(),
        root.join("modules"),
    )
    .unwrap();
    modules
        .set_event_services(
            ["guilds".into()].into_iter().collect(),
            Arc::new(NoNotification),
        )
        .unwrap();
    let names = [
        "guilds".into(),
        "guild_members".into(),
        "guild_moderation".into(),
    ]
    .into_iter()
    .collect();
    let bootstrap = Arc::new(
        DiscordBootstrap::with_runtime(
            base.core,
            Arc::new(OperationsCapture::default()),
            modules.clone(),
            names,
        )
        .unwrap(),
    );
    assert!(modules.event_intents().is_empty());
    assert_eq!(bootstrap.event_coverage()["connected"], false);
    let raw = json!({"op":0,"s":1,"t":"READY","d":{"v":10,"user":{"id":"505","username":"oracle","discriminator":"0","avatar":null,"bot":true},"guilds":[],"session_id":"fixture-session","resume_gateway_url":"wss://gateway.discord.gg","application":{"id":"505","flags":0}}});
    let discord::GatewayEvent::Dispatch { event, .. } =
        serde_json::from_str::<discord::GatewayEvent>(&raw.to_string()).unwrap()
    else {
        panic!("ready");
    };
    let ready =
        discord::FullEvent::from_event(event.into_event(), &mut None, &discord::Cache::default());
    bootstrap.observe_connection(&ready);
    assert_eq!(modules.event_intents().len(), 3);
    assert_eq!(bootstrap.event_coverage()["connected"], true);
    let stage: discord::ShardStageUpdateEvent =
        serde_json::from_str(r#"{"new":"Disconnected","old":"Connected","shard_id":0}"#).unwrap();
    // The public FullEvent is non-exhaustive; use the typed Event conversion.
    let disconnected = discord::FullEvent::from_event(
        Box::new(discord::Event::ShardStageUpdate(stage)),
        &mut None,
        &discord::Cache::default(),
    );
    bootstrap.observe_connection(&disconnected);
    assert!(modules.event_intents().is_empty());
    bootstrap.observe_connection(&ready);
    assert_eq!(modules.event_intents().len(), 3);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    bootstrap
        .clone()
        .run_gateway("invented.never.sent".parse().unwrap(), cancelled)
        .await
        .unwrap();
    assert!(modules.event_intents().is_empty());
    assert_eq!(bootstrap.event_coverage()["connected"], false);
    modules.shutdown().await.unwrap();
    storage.close().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

fn published_interaction(id: &str, args: Value) -> discord::CommandInteraction {
    let mut value = serde_json::to_value(interaction("101", "303", "32", None)).unwrap();
    value["data"] = json!({"id":id,"name":"activity-log","type":1,"options":[{"name":"probe","type":1,"options":args}]});
    serde_json::from_str(&value.to_string()).unwrap()
}
#[tokio::test]
async fn published_command_uses_authenticated_actor_and_exact_remote_id() {
    let expected = json!({"verified":true});
    let (bootstrap, operations, responder) = operations_setup(expected.clone(), false);
    let command = published_interaction(
        "999999999999999999",
        json!([{"name":"input","type":3,"value":"{\"actor\":\"attacker\",\"guild\":\"999\"}"}]),
    );
    assert!(
        bootstrap
            .handle_command(&command, &responder)
            .await
            .unwrap()
    );
    let requests = operations.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        matches!(&requests[0].0,PolicyContext::Discord{guild,user,manage_guild:true} if guild.as_str()=="101" && user.as_str()=="303")
    );
    assert_eq!(requests[0].1.as_str(), "101");
    assert!(
        matches!(&requests[0].2,oracle_operations::ingress::OperationRequest::InvokePublished{command_id,command_name,route,input} if command_id=="999999999999999999" && command_name=="activity-log" && route=="probe" && input==&json!({"actor":"attacker","guild":"999"}))
    );
    assert_eq!(responder.result.lock().unwrap().as_ref(), Some(&expected));
}
#[tokio::test]
async fn published_command_default_input_and_stale_id_are_preserved_for_registry_validation() {
    let (bootstrap, operations, responder) = operations_setup(json!({}), false);
    bootstrap
        .handle_command(&published_interaction("404", json!([])), &responder)
        .await
        .unwrap();
    assert!(
        matches!(&operations.requests.lock().unwrap()[0].2,oracle_operations::ingress::OperationRequest::InvokePublished{command_id,input,..} if command_id=="404" && input==&json!({}))
    );
}
#[tokio::test]
async fn malformed_published_commands_never_reach_operations() {
    for args in [
        json!([{"name":"module","type":3,"value":"other-module"}]),
        json!([{"name":"input","type":3,"value":"not-json"}]),
        json!([{"name":"input","type":3,"value":"{}"},{"name":"input","type":3,"value":"{}"}]),
        json!([{"name":"input","type":4,"value":1}]),
    ] {
        let (bootstrap, operations, responder) = operations_setup(json!({}), false);
        assert!(
            bootstrap
                .handle_command(&published_interaction("404", args), &responder)
                .await
                .unwrap()
        );
        assert!(operations.requests.lock().unwrap().is_empty());
        assert!(responder.events.lock().unwrap()[0].starts_with("reject:"));
    }
    let (bootstrap, operations, responder) = operations_setup(json!({}), false);
    let mut command = published_interaction("404", json!([]));
    command.member.as_mut().unwrap().user.id = discord::UserId::new(999);
    bootstrap
        .handle_command(&command, &responder)
        .await
        .unwrap();
    assert!(operations.requests.lock().unwrap().is_empty());
    assert!(responder.events.lock().unwrap()[0].starts_with("reject:"));
}
#[tokio::test]
async fn unrelated_published_commands_without_operations_are_not_claimed() {
    let (bootstrap, _) = setup();
    let (_, _, responder) = operations_setup(json!({}), false);
    assert!(
        !bootstrap
            .handle_command(&published_interaction("404", json!([])), &responder)
            .await
            .unwrap()
    );
    assert!(responder.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn published_commands_require_exactly_one_subcommand_and_share_cancellation() {
    for options in [
        json!([]),
        json!([{"name":"probe","type":1,"options":[]},{"name":"status","type":1,"options":[]}]),
        json!([{"name":"group","type":2,"options":[{"name":"probe","type":1,"options":[]}]}]),
    ] {
        let (bootstrap, operations, responder) = operations_setup(json!({}), false);
        let mut raw = serde_json::to_value(published_interaction("404", json!([]))).unwrap();
        raw["data"]["options"] = options;
        let command = serde_json::from_str(&raw.to_string()).unwrap();
        bootstrap
            .handle_command(&command, &responder)
            .await
            .unwrap();
        assert!(operations.requests.lock().unwrap().is_empty());
        assert!(responder.events.lock().unwrap()[0].starts_with("reject:"));
    }
    let (bootstrap, operations, responder) = operations_setup(json!({}), true);
    bootstrap
        .handle_operation(
            &published_interaction("404", json!([])),
            &responder,
            Duration::from_millis(5),
        )
        .await
        .unwrap();
    assert!(
        operations
            .token
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    assert!(
        responder
            .events
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("timed out")
    );
}
