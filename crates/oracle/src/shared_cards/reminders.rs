//! Host-owned reminder receipts and attendance evidence. Modules never choose
//! remote message identities or turn unknown reads into absent members.
use super::*;
use oracle_discord::run_reminders::{
    DiscordRunReminders, RunReminderAuthority, RunReminderMessage,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const HOUR: u64 = 3_600_000;
fn now_ms() -> Result<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|n| n.as_millis() as u64)
        .map_err(|_| Error::new(ErrorCode::Integrity))
}
fn invalid() -> Error {
    Error::new(ErrorCode::InvalidInput)
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    kind: String,
    starts_at: i64,
    not_before: u64,
    expires_at: u64,
    role_id: Option<String>,
    users: Vec<String>,
    text: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Delivery {
    request: RunReminderMessage,
    expected: Value,
    #[serde(default)]
    retryable: bool,
    #[serde(default)]
    seeded_at: Option<u64>,
    message_id: Option<String>,
    delivered_at: Option<u64>,
    deadline: Option<u64>,
    confirmed: BTreeSet<String>,
    settled: bool,
    coverage_unknown: bool,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    deliveries: BTreeMap<String, Delivery>,
}

struct AuthorityCheck {
    host: Weak<Host>,
    module: ModuleId,
    run_id: String,
    starts_at: i64,
    channel: String,
    frozen: RunReminderMessage,
    sending: bool,
    not_before: u64,
    expires_at: u64,
}
#[async_trait]
impl RunReminderAuthority for AuthorityCheck {
    async fn validate(&self, request: &RunReminderMessage) -> Result<()> {
        if request != &self.frozen {
            return Err(Error::new(ErrorCode::ForbiddenScope));
        }
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
        let shared = host.shared_cards.get().ok_or_else(invalid)?;
        let binding = shared
            .destinations
            .iter()
            .find(|b| {
                b.guild == request.guild && b.module == self.module && b.destination == "runs"
            })
            .ok_or_else(invalid)?;
        if binding.channel != self.channel
            || request.channel != binding.channel
            || request.role.is_some()
        {
            return Err(Error::new(ErrorCode::ForbiddenScope));
        }
        let source = host
            .modules
            .shared_card_source(&request.guild, &self.module, &self.run_id)
            .await?;
        if !configured(&source.configuration) {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        let lease = host
            .modules
            .shared_card_dispatch(
                &request.guild,
                &self.module,
                &source.session,
                source.generation,
                source.epoch,
            )
            .await?;
        let stored = host
            .storage
            .document_get(&self.module, &request.guild, "runs", &self.run_id)
            .await?
            .ok_or_else(invalid)?
            .value;
        validate_current_run(
            request,
            &stored["run"],
            self.starts_at,
            self.sending,
            now_ms()?,
            self.not_before..=self.expires_at,
        )?;
        lease.dispatch(|| ())?;
        Ok(())
    }
}
async fn save(
    shared: &SharedCards,
    guild: &GuildId,
    key: &str,
    revision: Option<u64>,
    journal: &Journal,
) -> Result<u64> {
    let value = serde_json::to_value(journal).map_err(|_| invalid())?;
    if serde_json::to_vec(&value).map_err(|_| invalid())?.len() > 60 * 1024 {
        return Err(Error::new(ErrorCode::QuotaExceeded));
    }
    Ok(shared
        .host()?
        .storage
        .workflow_put(guild, WorkflowKind::SharedCard, key, revision, &value)
        .await?
        .revision)
}
fn validate_current_run(
    request: &RunReminderMessage,
    run: &Value,
    starts_at: i64,
    sending: bool,
    now: u64,
    window: std::ops::RangeInclusive<u64>,
) -> Result<()> {
    if request.role.is_some()
        || run["schedule"]["starts_at"] != starts_at
        || !matches!(run["state"].as_str(), Some("open" | "locked"))
        || sending
            && (!window.contains(&now) || roster(run)? != request.users.iter().cloned().collect())
    {
        return Err(Error::new(ErrorCode::Conflict));
    }
    Ok(())
}
fn roster(run: &Value) -> Result<BTreeSet<String>> {
    let mut users: BTreeSet<String> = run["assignments"]
        .as_object()
        .ok_or_else(invalid)?
        .keys()
        .cloned()
        .collect();
    users.insert(run["owner_id"].as_str().ok_or_else(invalid)?.to_owned());
    Ok(users)
}
fn record_attendance(
    delivery: &mut Delivery,
    observed: Result<BTreeSet<String>>,
    finished: u64,
    start: u64,
) -> Result<()> {
    let deadline = delivery.deadline.ok_or_else(invalid)?;
    let sent = delivery.delivered_at.ok_or_else(invalid)?;
    if deadline > start
        || finished > deadline.saturating_add(60_000)
        || delivery
            .seeded_at
            .is_none_or(|seeded| seeded > sent.saturating_add(60_000))
    {
        delivery.coverage_unknown = true;
        return Ok(());
    }
    match observed {
        Ok(users) => {
            let expected = delivery.expected.as_object().ok_or_else(invalid)?;
            delivery
                .confirmed
                .extend(users.into_iter().filter(|id| expected.contains_key(id)));
            delivery.settled = finished >= deadline;
        }
        Err(_) if finished >= deadline => delivery.coverage_unknown = true,
        Err(_) => {}
    }
    Ok(())
}
fn status(delivery: &Delivery, intent: &Intent) -> Value {
    json!({"state":if delivery.coverage_unknown {"coverage_unknown"} else if delivery.settled {"settled"}
        else if delivery.message_id.is_some() {"delivered"} else if delivery.retryable {"pending"} else {"recovery_required"},
        "kind":intent.kind,"starts_at":intent.starts_at,"delivered_at":delivery.delivered_at,
        "deadline":delivery.deadline,"expected":delivery.expected,"confirmed":delivery.confirmed})
}
pub(super) async fn process(
    shared: &SharedCards,
    module: &ModuleId,
    guild: &GuildId,
    id: &str,
    document: Value,
) -> Result<Value> {
    process_inner(shared, module, guild, id, document, None).await
}
fn configured(values: &Value) -> bool {
    values["reminders"]
        .as_object()
        .is_some_and(|config| config.len() == 1)
        && values["reminders"]["timezone"]
            .as_str()
            .is_some_and(|zone| !zone.is_empty())
}

async fn process_inner(
    shared: &SharedCards,
    module: &ModuleId,
    guild: &GuildId,
    id: &str,
    document: Value,
    write_deadline: Option<tokio::time::Instant>,
) -> Result<Value> {
    if module.as_str() != "community.dandys-world"
        || document["run"]["id"] != id
        || document["run"]["guild_id"] != guild.as_str()
    {
        return Err(Error::new(ErrorCode::ForbiddenScope));
    }
    let intent: Intent =
        serde_json::from_value(document["reminder"].clone()).map_err(|_| invalid())?;
    let run = &document["run"];
    if run["schedule"]["starts_at"] != intent.starts_at
        || !matches!(run["state"].as_str(), Some("open" | "locked"))
    {
        return Err(Error::new(ErrorCode::Conflict));
    }
    let start = u64::try_from(intent.starts_at)
        .map_err(|_| invalid())?
        .checked_mul(1000)
        .ok_or_else(invalid)?;
    let attendance = intent.kind == "attendance";
    let binding = shared
        .destinations
        .iter()
        .find(|b| &b.guild == guild && &b.module == module && b.destination == "runs")
        .ok_or_else(invalid)?;
    let users = roster(run)?;
    if intent.role_id.is_some()
        || intent.users.len() != users.len()
        || intent.users.iter().cloned().collect::<BTreeSet<_>>() != users
    {
        return Err(invalid());
    }
    if attendance {
        if intent.not_before != start.saturating_sub(4 * HOUR)
            || intent.expires_at != start.saturating_sub(3 * HOUR)
        {
            return Err(invalid());
        }
    } else if !matches!(intent.kind.as_str(), "signups_open" | "tomorrow")
        || intent.kind == "tomorrow"
            && (intent.not_before != start.saturating_sub(24 * HOUR)
                || intent.expires_at != start.saturating_sub(4 * HOUR))
        || intent.kind == "signups_open"
            && (intent.expires_at != start.saturating_sub(24 * HOUR)
                || intent.not_before >= intent.expires_at
                || run["state"] != "open")
    {
        return Err(invalid());
    }
    let host = shared.host()?;
    let key = format!("reminders:{}:{id}", module.as_str());
    let stored = host
        .storage
        .workflow_get(guild, WorkflowKind::SharedCard, &key)
        .await?;
    let mut revision = stored.as_ref().map(|r| r.revision);
    let mut journal: Journal = stored
        .map(|r| serde_json::from_value(r.value))
        .transpose()
        .map_err(|_| invalid())?
        .unwrap_or_default();
    let effect_key = format!("{}:{}", intent.starts_at, intent.kind);
    let now = now_ms()?;
    let is_new = !journal.deliveries.contains_key(&effect_key);
    if is_new {
        if now < intent.not_before || now > intent.expires_at {
            return Ok(json!({"state":"expired"}));
        }
        if journal.deliveries.len() >= 16 {
            return Err(Error::new(ErrorCode::QuotaExceeded));
        }
        journal.deliveries.insert(
            effect_key.clone(),
            Delivery {
                request: RunReminderMessage {
                    guild: guild.clone(),
                    channel: binding.channel.clone(),
                    key: uuid::Uuid::new_v4().to_string(),
                    text: intent.text.clone(),
                    users: intent.users.clone(),
                    role: None,
                    attendance,
                },
                expected: run["assignments"].clone(),
                retryable: true,
                seeded_at: None,
                message_id: None,
                delivered_at: None,
                deadline: None,
                confirmed: BTreeSet::new(),
                settled: false,
                coverage_unknown: false,
            },
        );
        // Store the queued identity first. Every send separately claims uncertainty with CAS.
        revision = Some(save(shared, guild, &key, revision, &journal).await?);
    }
    let mut delivery = journal
        .deliveries
        .get(&effect_key)
        .cloned()
        .ok_or_else(invalid)?;
    if write_deadline.is_none() {
        return Ok(status(&delivery, &intent));
    }
    // Only definitely unsent requests may follow roster/text changes. Once a send
    // is possible, the marker and entire request remain frozen for recovery.
    if delivery.message_id.is_none() && delivery.retryable {
        if now < intent.not_before || now > intent.expires_at {
            return Ok(json!({"state":"expired"}));
        }
        delivery.request.channel = binding.channel.clone();
        delivery.request.text = intent.text.clone();
        delivery.request.users = intent.users.clone();
        delivery.request.role = None;
        delivery.expected = run["assignments"].clone();
    }
    let make_transport = |sending| {
        let authority = Arc::new(AuthorityCheck {
            host: Arc::downgrade(&host),
            module: module.clone(),
            run_id: id.into(),
            starts_at: intent.starts_at,
            channel: binding.channel.clone(),
            frozen: delivery.request.clone(),
            sending,
            not_before: intent.not_before,
            expires_at: intent.expires_at,
        });
        DiscordRunReminders::new(shared.adapter.clone(), authority)
    };
    let transport = make_transport(false);
    let sending_transport = make_transport(true);
    let source = host.modules.shared_card_source(guild, module, id).await?;
    if !configured(&source.configuration) {
        return Err(Error::new(ErrorCode::ForbiddenPermission));
    }
    let lease = host
        .modules
        .shared_card_dispatch(
            guild,
            module,
            &source.session,
            source.generation,
            source.epoch,
        )
        .await?;
    if delivery.message_id.is_none() {
        let observed = if delivery.retryable {
            delivery.retryable = false;
            journal
                .deliveries
                .insert(effect_key.clone(), delivery.clone());
            // A lost process after this CAS always recovers; another worker cannot send.
            revision = Some(save(shared, guild, &key, revision, &journal).await?);
            match sending_transport
                .send_before(
                    &delivery.request,
                    &lease.permit(),
                    lease.cancellation(),
                    write_deadline.ok_or_else(invalid)?,
                )
                .await
            {
                Ok(id) => Ok(Some(id)),
                Err(error) if error.code != ErrorCode::UnknownOutcome => {
                    tracing::debug!(error=?error.code,run=%id,"run reminder definitely not delivered; retry pending");
                    delivery.retryable = true;
                    journal.deliveries.insert(effect_key.clone(), delivery);
                    save(shared, guild, &key, revision, &journal).await?;
                    return Ok(json!({"state":"pending"}));
                }
                Err(error) => Err(error),
            }
        } else {
            transport.recover(&delivery.request).await
        };
        match observed {
            Ok(Some(message_id)) => {
                // Discord message snowflake gives the real send time, including recovery.
                let sent =
                    (message_id.parse::<u64>().map_err(|_| invalid())? >> 22) + 1_420_070_400_000;
                delivery.message_id = Some(message_id);
                delivery.delivered_at = Some(sent);
                delivery.deadline = attendance.then_some(sent.saturating_add(3 * HOUR));
            }
            _ => return Ok(json!({"state":"recovery_required"})),
        }
        journal
            .deliveries
            .insert(effect_key.clone(), delivery.clone());
        revision = Some(save(shared, guild, &key, revision, &journal).await?);
    }
    if attendance && !delivery.settled && !delivery.coverage_unknown {
        let deadline = delivery.deadline.ok_or_else(invalid)?;
        let observed_at = now_ms()?;
        if deadline > start || observed_at > deadline.saturating_add(60_000) {
            delivery.coverage_unknown = true;
        } else {
            let message = delivery.message_id.as_ref().ok_or_else(invalid)?;
            if delivery.seeded_at.is_none() {
                let sent = delivery.delivered_at.ok_or_else(invalid)?;
                if observed_at > sent.saturating_add(60_000) {
                    delivery.coverage_unknown = true;
                } else {
                    match transport
                        .seed_attendance(
                            &delivery.request,
                            message,
                            &lease.permit(),
                            lease.cancellation(),
                        )
                        .await
                    {
                        Ok(()) => delivery.seeded_at = Some(now_ms()?),
                        Err(_) => {
                            if now_ms()? > sent.saturating_add(60_000) {
                                delivery.coverage_unknown = true;
                            }
                        }
                    }
                    // Persist seed evidence separately so a later read failure cannot lose it.
                    journal
                        .deliveries
                        .insert(effect_key.clone(), delivery.clone());
                    revision = Some(save(shared, guild, &key, revision, &journal).await?);
                }
            }
            if delivery.seeded_at.is_some() && !delivery.coverage_unknown {
                let observed = transport.attendance(&delivery.request, message).await;
                record_attendance(&mut delivery, observed, now_ms()?, start)?;
            }
        }
        journal.deliveries.insert(effect_key, delivery.clone());
        save(shared, guild, &key, revision, &journal).await?;
    }
    Ok(status(&delivery, &intent))
}

#[derive(Clone, Debug)]
struct Work {
    guild: GuildId,
    module: ModuleId,
    id: String,
    document: Value,
    deadline: Option<u64>,
}
fn select_work(work: &[Work], cursor: usize, now: u64, limit: usize) -> Vec<usize> {
    if work.is_empty() {
        return vec![];
    }
    let mut order: Vec<_> = (0..work.len())
        .map(|offset| (cursor + offset) % work.len())
        .collect();
    // Stable sorting preserves rotating fairness for identical deadlines.
    order.sort_by_key(|index| {
        work[*index]
            .deadline
            .filter(|d| *d <= now.saturating_add(60_000))
    });
    // None normally sorts first; move nonurgent work after urgent deadlines.
    order.sort_by_key(|index| {
        work[*index]
            .deadline
            .is_none_or(|d| d > now.saturating_add(60_000))
    });
    order.truncate(limit);
    order
}
async fn pending_work(
    shared: &SharedCards,
    active: &BTreeSet<(GuildId, ModuleId, String)>,
) -> Result<Vec<Work>> {
    let host = shared.host()?;
    let mut work = Vec::new();
    let mut scopes = BTreeSet::new();
    let now = now_ms()?;
    for binding in &shared.destinations {
        if binding.destination != "runs"
            || binding.module.as_str() != "community.dandys-world"
            || !scopes.insert((binding.guild.clone(), binding.module.clone()))
        {
            continue;
        }
        let mut configuration_checked = false;
        for id in shared
            .journal
            .run_ids(&binding.guild, &binding.module)
            .await?
        {
            if active.contains(&(binding.guild.clone(), binding.module.clone(), id.clone())) {
                continue;
            }
            let Some(document) = host
                .storage
                .document_get(&binding.module, &binding.guild, "runs", &id)
                .await?
            else {
                continue;
            };
            if document.value["reminder"].is_null() {
                continue;
            }
            if !configuration_checked {
                let source = match host
                    .modules
                    .shared_card_source(&binding.guild, &binding.module, &id)
                    .await
                {
                    Ok(source) => source,
                    Err(_) => continue,
                };
                if !configured(&source.configuration) {
                    break;
                }
                configuration_checked = true;
            }
            let key = format!("reminders:{}:{id}", binding.module);
            let journal: Journal = host
                .storage
                .workflow_get(&binding.guild, WorkflowKind::SharedCard, &key)
                .await?
                .map(|row| serde_json::from_value(row.value))
                .transpose()
                .map_err(|_| invalid())?
                .unwrap_or_default();
            let intent: Intent = match serde_json::from_value(document.value["reminder"].clone()) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let effect_key = format!("{}:{}", intent.starts_at, intent.kind);
            let Some(delivery) = journal.deliveries.get(&effect_key) else {
                continue;
            };
            if delivery.settled
                || (delivery.retryable && now > intent.expires_at)
                || delivery.coverage_unknown
                || (!delivery.request.attendance && delivery.message_id.is_some())
            {
                continue;
            }
            if !matches!(
                document.value["run"]["state"].as_str(),
                Some("open" | "locked")
            ) {
                continue;
            }
            work.push(Work {
                guild: binding.guild.clone(),
                module: binding.module.clone(),
                id,
                document: document.value,
                deadline: delivery.deadline.or_else(|| {
                    delivery
                        .request
                        .attendance
                        .then_some(intent.not_before.saturating_add(60_000))
                }),
            });
        }
    }
    Ok(work)
}
/// Independent of module RPC deadlines and shared-card reconciliation. Pending
/// task futures are dropped on shutdown; prepared sends remain recovery-only.
pub(super) async fn run(shared: Arc<SharedCards>, cancel: CancellationToken) -> Result<()> {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut running = tokio::task::JoinSet::new();
    let mut active = BTreeSet::new();
    let mut task_keys = HashMap::new();
    let mut cursor = 0usize;
    loop {
        tokio::select! {biased;
            _=cancel.cancelled()=>return Ok(()),
            Some(finished)=running.join_next_with_id(),if !running.is_empty()=>{
                let task_id = match finished { Ok((id,()))=>id,Err(error)=>error.id() };
                if let Some(key)=task_keys.remove(&task_id) {active.remove(&key);}
            },
            _=interval.tick()=>{
                let scan=tokio::select! {biased;
                    _=cancel.cancelled()=>return Ok(()),
                    result=pending_work(&shared,&active)=>result,
                };
                let work=match scan {
                    Ok(work)=>work,
                    Err(error)=>{tracing::debug!(error=?error.code,"reminder scan deferred");continue;}
                };
                let selected=select_work(&work,cursor,now_ms()?,4usize.saturating_sub(running.len()));
                if !work.is_empty() {cursor=(cursor+selected.len().max(1))%work.len();}
                for index in selected {
                    let job=work[index].clone();let shared=shared.clone();let stop=cancel.child_token();
                    let key=(job.guild.clone(),job.module.clone(),job.id.clone());active.insert(key.clone());
                    let handle = running.spawn(async move {
                        let deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(20);
                        let write_deadline=deadline-std::time::Duration::from_secs(5);
                        let result=tokio::select! {biased;
                            _=stop.cancelled()=>Err(Error::new(ErrorCode::Cancelled)),
                            result=tokio::time::timeout_at(deadline,process_inner(&shared,&job.module,&job.guild,&job.id,job.document,Some(write_deadline)))=>result.unwrap_or_else(|_|Err(Error::new(ErrorCode::Cancelled))),
                        };
                        if let Err(error)=result {tracing::debug!(error=?error.code,run=%job.id,"reminder delivery deferred");}
                    });
                    task_keys.insert(handle.id(),key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn work(deadline: Option<u64>) -> Work {
        Work {
            guild: GuildId::new("100").unwrap(),
            module: ModuleId::new("community.dandys-world").unwrap(),
            id: "ABCD2345".into(),
            document: Value::Null,
            deadline,
        }
    }
    #[test]
    fn scheduler_rotates_equal_priority_work_and_bounds_concurrency() {
        let jobs = vec![work(None); 12];
        assert_eq!(select_work(&jobs, 0, 100, 4), vec![0, 1, 2, 3]);
        assert_eq!(select_work(&jobs, 4, 100, 4), vec![4, 5, 6, 7]);
        assert_eq!(select_work(&jobs, 8, 100, 4), vec![8, 9, 10, 11]);
        assert!(select_work(&jobs, 0, 100, 0).is_empty());
        assert!(select_work(&[], 0, 100, 4).is_empty());
    }
    #[test]
    fn scheduler_prioritizes_near_cutoff_and_rotates_identical_deadlines() {
        let jobs = vec![
            work(None),
            work(Some(70_000)),
            work(Some(1_200)),
            work(Some(1_100)),
            work(Some(1_200)),
        ];
        assert_eq!(select_work(&jobs, 4, 1_000, 3), vec![3, 4, 2]);
        assert_eq!(select_work(&jobs, 2, 1_000, 3), vec![3, 2, 4]);
    }
    #[test]
    fn disabled_or_non_timezone_configuration_stops_worker() {
        assert!(configured(
            &json!({"reminders":{"timezone":"America/New_York"}})
        ));
        assert!(!configured(&json!({})));
        assert!(!configured(
            &json!({"reminders":{"role_id":"123","timezone":"UTC"}})
        ));
        assert!(!configured(&json!({"reminders":{"timezone":""}})));
    }
    fn delivery() -> Delivery {
        Delivery {
            request: RunReminderMessage {
                guild: GuildId::new("100").unwrap(),
                channel: "200".into(),
                key: "frozen-key".into(),
                text: "Confirm".into(),
                users: vec!["300".into(), "400".into()],
                role: None,
                attendance: true,
            },
            expected: json!({"300":"pebble","400":"vee"}),
            retryable: false,
            message_id: Some("500".into()),
            delivered_at: Some(HOUR),
            deadline: Some(4 * HOUR),
            seeded_at: Some(HOUR + 1000),
            confirmed: BTreeSet::new(),
            settled: false,
            coverage_unknown: false,
        }
    }
    fn users(ids: &[&str]) -> Result<BTreeSet<String>> {
        Ok(ids.iter().map(|id| (*id).into()).collect())
    }
    #[test]
    fn attendance_keeps_early_confirmations_but_requires_complete_cutoff_read() {
        let mut d = delivery();
        record_attendance(&mut d, users(&["300", "999"]), 2 * HOUR, 5 * HOUR).unwrap();
        assert_eq!(d.confirmed, BTreeSet::from(["300".into()]));
        assert!(!d.settled);
        record_attendance(&mut d, users(&["400"]), 4 * HOUR + 30_000, 5 * HOUR).unwrap();
        assert!(d.settled);
        assert!(!d.coverage_unknown);
        assert_eq!(d.confirmed.len(), 2);
    }
    #[test]
    fn failed_or_late_cutoff_read_never_proves_absence() {
        for (read, at) in [(Err(invalid()), 4 * HOUR), (users(&[]), 4 * HOUR + 60_001)] {
            let mut d = delivery();
            record_attendance(&mut d, read, at, 5 * HOUR).unwrap();
            assert!(d.coverage_unknown);
            assert!(!d.settled);
        }
        let mut d = delivery();
        record_attendance(&mut d, Err(invalid()), 2 * HOUR, 5 * HOUR).unwrap();
        assert!(!d.coverage_unknown);
        assert!(!d.settled);
    }
    #[test]
    fn missing_or_late_seed_never_shortens_the_confirmation_window() {
        for seeded in [None, Some(HOUR + 60_001)] {
            let mut d = delivery();
            d.seeded_at = seeded;
            record_attendance(&mut d, users(&[]), 4 * HOUR, 5 * HOUR).unwrap();
            assert!(d.coverage_unknown);
            assert!(!d.settled);
        }
    }
    #[test]
    fn announcements_require_current_participants_and_host_and_never_allow_roles() {
        let mut request = delivery().request;
        request.attendance = false;
        let run = json!({"schedule":{"starts_at":1000},"state":"open",
            "owner_id":"300","assignments":{"400":"vee"}});
        assert!(validate_current_run(&request, &run, 1000, true, 15, 10..=20).is_ok());
        request.users = vec!["400".into()];
        assert!(validate_current_run(&request, &run, 1000, true, 15, 10..=20).is_err());
        request.users = vec!["300".into(), "400".into(), "500".into()];
        assert!(validate_current_run(&request, &run, 1000, true, 15, 10..=20).is_err());
        request.users = vec!["300".into(), "400".into()];
        request.role = Some("600".into());
        assert!(validate_current_run(&request, &run, 1000, true, 15, 10..=20).is_err());
        assert!(validate_current_run(&request, &run, 1000, false, 15, 10..=20).is_err());
    }
    #[test]
    fn freshness_fences_rescheduling_terminal_state_and_stale_unsent_rosters() {
        let d = delivery();
        let mut run = json!({"schedule":{"starts_at":1000},"state":"open",
            "owner_id":"300","assignments":{"300":"pebble","400":"vee"}});
        assert!(validate_current_run(&d.request, &run, 1000, true, 15, 10..=20).is_ok());
        run["assignments"]["600"] = json!("cosmo");
        assert!(validate_current_run(&d.request, &run, 1000, true, 15, 10..=20).is_err());
        assert!(validate_current_run(&d.request, &run, 1000, false, 25, 10..=20).is_ok());
        run["schedule"]["starts_at"] = json!(1001);
        assert!(validate_current_run(&d.request, &run, 1000, false, 15, 10..=20).is_err());
        run["schedule"]["starts_at"] = json!(1000);
        run["state"] = json!("cancelled");
        assert!(validate_current_run(&d.request, &run, 1000, false, 15, 10..=20).is_err());
    }
    #[test]
    fn legacy_uncertainty_is_never_reinterpreted_as_permission_to_resend() {
        let d = delivery();
        let mut value = serde_json::to_value(&d).unwrap();
        value.as_object_mut().unwrap().remove("retryable");
        value.as_object_mut().unwrap().remove("seeded_at");
        let recovered: Delivery = serde_json::from_value(value).unwrap();
        assert!(!recovered.retryable);
        assert_eq!(recovered.seeded_at, None);
        assert_eq!(recovered.request, d.request);
    }
}
