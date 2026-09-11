//! Bootstrap command descriptors and bounded smoke-test publication.
//! Durable command ownership and reconciliation live in oracle-operations.
use crate::{Error, Result, api};
use serenity::all as discord;

/// Only guild routes publish this descriptor; there is no global/bulk replacement API here.
pub fn oracle_command() -> discord::CreateCommand<'static> {
    let command = discord::CreateCommand::new("oracle")
        .description("Oracle framework administration")
        .kind(discord::CommandType::ChatInput)
        .default_member_permissions(discord::Permissions::MANAGE_GUILD)
        .add_option(discord::CreateCommandOption::new(
            discord::CommandOptionType::SubCommand,
            "status",
            "Show framework status",
        ))
        .add_option(
            discord::CreateCommandOption::new(
                discord::CommandOptionType::SubCommand,
                "control",
                "Pause or resume this server",
            )
            .add_sub_option(
                discord::CreateCommandOption::new(
                    discord::CommandOptionType::String,
                    "action",
                    "Requested control",
                )
                .required(true)
                .add_string_choice("pause", "pause")
                .add_string_choice("resume", "resume"),
            ),
        );
    descriptors()
        .into_iter()
        .fold(command, |command, option| command.add_option(option))
}
pub(super) fn command_matches(command: &discord::Command) -> bool {
    let Ok(expected) = serde_json::to_value(oracle_command()) else {
        return false;
    };
    let Ok(mut actual) = serde_json::to_value(command) else {
        return false;
    };
    // Discord omits empty localization maps in REST responses. The model
    // represents omission as None/null, whereas EditCommand emits empty maps.
    // Normalize only those optional maps, preserving all behavioral fields.
    fn normalize(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                for key in ["name_localizations", "description_localizations"] {
                    if object.get(key).is_some_and(serde_json::Value::is_null) {
                        object.insert(key.into(), serde_json::json!({}));
                    }
                }
                for value in object.values_mut() {
                    normalize(value);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    normalize(value);
                }
            }
            _ => {}
        }
    }
    normalize(&mut actual);
    described_fields_match(&expected, &actual)
}
fn described_fields_match(expected: &serde_json::Value, actual: &serde_json::Value) -> bool {
    match (expected, actual) {
        (serde_json::Value::Object(expected), serde_json::Value::Object(actual)) => {
            expected.iter().all(|(key, value)| {
                actual
                    .get(key)
                    .is_some_and(|actual| described_fields_match(value, actual))
            })
        }
        (serde_json::Value::Array(expected), serde_json::Value::Array(actual)) => {
            expected.len() == actual.len()
                && expected
                    .iter()
                    .zip(actual)
                    .all(|(expected, actual)| described_fields_match(expected, actual))
        }
        _ => expected == actual,
    }
}
pub struct PublishedCommand {
    guild: discord::GuildId,
    id: discord::CommandId,
    created: bool,
    original: Option<serde_json::Value>,
}
impl PublishedCommand {
    pub fn id(&self) -> discord::CommandId {
        self.id
    }
    pub fn created(&self) -> bool {
        self.created
    }
}
pub async fn publish_guild_command(
    http: &discord::Http,
    guild: discord::GuildId,
) -> Result<PublishedCommand> {
    let before = api(http.get_guild_commands(guild)).await?;
    if let Some(existing) = before
        .iter()
        .find(|command| command.name.as_str() == "oracle")
    {
        if !command_matches(existing) {
            return Err(Error::CommandConflict);
        }
        return Ok(PublishedCommand {
            guild,
            id: existing.id,
            created: false,
            original: serde_json::to_value(existing).ok(),
        });
    }
    let created = api(http.create_guild_command(guild, &oracle_command())).await?;
    Ok(PublishedCommand {
        guild,
        id: created.id,
        created: true,
        original: serde_json::to_value(&created).ok(),
    })
}
/// Separate readback preserves the caller's cleanup receipt if observation fails.
pub async fn verify_published_command(
    http: &discord::Http,
    receipt: &PublishedCommand,
) -> Result<()> {
    let current = api(http.get_guild_commands(receipt.guild)).await?;
    if current
        .iter()
        .any(|command| command.id == receipt.id && command_matches(command))
    {
        Ok(())
    } else {
        Err(Error::CommandConflict)
    }
}
/// Bounded smoke-test cleanup only removes a command created by this publication call.
pub async fn cleanup_published_command(
    http: &discord::Http,
    receipt: &PublishedCommand,
) -> Result<()> {
    if !receipt.created {
        return Ok(());
    }
    let current = api(http.get_guild_commands(receipt.guild)).await?;
    let Some(command) = current.iter().find(|command| command.id == receipt.id) else {
        return Ok(());
    };
    if !command_matches(command)
        || receipt
            .original
            .as_ref()
            .is_none_or(|original| serde_json::to_value(command).ok().as_ref() != Some(original))
    {
        return Err(Error::CommandConflict);
    }
    api(http.delete_guild_command(receipt.guild, receipt.id)).await?;
    if api(http.get_guild_commands(receipt.guild))
        .await?
        .iter()
        .any(|command| command.id == receipt.id)
    {
        return Err(Error::CommandConflict);
    }
    Ok(())
}

fn string(
    name: &'static str,
    description: &'static str,
    required: bool,
) -> discord::CreateCommandOption<'static> {
    discord::CreateCommandOption::new(discord::CommandOptionType::String, name, description)
        .required(required)
}
pub(super) fn descriptors() -> [discord::CreateCommandOption<'static>; 3] {
    let sub = |name, description| {
        discord::CreateCommandOption::new(discord::CommandOptionType::SubCommand, name, description)
    };
    [
        discord::CreateCommandOption::new(
            discord::CommandOptionType::SubCommandGroup,
            "agent",
            "Ask and control the configured assistant",
        )
        .add_sub_option(
            sub("ask", "Start a scoped assistant run").add_sub_option(string(
                "goal",
                "Requested result",
                true,
            )),
        )
        .add_sub_option(
            sub("inspect", "Inspect a saved run").add_sub_option(string("run", "Run ID", true)),
        )
        .add_sub_option(sub("cancel", "Cancel a run").add_sub_option(string("run", "Run ID", true)))
        .add_sub_option(
            sub("resume", "Resume a saved run")
                .add_sub_option(string("run", "Run ID", true))
                .add_sub_option(string(
                    "clarification",
                    "Answer the requested clarification",
                    false,
                )),
        )
        .add_sub_option(
            sub("approve", "Approve an exact reviewed structure plan")
                .add_sub_option(string("run", "Run ID", true))
                .add_sub_option(string("plan", "Plan ID", true))
                .add_sub_option(string("hash", "Exact reviewed plan hash", true)),
        ),
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
