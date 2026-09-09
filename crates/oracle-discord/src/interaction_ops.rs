//! Authenticated human operation parsing and exact response rendering.
use super::*;
use oracle_core::ModuleId;
use oracle_operations::ingress::OperationRequest;
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn is_operation(interaction: &discord::CommandInteraction) -> bool {
    interaction
        .data
        .options
        .first()
        .is_some_and(|option| matches!(option.name.as_str(), "structure" | "module-config"))
}
pub(super) fn parse(
    interaction: &discord::CommandInteraction,
) -> Result<(PolicyContext, GuildId, OperationRequest)> {
    let (context, guild) = authenticated_identity(interaction)?;
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
fn string(
    name: &'static str,
    description: &'static str,
    required: bool,
) -> discord::CreateCommandOption<'static> {
    discord::CreateCommandOption::new(discord::CommandOptionType::String, name, description)
        .required(required)
}
pub(super) fn descriptors() -> [discord::CreateCommandOption<'static>; 2] {
    let sub = |name, description| {
        discord::CreateCommandOption::new(discord::CommandOptionType::SubCommand, name, description)
    };
    [
        discord::CreateCommandOption::new(
            discord::CommandOptionType::SubCommandGroup,
            "structure",
            "Inspect and change server structure",
        )
        .add_sub_option(sub("inspect", "Inspect visible server structure"))
        .add_sub_option(
            sub("plan", "Plan exact server changes").add_sub_option(string(
                "request",
                "Structure request JSON",
                true,
            )),
        )
        .add_sub_option(
            sub("show", "Show the complete saved plan")
                .add_sub_option(string("plan", "Plan ID", true)),
        )
        .add_sub_option(
            sub("approve", "Approve the exact reviewed plan")
                .add_sub_option(string("plan", "Plan ID", true))
                .add_sub_option(string("hash", "Exact reviewed plan hash", true)),
        )
        .add_sub_option(
            sub("apply", "Apply a saved plan").add_sub_option(string("plan", "Plan ID", true)),
        ),
        discord::CreateCommandOption::new(
            discord::CommandOptionType::SubCommandGroup,
            "module-config",
            "Inspect and configure installed modules",
        )
        .add_sub_option(
            sub("inspect", "Inspect stored and effective configuration").add_sub_option(string(
                "module",
                "Module ID",
                true,
            )),
        )
        .add_sub_option(
            sub("plan", "Review a module configuration change")
                .add_sub_option(string("module", "Module ID", true))
                .add_sub_option(string("preset", "Published preset ID", false))
                .add_sub_option(string("values", "Configuration values JSON", false)),
        )
        .add_sub_option(
            sub("apply", "Apply the exact configuration plan")
                .add_sub_option(string("module", "Module ID", true))
                .add_sub_option(string("plan", "Plan ID", true)),
        )
        .add_sub_option(
            sub(
                "recover",
                "Recover an uncertain configuration acknowledgement",
            )
            .add_sub_option(string("module", "Module ID", true)),
        ),
    ]
}
