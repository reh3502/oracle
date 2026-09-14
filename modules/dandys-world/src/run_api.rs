//! Private host-bound run operations. No Discord command or UI publication here.
use dandys_world_core::runs::{
    domain::{Actor, Command, EligibilitySnapshot, RunMode},
    storage::{ConfirmationToken, Limits, Request, RunService},
};
use oracle_module_sdk::{CallContext, Result, RpcError};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

fn invalid() -> RpcError {
    RpcError::Remote("Invalid run options".into())
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| invalid())
}
fn encode(value: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| invalid())
}
fn rule(error: impl std::fmt::Display) -> RpcError {
    let text = error.to_string();
    RpcError::Remote(if text.contains("reached a run or receipt limit") {
        "Run storage capacity is reached (runs, receipts, or moderator audit). Close or clean up eligible records, wait for receipts to expire, or ask the run owner to manage it.".into()
    } else {
        text
    })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    name: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Id {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Signup {
    id: String,
    toon: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    after: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    id: String,
    command: Value,
    confirmation: Option<ConfirmationToken>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    id: String,
    command: Value,
}

pub fn command(mut value: Value) -> Result<Command> {
    let object = value.as_object_mut().ok_or_else(invalid)?;
    // Actual actor/revision comes from the stored host-bound challenge, never wire JSON.
    if object.contains_key("confirmation")
        || object.contains_key("actor")
        || object.contains_key("actor_id")
        || object.contains_key("revision")
    {
        return Err(invalid());
    }
    if matches!(
        object.get("action").and_then(Value::as_str),
        Some("cancel" | "remove")
    ) {
        object.insert("confirmation".into(), json!({"actor_id":"","revision":0}));
    }
    decode(value)
}
fn toon(label: &str, pinned: &EligibilitySnapshot) -> Result<String> {
    if pinned.toons.contains_key(label) {
        return Ok(label.to_owned());
    }
    let mut matching = pinned
        .toons
        .iter()
        .filter(|(_, name)| name.eq_ignore_ascii_case(label.trim()));
    let key = matching
        .next()
        .map(|(id, _)| id.clone())
        .ok_or_else(|| rule("That Toon is not in this run's approved catalog"))?;
    if matching.next().is_some() {
        return Err(invalid());
    }
    Ok(key)
}
fn resolve(command: &mut Command, pinned: &EligibilitySnapshot) -> Result<()> {
    match command {
        Command::EditDraft {
            allocations,
            host_toon,
            ..
        } => {
            if let Some(toon_name) = host_toon {
                *toon_name = toon(toon_name, pinned)?;
            }
            if let Some(rows) = allocations {
                *rows = resolve_rows(rows, pinned)?;
            }
        }
        Command::SetAllocations { allocations } => {
            *allocations = resolve_rows(allocations, pinned)?
        }
        Command::Join {
            toon: Some(toon_name),
        }
        | Command::Switch {
            toon: Some(toon_name),
        } => *toon_name = toon(toon_name, pinned)?,
        _ => {}
    }
    Ok(())
}
fn resolve_rows(
    rows: &std::collections::BTreeMap<String, u8>,
    pinned: &EligibilitySnapshot,
) -> Result<std::collections::BTreeMap<String, u8>> {
    let mut result = std::collections::BTreeMap::new();
    for (name, count) in rows {
        if result.insert(toon(name, pinned)?, *count).is_some() {
            return Err(invalid());
        }
    }
    Ok(result)
}

pub async fn invoke(
    context: CallContext,
    operation: &str,
    input: Value,
    current: Option<EligibilitySnapshot>,
    limits: Limits,
    now: u64,
) -> Result<Value> {
    let member = context
        .actor()
        .ok_or_else(|| rule("An authenticated guild member is required"))?;
    let actor = Actor {
        guild_id: context.guild().as_str().into(),
        user_id: member.user_id.as_str().into(),
        manage_all_runs: context.member_permissions().contains("manage_all_runs"),
    };
    let interaction = member.interaction_id.clone();
    let service = RunService::with_limits(context, limits).map_err(rule)?;
    let request = match operation {
        "run_create_organized" | "run_create_casual" => {
            let options: Create = decode(input)?;
            Request::Create {
                mode: if operation == "run_create_organized" {
                    RunMode::Organized
                } else {
                    RunMode::Casual
                },
                name: options.name,
            }
        }
        "run_view" => {
            let options: Id = decode(input)?;
            return encode(service.view(&actor, &options.id).await.map_err(rule)?);
        }
        "run_list" => {
            let options: List = decode(input)?;
            return encode(
                json!({"runs":service.list(&actor,options.after.as_deref()).await.map_err(rule)?}),
            );
        }
        "run_prepare" => {
            let options: Prepare = decode(input)?;
            Request::Prepare {
                run_id: options.id,
                command: command(options.command)?,
            }
        }
        "run_change" => {
            let options: Change = decode(input)?;
            Request::Change {
                run_id: options.id,
                command: command(options.command)?,
                confirmation: options.confirmation,
            }
        }
        "run_signup" => {
            let options: Signup = decode(input)?;
            Request::Change {
                run_id: options.id,
                command: Command::Join { toon: options.toon },
                confirmation: None,
            }
        }
        "run_leave" => {
            let options: Id = decode(input)?;
            Request::Change {
                run_id: options.id,
                command: Command::Leave,
                confirmation: None,
            }
        }
        _ => return Err(invalid()),
    };
    let mut request = request;
    if let Request::Change {
        run_id, command, ..
    }
    | Request::Prepare { run_id, command } = &mut request
    {
        let pinned = service
            .view(&actor, run_id)
            .await
            .map_err(rule)?
            .run
            .eligibility;
        resolve(command, &pinned)?;
    }
    // This deliberately invalid marker cannot authorize creation/publication or be
    // stored in a run. It lets the service replay an existing receipt first.
    let unavailable = EligibilitySnapshot {
        source_hash: String::new(),
        source_revisions: Default::default(),
        toons: Default::default(),
        observed_at: 0,
        fresh_until: 0,
        disputed: true,
    };
    let current = current.as_ref().unwrap_or(&unavailable);
    encode(
        service
            .execute(&actor, &interaction, request, current, now)
            .await
            .map_err(rule)?,
    )
}
