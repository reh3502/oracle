use chrono::{TimeZone, Utc};
use dandys_world_core::runs::{
    domain::{Actor, Assignment, EligibilitySnapshot, Run, RunMode, RunSchedule, RunState},
    reminders::{
        Attendance, HOUR_MS, ReminderConfiguration, ReminderKind, ReminderTimes,
        participant_recipients,
    },
};
use std::collections::{BTreeMap, BTreeSet};
fn ms(year: i32, month: u32, day: u32, hour: u32) -> u64 {
    Utc.with_ymd_and_hms(year, month, day, hour, 0, 0)
        .unwrap()
        .timestamp_millis() as u64
}
fn config() -> ReminderConfiguration {
    ReminderConfiguration {
        role_id: "123".into(),
        timezone: "America/New_York".into(),
    }
}
fn run(start: u64) -> Run {
    let mut run = Run::new(
        "ABCD2345".into(),
        &Actor {
            guild_id: "100".into(),
            user_id: "1".into(),
            manage_all_runs: false,
        },
        RunMode::Casual,
        None,
        EligibilitySnapshot {
            source_hash: "a".repeat(64),
            source_revisions: BTreeMap::from([("Toons".into(), 1)]),
            toons: BTreeMap::from([("poppy".into(), "Poppy".into())]),
            observed_at: 0,
            fresh_until: u64::MAX,
            disputed: false,
        },
        1000,
    )
    .unwrap();
    run.schedule = Some(RunSchedule {
        starts_at: (start / 1000) as i64,
        duration_minutes: 90,
        timezone: Some("America/New_York".into()),
    });
    run.state = RunState::Open;
    run.assignments = BTreeMap::from([
        (
            "1".into(),
            Assignment {
                toon: None,
                joined_at: 1000,
            },
        ),
        (
            "2".into(),
            Assignment {
                toon: None,
                joined_at: 1000,
            },
        ),
        (
            "3".into(),
            Assignment {
                toon: None,
                joined_at: 1000,
            },
        ),
    ]);
    run
}
#[test]
fn friday_eight_pm_uses_monday_noon_and_absolute_offsets_across_dst() {
    for (start, monday) in [
        (ms(2026, 9, 19, 0), ms(2026, 9, 14, 16)),
        (ms(2026, 11, 7, 1), ms(2026, 11, 2, 17)),
        (ms(2026, 3, 14, 0), ms(2026, 3, 9, 16)),
    ] {
        let run = run(start);
        let times = ReminderTimes::for_run(&run, &config()).unwrap().unwrap();
        assert_eq!(times.signups_open, monday);
        assert_eq!(times.tomorrow, start - 24 * HOUR_MS);
        assert_eq!(times.attendance, start - 4 * HOUR_MS);
        assert_eq!(times.attendance_deadline, start - HOUR_MS);
        assert_eq!(times.due(&run, monday - 1), None);
        assert_eq!(times.due(&run, monday), Some(ReminderKind::SignupsOpen));
        assert_eq!(
            times.due(&run, times.tomorrow),
            Some(ReminderKind::Tomorrow)
        );
        assert_eq!(
            times.due(&run, times.attendance),
            Some(ReminderKind::Attendance)
        );
        assert_eq!(times.due(&run, start - 3 * HOUR_MS + 1), None);
        assert_eq!(times.due(&run, start), None);
    }
}
#[test]
fn closed_or_unscheduled_runs_never_announce() {
    let mut run = run(ms(2026, 9, 19, 0));
    let times = ReminderTimes::for_run(&run, &config()).unwrap().unwrap();
    for state in [RunState::Draft, RunState::Completed, RunState::Cancelled] {
        run.state = state;
        for now in [times.signups_open, times.tomorrow, times.attendance] {
            assert_eq!(times.due(&run, now), None);
        }
    }
    run.state = RunState::Locked;
    assert_eq!(times.due(&run, times.signups_open), None);
    assert_eq!(
        times.due(&run, times.tomorrow),
        Some(ReminderKind::Tomorrow)
    );
    run.schedule = None;
    assert_eq!(ReminderTimes::for_run(&run, &config()).unwrap(), None);
}
#[test]
fn attendance_requires_delivery_and_tracks_only_the_original_signup() {
    let mut run = run(ms(2026, 9, 19, 0));
    let start = run.schedule.as_ref().unwrap().starts_at as u64 * 1000;
    assert!(Attendance::delivered(&run, "900".into(), start - 2 * HOUR_MS).is_err());
    let mut check = Attendance::delivered(&run, "900".into(), start - 4 * HOUR_MS).unwrap();
    check.observe(
        &BTreeSet::from(["1".into(), "999".into()]),
        check.delivered_at,
    );
    check.observe(&BTreeSet::new(), check.delivered_at + 1);
    assert_eq!(check.confirmed, BTreeSet::from(["1".into()]));
    assert!(check.missing(&run, check.deadline - 1).is_empty());
    run.assignments.get_mut("3").unwrap().joined_at += 1;
    assert_eq!(
        check
            .missing(&run, check.deadline)
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["2"]
    );
    check.observe(&BTreeSet::from(["2".into()]), check.deadline + 1);
    assert!(!check.confirmed.contains("2"));
    run.schedule.as_mut().unwrap().starts_at += 3600;
    assert!(check.missing(&run, check.deadline).is_empty());
}
#[test]
fn participant_ping_includes_host_even_when_not_signed_up_and_no_duplicates() {
    let mut run = run(ms(2026, 9, 19, 0));
    assert_eq!(
        participant_recipients(&run),
        BTreeSet::from(["1".into(), "2".into(), "3".into()])
    );
    run.assignments.remove("1");
    assert!(participant_recipients(&run).contains("1"));
}
