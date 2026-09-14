//! Deterministic reminder timing and attendance rules; delivery is a host concern.
use super::domain::{Assignment, Error, Run, RunState, valid_member_id};
use chrono::{Datelike, Days, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const HOUR_MS: u64 = 3_600_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReminderConfiguration {
    pub timezone: String,
}
impl ReminderConfiguration {
    pub fn validate(&self) -> Result<Tz, Error> {
        self.timezone.parse().map_err(|_| Error::InvalidInput)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ReminderKind {
    SignupsOpen,
    Tomorrow,
    Attendance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderTimes {
    pub signups_open: u64,
    pub tomorrow: u64,
    pub attendance: u64,
    pub attendance_deadline: u64,
    pub starts_at: u64,
}
impl ReminderTimes {
    pub fn for_run(run: &Run, config: &ReminderConfiguration) -> Result<Option<Self>, Error> {
        let zone = config.validate()?;
        let Some(schedule) = &run.schedule else {
            return Ok(None);
        };
        schedule.validate()?;
        let start = Utc
            .timestamp_opt(schedule.starts_at, 0)
            .single()
            .ok_or(Error::InvalidInput)?
            .with_timezone(&zone);
        let date = start.date_naive();
        let monday = date
            .checked_sub_days(Days::new(u64::from(date.weekday().num_days_from_monday())))
            .ok_or(Error::InvalidInput)?;
        let mut noon = zone
            .from_local_datetime(&monday.and_hms_opt(12, 0, 0).ok_or(Error::InvalidInput)?)
            .single()
            .ok_or(Error::InvalidInput)?;
        if noon >= start {
            let previous = monday
                .checked_sub_days(Days::new(7))
                .ok_or(Error::InvalidInput)?;
            noon = zone
                .from_local_datetime(&previous.and_hms_opt(12, 0, 0).ok_or(Error::InvalidInput)?)
                .single()
                .ok_or(Error::InvalidInput)?;
        }
        let starts_at = u64::try_from(start.timestamp_millis()).map_err(|_| Error::InvalidInput)?;
        Ok(Some(Self {
            signups_open: u64::try_from(noon.timestamp_millis())
                .map_err(|_| Error::InvalidInput)?,
            tomorrow: starts_at
                .checked_sub(24 * HOUR_MS)
                .ok_or(Error::InvalidInput)?,
            attendance: starts_at
                .checked_sub(4 * HOUR_MS)
                .ok_or(Error::InvalidInput)?,
            attendance_deadline: starts_at.checked_sub(HOUR_MS).ok_or(Error::InvalidInput)?,
            starts_at,
        }))
    }

    /// Expired announcements are skipped instead of replayed in a burst after downtime.
    /// A late attendance dispatch is permitted only if a full three-hour window remains.
    pub fn due(&self, run: &Run, now: u64) -> Option<ReminderKind> {
        if !matches!(run.state, RunState::Open | RunState::Locked) {
            return None;
        }
        if (self.attendance..=self.starts_at.saturating_sub(3 * HOUR_MS)).contains(&now) {
            Some(ReminderKind::Attendance)
        } else if (self.tomorrow..self.attendance).contains(&now) {
            Some(ReminderKind::Tomorrow)
        } else if run.state == RunState::Open && (self.signups_open..self.tomorrow).contains(&now) {
            Some(ReminderKind::SignupsOpen)
        } else {
            None
        }
    }
}

/// Snapshot the assignment identity, not just the user id: a later signup must not
/// be benched because an earlier signup failed to respond.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Attendance {
    pub starts_at: i64,
    pub message_id: String,
    pub delivered_at: u64,
    pub deadline: u64,
    pub expected: BTreeMap<String, Assignment>,
    pub confirmed: BTreeSet<String>,
    pub settled: bool,
}
impl Attendance {
    pub fn delivered(run: &Run, message_id: String, delivered_at: u64) -> Result<Self, Error> {
        let schedule = run.schedule.as_ref().ok_or(Error::ScheduleRequired)?;
        let starts_ms = u64::try_from(schedule.starts_at)
            .map_err(|_| Error::InvalidInput)?
            .checked_mul(1000)
            .ok_or(Error::InvalidInput)?;
        let deadline = delivered_at
            .checked_add(3 * HOUR_MS)
            .ok_or(Error::InvalidInput)?;
        if !valid_member_id(&message_id)
            || !matches!(run.state, RunState::Open | RunState::Locked)
            || delivered_at < starts_ms.saturating_sub(4 * HOUR_MS)
            || deadline > starts_ms
        {
            return Err(Error::InvalidInput);
        }
        Ok(Self {
            starts_at: schedule.starts_at,
            message_id,
            delivered_at,
            deadline,
            expected: run.assignments.clone(),
            confirmed: BTreeSet::new(),
            settled: false,
        })
    }

    /// Called with host-observed reaction users. Arbitrary operation JSON must
    /// never supply this evidence. Removing a reaction does not undo a check-in.
    pub fn observe(&mut self, users: &BTreeSet<String>, observed_at: u64) {
        if self.settled || observed_at < self.delivered_at || observed_at > self.deadline {
            return;
        }
        self.confirmed.extend(
            users
                .iter()
                .filter(|id| self.expected.contains_key(*id))
                .cloned(),
        );
    }

    pub fn missing(&self, run: &Run, now: u64) -> BTreeMap<String, Assignment> {
        if self.settled
            || now < self.deadline
            || !matches!(run.state, RunState::Open | RunState::Locked)
            || run.schedule.as_ref().map(|s| s.starts_at) != Some(self.starts_at)
        {
            return BTreeMap::new();
        }
        self.expected
            .iter()
            .filter(|(id, assignment)| {
                !self.confirmed.contains(*id)
                    && run
                        .assignments
                        .get(*id)
                        .is_some_and(|current| current.joined_at == assignment.joined_at)
            })
            .map(|(id, _)| (id.clone(), run.assignments[id].clone()))
            .collect()
    }
}

pub fn participant_recipients(run: &Run) -> BTreeSet<String> {
    run.assignments
        .keys()
        .cloned()
        .chain(std::iter::once(run.owner_id.clone()))
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReminderIntent {
    pub kind: ReminderKind,
    pub starts_at: i64,
    pub not_before: u64,
    pub expires_at: u64,
    pub role_id: Option<String>,
    pub users: Vec<String>,
    pub text: String,
}
pub fn intent(
    run: &Run,
    config: &ReminderConfiguration,
    now: u64,
    continue_attendance: bool,
) -> Result<Option<ReminderIntent>, Error> {
    let Some(times) = ReminderTimes::for_run(run, config)? else {
        return Ok(None);
    };
    let kind = if continue_attendance
        && matches!(run.state, RunState::Open | RunState::Locked)
        && now >= times.attendance
        && now < times.starts_at
    {
        Some(ReminderKind::Attendance)
    } else {
        times.due(run, now)
    };
    let Some(kind) = kind else {
        return Ok(None);
    };
    let start = run
        .schedule
        .as_ref()
        .ok_or(Error::ScheduleRequired)?
        .starts_at;
    let available = usize::from(run.capacity()).saturating_sub(run.assignments.len());
    let places = if run.state == RunState::Locked {
        "Signups are currently locked.".to_owned()
    } else if let Some(rows) = &run.allocations {
        let open: Vec<_> = rows
            .iter()
            .filter_map(|(toon, count)| {
                let occupied = run
                    .assignments
                    .values()
                    .filter(|a| a.toon.as_ref() == Some(toon))
                    .count();
                let free = usize::from(*count).saturating_sub(occupied);
                (free > 0).then(|| {
                    format!(
                        "{}: {free}",
                        run.eligibility.toons.get(toon).unwrap_or(toon)
                    )
                })
            })
            .collect();
        if open.is_empty() {
            "All Toon places are filled.".into()
        } else {
            format!("Open Toon places: {}.", open.join(", "))
        }
    } else {
        format!("{available} places still open.")
    };
    let (not_before, expires_at, message) = match kind {
        ReminderKind::SignupsOpen => (
            times.signups_open,
            times.tomorrow,
            format!(
                "Signups are open for {}!\nStarts <t:{start}:F> (<t:{start}:R>).\n{places}",
                run.name
            ),
        ),
        ReminderKind::Tomorrow => (
            times.tomorrow,
            times.attendance,
            format!(
                "{} is tomorrow: <t:{start}:F> (<t:{start}:R>).\n{places}",
                run.name
            ),
        ),
        ReminderKind::Attendance => (
            times.attendance,
            times.starts_at - 3 * HOUR_MS,
            format!(
                "Attendance check for {} — starts <t:{start}:F>.\nReact with ✅ within 3 hours of this message to keep your place. Players who do not respond will move to the bench.",
                run.name
            ),
        ),
    };
    Ok(Some(ReminderIntent {
        kind,
        starts_at: start,
        not_before,
        expires_at,
        role_id: None,
        users: participant_recipients(run).into_iter().collect(),
        text: format!("{message}\nRun {}", run.id),
    }))
}
