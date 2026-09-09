//! P6 deterministic Discord operation contract. All authorization lives outside model output.
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Category,
    Text,
    Voice,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Access {
    pub read: bool,
    pub write: bool,
    pub connect: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    pub guild: String,
    pub parent: Option<String>,
    pub name: String,
    pub kind: Kind,
    pub audience: BTreeMap<String, Access>,
    pub topic: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub guild: String,
    pub actor: String,
    pub actor_manage: bool,
    pub bot_manage: bool,
    pub bot_manage_roles: bool,
    pub bot_role_position: u32,
    pub minecraft_role_position: u32,
    pub staff_role_position: u32,
    pub complete: bool,
    pub allow_game_area: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub policy: Policy,
    pub channels: Vec<Channel>,
}
impl Snapshot {
    fn fingerprint(&self) -> String {
        format!("{:x}", Sha256::digest(serde_json::to_vec(self).unwrap()))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    PermissionDenied,
    VisibilityIncomplete,
    AmbiguousTarget,
    StalePlan,
    UnknownOutcome,
    ScopeMismatch,
    ReadbackMismatch,
    Io(String),
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
#[derive(Debug, Clone)]
pub struct Context {
    pub guild: String,
    pub actor: String,
    pub run: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Binding {
    guild: String,
    purpose: String,
    id: Option<String>,
    state: String,
}
/// Durable reservations survive a new process/executor/run. No timeout clears unknown creates.
pub struct Store {
    path: PathBuf,
    bindings: BTreeMap<(String, String), Binding>,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut bindings = BTreeMap::new();
        if path.exists() {
            for line in std::fs::read_to_string(path)?.lines() {
                let b: Binding =
                    serde_json::from_str(line).map_err(|e| Error::Io(e.to_string()))?;
                bindings.insert((b.guild.clone(), b.purpose.clone()), b);
            }
        }
        Ok(Self {
            path: path.into(),
            bindings,
        })
    }
    fn put(&mut self, b: Binding) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, &b).map_err(|e| Error::Io(e.to_string()))?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        self.bindings
            .insert((b.guild.clone(), b.purpose.clone()), b);
        Ok(())
    }
    fn get(&self, guild: &str, purpose: &str) -> Option<&Binding> {
        self.bindings.get(&(guild.into(), purpose.into()))
    }
}
#[derive(Debug, Clone)]
struct Desired {
    purpose: String,
    name: String,
    kind: Kind,
    audience: BTreeMap<String, Access>,
}
#[derive(Debug, Clone)]
pub struct Plan {
    owner: Context,
    fingerprint: String,
    desired: Vec<Desired>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    pub created: Vec<String>,
    pub reused: Vec<String>,
    pub verified: bool,
}
/// Real adapters must implement fresh scoped readback and preserve ambiguous create outcomes.
pub trait Discord {
    fn inspect(&mut self, ctx: &Context) -> Result<Snapshot>;
    fn create(&mut self, ctx: &Context, channel: Channel) -> Result<Channel>;
}
fn authorize(p: &Policy, c: &Context) -> Result<()> {
    if p.guild != c.guild || p.actor != c.actor {
        return Err(Error::ScopeMismatch);
    }
    if !p.actor_manage
        || !p.bot_manage
        || !p.bot_manage_roles
        || !p.allow_game_area
        || p.minecraft_role_position >= p.bot_role_position
        || p.staff_role_position >= p.bot_role_position
    {
        return Err(Error::PermissionDenied);
    }
    if !p.complete {
        return Err(Error::VisibilityIncomplete);
    }
    Ok(())
}
fn audience(info: bool) -> BTreeMap<String, Access> {
    [
        (
            "everyone".into(),
            Access {
                read: false,
                write: false,
                connect: false,
            },
        ),
        (
            "minecraft".into(),
            Access {
                read: true,
                write: !info,
                connect: true,
            },
        ),
        (
            "staff".into(),
            Access {
                read: true,
                write: true,
                connect: true,
            },
        ),
    ]
    .into()
}
fn desired() -> Vec<Desired> {
    [
        ("minecraft.category", "Minecraft", Kind::Category, false),
        ("minecraft.info", "minecraft-info", Kind::Text, true),
        ("minecraft.chat", "minecraft-chat", Kind::Text, false),
        ("minecraft.voice", "Minecraft Voice", Kind::Voice, false),
    ]
    .into_iter()
    .map(|(p, n, k, i)| Desired {
        purpose: p.into(),
        name: n.into(),
        kind: k,
        audience: audience(i),
    })
    .collect()
}
fn compatible(c: &Channel, d: &Desired, parent: &Option<String>) -> bool {
    c.name == d.name && c.kind == d.kind && &c.parent == parent && c.audience == d.audience
}
fn resolve(
    s: &Snapshot,
    store: &Store,
    d: &Desired,
    parent: &Option<String>,
) -> Result<Option<Channel>> {
    if let Some(b) = store.get(&s.policy.guild, &d.purpose) {
        if b.state == "unknown" {
            return Err(Error::UnknownOutcome);
        }
        let existing = s
            .channels
            .iter()
            .find(|c| Some(&c.id) == b.id.as_ref())
            .ok_or(Error::VisibilityIncomplete)?;
        if existing.guild != s.policy.guild {
            return Err(Error::ScopeMismatch);
        }
        return if compatible(existing, d, parent) {
            Ok(Some(existing.clone()))
        } else {
            Err(Error::StalePlan)
        };
    }
    let candidates: Vec<_> = s
        .channels
        .iter()
        .filter(|c| c.guild == s.policy.guild && c.name == d.name && &c.parent == parent)
        .collect();
    if candidates.len() > 1 {
        return Err(Error::AmbiguousTarget);
    }
    match candidates.first() {
        None => Ok(None),
        Some(c) if compatible(c, d, parent) => Ok(Some((*c).clone())),
        _ => Err(Error::AmbiguousTarget),
    }
}
pub fn plan(ctx: &Context, api: &mut impl Discord, store: &Store) -> Result<Plan> {
    let snapshot = api.inspect(ctx)?;
    authorize(&snapshot.policy, ctx)?;
    let items = desired();
    let category = resolve(&snapshot, store, &items[0], &None)?;
    for d in &items[1..] {
        if let Some(c) = &category {
            resolve(&snapshot, store, d, &Some(c.id.clone()))?;
        } else if store.get(&ctx.guild, &d.purpose).is_some() {
            return Err(Error::StalePlan);
        }
    }
    Ok(Plan {
        owner: ctx.clone(),
        fingerprint: snapshot.fingerprint(),
        desired: items,
    })
}
pub fn apply(
    ctx: &Context,
    plan: &Plan,
    api: &mut impl Discord,
    store: &mut Store,
) -> Result<Receipt> {
    if ctx.guild != plan.owner.guild || ctx.actor != plan.owner.actor || ctx.run != plan.owner.run {
        return Err(Error::ScopeMismatch);
    }
    let mut snapshot = api.inspect(ctx)?;
    authorize(&snapshot.policy, ctx)?;
    if snapshot.fingerprint() != plan.fingerprint {
        return Err(Error::StalePlan);
    }
    let mut receipt = Receipt {
        created: Vec::new(),
        reused: Vec::new(),
        verified: false,
    };
    let mut parent = None;
    for d in &plan.desired {
        // Fresh authority before each effect, including after a partial successful apply.
        snapshot = api.inspect(ctx)?;
        authorize(&snapshot.policy, ctx)?;
        let existing = resolve(&snapshot, store, d, &parent)?;
        let c = if let Some(existing) = existing {
            receipt.reused.push(existing.id.clone());
            existing
        } else {
            store.put(Binding {
                guild: ctx.guild.clone(),
                purpose: d.purpose.clone(),
                id: None,
                state: "unknown".into(),
            })?;
            let c = api.create(
                ctx,
                Channel {
                    id: String::new(),
                    guild: ctx.guild.clone(),
                    parent: parent.clone(),
                    name: d.name.clone(),
                    kind: d.kind.clone(),
                    audience: d.audience.clone(),
                    topic: String::new(),
                },
            )?;
            if c.guild != ctx.guild || !compatible(&c, d, &parent) {
                return Err(Error::ReadbackMismatch);
            }
            // Persist receipt ID before readback. A later missing ID blocks duplicate creation.
            store.put(Binding {
                guild: ctx.guild.clone(),
                purpose: d.purpose.clone(),
                id: Some(c.id.clone()),
                state: "known".into(),
            })?;
            receipt.created.push(c.id.clone());
            c
        };
        let readback = api.inspect(ctx)?;
        authorize(&readback.policy, ctx)?;
        let actual = readback
            .channels
            .iter()
            .find(|r| r.id == c.id)
            .ok_or(Error::ReadbackMismatch)?;
        if actual.guild != ctx.guild
            || !compatible(actual, d, &parent)
            || readback
                .channels
                .iter()
                .filter(|r| r.guild == ctx.guild && r.name == d.name && r.parent == parent)
                .count()
                != 1
        {
            return Err(Error::ReadbackMismatch);
        }
        store.put(Binding {
            guild: ctx.guild.clone(),
            purpose: d.purpose.clone(),
            id: Some(c.id.clone()),
            state: "verified".into(),
        })?;
        if d.kind == Kind::Category {
            parent = Some(c.id.clone());
        }
    }
    // Earlier resources may change while later requests are in flight. Completion
    // requires one final fresh view of every postcondition, not a union of old views.
    let final_snapshot = api.inspect(ctx)?;
    authorize(&final_snapshot.policy, ctx)?;
    let mut final_parent = None;
    for d in &plan.desired {
        let bound = store
            .get(&ctx.guild, &d.purpose)
            .ok_or(Error::ReadbackMismatch)?;
        let candidates: Vec<_> = final_snapshot
            .channels
            .iter()
            .filter(|c| c.guild == ctx.guild && c.name == d.name && c.parent == final_parent)
            .collect();
        if candidates.len() != 1
            || Some(&candidates[0].id) != bound.id.as_ref()
            || !compatible(candidates[0], d, &final_parent)
        {
            return Err(Error::ReadbackMismatch);
        }
        if d.kind == Kind::Category {
            final_parent = Some(candidates[0].id.clone());
        }
    }
    receipt.verified = true;
    Ok(receipt)
}
/// Stateful fake remote with independent write/read responses and failure injection.
/// Fixture authorization is not relied upon: executor must reject before invoking create.
pub struct Fixture {
    pub state: Snapshot,
    pub hidden: BTreeSet<String>,
    pub creates: usize,
    pub lose_response_at: Option<usize>,
    pub revoke_after: Option<usize>,
    pub corrupt_write: bool,
    pub duplicate_write: bool,
    pub duplicate_public: bool,
    pub edit_previous: bool,
}
impl Fixture {
    pub fn fresh() -> Self {
        Self {
            state: Snapshot {
                policy: Policy {
                    guild: "guild-a".into(),
                    actor: "admin".into(),
                    actor_manage: true,
                    bot_manage: true,
                    bot_manage_roles: true,
                    bot_role_position: 10,
                    minecraft_role_position: 2,
                    staff_role_position: 3,
                    complete: true,
                    allow_game_area: true,
                },
                channels: vec![],
            },
            hidden: BTreeSet::new(),
            creates: 0,
            lose_response_at: None,
            revoke_after: None,
            corrupt_write: false,
            duplicate_write: false,
            duplicate_public: false,
            edit_previous: false,
        }
    }
}
impl Discord for Fixture {
    fn inspect(&mut self, ctx: &Context) -> Result<Snapshot> {
        if ctx.guild != self.state.policy.guild {
            return Err(Error::ScopeMismatch);
        }
        let mut s = self.state.clone();
        s.channels.retain(|c| !self.hidden.contains(&c.id));
        if !self.hidden.is_empty() {
            s.policy.complete = false;
        }
        Ok(s)
    }
    fn create(&mut self, _: &Context, mut c: Channel) -> Result<Channel> {
        self.creates += 1;
        c.id = format!("fixture-{}", uuid::Uuid::new_v4());
        let response = c.clone();
        if self.edit_previous && self.creates == 2 {
            self.state.channels[0]
                .audience
                .get_mut("everyone")
                .unwrap()
                .read = true;
        }
        if self.corrupt_write {
            c.audience.get_mut("everyone").unwrap().read = true;
        }
        self.state.channels.push(c.clone());
        if self.duplicate_write {
            c.id = format!("external-{}", uuid::Uuid::new_v4());
            if self.duplicate_public {
                c.audience.get_mut("everyone").unwrap().read = true;
            }
            self.state.channels.push(c);
        }
        if self.revoke_after == Some(self.creates) {
            self.state.policy.bot_manage = false;
        }
        if self.lose_response_at == Some(self.creates) {
            Err(Error::UnknownOutcome)
        } else {
            Ok(response)
        }
    }
}
