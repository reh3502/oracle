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
    pub role_id: String,
    pub timezone: String,
}
impl ReminderConfiguration {
    pub fn validate(&self) -> Result<Tz, Error> {
        if !valid_member_id(&self.role_id) {
            return Err(Error::InvalidInput);
        }
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
                !self.confirmed.contains(*id) && run.assignments.get(*id) == Some(*assignment)
            })
            .map(|(id, assignment)| (id.clone(), assignment.clone()))
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
