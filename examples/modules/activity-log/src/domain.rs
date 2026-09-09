use oracle_contracts::{GuildEvent, GuildEventKind, GuildEventOrigin};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
fn unknown_origin() -> GuildEventOrigin {
    GuildEventOrigin::Unknown
}
pub const HOUR: u64 = 3_600_000;
pub const RETENTION: u64 = 14 * 24 * HOUR;
pub fn moderate() -> Value {
    json!({"preset":"moderate/v1","enabled":["moderation_audit","channel_changes","role_access_changes","member_role_changes","bans_unbans","membership_summary"],"excluded":["message_create","message_edit","message_delete","message_bodies","attachments","reactions","typing","presence","routine_voice"],"membership_summary_minutes":15,"retention_days":14,"retain_message_content":false,"retain_attachments":false,"self_origin_exclusion":true,"coalesce":true,"coalesce_seconds":30,"queue_limit":128,"dropped_event_summary":true})
}
pub fn valid_config(value: &Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    let preset = moderate();
    if preset
        .as_object()
        .unwrap()
        .iter()
        .any(|(k, v)| map.get(k) != Some(v))
    {
        return false;
    }
    if map.keys().any(|k| {
        !preset.as_object().unwrap().contains_key(k) && k != "destination" && k != "operator_note"
    }) {
        return false;
    }
    value
        .get("destination")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty() && s.len() <= 20 && s.bytes().all(|c| c.is_ascii_digit()))
        && value
            .get("operator_note")
            .is_none_or(|v| v.as_str().is_some_and(|s| s.len() <= 1024))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default = "unknown_origin")]
    pub origin: GuildEventOrigin,
    pub id: String,
    pub kind: GuildEventKind,
    pub at: u64,
    pub subject: Option<String>,
    pub actor: Option<String>,
    pub related: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Bucket {
    pub hour: u64,
    pub records: Vec<Metadata>,
}
impl Bucket {
    pub fn expire(&mut self, now: u64) -> bool {
        let before = self.records.len();
        self.records
            .retain(|r| now.saturating_sub(r.at) < RETENTION);
        before != self.records.len()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notification {
    pub created: u64,
    pub purpose: String,
    pub key: String,
    pub text: String,
    pub count: u64,
    pub due: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub pending: Vec<Notification>,
    pub dropped: u64,
    pub dropped_reported: u64,
    pub delivered: u64,
    pub observed: u64,
    pub clock: u64,
    pub moderation_observed: u64,
    pub unknown_origin_observations: u64,
    pub unknown_audit_actor_events: u64,
    pub unattributed_administrative_observations: u64,
    pub joined: u64,
    pub left: u64,
    pub membership_start: Option<u64>,
    pub retention_last_run: Option<u64>,
    pub last_error: Option<String>,
    pub probe_verified: bool,
    pub probe_destination: Option<String>,
    pub probe_revision: Option<u64>,
}
impl State {
    pub fn ingest(&mut self, event: &GuildEvent, now: u64, bucket: &mut Bucket) {
        if matches!(event.origin, GuildEventOrigin::Oracle)
            || matches!(
                event.kind,
                GuildEventKind::Unknown | GuildEventKind::Maintenance
            )
            || event.occurred_at_ms > now
            || now.saturating_sub(event.occurred_at_ms) >= RETENTION
        {
            return;
        }
        if event.id.is_empty()
            || event.id.len() > 128
            || event.id.chars().any(char::is_control)
            || [&event.subject_id, &event.actor_id, &event.related_id]
                .into_iter()
                .flatten()
                .any(|id| id.is_empty() || id.len() > 20 || !id.bytes().all(|c| c.is_ascii_digit()))
        {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let hour = event.occurred_at_ms / HOUR;
        if bucket.hour != hour {
            bucket.hour = hour;
            bucket.records.clear();
        }
        if bucket.records.iter().any(|r| r.id == event.id) {
            return;
        }
        if bucket.records.len() >= 128 {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        bucket.records.push(Metadata {
            origin: event.origin,
            id: event.id.clone(),
            kind: event.kind,
            at: event.occurred_at_ms,
            subject: event.subject_id.clone(),
            actor: event.actor_id.clone(),
            related: event.related_id.clone(),
        });
        self.observed = self.observed.saturating_add(1);
        if event.origin == GuildEventOrigin::Unknown {
            self.unknown_origin_observations = self.unknown_origin_observations.saturating_add(1);
            if event.kind == GuildEventKind::ModerationAudit {
                self.unknown_audit_actor_events = self.unknown_audit_actor_events.saturating_add(1);
            } else if !matches!(
                event.kind,
                GuildEventKind::MemberJoined | GuildEventKind::MemberLeft
            ) {
                self.unattributed_administrative_observations = self
                    .unattributed_administrative_observations
                    .saturating_add(1);
            }
        }

        if matches!(
            event.kind,
            GuildEventKind::ModerationAudit | GuildEventKind::Ban | GuildEventKind::Unban
        ) {
            self.moderation_observed = self.moderation_observed.saturating_add(1);
        }
        match event.kind {
            GuildEventKind::MemberJoined => {
                self.membership_start.get_or_insert(now / 900_000 * 900_000);
                self.joined = self.joined.saturating_add(1);
            }
            GuildEventKind::MemberLeft => {
                self.membership_start.get_or_insert(now / 900_000 * 900_000);
                self.left = self.left.saturating_add(1);
            }
            _ => {
                // Unattributed raw changes may be Oracle's own effects. Retain their
                // metadata separately; only authoritative external admin events notify.
                if event.origin != GuildEventOrigin::External {
                    return;
                }
                let key = format!(
                    "{:?}:{}",
                    event.kind,
                    event.subject_id.as_deref().unwrap_or("none")
                );
                if let Some(pending) = self
                    .pending
                    .iter_mut()
                    .find(|p| p.key == key && p.due > now)
                {
                    pending.count = pending.count.saturating_add(1);
                    return;
                }
                self.enqueue(Notification {
                    created: now,
                    purpose: format!("event:{}", event.id),
                    key,
                    text: format!(
                        "{:?}; subject {}; actor {}",
                        event.kind,
                        event.subject_id.as_deref().unwrap_or("unavailable"),
                        event.actor_id.as_deref().unwrap_or("unavailable")
                    ),
                    count: 1,
                    due: now.saturating_add(30_000),
                });
            }
        }
    }
    fn enqueue(&mut self, notification: Notification) {
        if self.pending.len() >= 128 {
            self.dropped = self.dropped.saturating_add(1);
        } else {
            self.pending.push(notification);
        }
    }
    pub fn maintenance(&mut self, now: u64) {
        if let Some(start) = self.membership_start
            && now >= start.saturating_add(900_000)
            && self.pending.len() < 128
        {
            self.enqueue(Notification {
                created: now,
                purpose: format!("membership:{start}"),
                key: "membership".into(),
                text: format!(
                    "Membership observations (cause not attributed): {} joined, {} left",
                    self.joined, self.left
                ),
                count: 1,
                due: now,
            });
            self.joined = 0;
            self.left = 0;
            self.membership_start = None;
        }
        if self.dropped > self.dropped_reported && self.pending.len() < 128 {
            let total = self.dropped;
            self.enqueue(Notification {
                created: now,
                purpose: format!("dropped:{total}"),
                key: "dropped".into(),
                text: format!(
                    "Dropped {} metadata events due to bounded capacity",
                    total - self.dropped_reported
                ),
                count: 1,
                due: now,
            });
            self.dropped_reported = total;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn event(id: &str, kind: GuildEventKind, at: u64) -> GuildEvent {
        GuildEvent {
            id: id.into(),
            kind,
            occurred_at_ms: at,
            origin: GuildEventOrigin::External,
            subject_id: Some("12".into()),
            actor_id: None,
            related_id: None,
        }
    }
    #[test]
    fn exact_preset_and_notes() {
        let mut c = moderate();
        c["destination"] = json!("123");
        c["operator_note"] = json!("keep this");
        assert!(valid_config(&c));
        assert_eq!(
            c["enabled"],
            json!([
                "moderation_audit",
                "channel_changes",
                "role_access_changes",
                "member_role_changes",
                "bans_unbans",
                "membership_summary"
            ])
        );
        assert_eq!(
            c["excluded"],
            json!([
                "message_create",
                "message_edit",
                "message_delete",
                "message_bodies",
                "attachments",
                "reactions",
                "typing",
                "presence",
                "routine_voice"
            ])
        );
        assert_eq!(c["retain_message_content"], false);
        assert_eq!(c["retain_attachments"], false);
        assert_eq!(c["self_origin_exclusion"], true);
        assert_eq!(c["dropped_event_summary"], true);
        assert_eq!(c["retention_days"], 14);
        assert_eq!(c["membership_summary_minutes"], 15);
        assert_eq!(c["queue_limit"], 128);
        assert_eq!(c["coalesce_seconds"], 30);
        c["retain_message_content"] = json!(true);
        assert!(!valid_config(&c));
    }
    #[test]
    fn dedup_coalesce_and_exclusion() {
        let mut state = State::default();
        let mut bucket = Bucket::default();
        let e = event("1", GuildEventKind::Ban, 100);
        state.ingest(&e, 100, &mut bucket);
        state.ingest(&e, 100, &mut bucket);
        state.ingest(&event("2", GuildEventKind::Ban, 200), 200, &mut bucket);
        assert_eq!(state.observed, 2);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].count, 2);
        let mut own = event("3", GuildEventKind::Ban, 300);
        own.origin = GuildEventOrigin::Oracle;
        state.ingest(&own, 300, &mut bucket);
        assert_eq!(state.observed, 2);
    }
    #[test]
    fn membership_only_at_fifteen_minutes() {
        let mut s = State::default();
        let mut b = Bucket::default();
        s.ingest(&event("1", GuildEventKind::MemberJoined, 100), 100, &mut b);
        s.ingest(&event("2", GuildEventKind::MemberLeft, 200), 200, &mut b);
        s.maintenance(899_999);
        assert!(s.pending.is_empty());
        s.maintenance(900_000);
        assert_eq!(
            s.pending[0].text,
            "Membership observations (cause not attributed): 1 joined, 1 left"
        );
    }
    #[test]
    fn retention_exact_boundary_and_serialization_bounds() {
        let mut s = State::default();
        let mut b = Bucket::default();
        for i in 0..128 {
            let mut e = event(&format!("{:0128}", i), GuildEventKind::RoleAccessChanged, i);
            e.subject_id = Some(format!("{:020}", i));
            e.actor_id = Some("12345678901234567890".into());
            e.related_id = e.actor_id.clone();
            s.ingest(&e, i, &mut b);
        }
        assert_eq!(s.pending.len(), 128);
        assert_eq!(b.records.len(), 128);
        assert!(serde_json::to_vec(&s).unwrap().len() < 64 * 1024);
        assert!(serde_json::to_vec(&b).unwrap().len() < 64 * 1024);
        assert!(!b.expire(RETENTION - 1));
        assert!(b.expire(RETENTION));
        assert_eq!(b.records.len(), 127);
        assert!(b.expire(RETENTION + 128));
        assert!(b.records.is_empty());
    }
    #[test]
    fn bounds_and_retention() {
        let mut s = State::default();
        let mut b = Bucket::default();
        for i in 0..140 {
            s.ingest(
                &event(&i.to_string(), GuildEventKind::Ban, i * 31_000),
                i * 31_000,
                &mut b,
            );
        }
        assert!(s.pending.len() <= 128);
        assert!(b.records.len() <= 128);
        assert!(s.dropped > 0);
        let before = s.observed;
        s.ingest(&event("expired", GuildEventKind::Ban, 0), RETENTION, &mut b);
        assert_eq!(s.observed, before);
    }
}
