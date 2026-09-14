//! Pure, bounded run transitions. Authentication and the storage CAS are host concerns.
//! A failed transition never changes its input; a successful one changes one aggregate.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_PLAYERS: usize = 8;
pub const MAX_RUN_BYTES: usize = 32 * 1024;

/// Constructed from the authenticated invocation envelope, never operation JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    pub guild_id: String,
    pub user_id: String,
    pub manage_all_runs: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Organized,
    Casual,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Draft,
    Open,
    Locked,
    Completed,
    Cancelled,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

/// The approved catalog evidence is copied into a run and never refreshed in place.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EligibilitySnapshot {
    pub source_hash: String,
    pub source_revisions: BTreeMap<String, u64>,
    /// Stable Toon key -> name as reviewed at setup time.
    pub toons: BTreeMap<String, String>,
    pub observed_at: u64,
    pub fresh_until: u64,
    pub disputed: bool,
}

impl EligibilitySnapshot {
    pub fn validate(&self) -> Result<(), Error> {
        if self.source_hash.len() != 64
            || !self.source_hash.bytes().all(|b| b.is_ascii_hexdigit())
            || self.source_revisions.is_empty()
            || self.source_revisions.len() > 16
            || self
                .source_revisions
                .iter()
                .any(|(key, rev)| !bounded_text(key, 120) || *rev == 0)
            || self.toons.is_empty()
            || self.toons.len() > 80
            || self
                .toons
                .iter()
                .any(|(key, name)| !bounded_text(key, 100) || !bounded_text(name, 100))
            || self.fresh_until < self.observed_at
        {
            return Err(Error::InvalidInput);
        }
        Ok(())
    }

    pub fn require_fresh(&self, now: u64) -> Result<(), Error> {
        self.validate().map_err(|_| Error::CatalogUnavailable)?;
        if self.disputed || now < self.observed_at || now >= self.fresh_until {
            return Err(Error::CatalogUnavailable);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub toon: Option<String>,
    pub joined_at: u64,
}

/// A private confirmation is validated against its authenticated actor and the
/// aggregate revision. The transport must issue it only after private review.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Confirmation {
    pub actor_id: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub id: String,
    pub guild_id: String,
    pub owner_id: String,
    pub name: String,
    pub mode: RunMode,
    pub state: RunState,
    /// Casual runs have no quota map, including an empty map.
    pub allocations: Option<BTreeMap<String, u8>>,
    pub host_toon: Option<String>,
    pub assignments: BTreeMap<String, Assignment>,
    pub eligibility: EligibilitySnapshot,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_owner_edit_at: u64,
    pub terminal_at: Option<u64>,
    /// Advances on every actual change, including unpublished draft edits.
    pub desired_card_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    remote = "Self",
    tag = "action",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Command {
    EditDraft {
        name: Option<String>,
        allocations: Option<BTreeMap<String, u8>>,
        host_toon: Option<String>,
    },
    SetMode {
        mode: RunMode,
        confirmation: Option<Confirmation>,
    },
    Publish,
    Join {
        toon: Option<String>,
    },
    Switch {
        toon: Option<String>,
    },
    Leave,
    SetAllocations {
        allocations: BTreeMap<String, u8>,
    },
    Rename {
        name: String,
    },
    Lock,
    Reopen,
    Complete,
    Cancel {
        confirmation: Confirmation,
    },
    Remove {
        member_id: String,
        confirmation: Confirmation,
    },
}

// Serde internally tagged unit variants ignore extra fields even when the enum
// denies unknown fields. Validate every command's object keys before decoding.
impl<'de> Deserialize<'de> for Command {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| D::Error::custom("command must be an object"))?;
        let action = object
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| D::Error::custom("missing action"))?;
        let allowed: &[&str] = match action {
            "edit_draft" => &["action", "name", "allocations", "host_toon"],
            "set_mode" => &["action", "mode", "confirmation"],
            "join" | "switch" => &["action", "toon"],
            "set_allocations" => &["action", "allocations"],
            "rename" => &["action", "name"],
            "cancel" => &["action", "confirmation"],
            "remove" => &["action", "member_id", "confirmation"],
            "publish" | "leave" | "lock" | "reopen" | "complete" => &["action"],
            _ => return Err(D::Error::custom("unknown command action")),
        };
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(D::Error::custom("unknown command field"));
        }
        Self::deserialize(value).map_err(D::Error::custom)
    }
}
impl Serialize for Command {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Self::serialize(self, serializer)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub changed: bool,
    pub state: RunState,
    pub card_revision: u64,
}

