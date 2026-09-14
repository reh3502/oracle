//! Reusable actor-bound protocol 1.2 controls. No member lease survives a response.
use super::*;
use oracle_core::member_read::MemberContext;
use oracle_operations::{
    executor::DispatchFence,
    ingress::{PrivateAction, PublishedReply, PublishedRequest},
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Mutex, time::Instant};
const TTL: Duration = Duration::from_secs(600);
#[derive(Clone)]
struct Action {
    operation: String,
    input: serde_json::Map<String, Value>,
    prompt: Option<oracle_core::PrivateCardPrompt>,
}
#[derive(Clone)]
struct Session {
    member: MemberContext,
    request: PublishedRequest,
    fence: Arc<dyn DispatchFence>,
    actions: HashMap<String, Action>,
    created: Instant,
}
#[derive(Default)]
pub(super) struct PrivateCards(Mutex<HashMap<String, Session>>);
impl PrivateCards {
    fn insert(&self, session: Session) -> Result<String> {
        let mut entries = self.0.lock().map_err(|_| Error::Transport)?;
        entries.retain(|_, s| s.created.elapsed() < TTL);
        if entries
            .values()
            .filter(|s| s.member.user == session.member.user)
            .count()
            >= 32
            && let Some(id) = entries
                .iter()
                .filter(|(_, s)| s.member.user == session.member.user)
                .min_by_key(|(_, s)| s.created)
                .map(|(id, _)| id.clone())
        {
            entries.remove(&id);
        }
        if entries.len() >= 256
            && let Some(id) = entries
                .iter()
                .min_by_key(|(_, s)| s.created)
                .map(|(id, _)| id.clone())
        {
            entries.remove(&id);
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        entries.insert(id.clone(), session);
        Ok(id)
    }
    fn get(&self, id: &str, key: &str, member: &MemberContext) -> Result<(Session, Action)> {
        let mut entries = self.0.lock().map_err(|_| Error::Transport)?;
        entries.retain(|_, s| s.created.elapsed() < TTL);
        let s = entries.get(id).ok_or(Error::InvalidInteraction)?;
        if s.member.user != member.user
            || s.member.guild != member.guild
            || s.member.channel != member.channel
        {
            return Err(Error::InvalidInteraction);
        }
        s.fence.dispatch(&mut || Ok(()))?;
        let action = s
            .actions
            .get(key)
            .cloned()
            .ok_or(Error::InvalidInteraction)?;
        Ok((s.clone(), action))
    }
    pub(super) fn payload(
        &self,
        card: &oracle_operations::published::PrivateCardPresentation,
        reply: &PublishedReply,
        member: &MemberContext,
        request: &PublishedRequest,
    ) -> Result<Value> {
        let mut actions = HashMap::new();
        for (i, b) in card.controls.buttons.iter().enumerate() {
            actions.insert(
                format!("b{i}"),
                Action {
                    operation: b.operation.clone(),
                    input: b.input.clone(),
                    prompt: b.prompt.clone(),
                },
            );
        }
        for (i, c) in card.controls.choices.iter().enumerate() {
            actions.insert(
                format!("c{i}"),
                Action {
                    operation: c.operation.clone(),
                    input: c.input.clone(),
                    prompt: None,
                },
            );
        }
        let mut request = request.clone();
        request.expected_binding = reply.binding.clone();
        request.private_action = None;
        request.interaction_id = None;
        let id = self.insert(Session {
            member: member.clone(),
            request,
            fence: reply
                .control_fence
                .clone()
                .ok_or(Error::InvalidInteraction)?,
            actions,
            created: Instant::now(),
        })?;
        let mut rows = Vec::new();
        if !card.controls.choices.is_empty() {
            let options: Vec<_> = card
                .controls
                .choices
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let mut v = json!({"label":c.label,"value":format!("c{i}")});
                    if !c.description.is_empty() {
                        v["description"] = json!(c.description);
                    }
                    v
                })
                .collect();
            rows.push(json!({"type":1,"components":[{"type":3,"custom_id":format!("op2:{id}:select"),"placeholder":if card.controls.select_placeholder.is_empty(){"Choose an option"}else{&card.controls.select_placeholder},"min_values":1,"max_values":1,"options":options}]}));
        }
        for (chunk, buttons) in card.controls.buttons.chunks(5).enumerate() {
            let buttons:Vec<_>=buttons.iter().enumerate().map(|(i,b)|json!({"type":2,"style":2,"label":b.label,"custom_id":format!("op2:{id}:b{}",chunk*5+i)})).collect();
            rows.push(json!({"type":1,"components":buttons}));
        }
        Ok(
            json!({"content":"","embeds":[card.embed],"components":rows,"allowed_mentions":{"parse":[]},"attachments":[]}),
        )
    }
}
fn split(id: &str) -> Result<(&str, &str)> {
    let mut p = id.split(':');
    if p.next() != Some("op2") {
        return Err(Error::InvalidInteraction);
    }
    let id = p.next().ok_or(Error::InvalidInteraction)?;
    let key = p.next().ok_or(Error::InvalidInteraction)?;
    if p.next().is_some() {
        return Err(Error::InvalidInteraction);
    }
    Ok((id, key))
}
fn request(
    s: &Session,
    a: Action,
    input: Option<(&str, Option<&str>)>,
    interaction_id: &str,
) -> Result<PublishedRequest> {
    let mut values = a.input;
    match (a.prompt, input) {
        (Some(p), Some((v, selected)))
            if !v.trim().is_empty() && v.encode_utf16().count() <= usize::from(p.max_length) =>
        {
            match (&p.select, selected) {
                (Some(select), Some(value)) if select.choices.iter().any(|c| c.value == value) => {
                    values.insert(select.option.clone(), Value::String(value.into()));
                }
                (None, None) => {}
                _ => return Err(Error::InvalidInteraction),
            }
            values.insert(p.option, Value::String(v.trim().into()));
        }
        (None, None) => {}
        _ => return Err(Error::InvalidInteraction),
    }
    let mut request = s.request.clone();
    request.member_only = false;
    request.interaction_id = Some(interaction_id.into());
    request.private_action = Some(PrivateAction {
        operation: a.operation,
        input: values,
    });
    Ok(request)
}
impl DiscordBootstrap {
    #[allow(clippy::too_many_arguments)] // Authenticated interaction and immutable control are separate authority inputs.
    async fn run_private(
        &self,
        s: Session,
        a: Action,
        input: Option<(&str, Option<&str>)>,
        member: MemberContext,
        interaction_id: &str,
        application: u64,
        token: &str,
    ) -> Result<()> {
        let request = request(&s, a, input, interaction_id)?;
        let reader = self
            .published_reader
            .as_ref()
            .ok_or(Error::InvalidInteraction)?;
        let member = reader.refresh_member(&member).await?;
        s.fence.dispatch(&mut || Ok(()))?;
        let actor = PolicyContext::Discord {
            user: member.user.clone(),
            guild: member.guild.clone(),
            manage_guild: false,
        };
        let cancel = CancellationToken::new();
        let _guard = cancel.clone().drop_guard();
        let reply = tokio::time::timeout(
            Duration::from_secs(15),
            self.operations
                .as_ref()
                .ok_or(Error::InvalidInteraction)?
                .execute_published(&actor, &member, &member.guild, request.clone(), &cancel),
        )
        .await
        .map_err(|_| Error::Transport)??;
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
                application,
                token,
                &payload,
                &member,
                reply
                    .mutation_policy
                    .as_ref()
                    .ok_or(Error::InvalidInteraction)?,
                reply.fence.ok_or(Error::InvalidInteraction)?,
                cancel,
            )
            .await?;
        Ok(())
    }
    pub(super) async fn handle_private_component(
        &self,
        i: &discord::ComponentInteraction,
        http: &discord::Http,
    ) -> Result<()> {
        let resolved = (|| {
            let member = super::interactive_cards::identity(
                i.guild_id,
                i.channel_id,
                &i.user,
                i.member.as_deref(),
            )?;
            let (id, key) = split(i.data.custom_id.as_str())?;
            let key = match (&i.data.kind, key) {
                (discord::ComponentInteractionDataKind::Button, k) if k.starts_with('b') => k,
                (discord::ComponentInteractionDataKind::StringSelect { values }, "select")
                    if values.len() == 1 =>
                {
                    values[0].as_str()
                }
                _ => return Err(Error::InvalidInteraction),
            };
            let (s, a) = self.cards.1.get(id, key, &member)?;
            Ok((member, s, a))
        })();
        let (member, s, a) =
            match resolved {
                Ok(v) => v,
                Err(_) => return api(i.create_response(
                    http,
                    discord::CreateInteractionResponse::Message(
                        discord::CreateInteractionResponseMessage::new()
                            .content(
                                "These controls have expired. Run /hostrun or /dw run to resume.",
                            )
                            .ephemeral(true),
                    ),
                ))
                .await,
            };
        if let Some(p) = &a.prompt {
            let mut child = s.clone();
            child.actions = HashMap::from([("input".into(), a.clone())]);
            let id = self.cards.1.insert(child)?;
            let input = discord::CreateInputText::new(discord::InputTextStyle::Short, "value")
                .placeholder(p.placeholder.clone())
                .max_length(p.max_length)
                .required(true);
            let mut components = Vec::new();
            if let Some(select) = &p.select {
                let options = select
                    .choices
                    .iter()
                    .map(|c| discord::CreateSelectMenuOption::new(c.label.clone(), c.value.clone()))
                    .collect::<Vec<_>>();
                components.push(discord::CreateModalComponent::Label(
                    discord::CreateLabel::select_menu(
                        select.label.clone(),
                        discord::CreateSelectMenu::new(
                            "selection",
                            discord::CreateSelectMenuKind::String {
                                options: options.into(),
                            },
                        )
                        .min_values(1)
                        .max_values(1)
                        .required(true),
                    ),
                ));
            }
            components.push(discord::CreateModalComponent::Label(
                discord::CreateLabel::input_text(p.label.clone(), input),
            ));
            return api(i.create_response(
                http,
                discord::CreateInteractionResponse::Modal(
                    discord::CreateModal::new(format!("op2:{id}:input"), p.label.clone())
                        .components(components),
                ),
            ))
            .await;
        }
        api(i.create_response(http, discord::CreateInteractionResponse::Acknowledge)).await?;
        if self
            .run_private(
                s,
                a,
                None,
                member,
                &i.id.to_string(),
                i.application_id.get(),
                i.token.as_str(),
            )
            .await
            .is_err()
        {
            api(i.edit_response(
                http,
                discord::EditInteractionResponse::new()
                    .content("That action could not finish. Try again or reopen /dw run.")
                    .allowed_mentions(discord::CreateAllowedMentions::new()),
            ))
            .await?;
        }
        Ok(())
    }
    pub(super) async fn handle_private_modal(
        &self,
        i: &discord::ModalInteraction,
        http: &discord::Http,
    ) -> Result<()> {
        let resolved = (|| {
            let member = super::interactive_cards::identity(
                i.guild_id,
                i.channel_id,
                &i.user,
                i.member.as_deref(),
            )?;
            let (id, key) = split(i.data.custom_id.as_str())?;
            if key != "input" {
                return Err(Error::InvalidInteraction);
            }
            let (s, a) = self.cards.1.get(id, key, &member)?;
            let mut text = None;
            let mut selected = None;
            for component in &i.data.components {
                let discord::ModalComponent::Label(label) = component else {
                    return Err(Error::InvalidInteraction);
                };
                match &label.component {
                    discord::LabelComponent::InputText(input)
                        if input.custom_id.as_str() == "value" && text.is_none() =>
                    {
                        text = Some(input.value.to_string())
                    }
                    discord::LabelComponent::SelectMenu(select)
                        if select.custom_id.as_str() == "selection"
                            && selected.is_none()
                            && matches!(select.kind, discord::SelectMenuKind::String { .. })
                            && select.values.len() == 1 =>
                    {
                        selected = Some(select.values[0].clone())
                    }
                    _ => return Err(Error::InvalidInteraction),
                }
            }
            let text = text.ok_or(Error::InvalidInteraction)?;
            request(
                &s,
                a.clone(),
                Some((&text, selected.as_deref())),
                &i.id.to_string(),
            )?;
            Ok((member, s, a, (text, selected)))
        })();
        let (member, s, a, input) =
            match resolved {
                Ok(v) => v,
                Err(_) => return api(i.create_response(
                    http,
                    discord::CreateInteractionResponse::Message(
                        discord::CreateInteractionResponseMessage::new()
                            .content(
                                "These controls have expired. Run /hostrun or /dw run to resume.",
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
        if self
            .run_private(
                s,
                a,
                Some((&input.0, input.1.as_deref())),
                member,
                &i.id.to_string(),
                i.application_id.get(),
                i.token.as_str(),
            )
            .await
            .is_err()
        {
            api(i.edit_response(
                http,
                discord::EditInteractionResponse::new()
                    .content("That action could not finish. Reopen /dw run to try again.")
                    .allowed_mentions(discord::CreateAllowedMentions::new()),
            ))
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fence(std::sync::atomic::AtomicBool);
    impl DispatchFence for Fence {
        fn dispatch(
            &self,
            send: &mut dyn FnMut() -> oracle_core::Result<()>,
        ) -> oracle_core::Result<()> {
            if self.0.load(Ordering::SeqCst) {
                send()
            } else {
                Err(oracle_core::Error::new(ErrorCode::ModuleUnavailable))
            }
        }
    }
    fn member() -> MemberContext {
        MemberContext {
            user: UserId::new("123").unwrap(),
            guild: GuildId::new("456").unwrap(),
            channel: "789".into(),
            roles: Default::default(),
            observed_at: Instant::now(),
        }
    }
    fn session() -> Session {
        Session {
            member: member(),
            request: PublishedRequest {
                interaction_id: None,
                private_action: None,
                expected_binding: Some("module/session/1/2".into()),
                member_only: false,
                command_id: "987".into(),
                command_name: "dw".into(),
                route: "run".into(),
                options: Default::default(),
            },
            fence: Arc::new(Fence(true.into())),
            actions: HashMap::from([(
                "b0".into(),
                Action {
                    operation: "run_ui".into(),
                    input: json!({"id":"ABCD1234","expected_revision":7,"toon":"toon:astro"})
                        .as_object()
                        .unwrap()
                        .clone(),
                    prompt: Some(oracle_core::PrivateCardPrompt {
                        option: "count".into(),
                        label: "Places".into(),
                        max_length: 1,
                        placeholder: "1–8".into(),
                        select: None,
                    }),
                },
            )]),
            created: Instant::now(),
        }
    }
    #[test]
    fn reusable_owner_controls_keep_pinned_inputs_and_use_new_interaction_identity() {
        let cards = PrivateCards::default();
        let id = cards.insert(session()).unwrap();
        for interaction in ["111", "222"] {
            let (s, a) = cards.get(&id, "b0", &member()).unwrap();
            let request = request(&s, a, Some(("2", None)), interaction).unwrap();
            assert_eq!(request.interaction_id.as_deref(), Some(interaction));
            let action = request.private_action.unwrap();
            assert_eq!(action.input["toon"], "toon:astro");
            assert_eq!(action.input["expected_revision"], 7);
            assert_eq!(action.input["count"], "2");
            assert!(!request.member_only);
        }
        assert!(cards.get(&id, "b0", &member()).is_ok());
    }
    #[test]
    fn combined_modal_accepts_only_offered_choices_and_keeps_revision() {
        let s = session();
        let mut a = s.actions["b0"].clone();
        a.input.remove("toon");
        a.prompt.as_mut().unwrap().select = Some(oracle_core::PrivateCardPromptSelect {
            option: "toon".into(),
            label: "Toon".into(),
            choices: vec![oracle_core::PrivateCardPromptOption {
                label: "Pebble".into(),
                value: "toon:pebble".into(),
            }],
        });
        for selected in [None, Some("toon:astro"), Some(""), Some("actor_id")] {
            assert!(request(&s, a.clone(), Some(("2", selected)), "111").is_err());
        }
        let accepted = request(&s, a.clone(), Some(("2", Some("toon:pebble"))), "111")
            .unwrap()
            .private_action
            .unwrap();
        assert_eq!(accepted.input["toon"], "toon:pebble");
        assert_eq!(accepted.input["count"], "2");
        assert_eq!(accepted.input["expected_revision"], 7);
        assert!(a.input.get("toon").is_none());
        assert!(
            request(
                &s,
                s.actions["b0"].clone(),
                Some(("2", Some("toon:pebble"))),
                "111"
            )
            .is_err()
        );
    }
    #[test]
    fn foreign_context_expiry_and_registry_revocation_fail_closed() {
        let cards = PrivateCards::default();
        let id = cards.insert(session()).unwrap();
        for m in [
            MemberContext {
                user: UserId::new("999").unwrap(),
                ..member()
            },
            MemberContext {
                guild: GuildId::new("999").unwrap(),
                ..member()
            },
            MemberContext {
                channel: "999".into(),
                ..member()
            },
        ] {
            assert!(cards.get(&id, "b0", &m).is_err());
        }
        assert!(cards.get(&id, "b0", &member()).is_ok());
        let mut s = session();
        s.created = Instant::now() - TTL;
        let id = cards.insert(s).unwrap();
        assert!(cards.get(&id, "b0", &member()).is_err());
        let mut s = session();
        s.fence = Arc::new(Fence(false.into()));
        let id = cards.insert(s).unwrap();
        assert!(cards.get(&id, "b0", &member()).is_err());
    }
    #[test]
    fn cancelled_modal_does_not_consume_parent_and_invalid_values_do_not_change_inputs() {
        let cards = PrivateCards::default();
        let id = cards.insert(session()).unwrap();
        let (s, a) = cards.get(&id, "b0", &member()).unwrap();
        for value in [None, Some(""), Some("12"), Some("😀")] {
            assert!(request(&s, a.clone(), value.map(|v| (v, None)), "111").is_err());
        }
        assert!(cards.get(&id, "b0", &member()).is_ok());
        assert_eq!(s.actions["b0"].input.get("count"), None);
    }
}
