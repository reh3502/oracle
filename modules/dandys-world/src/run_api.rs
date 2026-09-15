//! Authenticated run commands, private setup cards and durable publication requests.
use dandys_world_core::runs::{
    domain::{Actor, Command, EligibilitySnapshot, RunMode},
    storage::{ConfirmationToken, Limits, Request, RunService},
    ui::{self, Action, Input as UiInput, View},
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
    expected_revision: Option<u64>,
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
        Command::SetAllocation { toon: label, .. }
        | Command::RemoveAllocation { toon: label }
        | Command::SetHostToon { toon: label } => *label = toon(label, pinned)?,
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
    let service = RunService::with_limits(context.clone(), limits).map_err(rule)?;
    if operation == "run_ui" {
        let options: UiInput = decode(input)?;
        let mut stored = service
            .view(&actor, &options.id)
            .await
            .map_err(|error| rule(ui::human_error(&error)))?;
        if options.action == Action::View {
            return encode(json!({"reply":status_card(&context,&stored,&actor,&options).await}));
        }
        let mut command = if options.action == Action::Repost {
            None
        } else if options.action == Action::SetSchedule {
            let parsed = (|| {
                let start = dandys_world_core::runs::schedule::parse_start(
                    options.starts_at.as_deref().unwrap_or(""),
                    Some(
                        options
                            .timezone
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("America/New_York"),
                    ),
                )?;
                let duration = dandys_world_core::runs::schedule::parse_duration(
                    options.duration.as_deref().unwrap_or(""),
                )?;
                Ok::<_, dandys_world_core::runs::schedule::ScheduleError>((start, duration))
            })();
            let (start, duration) = match parsed {
                Ok(value) => value,
                Err(error) => {
                    let view =
                        if stored.run.state == dandys_world_core::runs::domain::RunState::Draft {
                            View::Review
                        } else {
                            View::Manage
                        };
                    return encode(
                        json!({"reply":ui::render(&stored,&actor,&UiInput::show(&options.id,view),Some(&error.to_string()))}),
                    );
                }
            };
            Some(Command::SetSchedule {
                schedule: dandys_world_core::runs::domain::RunSchedule {
                    starts_at: start.starts_at,
                    duration_minutes: duration,
                    timezone: start.timezone,
                },
            })
        } else {
            Some(
                options
                    .command()
                    .map_err(|error| rule(ui::human_error(&error.into())))?,
            )
        };
        if let Some(command) = &mut command {
            resolve(command, &stored.run.eligibility)?;
        }
        let expected = if options.guarded() {
            Some(options.expected_revision.ok_or_else(invalid)?)
        } else {
            None
        };
        if expected.is_some_and(|revision| revision != stored.run.desired_card_revision) {
            return encode(
                json!({"reply":ui::render(&stored,&actor,&UiInput::show(&options.id,View::Summary),Some("The run changed. Review the latest details and try again."))}),
            );
        }
        let request = if options.action == Action::Repost {
            let status = context.shared_card_status(&options.id).await?;
            if status.state != oracle_module_sdk::SharedCardState::Missing {
                return encode(
                    json!({"reply":status_card(&context,&stored,&actor,&UiInput::show(&options.id,View::Summary)).await}),
                );
            }
            Request::Repost {
                run_id: options.id.clone(),
                expected_revision: expected.ok_or_else(invalid)?,
            }
        } else if options.prepares() {
            Request::Prepare {
                run_id: options.id.clone(),
                command: command.ok_or_else(invalid)?,
            }
        } else {
            Request::Change {
                run_id: options.id.clone(),
                command: command.ok_or_else(invalid)?,
                confirmation: options.confirmation().map_err(rule)?,
                expected_revision: expected,
            }
        };
        let marker = unavailable_catalog();
        let result = service
            .execute(
                &actor,
                &interaction,
                request,
                current.as_ref().unwrap_or(&marker),
                now,
            )
            .await;
        let publication_notice = if matches!(
            &result,
            Ok(dandys_world_core::runs::storage::Response::Applied { .. })
        ) {
            reconcile_one(&context, &service, &options.id).await
        } else {
            None
        };
        stored = service
            .view(&actor, &options.id)
            .await
            .map_err(|error| rule(ui::human_error(&error)))?;
        return match result {
            Ok(dandys_world_core::runs::storage::Response::Confirm {
                confirmation,
                revision,
                ..
            }) => {
                encode(json!({"reply":ui::confirm(&stored,&actor,&options,&confirmation,revision)}))
            }
            Ok(result) => {
                let view = match options.action {
                    Action::SetCount | Action::RemoveToon => View::Toons,
                    Action::SetHost => View::Review,
                    Action::Rename | Action::SetSchedule
                        if stored.run.state == dandys_world_core::runs::domain::RunState::Draft =>
                    {
                        View::Review
                    }
                    Action::Lock | Action::Reopen => View::Manage,
                    Action::Restore => View::Bench,
                    _ => View::Summary,
                };
                let mut next = UiInput::show(&options.id, view);
                next.page = options.page;
                let notice = if let Some(notice) = publication_notice.as_deref() {
                    notice
                } else if options.action == Action::Post {
                    "Run saved. Still checking the post."
                } else if stored.publication.is_some() {
                    "Your change is saved. The public card may still be catching up."
                } else {
                    "Saved."
                };
                encode(
                    json!({"result":result,"reply":ui::render(&stored,&actor,&next,Some(notice))}),
                )
            }
            Err(error) => encode(
                json!({"reply":ui::render(&stored,&actor,&UiInput::show(&options.id,View::Summary),Some(&ui::human_error(&error)))}),
            ),
        };
    }
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
            let stored = service
                .view(&actor, &options.id)
                .await
                .map_err(|error| rule(ui::human_error(&error)))?;
            let mut output = encode(&stored)?;
            output["reply"] = status_card(
                &context,
                &stored,
                &actor,
                &UiInput::show(&options.id, View::Summary),
            )
            .await;
            return Ok(output);
        }
        "run_list" | "run_list_page" => {
            let options: List = decode(input)?;
            let runs = service
                .list(&actor, options.after.as_deref())
                .await
                .map_err(rule)?;
            let choices:Vec<_>=runs.iter().map(|stored|json!({"label":stored.run.name,"description":format!("{} · {} / {} places",stored.run.id,stored.run.assignments.len(),stored.run.capacity()),"operation":"run_ui","input":{"action":"view","id":stored.run.id}})).collect();
            let buttons = if runs.len() == 25 {
                vec![
                    json!({"label":"Next runs","operation":"run_list_page","input":{"after":runs.last().unwrap().run.id}}),
                ]
            } else {
                vec![]
            };
            return encode(
                json!({"runs":runs,"reply":{"card":{"title":"Open runs","description":if choices.is_empty(){"No open runs yet. Use /hostrun organized or /hostrun casual to start one."}else{"Choose a run to open your private signup controls."},"fields":[],"footer":"Run IDs can also be used with /signup"},"choices":choices,"buttons":buttons,"select_placeholder":"Choose a run…"}}),
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
                expected_revision: options.expected_revision,
            }
        }
        "run_signup" => {
            let options: Signup = decode(input)?;
            let stored = service
                .view(&actor, &options.id)
                .await
                .map_err(|error| rule(ui::human_error(&error)))?;
            if (options.toon.is_none() && stored.run.mode == RunMode::Organized)
                || stored.run.assignments.contains_key(&actor.user_id)
            {
                return encode(
                    json!({"reply":ui::render(&stored,&actor,&UiInput::show(&options.id,View::Join),None)}),
                );
            }
            Request::Change {
                run_id: options.id,
                command: Command::Join { toon: options.toon },
                confirmation: None,
                expected_revision: None,
            }
        }
        "run_leave" => {
            let options: Id = decode(input)?;
            Request::Change {
                run_id: options.id,
                command: Command::Leave,
                confirmation: None,
                expected_revision: None,
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
    let unavailable = unavailable_catalog();
    let current = current.as_ref().unwrap_or(&unavailable);
    let result = service
        .execute(&actor, &interaction, request, current, now)
        .await
        .map_err(|error| rule(ui::human_error(&error)))?;
    let id = match &result {
        dandys_world_core::runs::storage::Response::Applied { result } => &result.run_id,
        dandys_world_core::runs::storage::Response::Confirm { run_id, .. } => run_id,
    };
    let publication_notice = if matches!(operation, "run_change" | "run_leave" | "run_signup") {
        reconcile_one(&context, &service, id).await
    } else {
        None
    };
    let stored = service.view(&actor, id).await.map_err(rule)?;
    let notice = if publication_notice.is_some() {
        publication_notice.as_deref()
    } else if operation.starts_with("run_create_") {
        Some("Your draft is saved. Continue setup below.")
    } else if stored.publication.is_some() {
        Some("Saved. The public card may still be catching up.")
    } else {
        None
    };
    let mut output = encode(result.clone())?;
    output["reply"] = ui::render(&stored, &actor, &UiInput::show(id, View::Summary), notice);
    Ok(output)
}
fn unavailable_catalog() -> EligibilitySnapshot {
    EligibilitySnapshot {
        source_hash: String::new(),
        source_revisions: Default::default(),
        toons: Default::default(),
        observed_at: 0,
        fresh_until: 0,
        disputed: true,
    }
}

pub async fn reconcile_one(
    context: &CallContext,
    service: &RunService<CallContext>,
    id: &str,
) -> Option<String> {
    let projection = service.projection(id).await.ok()?;
    if projection["intent"].is_null() {
        return None;
    }
    let revision = projection["expected_revision"].as_u64()?;
    let status =
        match context.shared_card_enqueue(id, revision).await {
            Ok(status) => status,
            Err(_) => return Some(
                "Your run is saved. Still checking the public post; use /dw run to check again."
                    .into(),
            ),
        };
    if status.state == oracle_module_sdk::SharedCardState::Confirmed
        && let Some(confirmed) = status.confirmed_revision
        && confirmed
            >= projection["intent"]["desired_revision"]
                .as_u64()
                .unwrap_or(u64::MAX)
    {
        let _ = service.release_confirmed(id, confirmed).await;
        return Some("Saved. The public card is up to date.".into());
    }
    Some(match status.state {
        oracle_module_sdk::SharedCardState::Missing=>"Your run is saved, but its public message was deleted. Ask the host to repost it.",
        oracle_module_sdk::SharedCardState::Rejected=>"Your run is saved, but the bot cannot post in the configured channel. Ask a moderator to check its permissions.",
        oracle_module_sdk::SharedCardState::RecoveryRequired=>"Your run is saved. The public post needs a moderator to check delivery before it can be retried.",
        _=>"Your run is saved. Still checking the public post.",
    }.into())
}

async fn status_card(
    context: &CallContext,
    stored: &dandys_world_core::runs::storage::StoredRun,
    actor: &Actor,
    input: &UiInput,
) -> Value {
    use oracle_module_sdk::SharedCardState;
    if stored.publication.is_none() {
        return ui::render(stored, actor, input, None);
    }
    let status = context.shared_card_status(&stored.run.id).await.ok();
    let message = match status.as_ref().map(|s| &s.state) {
        Some(SharedCardState::Confirmed)
            if status
                .as_ref()
                .and_then(|s| s.confirmed_revision)
                .is_some_and(|r| r >= stored.run.desired_card_revision) =>
        {
            "The public card is up to date."
        }
        Some(SharedCardState::Missing) => {
            "The public card is missing. The host or a moderator can explicitly repost it."
        }
        Some(SharedCardState::RecoveryRequired) => {
            "The saved run is safe. The host is checking an uncertain post; do not post another copy."
        }
        Some(SharedCardState::Rejected) => {
            "The run is saved. Public posting needs an operator to check the runs destination or permissions."
        }
        _ => "The run is saved. Still checking the public post.",
    };
    let mut reply = ui::render(stored, actor, input, Some(message));
    if !stored.run.state.is_terminal()
        && (actor.user_id == stored.run.owner_id || actor.manage_all_runs)
        && status.is_some_and(|s| s.state == SharedCardState::Missing)
        && let Some(buttons) = reply["buttons"].as_array_mut()
    {
        buttons.push(json!({"label":"Repost missing card","operation":"run_ui","input":{"action":"repost","id":stored.run.id,"expected_revision":stored.run.desired_card_revision}}));
    }
    reply
}