#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Error {
    #[error("invalid or oversized run input")]
    InvalidInput,
    #[error("this action requires the run owner or a configured moderator")]
    Forbidden,
    #[error("the run is closed for this action")]
    Closed,
    #[error("this action is not valid in the current run state")]
    InvalidTransition,
    #[error("published run mode cannot change")]
    ModeImmutable,
    #[error("a fresh, approved Toon catalog is required")]
    CatalogUnavailable,
    #[error("the selected Toon is not in the run's approved eligibility set")]
    IneligibleToon,
    #[error("the current catalog no longer supports a selected Toon; review setup")]
    CatalogChanged,
    #[error("select a required Toon before joining or publishing")]
    ToonRequired,
    #[error("that Toon is not allocated in this run")]
    UnallocatedToon,
    #[error("the run or selected Toon has no free place")]
    Full,
    #[error("already joined; use the explicit switch action to change Toon")]
    AlreadyJoined,
    #[error("join the run before switching Toon")]
    NotJoined,
    #[error("capacity cannot be reduced below existing occupancy")]
    BelowOccupancy,
    #[error("private confirmation is required")]
    ConfirmationRequired,
    #[error("the run or actor changed; review a fresh private confirmation")]
    StaleConfirmation,
}

pub fn normalize_run_id(value: &str) -> Result<String, Error> {
    let value = value.trim().to_ascii_uppercase();
    if value.len() != 8
        || !value
            .bytes()
            .all(|b| b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ".contains(&b))
    {
        return Err(Error::InvalidInput);
    }
    Ok(value)
}

pub fn valid_member_id(value: &str) -> bool {
    !value.starts_with('0')
        && value.len() <= 20
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|id| id > 0)
}

fn bounded_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= max && !value.chars().any(char::is_control)
}

impl Run {
    pub fn new(
        id: String,
        actor: &Actor,
        mode: RunMode,
        name: Option<String>,
        eligibility: EligibilitySnapshot,
        now: u64,
    ) -> Result<Self, Error> {
        eligibility.require_fresh(now)?;
        let run = Self {
            id: normalize_run_id(&id)?,
            guild_id: actor.guild_id.clone(),
            owner_id: actor.user_id.clone(),
            name: name.unwrap_or_else(|| "Dandy's World run".to_owned()),
            mode,
            state: RunState::Draft,
            allocations: (mode == RunMode::Organized).then(BTreeMap::new),
            host_toon: None,
            assignments: BTreeMap::new(),
            eligibility,
            created_at: now,
            updated_at: now,
            last_owner_edit_at: now,
            terminal_at: None,
            desired_card_revision: 1,
        };
        run.validate()?;
        Ok(run)
    }

    pub fn capacity(&self) -> u8 {
        match &self.allocations {
            Some(allocations) => allocations.values().copied().fold(0u8, u8::saturating_add),
            None => MAX_PLAYERS as u8,
        }
    }

    pub fn validate(&self) -> Result<(), Error> {
        self.eligibility.validate()?;
        if normalize_run_id(&self.id)? != self.id
            || !valid_member_id(&self.guild_id)
            || !valid_member_id(&self.owner_id)
            || !bounded_text(&self.name, 80)
            || self.desired_card_revision == 0
            || self.updated_at < self.created_at
            || self.last_owner_edit_at < self.created_at
            || self.last_owner_edit_at > self.updated_at
            || self.state.is_terminal() != self.terminal_at.is_some()
            || self
                .terminal_at
                .is_some_and(|t| t < self.created_at || t > self.updated_at)
            || self.assignments.len() > MAX_PLAYERS
            || (self.state == RunState::Draft && !self.assignments.is_empty())
        {
            return Err(Error::InvalidInput);
        }
        match (self.mode, &self.allocations) {
            (RunMode::Casual, None) if self.host_toon.is_none() => (),
            (RunMode::Organized, Some(allocations)) => {
                self.validate_allocations(allocations)?;
                if self.state != RunState::Draft
                    && self.state != RunState::Cancelled
                    && allocations.is_empty()
                {
                    return Err(Error::InvalidInput);
                }
                if let Some(toon) = &self.host_toon {
                    self.validate_toon(toon)?;
                }
            }
            _ => return Err(Error::InvalidInput),
        }
        for (member, assignment) in &self.assignments {
            if !valid_member_id(member)
                || assignment.joined_at < self.created_at
                || assignment.joined_at > self.updated_at
            {
                return Err(Error::InvalidInput);
            }
            self.validate_selection(assignment.toon.as_deref())?;
        }
        if serde_json::to_vec(self)
            .map_err(|_| Error::InvalidInput)?
            .len()
            > MAX_RUN_BYTES
        {
            return Err(Error::InvalidInput);
        }
        Ok(())
    }

