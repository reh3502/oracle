//! Plan construction is pure. Persisted execution uses these same typed steps.
use crate::permissions::{self, Member, Overwrite, Role};
use oracle_core::{Error, ErrorCode, GuildId, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Category,
    Text,
    Voice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    pub id: String,
    pub guild: GuildId,
    pub parent: Option<String>,
    pub kind: ChannelKind,
    pub name: String,
    pub overwrites: Vec<Overwrite>,
}

/// Host-owned fresh facts. A list response alone does not establish coverage.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub guild: GuildId,
    pub owner: String,
    pub actor: Member,
    pub bot: Member,
    pub roles: Vec<Role>,
    pub channels: Vec<Channel>,
    pub complete: bool,
    pub observed_at: u64,
}

/// Parent references another logical key in this request, ordered before its children.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredChannel {
    pub key: String,
    pub name: String,
    pub kind: ChannelKind,
    pub parent: Option<String>,
    pub existing_id: Option<String>,
    /// Omission preserves an existing channel or inherits the chosen parent's access.
    pub overwrites: Option<Vec<Overwrite>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructureRequest {
    pub channels: Vec<DesiredChannel>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Create,
    Update,
    Reuse,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub key: String,
    pub name: String,
    pub kind: ChannelKind,
    pub parent_key: Option<String>,
    pub before: Option<Channel>,
    pub overwrites: Vec<Overwrite>,
    pub change: Change,
    pub approval_required: bool,
}

fn invalid() -> Error {
    Error::new(ErrorCode::InvalidInput)
}
fn denied() -> Error {
    Error::new(ErrorCode::ForbiddenPermission)
}
fn id(value: &str) -> bool {
    oracle_core::UserId::new(value).is_ok()
}
fn key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn overwrites(mut values: Vec<Overwrite>) -> Vec<Overwrite> {
    values.sort_by_key(|v| (v.id.clone(), format!("{:?}", v.kind)));
    values
}

impl Snapshot {
    /// Filters before returning inspection to an actor. Hidden metadata is never returned.
    pub fn visible(mut self) -> Result<Self> {
        permissions::guild_permissions(self.guild.as_str(), &self.owner, &self.roles, &self.actor)?;
        permissions::guild_permissions(self.guild.as_str(), &self.owner, &self.roles, &self.bot)?;
        let mut seen = BTreeSet::new();
        for channel in &self.channels {
            if channel.guild != self.guild || !id(&channel.id) || !seen.insert(&channel.id) {
                return Err(Error::new(ErrorCode::ForbiddenScope));
            }
        }
        let mut visible = Vec::new();
        for mut channel in self.channels {
            match permissions::require_visible_to_both(
                self.guild.as_str(),
                &self.owner,
                &self.roles,
                &self.actor,
                &self.bot,
                &channel.overwrites,
            ) {
                Ok(()) => {
                    channel.overwrites = overwrites(channel.overwrites);
                    visible.push(channel);
                }
                Err(_) => self.complete = false,
            }
        }
        visible.sort_by(|a, b| a.id.cmp(&b.id));
        self.roles.sort_by(|a, b| a.id.cmp(&b.id));
        self.actor.roles.sort();
        self.bot.roles.sort();
        self.channels = visible;
        Ok(self)
    }

    pub fn fingerprint(&self) -> Result<String> {
        let mut snapshot = self.clone().visible()?;
        snapshot.observed_at = 0;
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&snapshot).map_err(|_| invalid())?)
        ))
    }
}

/// Conservatively classify a permission expansion. Mixed changes require approval too.
fn expands(before: &[Overwrite], after: &[Overwrite]) -> bool {
    let find = |values: &[Overwrite], value: &Overwrite| {
        values
            .iter()
            .find(|candidate| candidate.id == value.id && candidate.kind == value.kind)
            .map(|v| (v.allow, v.deny))
            .unwrap_or((0, 0))
    };
    after.iter().any(|v| v.allow & !find(before, v).0 != 0)
        || before.iter().any(|v| v.deny & !find(after, v).1 != 0)
}

