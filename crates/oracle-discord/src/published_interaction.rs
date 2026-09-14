//! Typed published commands remain separate from framework/operator parsing.
use super::*;
use oracle_core::member_read::MemberContext;
use oracle_operations::ingress::PublishedRequest;
use serde_json::{Map, Value};

pub(super) fn parse(
    interaction: &discord::CommandInteraction,
) -> Result<(PolicyContext, MemberContext, PublishedRequest)> {
    let (actor, guild) = authenticated_identity(interaction)?;
    let PolicyContext::Discord { user, .. } = &actor else {
        return Err(Error::InvalidInteraction);
    };
    fn name(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 32
            && s.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))
    }
    if !name(interaction.data.name.as_str()) {
        return Err(Error::InvalidInteraction);
    }
    let (route, options) = match interaction.data.options.as_ref() {
        [command]
            if matches!(
                command.value,
                discord::CommandDataOptionValue::SubCommand(_)
            ) =>
        {
            if !name(command.name.as_str()) {
                return Err(Error::InvalidInteraction);
            }
            let discord::CommandDataOptionValue::SubCommand(options) = &command.value else {
                unreachable!()
            };
            (command.name.to_string(), options.as_ref())
        }
        options => (String::new(), options),
    };
    if options.len() > 25 {
        return Err(Error::InvalidInteraction);
    }
    let mut values = Map::new();
    for option in options {
        if !name(option.name.as_str()) {
            return Err(Error::InvalidInteraction);
        }
        let value = match &option.value {
            discord::CommandDataOptionValue::String(value) if value.len() <= 16 * 1024 => {
                Value::String(value.to_string())
            }
            discord::CommandDataOptionValue::Integer(value) => Value::from(*value),
            discord::CommandDataOptionValue::Boolean(value) => Value::Bool(*value),
            _ => return Err(Error::InvalidInteraction),
        };
        if values.insert(option.name.to_string(), value).is_some() {
            return Err(Error::InvalidInteraction);
        }
    }
    UserId::new(interaction.data.id.to_string())?;
    let member = MemberContext {
        guild,
        user: user.clone(),
        channel: interaction.channel_id.to_string(),
        roles: interaction
            .member
            .as_ref()
            .ok_or(Error::InvalidInteraction)?
            .roles
            .iter()
            .map(ToString::to_string)
            .collect(),
        observed_at: std::time::Instant::now(),
    };
    Ok((
        actor,
        member,
        PublishedRequest {
            interaction_id: Some(interaction.id.to_string()),
            private_action: None,
            command_id: interaction.data.id.to_string(),
            command_name: interaction.data.name.to_string(),
            route,
            options: values,
            expected_binding: None,
            member_only: false,
        },
    ))
}

impl DiscordBootstrap {
    pub(super) async fn handle_published(
        &self,
        interaction: &discord::CommandInteraction,
        responder: &dyn InteractionResponder,
        timeout: Duration,
    ) -> Result<bool> {
        let (actor, mut member, request) = match parse(interaction) {
            Ok(value) => value,
            Err(_) => {
                responder
                    .reject_ephemeral("Oracle requires a valid guild command and member identity.")
                    .await?;
                return Ok(true);
            }
        };
        responder.defer_ephemeral().await?;
        if let Err(_error) = self.core.status(&actor, Some(&member.guild)).await {
            responder
                .complete("This command isn’t available to you here right now. Ask a server helper if you need a hand.")
                .await?;
            return Ok(true);
        }
        let Some(operations) = &self.operations else {
            return Err(Error::Core(ErrorCode::ModuleUnavailable));
        };
        let member_route = match tokio::time::timeout(
            timeout,
            operations.published_uses_member_identity(&actor, &member, &member.guild, &request),
        )
        .await
        {
            Ok(Ok(value)) => value,
            _ => {
                responder
                    .complete("This command is unavailable. Please try again.")
                    .await?;
                return Ok(true);
            }
        };
        if member_route && let Some(reader) = &self.published_reader {
            member = match reader.refresh_member(&member).await {
                Ok(member) => member,
                Err(_error) => {
                    responder
                        .complete("This command isn’t available to you here right now. Ask a server helper if you need a hand.")
                        .await?;
                    return Ok(true);
                }
            };
        }
        let cancel = CancellationToken::new();
        struct CancelOnDrop(CancellationToken);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _guard = CancelOnDrop(cancel.clone());
        let result = tokio::time::timeout(
            timeout,
            operations.execute_published(&actor, &member, &member.guild, request, &cancel),
        )
        .await;
        match result {
            Ok(Ok(reply)) => {
                if let Err(_error) = responder
                    .complete_published(&reply, &member, cancel.clone())
                    .await
                {
                    responder.complete("I couldn’t show that answer. Try the command again in a moment.").await?;
                }
            }
            Ok(Err(_error)) => {
                responder
                    .complete("This command isn’t available to you here right now. Ask a server helper if you need a hand.")
                    .await?
            }
            Err(_) => {
                cancel.cancel();
                responder
                    .complete("This query timed out. Please try again.")
                    .await?;
            }
        }
        Ok(true)
    }
}