    fn validate_toon(&self, toon: &str) -> Result<(), Error> {
        if !bounded_text(toon, 100) {
            return Err(Error::InvalidInput);
        }
        if !self.eligibility.toons.contains_key(toon) {
            return Err(Error::IneligibleToon);
        }
        Ok(())
    }

    fn validate_selection(&self, toon: Option<&str>) -> Result<(), Error> {
        match toon {
            Some(toon) => {
                self.validate_toon(toon)?;
                if self
                    .allocations
                    .as_ref()
                    .is_some_and(|a| !a.contains_key(toon))
                {
                    return Err(Error::UnallocatedToon);
                }
            }
            None if self.mode == RunMode::Organized => return Err(Error::ToonRequired),
            None => (),
        }
        Ok(())
    }

    fn validate_allocations(&self, allocations: &BTreeMap<String, u8>) -> Result<(), Error> {
        if allocations.len() > MAX_PLAYERS
            || allocations.values().any(|n| *n == 0)
            || allocations.values().map(|n| usize::from(*n)).sum::<usize>() > MAX_PLAYERS
        {
            return Err(Error::InvalidInput);
        }
        for toon in allocations.keys() {
            self.validate_toon(toon)?;
        }
        for assignment in self.assignments.values() {
            let toon = assignment.toon.as_ref().ok_or(Error::ToonRequired)?;
            let occupancy = self
                .assignments
                .values()
                .filter(|a| a.toon.as_ref() == Some(toon))
                .count();
            if usize::from(*allocations.get(toon).unwrap_or(&0)) < occupancy {
                return Err(Error::BelowOccupancy);
            }
        }
        Ok(())
    }

    fn require_manager(&self, actor: &Actor) -> Result<(), Error> {
        if actor.user_id != self.owner_id && !actor.manage_all_runs {
            return Err(Error::Forbidden);
        }
        Ok(())
    }

    fn confirm(&self, actor: &Actor, confirmation: &Confirmation) -> Result<(), Error> {
        if confirmation.actor_id != actor.user_id
            || confirmation.revision != self.desired_card_revision
        {
            return Err(Error::StaleConfirmation);
        }
        Ok(())
    }

    fn assign(
        &mut self,
        actor: &Actor,
        toon: &Option<String>,
        now: u64,
        switching: bool,
    ) -> Result<(), Error> {
        if self.state != RunState::Open {
            return Err(Error::Closed);
        }
        self.validate_selection(toon.as_deref())?;
        let existing = self.assignments.get(&actor.user_id);
        match existing {
            Some(existing) if existing.toon == *toon => return Ok(()),
            Some(_) if !switching => return Err(Error::AlreadyJoined),
            None if switching => return Err(Error::NotJoined),
            _ => (),
        }
        if existing.is_none() && self.assignments.len() >= usize::from(self.capacity()) {
            return Err(Error::Full);
        }
        if let (Some(allocations), Some(toon)) = (&self.allocations, toon) {
            let occupied = self
                .assignments
                .iter()
                .filter(|(member, a)| *member != &actor.user_id && a.toon.as_ref() == Some(toon))
                .count();
            if occupied >= usize::from(*allocations.get(toon).ok_or(Error::UnallocatedToon)?) {
                return Err(Error::Full);
            }
        }
        let joined_at = existing.map_or(now, |a| a.joined_at);
        self.assignments.insert(
            actor.user_id.clone(),
            Assignment {
                toon: toon.clone(),
                joined_at,
            },
        );
        Ok(())
    }
}

