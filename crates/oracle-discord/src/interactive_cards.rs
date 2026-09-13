//! Host-owned, short-lived controls. Modules never receive Discord interaction tokens.
use super::*;
use oracle_core::member_read::MemberContext;
use oracle_operations::{
    executor::DispatchFence,
    ingress::{PublishedReply, PublishedRequest},
    published::{CardPresentation, CardPrompt},
};
use serde_json::{Map, Value, json};
use std::{collections::HashMap, sync::Mutex, time::Instant};
const TTL: Duration = Duration::from_secs(600);
const CAPACITY: usize = 256;
const EXPIRED: &str =
    "This card has expired or was already used. Run the command again to get a fresh one.";
#[derive(Clone)]
struct Action {
    back: bool,
    route: String,
    options: Map<String, Value>,
    prompt: Option<CardPrompt>,
}
#[derive(Clone)]
struct Session {
    member: MemberContext,
    command_id: String,
    command_name: String,
    binding: String,
    fence: Arc<dyn DispatchFence>,
    actions: HashMap<String, Action>,
    created: Instant,
    current: PublishedRequest,
    history: Vec<PublishedRequest>,
}
#[derive(Default)]
pub(super) struct Cards(Mutex<HashMap<String, Session>>);
impl Cards {
    fn insert(&self, session: Session) -> Result<String> {
        let mut entries = self.0.lock().map_err(|_| Error::Transport)?;
        entries.retain(|_, s| s.created.elapsed() < TTL);
        if entries
            .values()
            .filter(|s| s.member.user == session.member.user)
            .count()
            >= 32
            && let Some(oldest) = entries
                .iter()
                .filter(|(_, s)| s.member.user == session.member.user)
                .min_by_key(|(_, s)| s.created)
                .map(|(k, _)| k.clone())
        {
            entries.remove(&oldest);
        }
        if entries.len() >= CAPACITY
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, s)| s.created)
                .map(|(k, _)| k.clone())
        {
            entries.remove(&oldest);
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        entries.insert(id.clone(), session);
        Ok(id)
    }
    fn take(&self, id: &str, member: &MemberContext, action: &str) -> Result<(Session, Action)> {
        let mut entries = self.0.lock().map_err(|_| Error::Transport)?;
        entries.retain(|_, s| s.created.elapsed() < TTL);
        let s = entries.get(id).ok_or(Error::InvalidInteraction)?;
        if s.member.user != member.user
            || s.member.guild != member.guild
            || s.member.channel != member.channel
        {
            return Err(Error::InvalidInteraction);
        }
        let a = s
            .actions
            .get(action)
            .cloned()
            .ok_or(Error::InvalidInteraction)?;
        s.fence.dispatch(&mut || Ok(()))?;
        // Opening a modal leaves the original card usable if the child cancels it.
        let s = if a.prompt.is_some() && action != "input" {
            s.clone()
        } else {
            entries.remove(id).ok_or(Error::InvalidInteraction)?
        };
        Ok((s, a))
    }
    pub(super) fn payload(
        &self,
        card: &CardPresentation,
        reply: &PublishedReply,
        member: &MemberContext,
        request: &PublishedRequest,
    ) -> Result<Value> {
        self.payload_with_history(card, reply, member, request, Vec::new())
    }
    fn payload_with_history(
        &self,
        card: &CardPresentation,
        reply: &PublishedReply,
        member: &MemberContext,
        request: &PublishedRequest,
        mut history: Vec<PublishedRequest>,
    ) -> Result<Value> {
        if history.len() > 8 {
            history.drain(..history.len() - 8);
        }
        let has_back = !history.is_empty();
        let mut actions = HashMap::new();
        for (i, b) in card.buttons.iter().enumerate() {
            actions.insert(
                format!("b{i}"),
                Action {
                    back: false,
                    route: b.route.clone(),
                    options: b.options.clone(),
                    prompt: b.prompt.clone(),
                },
            );
        }
        for (i, c) in card.choices.iter().enumerate() {
            actions.insert(
                format!("c{i}"),
                Action {
                    back: false,
                    route: c.route.clone(),
                    options: c.options.clone(),
                    prompt: None,
                },
            );
        }
        if has_back {
            actions.insert(
                "back".into(),
                Action {
                    back: true,
                    route: String::new(),
                    options: Map::new(),
                    prompt: None,
                },
            );
        }
        let mut rows = Vec::new();
        if !actions.is_empty() {
            let id = self.insert(Session {
                member: member.clone(),
                command_id: request.command_id.clone(),
                command_name: request.command_name.clone(),
                binding: reply.binding.clone().ok_or(Error::InvalidInteraction)?,
                fence: reply
                    .control_fence
                    .clone()
                    .ok_or(Error::InvalidInteraction)?,
                actions,
                created: Instant::now(),
                current: request.clone(),
                history,
            })?;
            if !card.choices.is_empty() {
                let options: Vec<_> = card
                    .choices
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let mut option = json!({"label":c.label,"value":format!("c{i}")});
                        if !c.description.is_empty() {
                            option["description"] = json!(c.description);
                        }
                        option
                    })
                    .collect();
                rows.push(json!({"type":1,"components":[{"type":3,"custom_id":format!("oc:{id}:select"),"placeholder": if card.choices.iter().all(|c| c.options.contains_key("field")) { "Choose a detail…" } else { "Choose a match…" },"min_values":1,"max_values":1,"options":options}]}));
            }
            for (chunk, buttons) in card.buttons.chunks(5).enumerate() {
                let buttons:Vec<_> = buttons.iter().enumerate().map(|(i,b)|json!({"type":2,"style":2,"label":b.label,"custom_id":format!("oc:{id}:b{}",chunk*5+i)})).collect();
                rows.push(json!({"type":1,"components":buttons}));
            }
            if has_back {
                rows.push(json!({"type":1,"components":[{"type":2,"style":2,"label":"Back","custom_id":format!("oc:{id}:back")}]}));
            }
        }
        Ok(
            json!({"content":"","embeds":[card.embed],"components":rows,"allowed_mentions":{"parse":[]},"attachments":[]}),
        )
    }
}
fn identity(
    guild: Option<discord::GuildId>,
    channel: discord::GenericChannelId,
    user: &discord::User,
    member: Option<&discord::Member>,
) -> Result<MemberContext> {
    let guild = guild.ok_or(Error::InvalidInteraction)?;
    let member = member.ok_or(Error::InvalidInteraction)?;
    if member.guild_id != guild || member.user.id != user.id || user.bot() {
        return Err(Error::InvalidInteraction);
    }
    Ok(MemberContext {
        guild: GuildId::new(guild.to_string())?,
        user: UserId::new(user.id.to_string())?,
        channel: channel.to_string(),
        roles: member.roles.iter().map(ToString::to_string).collect(),
        observed_at: Instant::now(),
    })
}
fn split(id: &str) -> Result<(&str, &str)> {
    let mut p = id.split(':');
    if p.next() != Some("oc") {
        return Err(Error::InvalidInteraction);
    }
    let a = p.next().ok_or(Error::InvalidInteraction)?;
    let b = p.next().ok_or(Error::InvalidInteraction)?;
    if p.next().is_some() {
        return Err(Error::InvalidInteraction);
    }
    Ok((a, b))
}
fn request(session: &Session, action: Action, input: Option<&str>) -> Result<PublishedRequest> {
    let mut options = action.options;
    match (action.prompt, input) {
        (Some(prompt), Some(value))
            if !value.trim().is_empty()
                && value.chars().count() <= usize::from(prompt.max_length) =>
        {
            options.insert(prompt.option, Value::String(value.trim().to_owned()));
        }
        (None, None) => {}
        _ => return Err(Error::InvalidInteraction),
    }
    Ok(PublishedRequest {
        command_id: session.command_id.clone(),
        command_name: session.command_name.clone(),
        route: action.route,
        options,
        expected_binding: Some(session.binding.clone()),
        member_only: true,
    })
}
// Back replays a saved query, never guesses a page offset from current results.
fn navigate(
    session: &Session,
    action: Action,
    input: Option<&str>,
) -> Result<(PublishedRequest, Vec<PublishedRequest>)> {
    let mut history = session.history.clone();
    if action.back {
        if input.is_some() {
            return Err(Error::InvalidInteraction);
        }
        let mut prior = history.pop().ok_or(Error::InvalidInteraction)?;
        prior.expected_binding = Some(session.binding.clone());
        prior.member_only = true;
        return Ok((prior, history));
    }
    let next = request(session, action, input)?;
    if input.is_some() {
        // A submitted question starts a new conversation card; its parent stays usable.
        history.clear();
    } else {
        history.push(session.current.clone());
        if history.len() > 8 {
            history.remove(0);
        }
    }
    Ok((next, history))
}
impl DiscordBootstrap {
    async fn run_card(
        &self,
        session: Session,
        action: Action,
        input: Option<&str>,
        member: MemberContext,
        application: u64,
        token: &str,
    ) -> Result<()> {
        let (request, history) = navigate(&session, action, input)?;
        session.fence.dispatch(&mut || Ok(()))?;
        let reader = self
            .published_reader
            .as_ref()
            .ok_or(Error::InvalidInteraction)?;
        let member = reader.refresh_member(&member).await?;
        let actor = PolicyContext::Discord {
            user: member.user.clone(),
            guild: member.guild.clone(),
            manage_guild: false,
        };
        let operations = self.operations.as_ref().ok_or(Error::InvalidInteraction)?;
        let cancel = CancellationToken::new();
        let _guard = cancel.clone().drop_guard();
        let reply = tokio::time::timeout(
            Duration::from_secs(15),
            operations.execute_published(&actor, &member, &member.guild, request.clone(), &cancel),
        )
        .await
        .map_err(|_| Error::Transport)??;
        let payload = match &reply.card {
            Some(card) => self
                .cards
                .payload_with_history(card, &reply, &member, &request, history)?,
            None => {
                json!({"content":reply.text.as_deref().unwrap_or("No answer yet. Try another question."),"embeds":[],"components":[],"allowed_mentions":{"parse":[]}})
            }
        };
        reader
            .send_member_payload(
                application,
                token,
                &payload,
                &member,
                reply.policy.as_ref().ok_or(Error::InvalidInteraction)?,
                reply.fence.ok_or(Error::InvalidInteraction)?,
                cancel,
            )
            .await?;
        Ok(())
    }
    pub(super) async fn handle_card_component(
        &self,
        i: &discord::ComponentInteraction,
        http: &discord::Http,
    ) -> Result<()> {
        if !i.data.custom_id.starts_with("oc:") {
            return Ok(());
        }
        let resolved = (|| {
            let member = identity(i.guild_id, i.channel_id, &i.user, i.member.as_deref())?;
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
            let (s, a) = self.cards.take(id, &member, key)?;
            Ok((member, s, a))
        })();
        let (member, session, action) = match resolved {
            Ok(v) => v,
            Err(_) => {
                api(i.create_response(
                    http,
                    discord::CreateInteractionResponse::Message(
                        discord::CreateInteractionResponseMessage::new()
                            .content(EXPIRED)
                            .ephemeral(true),
                    ),
                ))
                .await?;
                return Ok(());
            }
        };
        if let Some(prompt) = &action.prompt {
            let mut next = session;
            next.created = Instant::now();
            next.actions = HashMap::from([("input".to_owned(), action.clone())]);
            let id = self.cards.insert(next)?;
            let input = discord::CreateInputText::new(discord::InputTextStyle::Short, "question")
                .placeholder(prompt.placeholder.clone())
                .max_length(prompt.max_length)
                .required(true);
            return api(i.create_response(
                http,
                discord::CreateInteractionResponse::Modal(
                    discord::CreateModal::new(format!("oc:{id}:input"), "Ask a question")
                        .components(vec![discord::CreateModalComponent::Label(
                            discord::CreateLabel::input_text(prompt.label.clone(), input),
                        )]),
                ),
            ))
            .await;
        }
        api(i.create_response(http, discord::CreateInteractionResponse::Acknowledge)).await?;
        if self
            .run_card(
                session,
                action,
                None,
                member,
                i.application_id.get(),
                i.token.as_str(),
            )
            .await
            .is_err()
        {
            api(i.edit_response(
                http,
                discord::EditInteractionResponse::new()
                    .content("I couldn’t open that answer. Run the command again to try a fresh question.")
                    .embeds(vec![])
                    .components(vec![])
                    .allowed_mentions(discord::CreateAllowedMentions::new()),
            ))
            .await?;
        }
        Ok(())
    }
    pub(super) async fn handle_card_modal(
        &self,
        i: &discord::ModalInteraction,
        http: &discord::Http,
    ) -> Result<()> {
        if !i.data.custom_id.starts_with("oc:") {
            return Ok(());
        }
        let resolved = (|| {
            let member = identity(i.guild_id, i.channel_id, &i.user, i.member.as_deref())?;
            let (id, key) = split(i.data.custom_id.as_str())?;
            if key != "input" {
                return Err(Error::InvalidInteraction);
            }
            let [discord::ModalComponent::Label(label)] = i.data.components.as_ref() else {
                return Err(Error::InvalidInteraction);
            };
            let discord::LabelComponent::InputText(input) = &label.component else {
                return Err(Error::InvalidInteraction);
            };
            if input.custom_id.as_str() != "question" {
                return Err(Error::InvalidInteraction);
            }
            let (s, a) = self.cards.take(id, &member, key)?;
            request(&s, a.clone(), Some(input.value.as_str()))?;
            Ok((member, s, a, input.value.to_string()))
        })();
        let (member, session, action, input) = match resolved {
            Ok(v) => v,
            Err(_) => {
                api(i.create_response(
                    http,
                    discord::CreateInteractionResponse::Message(
                        discord::CreateInteractionResponseMessage::new()
                            .content(EXPIRED)
                            .ephemeral(true),
                    ),
                ))
                .await?;
                return Ok(());
            }
        };
        api(i.create_response(
            http,
            discord::CreateInteractionResponse::Defer(
                discord::CreateInteractionResponseMessage::new().ephemeral(true),
            ),
        ))
        .await?;
        if self
            .run_card(
                session,
                action,
                Some(&input),
                member,
                i.application_id.get(),
                i.token.as_str(),
            )
            .await
            .is_err()
        {
            api(i.edit_response(
                http,
                discord::EditInteractionResponse::new()
                    .content(
                        "I couldn’t answer that just now. Run the command again and try another question.",
                    )
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
            guild: GuildId::new("123").unwrap(),
            user: UserId::new("456").unwrap(),
            channel: "789".into(),
            roles: Default::default(),
            observed_at: Instant::now(),
        }
    }
    fn action() -> Action {
        Action {
            back: false,
            route: "lookup".into(),
            options: json!({"name":"toon:pebble","field":"health","page":2})
                .as_object()
                .unwrap()
                .clone(),
            prompt: None,
        }
    }
    fn session() -> Session {
        Session {
            member: member(),
            command_id: "1234".into(),
            command_name: "dw".into(),
            binding: "generation-7".into(),
            fence: Arc::new(Fence(true.into())),
            actions: HashMap::from([("b0".into(), action())]),
            created: Instant::now(),
            current: PublishedRequest {
                command_id: "1234".into(),
                command_name: "dw".into(),
                route: "lookup".into(),
                options: action().options,
                expected_binding: None,
                member_only: false,
            },
            history: Vec::new(),
        }
    }
    #[test]
    fn click_keeps_saved_query_and_binding_and_consumes_siblings() {
        let cards = Cards::default();
        let mut s = session();
        s.actions.insert("b1".into(), action());
        let id = cards.insert(s).unwrap();
        let (s, a) = cards.take(&id, &member(), "b0").unwrap();
        let r = request(&s, a, None).unwrap();
        assert_eq!(r.command_id, "1234");
        assert_eq!(r.command_name, "dw");
        assert_eq!(r.route, "lookup");
        assert_eq!(
            r.options,
            json!({"name":"toon:pebble","field":"health","page":2})
                .as_object()
                .unwrap()
                .clone()
        );
        assert!(r.member_only);
        assert_eq!(r.expected_binding.as_deref(), Some("generation-7"));
        assert!(cards.take(&id, &member(), "b1").is_err());
    }
    #[test]
    fn foreign_context_and_tampering_do_not_consume_owner_card() {
        let cards = Cards::default();
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
            assert!(cards.take(&id, &m, "b0").is_err());
        }
        assert!(cards.take(&id, &member(), "b100").is_err());
        assert!(cards.take("made-up", &member(), "b0").is_err());
        assert!(cards.take(&id, &member(), "b0").is_ok());
    }
    #[test]
    fn expired_and_revoked_cards_never_dispatch() {
        let cards = Cards::default();
        let mut expired = session();
        expired.created = Instant::now() - TTL;
        let id = cards.insert(expired).unwrap();
        assert!(cards.take(&id, &member(), "b0").is_err());
        let mut s = session();
        s.fence = Arc::new(Fence(false.into()));
        let id = cards.insert(s).unwrap();
        assert!(cards.take(&id, &member(), "b0").is_err());
        assert!(Cards::default().take(&id, &member(), "b0").is_err());
    }
    #[test]
    fn modal_accepts_only_bounded_input_for_declared_option() {
        let mut a = action();
        a.prompt = Some(CardPrompt {
            label: "Question".into(),
            option: "question".into(),
            placeholder: "Ask here".into(),
            max_length: 5,
        });
        assert!(request(&session(), a.clone(), None).is_err());
        assert!(request(&session(), a.clone(), Some(" ")).is_err());
        assert!(request(&session(), a.clone(), Some("123456")).is_err());
        let r = request(&session(), a.clone(), Some(" hello"));
        assert!(r.is_err());
        let r = request(&session(), a, Some("hey")).unwrap();
        assert_eq!(r.options["question"], "hey");
        assert_eq!(r.options["field"], "health");
        assert!(request(&session(), action(), Some("extra")).is_err());
    }
    #[test]
    fn cancelled_modal_keeps_parent_card_and_submit_is_one_shot() {
        let cards = Cards::default();
        let mut s = session();
        let mut a = action();
        a.prompt = Some(CardPrompt {
            label: "Question".into(),
            option: "question".into(),
            placeholder: "Ask here".into(),
            max_length: 100,
        });
        s.actions.insert("b0".into(), a.clone());
        let id = cards.insert(s).unwrap();
        assert!(cards.take(&id, &member(), "b0").is_ok());
        assert!(cards.take(&id, &member(), "b0").is_ok());
        let mut s = session();
        s.actions = HashMap::from([("input".into(), a)]);
        let id = cards.insert(s).unwrap();
        assert!(cards.take(&id, &member(), "input").is_ok());
        assert!(cards.take(&id, &member(), "input").is_err());
    }
    #[test]
    fn cache_is_bounded_per_child() {
        let cards = Cards::default();
        for _ in 0..100 {
            cards.insert(session()).unwrap();
        }
        assert_eq!(cards.0.lock().unwrap().len(), 32);
    }
    #[test]
    fn rich_payload_has_select_and_buttons_without_internal_ids_in_labels() {
        use oracle_operations::published::{CardButton, CardChoice};
        let card = CardPresentation {
            embed: json!({"title":"Which Dandy?"}),
            buttons: vec![CardButton {
                label: "Ask a question".into(),
                route: "ask".into(),
                options: Map::new(),
                prompt: None,
            }],
            choices: vec![CardChoice {
                label: "Dandy (Toon)".into(),
                description: "Playable character".into(),
                route: "lookup".into(),
                options: action().options,
            }],
        };
        let reply = PublishedReply {
            card: Some(card.clone()),
            binding: Some("binding".into()),
            control_fence: Some(Arc::new(Fence(true.into()))),
            value: Value::Null,
            text: None,
            policy: None,
            fence: None,
        };
        let req = request(&session(), action(), None).unwrap();
        let cards = Cards::default();
        let payload = cards.payload(&card, &reply, &member(), &req).unwrap();
        assert_eq!(payload["embeds"][0]["title"], "Which Dandy?");
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
        assert!(payload.get("flags").is_none());
        let select = &payload["components"][0]["components"][0];
        assert_eq!(select["type"], 3);
        assert_eq!(select["options"][0]["label"], "Dandy (Toon)");
        assert_eq!(select["options"][0]["value"], "c0");
        let (id, key) = split(select["custom_id"].as_str().unwrap()).unwrap();
        assert_eq!(key, "select");
        assert_eq!(id.len(), 32);
        let (s, a) = cards.take(id, &member(), "c0").unwrap();
        assert_eq!(request(&s, a, None).unwrap().options["field"], "health");
    }
    #[test]
    fn simultaneous_clicks_can_only_claim_one_action() {
        let cards = Arc::new(Cards::default());
        let id = cards.insert(session()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let cards = cards.clone();
                let id = id.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    cards.take(&id, &member(), "b0").is_ok()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| usize::from(t.join().unwrap()))
                .sum::<usize>(),
            1
        );
    }
    #[test]
    fn identity_rejects_wrong_guild_user_bots_and_missing_members() {
        let user: discord::User = serde_json::from_value(
            json!({"id":"456","username":"player","discriminator":"0","avatar":null}),
        )
        .unwrap();
        let mut member:discord::Member=serde_json::from_value(json!({"guild_id":"123","user":user,"roles":[],"joined_at":null,"deaf":false,"mute":false,"flags":0})).unwrap();
        let guild = Some(discord::GuildId::new(123));
        let channel = discord::GenericChannelId::new(789);
        assert!(identity(guild, channel, &user, Some(&member)).is_ok());
        member.guild_id = discord::GuildId::new(999);
        assert!(identity(guild, channel, &user, Some(&member)).is_err());
        member.guild_id = discord::GuildId::new(123);
        let mut stranger = user.clone();
        stranger.id = discord::UserId::new(999);
        assert!(identity(guild, channel, &stranger, Some(&member)).is_err());
        assert!(identity(None, channel, &user, Some(&member)).is_err());
        assert!(identity(guild, channel, &user, None).is_err());
        let bot: discord::User = serde_json::from_value(
            json!({"id":"456","username":"bot","discriminator":"0","avatar":null,"bot":true}),
        )
        .unwrap();
        assert!(identity(guild, channel, &bot, Some(&member)).is_err());
    }
    #[test]
    fn back_restores_exact_offsets_when_page_lengths_differ() {
        let mut s = session();
        s.current.route = "search".into();
        s.current.options = json!({"query":"Dandy","offset":0,"field":"health"})
            .as_object()
            .unwrap()
            .clone();
        let mut next = action();
        next.route = "search".into();
        next.options = json!({"query":"Dandy","offset":12,"field":"health"})
            .as_object()
            .unwrap()
            .clone();
        let (r, h) = navigate(&s, next.clone(), None).unwrap();
        s.current = r;
        s.history = h;
        next.options.insert("offset".into(), json!(17));
        let (r, h) = navigate(&s, next, None).unwrap();
        s.current = r;
        s.history = h;
        let back = Action {
            back: true,
            route: String::new(),
            options: Map::new(),
            prompt: None,
        };
        let (r, h) = navigate(&s, back.clone(), None).unwrap();
        assert_eq!(r.options["offset"], 12);
        assert_eq!(r.options["query"], "Dandy");
        assert_eq!(r.options["field"], "health");
        assert_eq!(r.route, "search");
        assert!(r.member_only);
        assert_eq!(r.expected_binding.as_deref(), Some("generation-7"));
        s.current = r;
        s.history = h;
        let (r, h) = navigate(&s, back.clone(), None).unwrap();
        assert_eq!(r.options["offset"], 0);
        assert!(h.is_empty());
        s.history = h;
        assert!(navigate(&s, back, None).is_err());
    }
    #[test]
    fn history_is_bounded_and_new_modal_question_starts_fresh() {
        let mut s = session();
        for page in 0..20 {
            let mut a = action();
            a.options.insert("page".into(), json!(page));
            let (r, h) = navigate(&s, a, None).unwrap();
            s.current = r;
            s.history = h;
            assert!(s.history.len() <= 8);
        }
        assert_eq!(s.history.len(), 8);
        assert_eq!(s.history[0].options["page"], 11);
        let mut a = action();
        a.prompt = Some(CardPrompt {
            label: "Question".into(),
            option: "question".into(),
            placeholder: "Ask here".into(),
            max_length: 100,
        });
        let (_, h) = navigate(&s, a, Some("How fast is Pebble?")).unwrap();
        assert!(h.is_empty());
    }
    #[test]
    fn back_is_visible_even_when_new_card_has_no_module_actions() {
        let cards = Cards::default();
        let card = CardPresentation {
            embed: json!({"title":"Answer"}),
            buttons: vec![],
            choices: vec![],
        };
        let reply = PublishedReply {
            card: Some(card.clone()),
            binding: Some("binding".into()),
            control_fence: Some(Arc::new(Fence(true.into()))),
            value: Value::Null,
            text: None,
            policy: None,
            fence: None,
        };
        let s = session();
        let payload = cards
            .payload_with_history(
                &card,
                &reply,
                &member(),
                &s.current,
                vec![s.current.clone()],
            )
            .unwrap();
        let button = &payload["components"][0]["components"][0];
        assert_eq!(button["label"], "Back");
        let (id, key) = split(button["custom_id"].as_str().unwrap()).unwrap();
        let (s, a) = cards.take(id, &member(), key).unwrap();
        assert!(a.back);
        assert!(navigate(&s, a, None).is_ok());
        assert!(cards.take(id, &member(), key).is_err());
    }
}
