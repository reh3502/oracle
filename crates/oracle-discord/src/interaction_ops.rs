//! Authenticated human operation parsing and exact response rendering.
use super::*;
use oracle_core::ModuleId;
use oracle_operations::ingress::OperationRequest;
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn is_operation(interaction: &discord::CommandInteraction) -> bool {
    interaction.data.options.first().is_some_and(|option| {
        matches!(
            option.name.as_str(),
            "structure" | "module-config" | "agent"
        )
    })
}
pub(super) fn parse(
    interaction: &discord::CommandInteraction,
) -> Result<(PolicyContext, GuildId, OperationRequest)> {
    let (context, guild) = authenticated_identity(interaction)?;
    if interaction.data.name.as_str() != "oracle" {
        return Ok((context, guild, parse_published(interaction)?));
    }
    let [group] = interaction.data.options.as_ref() else {
        return Err(Error::InvalidInteraction);
    };
    let discord::CommandDataOptionValue::SubCommandGroup(commands) = &group.value else {
        return Err(Error::InvalidInteraction);
    };
    let [command] = commands.as_ref() else {
        return Err(Error::InvalidInteraction);
    };
    let discord::CommandDataOptionValue::SubCommand(options) = &command.value else {
        return Err(Error::InvalidInteraction);
    };
    let mut args = BTreeMap::new();
    for option in options {
        let discord::CommandDataOptionValue::String(value) = &option.value else {
            return Err(Error::InvalidInteraction);
        };
        if value.len() > 16 * 1024
            || args
                .insert(option.name.to_string(), value.to_string())
                .is_some()
        {
            return Err(Error::InvalidInteraction);
        }
    }
    fn required(args: &mut BTreeMap<String, String>, key: &str) -> Result<String> {
        args.remove(key)
            .filter(|v| !v.is_empty())
            .ok_or(Error::InvalidInteraction)
    }
    fn module(args: &mut BTreeMap<String, String>) -> Result<ModuleId> {
        ModuleId::new(required(args, "module")?).map_err(|_| Error::InvalidInteraction)
    }
    fn json(value: &str) -> Result<Value> {
        serde_json::from_str(value).map_err(|_| Error::InvalidInteraction)
    }
    let request = match (group.name.as_str(), command.name.as_str()) {
        ("agent", action) => {
            use oracle_operations::ingress::AgentRequest;
            let request = match action {
                "ask" => AgentRequest::Ask {
                    goal: required(&mut args, "goal")?,
                },
                "inspect" => AgentRequest::Inspect {
                    run: required(&mut args, "run")?,
                },
                "cancel" => AgentRequest::Cancel {
                    run: required(&mut args, "run")?,
                },
                "resume" => AgentRequest::Resume {
                    run: required(&mut args, "run")?,
                    clarification: args.remove("clarification"),
                },
                "approve" => AgentRequest::Approve {
                    run: required(&mut args, "run")?,
                    plan: required(&mut args, "plan")?,
                    hash: required(&mut args, "hash")?,
                },
                _ => return Err(Error::InvalidInteraction),
            };
            OperationRequest::Agent { request }
        }
        ("structure", "inspect") => OperationRequest::Inspect,
        ("structure", "plan") => OperationRequest::Plan {
            request: serde_json::from_value(json(&required(&mut args, "request")?)?)
                .map_err(|_| Error::InvalidInteraction)?,
        },
        ("structure", "show") => OperationRequest::Show {
            plan: required(&mut args, "plan")?,
        },
        ("structure", "approve") => OperationRequest::Approve {
            plan: required(&mut args, "plan")?,
            hash: required(&mut args, "hash")?,
        },
        ("structure", "apply") => OperationRequest::Apply {
            plan: required(&mut args, "plan")?,
        },
        ("module-config", "inspect") => OperationRequest::ConfigurationInspect {
            module: module(&mut args)?,
        },
        ("module-config", "plan") => OperationRequest::ConfigurationPlan {
            module: module(&mut args)?,
            preset: args.remove("preset"),
            values: args
                .remove("values")
                .map(|v| json(&v))
                .transpose()?
                .unwrap_or_else(|| serde_json::json!({})),
        },
        ("module-config", "apply") => OperationRequest::ConfigurationApply {
            module: module(&mut args)?,
            plan: required(&mut args, "plan")?,
        },
        ("module-config", "recover") => OperationRequest::ConfigurationRecover {
            module: module(&mut args)?,
        },
        _ => return Err(Error::InvalidInteraction),
    };
    if !args.is_empty() {
        return Err(Error::InvalidInteraction);
    }
    Ok((context, guild, request))
}
fn parse_published(interaction: &discord::CommandInteraction) -> Result<OperationRequest> {
    fn name(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 32
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
    }
    let command_id = interaction.data.id.to_string();
    UserId::new(&command_id).map_err(|_| Error::InvalidInteraction)?;
    let command_name = interaction.data.name.to_string();
    if !name(&command_name) {
        return Err(Error::InvalidInteraction);
    }
    let [command] = interaction.data.options.as_ref() else {
        return Err(Error::InvalidInteraction);
    };
    let discord::CommandDataOptionValue::SubCommand(options) = &command.value else {
        return Err(Error::InvalidInteraction);
    };
    if !name(command.name.as_str()) {
        return Err(Error::InvalidInteraction);
    }
    let input = match options.as_ref() {
        [] => serde_json::json!({}),
        [option] if option.name.as_str() == "input" => {
            let discord::CommandDataOptionValue::String(value) = &option.value else {
                return Err(Error::InvalidInteraction);
            };
            if value.len() > 16 * 1024 {
                return Err(Error::InvalidInteraction);
            }
            serde_json::from_str(value).map_err(|_| Error::InvalidInteraction)?
        }
        _ => return Err(Error::InvalidInteraction),
    };
    Ok(OperationRequest::InvokePublished {
        command_id,
        command_name,
        route: command.name.to_string(),
        input,
    })
}

pub(super) enum ExactResponse {
    Text(String),
    Attachment {
        filename: &'static str,
        bytes: Vec<u8>,
    },
}
pub(super) fn render(value: &Value) -> Result<ExactResponse> {
    let text = serde_json::to_string_pretty(value).map_err(|_| Error::Transport)?;
    if text.chars().count() <= 1800 {
        Ok(ExactResponse::Text(format!("```json\n{text}\n```")))
    } else {
        Ok(ExactResponse::Attachment {
            filename: if value.get("plan").is_some() || value.get("hash").is_some() {
                "plan.json"
            } else {
                "result.json"
            },
            bytes: text.into_bytes(),
        })
    }
}
