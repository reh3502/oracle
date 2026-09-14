//! Public controls authorize each click independently and return a private wizard.
use super::*;
use oracle_operations::ingress::SharedControlRequest;
impl DiscordBootstrap {
    pub(super) async fn handle_shared_component(
        &self,
        i: &discord::ComponentInteraction,
        http: &discord::Http,
    ) -> Result<()> {
        let resolved = (|| {
            if !matches!(i.data.kind, discord::ComponentInteractionDataKind::Button)
                || i.message.channel_id != i.channel_id
                || !i.message.author.bot()
                || i.message.webhook_id.is_some()
                || i.message
                    .guild_id
                    .is_some_and(|guild| Some(guild) != i.guild_id)
            {
                return Err(Error::InvalidInteraction);
            }
            let member = super::interactive_cards::identity(
                i.guild_id,
                i.channel_id,
                &i.user,
                i.member.as_deref(),
            )?;
            Ok(member)
        })();
        let member =
            match resolved {
                Ok(member) => member,
                Err(_) => return api(i.create_response(
                    http,
                    discord::CreateInteractionResponse::Message(
                        discord::CreateInteractionResponseMessage::new()
                            .content(
                                "These controls are unavailable. Use /dw run for fresh controls.",
                            )
                            .ephemeral(true),
                    ),
                ))
                .await,
            };
        api(i.create_response(
            http,
            discord::CreateInteractionResponse::Defer(
                discord::CreateInteractionResponseMessage::new().ephemeral(true),
            ),
        ))
        .await?;
        let cancel = CancellationToken::new();
        let _guard = cancel.clone().drop_guard();
        let action = async {
            let reader = self
                .published_reader
                .as_ref()
                .ok_or(Error::InvalidInteraction)?;
            if reader.bot_id().await? != i.message.author.id {
                return Err(Error::InvalidInteraction);
            }
            let member = reader.refresh_member(&member).await?;
            let actor = PolicyContext::Discord {
                user: member.user.clone(),
                guild: member.guild.clone(),
                manage_guild: false,
            };
            let operations = self.operations.as_ref().ok_or(Error::InvalidInteraction)?;
            let mut request = operations
                .resolve_shared(
                    &actor,
                    &member,
                    SharedControlRequest {
                        custom_id: i.data.custom_id.to_string(),
                        message_id: i.message.id.to_string(),
                        application_id: i.application_id.to_string(),
                        author_id: i.message.author.id.to_string(),
                        interaction_id: i.id.to_string(),
                    },
                    &cancel,
                )
                .await?;
            if request.expected_binding.is_none()
                || request.private_action.is_none()
                || request.member_only
            {
                return Err(Error::InvalidInteraction);
            }
            request.interaction_id = Some(i.id.to_string());
            let reply = operations
                .execute_published(&actor, &member, &member.guild, request.clone(), &cancel)
                .await?;
            let mut card = reply
                .private_card
                .clone()
                .ok_or(Error::InvalidInteraction)?;
            reader
                .resolve_card_members(&member.guild, &mut card, &cancel)
                .await?;
            let payload = self.cards.1.payload(&card, &reply, &member, &request)?;
            reader
                .send_mutation_payload(
                    i.application_id.get(),
                    i.token.as_str(),
                    &payload,
                    &member,
                    reply
                        .mutation_policy
                        .as_ref()
                        .ok_or(Error::InvalidInteraction)?,
                    reply.fence.ok_or(Error::InvalidInteraction)?,
                    cancel.clone(),
                )
                .await?;
            Ok::<(), Error>(())
        };
        if !matches!(
            tokio::time::timeout(Duration::from_secs(20), action).await,
            Ok(Ok(()))
        ) {
            cancel.cancel();
            api(i.edit_response(
                http,
                discord::EditInteractionResponse::new()
                    .content(
                        "That run’s controls are unavailable right now. Use /dw run to reopen it.",
                    )
                    .embeds(vec![])
                    .components(vec![])
                    .allowed_mentions(discord::CreateAllowedMentions::new()),
            ))
            .await?;
        }
        Ok(())
    }
}
