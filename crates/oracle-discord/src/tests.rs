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
fn descriptor_has_only_status_and_scoped_pause_resume() {
    let definition = serde_json::to_value(oracle_command()).unwrap();
    assert_eq!(definition["name"], "oracle");
    assert_eq!(definition["default_member_permissions"], "32");
    assert_eq!(definition["options"].as_array().unwrap().len(), 2);
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
    let command: discord::Command = serde_json::from_value(payload.clone()).unwrap();
    assert!(command_matches(&command));
    payload["default_member_permissions"] = json!("0");
    let changed: discord::Command = serde_json::from_value(payload).unwrap();
    assert!(!command_matches(&changed));
}