/// Apply one authenticated operation against one storage revision. Persist the
/// returned run and operation receipt in the same compare-and-swap batch.
pub fn apply(
    run: &Run,
    actor: &Actor,
    command: &Command,
    current: &EligibilitySnapshot,
    now: u64,
) -> Result<(Run, Outcome), Error> {
    run.validate()?;
    if !valid_member_id(&actor.user_id) || actor.guild_id != run.guild_id {
        return Err(Error::Forbidden);
    }
    if now < run.updated_at {
        return Err(Error::InvalidInput);
    }
    let mut next = run.clone();
    match command {
        Command::Join { toon } => next.assign(actor, toon, now, false)?,
        Command::Switch { toon } => next.assign(actor, toon, now, true)?,
        Command::Leave => {
            if !matches!(run.state, RunState::Open | RunState::Locked) {
                return Err(Error::Closed);
            }
            next.assignments.remove(&actor.user_id);
        }
        _ => {
            run.require_manager(actor)?;
            match command {
                Command::EditDraft {
                    name,
                    allocations,
                    host_toon,
                } => {
                    if run.state != RunState::Draft {
                        return Err(Error::InvalidTransition);
                    }
                    if let Some(name) = name {
                        next.name.clone_from(name);
                    }
                    if let Some(allocations) = allocations {
                        if run.mode != RunMode::Organized {
                            return Err(Error::InvalidInput);
                        }
                        next.validate_allocations(allocations)?;
                        next.allocations = Some(allocations.clone());
                    }
                    if let Some(toon) = host_toon {
                        if run.mode != RunMode::Organized {
                            return Err(Error::InvalidInput);
                        }
                        next.validate_toon(toon)?;
                        next.host_toon = Some(toon.clone());
                    }
                }
                Command::SetMode { mode, confirmation } => {
                    if run.state != RunState::Draft {
                        return Err(Error::ModeImmutable);
                    }
                    if *mode != run.mode {
                        if *mode == RunMode::Casual {
                            run.confirm(
                                actor,
                                confirmation.as_ref().ok_or(Error::ConfirmationRequired)?,
                            )?;
                        }
                        next.mode = *mode;
                        next.allocations = (*mode == RunMode::Organized).then(BTreeMap::new);
                        next.host_toon = None;
                    }
                }
                Command::Publish => {
                    if run.state != RunState::Draft {
                        return Err(Error::InvalidTransition);
                    }
                    current.require_fresh(now)?;
                    // Check the approved source now, while retaining the reviewed names/evidence.
                    if let Some(allocations) = &run.allocations {
                        if allocations.is_empty() {
                            return Err(Error::ToonRequired);
                        }
                        for toon in allocations.keys() {
                            if !current.toons.contains_key(toon) {
                                return Err(Error::CatalogChanged);
                            }
                        }
                        run.validate_selection(run.host_toon.as_deref())?;
                    }
                    if run.mode == RunMode::Casual
                        && !run.eligibility.toons.keys().eq(current.toons.keys())
                    {
                        return Err(Error::CatalogChanged);
                    }
                    next.state = RunState::Open;
                    next.assignments.insert(
                        run.owner_id.clone(),
                        Assignment {
                            toon: run.host_toon.clone(),
                            joined_at: now,
                        },
                    );
                }
                Command::SetAllocations { allocations } => {
                    if run.mode != RunMode::Organized {
                        return Err(Error::InvalidInput);
                    }
                    if run.state.is_terminal() {
                        return Err(Error::Closed);
                    }
                    next.validate_allocations(allocations)?;
                    next.allocations = Some(allocations.clone());
                }
                Command::Rename { name } => {
                    if run.state.is_terminal() {
                        return Err(Error::Closed);
                    }
                    next.name.clone_from(name);
                }
                Command::Lock => match run.state {
                    RunState::Open => next.state = RunState::Locked,
                    RunState::Locked => (),
                    _ => return Err(Error::InvalidTransition),
                },
                Command::Reopen => match run.state {
                    RunState::Locked => next.state = RunState::Open,
                    RunState::Open => (),
                    _ => return Err(Error::InvalidTransition),
                },
                Command::Complete => match run.state {
                    RunState::Locked => {
                        next.state = RunState::Completed;
                        next.terminal_at = Some(now);
                    }
                    RunState::Completed => (),
                    _ => return Err(Error::InvalidTransition),
                },
                Command::Cancel { confirmation } => {
                    run.confirm(actor, confirmation)?;
                    match run.state {
                        RunState::Draft | RunState::Open | RunState::Locked => {
                            next.state = RunState::Cancelled;
                            next.terminal_at = Some(now);
                        }
                        RunState::Cancelled => (),
                        _ => return Err(Error::InvalidTransition),
                    }
                }
                Command::Remove {
                    member_id,
                    confirmation,
                } => {
                    if !valid_member_id(member_id) {
                        return Err(Error::InvalidInput);
                    }
                    run.confirm(actor, confirmation)?;
                    if !matches!(run.state, RunState::Open | RunState::Locked) {
                        return Err(Error::Closed);
                    }
                    next.assignments.remove(member_id);
                }
                Command::Join { .. } | Command::Switch { .. } | Command::Leave => unreachable!(),
            }
        }
    }
    let changed = next != *run;
    if changed {
        next.updated_at = now;
        if run.state == RunState::Draft && actor.user_id == run.owner_id {
            next.last_owner_edit_at = now;
        }
        next.desired_card_revision = run
            .desired_card_revision
            .checked_add(1)
            .ok_or(Error::InvalidInput)?;
    }
    next.validate()?;
    let outcome = Outcome {
        changed,
        state: next.state,
        card_revision: next.desired_card_revision,
    };
    Ok((next, outcome))
}