/// Bindings come only from durable host records, never from caller assertions.
pub fn build_steps(
    snapshot: &Snapshot,
    request: &StructureRequest,
    bindings: &BTreeMap<String, String>,
) -> Result<Vec<Step>> {
    let snapshot = snapshot.clone().visible()?;
    if request.channels.is_empty() || request.channels.len() > 20 {
        return Err(invalid());
    }
    for member in [&snapshot.actor, &snapshot.bot] {
        if permissions::guild_permissions(
            snapshot.guild.as_str(),
            &snapshot.owner,
            &snapshot.roles,
            member,
        )? & permissions::MANAGE_CHANNELS
            == 0
        {
            return Err(denied());
        }
    }
    let mut steps: Vec<Step> = Vec::new();
    let mut keys = BTreeSet::new();
    let mut selected = BTreeSet::new();
    for desired in &request.channels {
        if !key(&desired.key)
            || !keys.insert(desired.key.clone())
            || desired.name.is_empty()
            || desired.name.chars().count() > 100
            || desired.name.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let parent = desired
            .parent
            .as_ref()
            .map(|parent| {
                steps
                    .iter()
                    .find(|s| &s.key == parent && s.kind == ChannelKind::Category)
                    .ok_or_else(invalid)
            })
            .transpose()?;
        if desired.kind == ChannelKind::Category && parent.is_some() {
            return Err(invalid());
        }
        let parent_id = parent.and_then(|p| p.before.as_ref().map(|c| c.id.clone()));
        let explicit = desired.existing_id.as_ref();
        let bound = bindings.get(&desired.key);
        if explicit.is_some() && bound.is_some() && explicit != bound {
            return Err(Error::new(ErrorCode::Conflict));
        }
        let existing = if let Some(existing) = bound.or(explicit) {
            if !id(existing) {
                return Err(invalid());
            }
            Some(
                snapshot
                    .channels
                    .iter()
                    .find(|c| &c.id == existing)
                    .ok_or_else(|| Error::new(ErrorCode::NotFound))?
                    .clone(),
            )
        } else {
            let candidates: Vec<_> = snapshot
                .channels
                .iter()
                .filter(|c| {
                    c.name == desired.name
                        && c.kind == desired.kind
                        && c.parent == parent_id
                        && (parent.is_none() || parent_id.is_some())
                })
                .collect();
            if candidates.len() > 1 {
                return Err(Error::new(ErrorCode::Conflict));
            }
            candidates.first().map(|c| (*c).clone())
        };
        if existing.is_none() && !snapshot.complete {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        if let Some(existing) = &existing {
            if existing.kind != desired.kind || !selected.insert(existing.id.clone()) {
                return Err(Error::new(ErrorCode::Conflict));
            }
            // Moving channels is explicit and reviewed, never an accidental name match.
            if existing.parent != parent_id && explicit.is_none() && bound.is_none() {
                return Err(Error::new(ErrorCode::Conflict));
            }
        }
        let inherited = parent.map(|p| p.overwrites.clone()).unwrap_or_default();
        let before_access = existing
            .as_ref()
            .map(|c| c.overwrites.clone())
            .unwrap_or(inherited);
        let access = overwrites(
            desired
                .overwrites
                .clone()
                .unwrap_or_else(|| before_access.clone()),
        );
        // Also validates duplicate/unknown overwrite roles and prevents locking the operator out.
        permissions::require_visible_to_both(
            snapshot.guild.as_str(),
            &snapshot.owner,
            &snapshot.roles,
            &snapshot.actor,
            &snapshot.bot,
            &access,
        )?;
        if access != before_access {
            for member in [&snapshot.actor, &snapshot.bot] {
                if permissions::guild_permissions(
                    snapshot.guild.as_str(),
                    &snapshot.owner,
                    &snapshot.roles,
                    member,
                )? & permissions::MANAGE_ROLES
                    == 0
                {
                    return Err(denied());
                }
            }
        }
        let change = match &existing {
            None => Change::Create,
            Some(c)
                if c.name == desired.name && c.parent == parent_id && c.overwrites == access =>
            {
                Change::Reuse
            }
            Some(_) => Change::Update,
        };
        steps.push(Step {
            key: desired.key.clone(),
            name: desired.name.clone(),
            kind: desired.kind,
            parent_key: desired.parent.clone(),
            before: existing,
            approval_required: expands(&before_access, &access),
            overwrites: access,
            change,
        });
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{
        MANAGE_CHANNELS, MANAGE_ROLES, OverwriteKind, SEND_MESSAGES, VIEW_CHANNEL,
    };
    fn snapshot() -> Snapshot {
        Snapshot {
            guild: GuildId::new("100").unwrap(),
            owner: "99".into(),
            actor: Member {
                id: "50".into(),
                roles: vec![],
                timed_out: false,
            },
            bot: Member {
                id: "60".into(),
                roles: vec![],
                timed_out: false,
            },
            roles: vec![Role {
                id: "100".into(),
                position: 0,
                permissions: MANAGE_CHANNELS | MANAGE_ROLES | VIEW_CHANNEL | SEND_MESSAGES,
                managed: false,
            }],
            channels: vec![],
            complete: true,
            observed_at: 1,
        }
    }
    fn desired(key: &str, name: &str, kind: ChannelKind, parent: Option<&str>) -> DesiredChannel {
        DesiredChannel {
            key: key.into(),
            name: name.into(),
            kind,
            parent: parent.map(str::to_owned),
            existing_id: None,
            overwrites: None,
        }
    }
    fn layout() -> StructureRequest {
        StructureRequest {
            channels: vec![
                desired(
                    "minecraft.category",
                    "Minecraft",
                    ChannelKind::Category,
                    None,
                ),
                desired(
                    "minecraft.info",
                    "minecraft-info",
                    ChannelKind::Text,
                    Some("minecraft.category"),
                ),
                desired(
                    "minecraft.voice",
                    "Minecraft Voice",
                    ChannelKind::Voice,
                    Some("minecraft.category"),
                ),
            ],
        }
    }
    #[test]
    fn minecraft_creation_then_readback_is_a_noop() {
        let mut s = snapshot();
        let steps = build_steps(&s, &layout(), &BTreeMap::new()).unwrap();
        assert_eq!(steps.len(), 3);
        assert!(
            steps
                .iter()
                .all(|s| s.change == Change::Create && !s.approval_required)
        );
        for (i, step) in steps.iter().enumerate() {
            s.channels.push(Channel {
                id: (200 + i).to_string(),
                guild: s.guild.clone(),
                parent: step.parent_key.as_ref().map(|_| "200".into()),
                kind: step.kind,
                name: step.name.clone(),
                overwrites: step.overwrites.clone(),
            });
        }
        let repeated = build_steps(&s, &layout(), &BTreeMap::new()).unwrap();
        assert!(repeated.iter().all(|s| s.change == Change::Reuse));
    }
    #[test]
    fn hidden_channels_are_filtered_and_prevent_duplicate_creation() {
        let mut s = snapshot();
        s.channels.push(Channel {
            id: "200".into(),
            guild: s.guild.clone(),
            parent: None,
            kind: ChannelKind::Category,
            name: "Minecraft".into(),
            overwrites: vec![Overwrite {
                id: "100".into(),
                kind: OverwriteKind::Role,
                allow: 0,
                deny: VIEW_CHANNEL,
            }],
        });
        let visible = s.clone().visible().unwrap();
        assert!(visible.channels.is_empty());
        assert!(!visible.complete);
        assert!(build_steps(&s, &layout(), &BTreeMap::new()).is_err());
        assert!(
            build_steps(
                &snapshot(),
                &layout(),
                &BTreeMap::from([("minecraft.category".into(), "200".into())])
            )
            .is_err()
        );
    }
    #[test]
    fn ambiguity_and_cross_guild_metadata_cannot_select_a_target() {
        let mut s = snapshot();
        let channel = Channel {
            id: "200".into(),
            guild: s.guild.clone(),
            parent: None,
            kind: ChannelKind::Category,
            name: "Minecraft".into(),
            overwrites: vec![],
        };
        s.channels = vec![
            channel.clone(),
            Channel {
                id: "201".into(),
                ..channel.clone()
            },
        ];
        assert_eq!(
            build_steps(&s, &layout(), &BTreeMap::new())
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        s.channels = vec![Channel {
            guild: GuildId::new("101").unwrap(),
            ..channel
        }];
        assert_eq!(s.visible().unwrap_err().code, ErrorCode::ForbiddenScope);
    }
    #[test]
    fn removing_an_existing_deny_requires_exact_approval() {
        let mut s = snapshot();
        s.channels.push(Channel {
            id: "200".into(),
            guild: s.guild.clone(),
            parent: None,
            kind: ChannelKind::Category,
            name: "Minecraft".into(),
            overwrites: vec![Overwrite {
                id: "100".into(),
                kind: OverwriteKind::Role,
                allow: 0,
                deny: SEND_MESSAGES,
            }],
        });
        let mut request = layout();
        request.channels.truncate(1);
        let unchanged = build_steps(&s, &request, &BTreeMap::new()).unwrap();
        assert_eq!(unchanged[0].change, Change::Reuse);
        assert!(!unchanged[0].approval_required);
        request.channels[0].overwrites = Some(vec![]);
        let expanded = build_steps(&s, &request, &BTreeMap::new()).unwrap();
        assert_eq!(expanded[0].change, Change::Update);
        assert!(expanded[0].approval_required);
        s.roles[0].permissions &= !MANAGE_ROLES;
        assert!(build_steps(&s, &request, &BTreeMap::new()).is_err());
    }
    #[test]
    fn fingerprint_ignores_fetch_time_but_detects_permissions() {
        let s = snapshot();
        let mut newer = s.clone();
        newer.observed_at = 1000;
        assert_eq!(s.fingerprint().unwrap(), newer.fingerprint().unwrap());
        newer.roles[0].permissions &= !SEND_MESSAGES;
        assert_ne!(s.fingerprint().unwrap(), newer.fingerprint().unwrap());
    }
    #[test]
    fn invalid_parent_order_and_duplicate_keys_are_rejected() {
        let mut request = layout();
        request.channels.swap(0, 1);
        assert!(build_steps(&snapshot(), &request, &BTreeMap::new()).is_err());
        request = layout();
        request.channels[1].key = request.channels[0].key.clone();
        assert!(build_steps(&snapshot(), &request, &BTreeMap::new()).is_err());
    }
}
