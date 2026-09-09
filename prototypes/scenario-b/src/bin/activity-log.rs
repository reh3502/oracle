//! Separately installed P7 module fixture. All preset domain values live here.
use async_trait::async_trait;
use oracle_process_prototype::rpc::{RpcError, RpcHandler, RpcPeer};
use oracle_scenario_b::{Capabilities, Config, IDENTITY, SCOPE};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
fn moderate() -> Config {
    Config {
        preset: "moderate/v1".into(),
        enabled: [
            "moderation_audit",
            "channel_changes",
            "role_access_changes",
            "member_role_changes",
            "bans_unbans",
            "membership_summary",
        ]
        .map(str::to_owned)
        .to_vec(),
        excluded: [
            "message_create",
            "message_edit",
            "message_delete",
            "message_bodies",
            "attachments",
            "reactions",
            "typing",
            "presence",
            "routine_voice",
        ]
        .map(str::to_owned)
        .to_vec(),
        membership_summary_minutes: 15,
        retention_days: 14,
        retain_message_content: false,
        retain_attachments: false,
        self_origin_exclusion: true,
        coalesce: true,
        queue_limit: 128,
        dropped_event_summary: true,
        destination: String::new(),
        operator_note: String::new(),
    }
}
#[derive(Default)]
struct State {
    session: String,
    generation: u64,
    effective: Option<(u64, Config)>,
    prepared: Option<Config>,
    healthy: bool,
}
struct Logger(Mutex<State>);
#[async_trait]
impl RpcHandler for Logger {
    async fn handle(
        &self,
        _peer: RpcPeer,
        method: String,
        params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        let mut state = self.0.lock().await;
        match method.as_str() {
            "hello" => {
                return Ok(
                    json!({"identity":IDENTITY,"protocol":1,"build":env!("CARGO_PKG_VERSION")}),
                );
            }
            "initialize" => {
                state.session = params["session"]
                    .as_str()
                    .ok_or(RpcError::Protocol("session".into()))?
                    .into();
                state.generation = params["generation"]
                    .as_u64()
                    .ok_or(RpcError::Protocol("generation".into()))?;
                return Ok(json!({"initialized":true}));
            }
            "shutdown" => return Ok(json!({"shutdown":true})),
            _ => (),
        }
        if params["session"] != state.session
            || params["generation"] != state.generation
            || params["scope"] != SCOPE
        {
            return Err(RpcError::Remote("stale_or_foreign_envelope".into()));
        }
        let input = &params["input"];
        match method.as_str() {
            "describe" => Ok(
                json!({"module":IDENTITY,"generation":state.generation,"preset":moderate(),"tools":["logging_config_inspect_v1","logging_config_plan_v1","logging_config_apply_v1","logging_health_inspect_v1"],"required_capabilities":["guild_members","guilds","guild_moderation","view_audit_log"]}),
            ),
            "prepare" => {
                let config: Config = serde_json::from_value(input["config"].clone())
                    .map_err(|_| RpcError::Remote("config_schema".into()))?;
                let capabilities: Capabilities =
                    serde_json::from_value(input["capabilities"].clone())
                        .map_err(|_| RpcError::Remote("capability_schema".into()))?;
                if !(capabilities.guild_members
                    && capabilities.guilds
                    && capabilities.guild_moderation
                    && capabilities.view_audit_log)
                {
                    return Err(RpcError::Remote("moderate_missing_capability".into()));
                }
                let mut expected = moderate();
                expected.destination = config.destination.clone();
                expected.operator_note = config.operator_note.clone();
                if expected != config || config.destination.is_empty() {
                    return Err(RpcError::Remote("unsupported_preset_values".into()));
                }
                state.prepared = Some(config);
                Ok(json!({"prepared":true}))
            }
            "activate" => {
                let revision = input["revision"]
                    .as_u64()
                    .ok_or(RpcError::Protocol("revision".into()))?;
                let config: Config = serde_json::from_value(input["config"].clone())
                    .map_err(|_| RpcError::Remote("config_schema".into()))?;
                if state.prepared.as_ref() != Some(&config)
                    || state
                        .effective
                        .as_ref()
                        .is_some_and(|(effective, _)| *effective > revision)
                {
                    return Err(RpcError::Remote("unprepared_or_stale_activation".into()));
                }
                state.effective = Some((revision, config));
                state.healthy = input["unhealthy_subscriptions"] != true;
                if input["crash_before_ack"] == true {
                    std::process::exit(73)
                }
                Ok(json!({"activated":revision}))
            }
            "health" => Ok(match &state.effective {
                Some((revision, config)) => {
                    json!({"effective_revision":revision,"config":config,"subscriptions_healthy":state.healthy,"observation":"deterministic fixture subscriptions; no real Discord event observed"})
                }
                None => json!({"effective_revision":null,"subscriptions_healthy":false}),
            }),
            _ => Err(RpcError::Remote("unknown_logging_operation".into())),
        }
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    let logger = Arc::new(Logger(Mutex::new(State::default())));
    let peer = RpcPeer::new(tokio::io::stdin(), tokio::io::stdout(), logger);
    peer.wait_closed().await;
}
