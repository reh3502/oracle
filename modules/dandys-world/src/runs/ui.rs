//! Pure, bounded run cards. Page/cursor state never replaces saved draft data.
use super::{
    domain::{Actor, Command, Confirmation, Error, Run, RunMode, RunState},
    storage::{ConfirmationToken, StoredRun},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    View,
    SetCount,
    RemoveToon,
    SetHost,
    Rename,
    SetSchedule,
    Post,
    Repost,
    PrepareAll,
    ConfirmAll,
    PrepareCancel,
    ConfirmCancel,
    Join,
    Switch,
    ClearToon,
    Leave,
    Lock,
    Reopen,
    Complete,
    PrepareRemove,
    ConfirmRemove,
    Restore,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Summary,
    Toons,
    Toon,
    Host,
    Review,
    Players,
    Join,
    Switch,
    Manage,
    Remove,
    Bench,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub action: Action,
    pub id: String,
    #[serde(default)]
    pub view: View,
    pub page: Option<u8>,
    pub toon: Option<String>,
    pub count: Option<String>,
    pub name: Option<String>,
    pub starts_at: Option<String>,
    pub timezone: Option<String>,
    pub duration: Option<String>,
    pub member: Option<String>,
    pub expected_revision: Option<u64>,
    pub confirmation_interaction: Option<String>,
    pub confirmation_token: Option<String>,
}
impl Input {
    pub fn show(id: &str, view: View) -> Self {
        Self {
            action: Action::View,
            id: id.into(),
            view,
            page: None,
            toon: None,
            count: None,
            name: None,
            starts_at: None,
            timezone: None,
            duration: None,
            member: None,
            expected_revision: None,
            confirmation_interaction: None,
            confirmation_token: None,
        }
    }
    pub fn confirmation(&self) -> Result<Option<ConfirmationToken>, Error> {
        match (&self.confirmation_interaction, &self.confirmation_token) {
            (None, None) => Ok(None),
            (Some(interaction), Some(token))
                if super::domain::valid_member_id(interaction)
                    && token.len() == 32
                    && token.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                Ok(Some(ConfirmationToken {
                    interaction_id: interaction.clone(),
                    token: token.clone(),
                }))
            }
            _ => Err(Error::InvalidInput),
        }
    }
    pub fn command(&self) -> Result<Command, Error> {
        let dummy = || Confirmation {
            actor_id: String::new(),
            revision: 0,
        };
        let toon = || self.toon.clone().ok_or(Error::ToonRequired);
        Ok(match self.action {
            Action::SetCount => {
                let count = self.count.as_deref().ok_or(Error::InvalidInput)?;
                if count.len() != 1 || !matches!(count.as_bytes()[0], b'1'..=b'8') {
                    return Err(Error::InvalidInput);
                }
                Command::SetAllocation {
                    toon: toon()?,
                    count: count.as_bytes()[0] - b'0',
                }
            }
            Action::RemoveToon => Command::RemoveAllocation { toon: toon()? },
            Action::SetHost => Command::SetHostToon { toon: toon()? },
            Action::Rename => Command::Rename {
                name: self.name.clone().ok_or(Error::InvalidInput)?,
            },
            Action::Post => Command::Publish,
            Action::PrepareAll | Action::ConfirmAll => Command::SetMode {
                mode: RunMode::Casual,
                confirmation: None,
            },
            Action::PrepareCancel | Action::ConfirmCancel => Command::Cancel {
                confirmation: dummy(),
            },
            Action::Join => Command::Join {
                toon: self.toon.clone(),
            },
            Action::Switch => Command::Switch {
                toon: Some(toon()?),
            },
            Action::ClearToon => Command::Switch { toon: None },
            Action::Leave => Command::Leave,
            Action::Lock => Command::Lock,
            Action::Reopen => Command::Reopen,
            Action::Complete => Command::Complete,
            Action::Restore => Command::Restore {
                member_id: self.member.clone().ok_or(Error::InvalidInput)?,
                toon: self.toon.clone(),
            },
            Action::PrepareRemove | Action::ConfirmRemove => Command::Remove {
                member_id: self.member.clone().ok_or(Error::InvalidInput)?,
                confirmation: dummy(),
            },
            Action::View | Action::Repost | Action::SetSchedule => return Err(Error::InvalidInput),
        })
    }
    pub fn guarded(&self) -> bool {
        !matches!(
            self.action,
            Action::View | Action::Join | Action::Switch | Action::ClearToon | Action::Leave
        )
    }
    pub fn prepares(&self) -> bool {
        matches!(
            self.action,
            Action::PrepareAll | Action::PrepareCancel | Action::PrepareRemove
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicAction {
    pub name: String,
    pub label: String,
    pub operation: String,
    pub input: Value,
}
fn text(raw: &str, limit: usize) -> String {
    let mut count = 0;
    raw.chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '@' => '＠',
            '<' => '‹',
            '>' => '›',
            _ => c,
        })
        .take_while(|c| {
            count += c.len_utf16();
            count <= limit
        })
        .collect()
}
fn toon_name(run: &Run, toon: &str) -> String {
    text(
        run.eligibility
            .toons
            .get(toon)
            .map(String::as_str)
            .unwrap_or("Unknown Toon"),
        100,
    )
}
fn base(run: &Run, action: &str) -> Value {
    json!({"action":action,"id":run.id,"expected_revision":run.desired_card_revision})
}
fn button(label: &str, input: Value) -> Value {
    json!({"label":label,"operation":"run_ui","input":input})
}
fn view_button(run: &Run, label: &str, view: &str) -> Value {
    let mut input = base(run, "view");
    input["view"] = json!(view);
    button(label, input)
}
fn prompt(
    run: &Run,
    label: &str,
    action: &str,
    option: &str,
    field_label: &str,
    max: u16,
    extra: Option<(&str, &str)>,
) -> Value {
    let mut input = base(run, action);
    if let Some((key, value)) = extra {
        input[key] = json!(value);
    }
    let mut result = button(label, input);
    result["prompt"] = json!({"option":option,"label":text(field_label,45),"max_length":max});
    result
}
fn field(name: &str, value: String) -> Value {
    json!({"name":name,"value":if value.is_empty(){"None".into()}else{value},"inline":false})
}
fn shell(
    run: &Run,
    title: &str,
    description: String,
    mut fields: Vec<Value>,
    buttons: Vec<Value>,
    choices: Vec<Value>,
) -> Value {
    if !fields.iter().any(|field| field["name"] == "Starts") {
        fields.extend(schedule_fields(run));
    }
    json!({"card":{"title":format!("{} · {}",title,text(&run.name,160)),"description":description,"fields":fields,"footer":format!("Run {} · Saved setup survives expired controls",run.id)},"buttons":buttons,"choices":choices,"select_placeholder":"Choose a Toon…"})
}
fn schedule_fields(run: &Run) -> Vec<Value> {
    match &run.schedule {
        Some(schedule) => {
            let duration = schedule.duration_minutes;
            let duration = match (duration / 60, duration % 60) {
                (0, minutes) => format!("{minutes} min"),
                (hours, 0) => format!("{hours} h"),
                (hours, minutes) => format!("{hours} h {minutes} min"),
            };
            vec![
                field(
                    "Starts",
                    format!("<t:{}:F>\n<t:{}:R>", schedule.starts_at, schedule.starts_at),
                ),
                field("Estimated duration", duration),
            ]
        }
        None => vec![
            field("Starts", "Not set".into()),
            field("Estimated duration", "Not set".into()),
        ],
    }
}
fn schedule_button(run: &Run) -> Value {
    let mut button = prompt(
        run,
        "Set date & duration",
        "set_schedule",
        "starts_at",
        "Run start date and time",
        100,
        None,
    );
    button["prompt"]["placeholder"] =
        json!("YYYY-MM-DD HH:MM (24-hour) or paste a Hammertime timestamp");
    let mut zone = json!({"option":"timezone","label":"Time zone","max_length":100,"placeholder":"e.g. America/New_York, Europe/London, UTC"});
    let mut duration = json!({"option":"duration","label":"Estimated duration","max_length":40,"placeholder":"e.g. 90m or 1h 30m"});
    if let Some(schedule) = &run.schedule {
        let input_zone = schedule
            .timezone
            .as_deref()
            .unwrap_or("UTC")
            .parse::<chrono_tz::Tz>()
            .unwrap_or(chrono_tz::UTC);
        if schedule.starts_at % 60 != 0 {
            button["prompt"]["value"] = json!(format!("<t:{}:F>", schedule.starts_at));
        } else if let Some(start) = chrono::DateTime::from_timestamp(schedule.starts_at, 0) {
            button["prompt"]["value"] = json!(
                start
                    .with_timezone(&input_zone)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            );
        }
        zone["value"] = json!(schedule.timezone.as_deref().unwrap_or("UTC"));
        duration["value"] = json!(format!("{}m", schedule.duration_minutes));
    }
    button["prompt"]["additional_fields"] = json!([zone, duration]);
    button
}
fn setup_fields(run: &Run) -> Vec<Value> {
    let mut fields = schedule_fields(run);
    if let Some(allocations) = &run.allocations {
        for (toon, count) in allocations {
            fields.push(field(
                &toon_name(run, toon),
                format!("{count} place{}", if *count == 1 { "" } else { "s" }),
            ));
        }
        fields.push(field(
            "Your Toon",
            run.host_toon
                .as_ref()
                .map(|t| toon_name(run, t))
                .unwrap_or_else(|| "Choose a Toon from your required list".into()),
        ));
        fields.push(field(
            "Planned places",
            format!(
                "{} of 8 planning places, including the host",
                run.capacity()
            ),
        ));
    } else {
        fields.push(field(
            "Toon policy",
            "Any playable Toon. No required Toons or individual Toon limits.".into(),
        ));
        fields.push(field(
            "Player limit",
            "8 planning places, including the host".into(),
        ));
    }
    fields
}
fn players(run: &Run, actor: &Actor) -> Vec<Value> {
    let members:Vec<_>=run.assignments.iter().map(|(id,a)|json!({
        "user_id":id,
        "prefix":if id==&run.owner_id {"Host · "}else if id==&actor.user_id {"You · "}else{""},
        "suffix":format!(" — {}",a.toon.as_ref().map(|t|toon_name(run,t)).unwrap_or_else(||"Any Toon".into())),
    })).collect();
    if members.is_empty() {
        vec![field("Players", "No signups yet".into())]
    } else {
        vec![json!({"name":"Players","value":"","inline":false,"members":members})]
    }
}
/// A public projection is saved atomically with the roster, never inferred from
/// an acknowledged interaction. Identity/delivery belongs to the host journal.
pub fn public_projection(run: &Run) -> (Value, Vec<PublicAction>) {
    let state = match run.state {
        RunState::Draft => "Draft",
        RunState::Open => {
            if run.assignments.len() == usize::from(run.capacity()) {
                "Full"
            } else {
                "Open"
            }
        }
        RunState::Locked => "Locked",
        RunState::Completed => "Completed",
        RunState::Cancelled => "Cancelled",
    };
    let mode = if run.mode == RunMode::Organized {
        "Organized"
    } else {
        "Casual · Any Toon"
    };
    let mut card = json!({"title":text(&run.name,160),"description":format!("{} · {}\nRun {}\n{} / {} places filled · {} available",mode,state,run.id,run.assignments.len(),run.capacity(),usize::from(run.capacity()).saturating_sub(run.assignments.len())),"fields":[],"footer":"Join or view players to open your private controls"});
    let mut public_fields =
        vec![json!({"name":"Host","value":"","inline":false,"members":[{"user_id":run.owner_id}]})];
    if let Some(rows) = &run.allocations {
        public_fields.push(field(
            "Required Toons",
            rows.iter()
                .map(|(toon, count)| {
                    format!(
                        "{} · {} / {} filled · {} available",
                        toon_name(run, toon),
                        run.assignments
                            .values()
                            .filter(|a| a.toon.as_ref() == Some(toon))
                            .count(),
                        count,
                        usize::from(*count).saturating_sub(
                            run.assignments
                                .values()
                                .filter(|a| a.toon.as_ref() == Some(toon))
                                .count()
                        )
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    }
    if !run.bench.is_empty() {
        public_fields.push(field(
            "Bench",
            format!(
                "{} players · open View players for details",
                run.bench.len()
            ),
        ));
    }
    public_fields.extend(schedule_fields(run));
    card["fields"] = json!(public_fields);
    let mut actions = Vec::new();
    if !run.state.is_terminal() {
        if run.state == RunState::Open {
            actions.push(PublicAction {
                name: "join".into(),
                label: "Join".into(),
                operation: "run_ui".into(),
                input: json!({"action":"view","id":run.id,"view":"join"}),
            });
        }
        actions.push(PublicAction {
            name: "leave".into(),
            label: "Leave".into(),
            operation: "run_ui".into(),
            input: json!({"action":"leave","id":run.id}),
        });
        actions.push(PublicAction {
            name: "players".into(),
            label: "View players".into(),
            operation: "run_ui".into(),
            input: json!({"action":"view","id":run.id,"view":"players"}),
        });
    }
    (card, actions)
}

pub fn render(stored: &StoredRun, actor: &Actor, input: &Input, notice: Option<&str>) -> Value {
    let run = &stored.run;
    let manager = actor.user_id == run.owner_id || actor.manage_all_runs;
    let mut buttons = Vec::new();
    let mut choices = Vec::new();
    let mut fields = Vec::new();
    let mut description = notice.map(|s| format!("{s}\n\n")).unwrap_or_default();
    let title;
    if run.mode == RunMode::Casual && matches!(input.view, View::Toons | View::Toon | View::Host) {
        return render(stored, actor, &Input::show(&run.id, View::Review), notice);
    }
    let view = if input.view == View::Summary && run.state == RunState::Draft {
        if run.mode == RunMode::Organized {
            View::Toons
        } else {
            View::Review
        }
    } else {
        input.view
    };
    if matches!(
        view,
        View::Toons | View::Toon | View::Host | View::Review | View::Manage | View::Remove
    ) && !manager
    {
        return shell(run,"Run", "Only the host or a configured moderator can manage this run. Open your personal controls below.".into(),players(run,actor),vec![view_button(run,"My signup","summary")],vec![]);
    }
    match view {
        View::Toons => {
            if run.mode == RunMode::Casual {
                return render(stored, actor, &Input::show(&run.id, View::Review), notice);
            }
            title = "Choose required Toons";
            if run.state == RunState::Draft {
                description.push_str("Not posted yet · Organized\n");
            }
            fields = setup_fields(run);
            let mut available: Vec<_> = run.eligibility.toons.iter().collect();
            available.sort_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(b.0)));
            let page =
                usize::from(input.page.unwrap_or(0)).min(available.len().saturating_sub(1) / 25);
            description.push_str(&format!(
                "Page {} of {} · {} Toons total. Open Add or edit Toon to choose a Toon and its count. To see the other Toons, close the form and use Next Toons or Previous Toons below. Your choices stay saved.",
                page + 1,
                available.len().div_ceil(25),
                available.len()
            ));
            let options: Vec<_> = available
                .iter()
                .skip(page * 25)
                .take(25)
                .map(|(toon, name)| json!({"label":text(name,80),"value":toon}))
                .collect();
            if !options.is_empty() {
                let mut add = prompt(
                    run,
                    "Add or edit Toon",
                    "set_count",
                    "count",
                    "Number of places (1–8)",
                    1,
                    None,
                );
                add["input"]["page"] = json!(page);
                add["prompt"]["select"] = json!({"option":"toon","label":format!("Toon · page {} of {}",page+1,available.len().div_ceil(25)),"choices":options});
                buttons.push(add);
            }
            for (toon, count) in run.allocations.iter().flat_map(|a| a.iter()) {
                let mut choice = base(run, "view");
                choice["view"] = json!("toon");
                choice["toon"] = json!(toon);
                choice["page"] = json!(page);
                choices.push(json!({"label":text(&toon_name(run,toon),80),"description":format!("{count} required · edit or remove"),"operation":"run_ui","input":choice}));
            }
            for (label, next) in [
                ("Previous Toons", page.checked_sub(1)),
                (
                    "Next Toons",
                    (page + 1 < available.len().div_ceil(25)).then_some(page + 1),
                ),
            ] {
                if let Some(next) = next {
                    let mut nav = base(run, "view");
                    nav["view"] = json!("toons");
                    nav["page"] = json!(next);
                    buttons.push(button(label, nav));
                }
            }
            if run.state == RunState::Draft {
                buttons.push(view_button(run, "Choose my Toon", "host"));
                buttons.push(view_button(run, "Review", "review"));
                buttons.push(button("All Toons", base(run, "prepare_all")));
            } else {
                buttons.push(view_button(run, "Back", "manage"));
            }
        }
        View::Toon => {
            let Some(toon) = input
                .toon
                .as_ref()
                .filter(|t| run.eligibility.toons.contains_key(*t))
            else {
                return shell(
                    run,
                    "Choose a Toon",
                    "Choose a Toon from the saved list.".into(),
                    vec![],
                    vec![view_button(run, "Toon list", "toons")],
                    vec![],
                );
            };
            title = "Required Toon";
            description.push_str(&toon_name(run, toon));
            let count = run
                .allocations
                .as_ref()
                .and_then(|a| a.get(toon))
                .copied()
                .unwrap_or(0);
            fields.push(field("Current count", format!("{count} places")));
            let mut edit = prompt(
                run,
                "Set count",
                "set_count",
                "count",
                "Number of places (1–8)",
                1,
                Some(("toon", toon)),
            );
            edit["input"]["page"] = json!(input.page.unwrap_or(0));
            buttons.push(edit);
            if count > 0 {
                let mut remove = base(run, "remove_toon");
                remove["toon"] = json!(toon);
                remove["page"] = json!(input.page.unwrap_or(0));
                buttons.push(button("Remove Toon", remove));
                if run.state == RunState::Draft {
                    let mut own = base(run, "set_host");
                    own["toon"] = json!(toon);
                    buttons.push(button("Use as my Toon", own));
                }
            }
            let mut back = base(run, "view");
            back["view"] = json!("toons");
            back["page"] = json!(input.page.unwrap_or(0));
            buttons.push(button("Back to Toons", back));
        }
        View::Host => {
            title = "Choose your Toon";
            description.push_str(
                "Your place is included in the count. You join automatically when you post.",
            );
            if let Some(rows) = &run.allocations {
                for (toon, count) in rows {
                    let mut choice = base(run, "set_host");
                    choice["toon"] = json!(toon);
                    choices.push(json!({"label":text(&toon_name(run,toon),80),"description":format!("{count} allocated places, including yours"),"operation":"run_ui","input":choice}));
                }
            }
            if choices.is_empty() {
                description.push_str(" Add at least one required Toon first.");
            }
            buttons.push(view_button(run, "Back to Toons", "toons"));
        }
        View::Review => {
            title = if run.mode == RunMode::Casual {
                "Casual run · Any Toon"
            } else {
                "Review organized run"
            };
            fields = setup_fields(run);
            description.push_str("Not posted yet. Review your setup, then Post run. The host joins automatically. This is a planning roster; game access is not verified.");
            if run.state == RunState::Draft {
                buttons.push(schedule_button(run));
                if run.schedule.is_none() {
                    description
                        .push_str(" Set the date, time and estimated duration before posting.");
                }
                if run.schedule.is_some()
                    && (run.mode == RunMode::Casual
                        || run.host_toon.as_ref().is_some_and(|t| {
                            run.allocations.as_ref().is_some_and(|a| a.contains_key(t))
                        }))
                {
                    buttons.push(button("Post run", base(run, "post")));
                }
                if run.mode == RunMode::Organized {
                    buttons.push(view_button(run, "Edit Toons", "toons"));
                    buttons.push(view_button(run, "Choose my Toon", "host"));
                }
                buttons.push(prompt(
                    run, "Rename", "rename", "name", "Run name", 80, None,
                ));
                buttons.push(button("Cancel setup", base(run, "prepare_cancel")));
            } else {
                buttons.push(view_button(run, "Run details", "summary"));
            }
        }
        View::Join | View::Switch => {
            title = if view == View::Switch {
                "Switch your Toon"
            } else {
                "Join run"
            };
            if run.state != RunState::Open {
                description.push_str("This run is closed to new signups and Toon changes.");
                buttons.push(view_button(run, "Run details", "summary"));
            } else if view == View::Join && run.assignments.contains_key(&actor.user_id) {
                description
                    .push_str("You're already in this run. Use Switch Toon to change your choice.");
                buttons.push(view_button(run, "Switch Toon", "switch"));
                buttons.push(button("Leave", base(run, "leave")));
            } else if view == View::Join && run.assignments.len() >= usize::from(run.capacity()) {
                description.push_str("This run is full. Check again after someone leaves.");
                buttons.push(view_button(run, "View players", "players"));
                buttons.push(view_button(run, "Back", "summary"));
            } else {
                description.push_str(if run.mode == RunMode::Casual {
                    "Choose any playable Toon, or leave the choice empty. There are no individual Toon limits."
                } else if view == View::Switch {
                    "Only the host's selected Toons with open places are shown. Your current place stays saved if the new Toon is full."
                } else {
                    "Choose from the host's selected Toons with open places. Other Toons are not included in this organized run."
                });
                let mut available: Vec<_> = run
                    .eligibility
                    .toons
                    .iter()
                    .filter(|(toon, _)| {
                        run.allocations.as_ref().is_none_or(|a| {
                            a.get(*toon).is_some_and(|count| {
                                let occupied = run
                                    .assignments
                                    .values()
                                    .filter(|assignment| assignment.toon.as_ref() == Some(*toon))
                                    .count();
                                occupied < usize::from(*count)
                            })
                        })
                    })
                    .collect();
                available.sort_by(|a, b| a.1.cmp(b.1));
                let page = usize::from(input.page.unwrap_or(0))
                    .min(available.len().saturating_sub(1) / 25);
                if !available.is_empty() {
                    description.push_str(&format!(
                        "\nShowing Toons {}–{} of {} · Page {} of {}.",
                        page * 25 + 1,
                        ((page + 1) * 25).min(available.len()),
                        available.len(),
                        page + 1,
                        available.len().div_ceil(25)
                    ));
                    if available.len() > 25 {
                        description.push_str(" Use Next Toons or Previous Toons to see the rest.");
                    }
                }
                for (toon, name) in available.iter().skip(page * 25).take(25) {
                    let mut choice = base(
                        run,
                        if view == View::Switch {
                            "switch"
                        } else {
                            "join"
                        },
                    );
                    choice["toon"] = json!(toon);
                    choices.push(json!({"label":text(name,80),"description":if run.mode==RunMode::Casual{"Optional choice; no Toon quota".into()}else{let occupied=run.assignments.values().filter(|a|a.toon.as_ref()==Some(toon)).count();format!("{} places available",usize::from(run.allocations.as_ref().unwrap()[*toon]).saturating_sub(occupied))},"operation":"run_ui","input":choice}));
                }
                for (label, next) in [
                    ("Previous Toons", page.checked_sub(1)),
                    (
                        "Next Toons",
                        (page + 1 < available.len().div_ceil(25)).then_some(page + 1),
                    ),
                ] {
                    if let Some(next) = next {
                        let mut nav = base(run, "view");
                        nav["view"] = json!(view);
                        nav["page"] = json!(next);
                        buttons.push(button(label, nav));
                    }
                }
                if run.mode == RunMode::Casual {
                    buttons.push(button(
                        if view == View::Switch {
                            "Clear optional Toon"
                        } else {
                            "Join without a Toon"
                        },
                        base(
                            run,
                            if view == View::Switch {
                                "clear_toon"
                            } else {
                                "join"
                            },
                        ),
                    ));
                }
                buttons.push(view_button(run, "Back", "summary"));
            }
        }
        View::Players => {
            title = "Players";
            fields = players(run, actor);
            description.push_str(&format!(
                "{} / {} places filled",
                run.assignments.len(),
                run.capacity()
            ));
            if !run.bench.is_empty() {
                buttons.push(view_button(run, "View bench", "bench"));
            }
            buttons.push(view_button(run, "My signup", "summary"));
        }
        View::Manage => {
            title = "Manage run";
            description.push_str(
                "Management controls are private. Every action checks your current permission.",
            );
            fields = players(run, actor);
            fields.extend(schedule_fields(run));
            match run.state {
                RunState::Open => buttons.push(button("Lock signups", base(run, "lock"))),
                RunState::Locked => {
                    buttons.push(button("Reopen signups", base(run, "reopen")));
                    buttons.push(button("Complete run", base(run, "complete")));
                }
                _ => (),
            }
            if matches!(run.state, RunState::Open | RunState::Locked) {
                if run.mode == RunMode::Organized {
                    buttons.push(view_button(run, "Edit requirements", "toons"));
                }
                buttons.push(view_button(run, "Remove a signup", "remove"));
                if !run.bench.is_empty() {
                    buttons.push(view_button(run, "View bench", "bench"));
                }
                buttons.push(schedule_button(run));
                buttons.push(prompt(
                    run, "Rename", "rename", "name", "Run name", 80, None,
                ));
                buttons.push(button("Cancel run", base(run, "prepare_cancel")));
            }
            buttons.push(view_button(run, "Back", "summary"));
        }
        View::Bench => {
            title = "Bench";
            description.push_str(
                "These players missed the attendance check and do not occupy active places.",
            );
            let page =
                usize::from(input.page.unwrap_or(0)).min(run.bench.len().saturating_sub(1) / 8);
            let members:Vec<_>=run.bench.iter().skip(page*8).take(8).enumerate().map(|(i,(member,assignment))| {
                if manager && run.state==RunState::Open {
                    let mut choice=base(run,"restore");
                    choice["member"]=json!(member);
                    if let Some(toon)=&assignment.toon { choice["toon"]=json!(toon); }
                    choices.push(json!({"label":format!("Restore player {}",page*8+i+1),"member_id":member,
                        "description":assignment.toon.as_ref().map(|t|toon_name(run,t)).unwrap_or_else(||"Any Toon".into()),
                        "operation":"run_ui","input":choice}));
                }
                json!({"user_id":member,"suffix":assignment.toon.as_ref().map(|t|format!(" — {}",toon_name(run,t))).unwrap_or_default()})
            }).collect();
            if !members.is_empty() {
                fields.push(
                    json!({"name":"Benched players","value":"","inline":false,"members":members}),
                );
            }
            for (label, next) in [
                ("Previous", page.checked_sub(1)),
                (
                    "Next",
                    (page + 1 < run.bench.len().div_ceil(8)).then_some(page + 1),
                ),
            ] {
                if let Some(next) = next {
                    let mut b = view_button(run, label, "bench");
                    b["input"]["page"] = json!(next);
                    buttons.push(b);
                }
            }
            if manager && run.state == RunState::Locked {
                description.push_str(" Reopen signups in Manage run before restoring a player.");
            }
            buttons.push(view_button(run, "Back", "summary"));
        }
        View::Remove => {
            title = "Remove a signup";
            fields = players(run, actor);
            description.push_str("Choose a player, then review a private confirmation. Removing the host does not transfer ownership.");
            for (i, (member, assignment)) in run.assignments.iter().enumerate() {
                let mut choice = base(run, "prepare_remove");
                choice["member"] = json!(member);
                choices.push(json!({"label":if member==&run.owner_id{"Host".into()}else if member==&actor.user_id{"You".into()}else{format!("Player {}",i+1)},"description":assignment.toon.as_ref().map(|t|toon_name(run,t)).unwrap_or_else(||"Any Toon".into()),"member_id":member,"operation":"run_ui","input":choice}));
            }
            buttons.push(view_button(run, "Back", "manage"));
        }
        View::Summary => {
            title = match run.state {
                RunState::Open => "Open run",
                RunState::Locked => "Locked run",
                RunState::Completed => "Completed run",
                RunState::Cancelled => "Cancelled run",
                RunState::Draft => "Draft",
            };
            description.push_str(&format!(
                "{} / {} places filled. {}",
                run.assignments.len(),
                run.capacity(),
                if run.mode == RunMode::Casual {
                    "Any playable Toon; choosing one is optional."
                } else {
                    "Required Toon counts include the host."
                }
            ));
            fields = players(run, actor);
            if let Some(assignment) = run.assignments.get(&actor.user_id) {
                description.push_str(&format!(
                    "\nYou're signed up{}.",
                    assignment
                        .toon
                        .as_ref()
                        .map(|t| format!(" as {}", toon_name(run, t)))
                        .unwrap_or_default()
                ));
                if run.state == RunState::Open {
                    buttons.push(view_button(run, "Switch Toon", "switch"));
                }
                if !run.state.is_terminal() {
                    buttons.push(button("Leave", base(run, "leave")));
                }
            } else if run.state == RunState::Open {
                if run.mode == RunMode::Casual {
                    buttons.push(button("Join", base(run, "join")));
                } else {
                    buttons.push(view_button(run, "Join", "join"));
                }
            }
            if manager && !run.state.is_terminal() {
                buttons.push(view_button(run, "Manage", "manage"));
            }
            buttons.push(view_button(run, "View players", "players"));
        }
    }
    shell(run, title, description, fields, buttons, choices)
}

pub fn confirm(
    stored: &StoredRun,
    actor: &Actor,
    input: &Input,
    token: &ConfirmationToken,
    revision: u64,
) -> Value {
    if stored.run.desired_card_revision != revision {
        return render(
            stored,
            actor,
            &Input::show(&stored.run.id, View::Summary),
            Some("The run changed. Review the latest details and confirm again."),
        );
    }
    let (title, warning, action, label) = match input.action {
        Action::PrepareAll => (
            "Switch to Any Toon?",
            "This clears every required Toon count and your selected host Toon. The total player limit stays eight.",
            "confirm_all",
            "Use Any Toon",
        ),
        Action::PrepareRemove => (
            "Remove this signup?",
            "This removes the selected player's place. Ownership stays with the host.",
            "confirm_remove",
            "Remove signup",
        ),
        _ => (
            "Cancel this run?",
            "This ends the run. Existing signups are retained for the normal retention period.",
            "confirm_cancel",
            "Cancel run",
        ),
    };
    let mut yes = base(&stored.run, action);
    yes["confirmation_interaction"] = json!(token.interaction_id);
    yes["confirmation_token"] = json!(token.token);
    if let Some(member) = &input.member {
        yes["member"] = json!(member);
    }
    shell(
        &stored.run,
        title,
        warning.into(),
        setup_fields(&stored.run),
        vec![
            button(label, yes),
            view_button(&stored.run, "Keep current setup", "summary"),
        ],
        vec![],
    )
}

pub fn human_error(error: &super::storage::Error) -> String {
    use super::storage::Error as S;
    match error {
        S::Rule(Error::ScheduleRequired)=>"Set the run date, time and estimated duration, then review and post.".into(),
        S::Rule(Error::StartInPast)=>"That start time is in the past. Choose a future date and time.".into(),
        S::Rule(Error::Full)=>"That place filled before you joined. Choose another Toon or check again later. Your previous signup is unchanged.".into(),
        S::Rule(Error::AlreadyJoined)=>"You're already signed up. Choose Switch Toon to change your choice.".into(),
        S::Rule(Error::Closed)=>"This run is closed to that action. Open its details to see the current status.".into(),
        S::Rule(Error::Forbidden)=>"Only the host or a configured moderator can manage this run.".into(),
        S::Rule(Error::StaleRevision|Error::StaleConfirmation)=>"The run changed. Review the latest details and try again.".into(),
        S::Rule(Error::CatalogUnavailable|Error::CatalogChanged)=>"The current playable Toon list cannot confirm this setup. Your saved draft is safe; review it after the wiki data is refreshed.".into(),
        S::Rule(Error::Benched)=>"You are on the bench. Ask the host to restore your place.".into(),
        S::Rule(Error::BelowOccupancy)=>"Players already occupy those places. Remove or move their signups before reducing that count.".into(),
        S::Rule(Error::InvalidInput)=>"Check the entry: counts must be a single number from 1 to 8, and the name must be 1–80 characters.".into(),
        S::Busy|S::Conflict=>"Someone else updated the run. Try again; your saved signup has not been replaced.".into(),
        S::OwnerPublishedLimit{limit}=>format!("Not posted: you already have {limit} active runs, which is the per-host limit. Complete or cancel one of your open or locked runs, then press Post run here again. This draft is saved; you do not need to create another."),
        S::Limit=>"The run limit has been reached. Finish an active run, wait for older records to expire, or ask a moderator for help.".into(),
        S::NotFound=>"That run is no longer available. Use /dw runs to find an open run or /hostrun to start setup.".into(),
        S::Interaction=>"These controls are too old. Use /dw run with the run ID, or /hostrun to resume your draft.".into(),
        S::Rule(Error::InvalidTransition)=>"That action is no longer available. Open the latest run details to continue.".into(),
        S::Rule(Error::ModeImmutable)=>"A posted run keeps its mode. Start a new setup to use a different mode.".into(),
        S::Rule(Error::IneligibleToon|Error::UnallocatedToon)=>"Choose a Toon from this run’s list of available Toons.".into(),
        S::Rule(Error::ToonRequired)=>"Choose a Toon from the required list before joining or posting.".into(),
        S::Rule(Error::NotJoined)=>"Join this run before changing your Toon.".into(),
        S::Rule(Error::ConfirmationRequired)=>"Review the change and use the confirmation button to continue.".into(),
        S::Unavailable=>"Your saved run is temporarily unavailable. Try opening it again shortly.".into(),
        S::Corrupt=>"This saved run needs an operator’s help before it can be opened.".into(),
    }
}
