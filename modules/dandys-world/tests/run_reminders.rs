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

#[test]
fn benching_frees_places_preserves_toons_and_requires_host_restore() {
    use dandys_world_core::runs::domain::{Command, Error, apply, bench_absent};
    let run = run(ms(2026, 9, 19, 0));
    let start = run.schedule.as_ref().unwrap().starts_at;
    let deadline = start as u64 * 1000 - HOUR_MS;
    let confirmed = BTreeSet::from(["1".into(), "3".into()]);
    let next = bench_absent(
        &run,
        &run.assignments,
        &confirmed,
        start,
        deadline,
        deadline,
    )
    .unwrap();
    assert_eq!(next.assignments.len(), 2);
    assert_eq!(next.bench.get("2"), run.assignments.get("2"));
    assert_eq!(next.desired_card_revision, run.desired_card_revision + 1);
    assert_eq!(
        bench_absent(
            &next,
            &run.assignments,
            &confirmed,
            start,
            deadline,
            deadline
        )
        .unwrap(),
        next
    );
    let member = Actor {
        guild_id: "100".into(),
        user_id: "2".into(),
        manage_all_runs: false,
    };
    assert_eq!(
        apply(
            &next,
            &member,
            &Command::Join { toon: None },
            &run.eligibility,
            deadline + 1
        ),
        Err(Error::Benched)
    );
    assert_eq!(
        apply(
            &next,
            &member,
            &Command::Restore {
                member_id: "2".into(),
                toon: None
            },
            &run.eligibility,
            deadline + 1
        ),
        Err(Error::Forbidden)
    );
    let host = Actor {
        user_id: "1".into(),
        ..member
    };
    let restored = apply(
        &next,
        &host,
        &Command::Restore {
            member_id: "2".into(),
            toon: None,
        },
        &run.eligibility,
        deadline + 1,
    )
    .unwrap()
    .0;
    assert!(restored.bench.is_empty());
    assert_eq!(restored.assignments.len(), 3);
}

#[test]
fn every_reminder_only_pings_roster_and_host() {
    use dandys_world_core::runs::reminders::intent;
    let run = run(ms(2026, 9, 19, 0));
    let times = ReminderTimes::for_run(&run, &config()).unwrap().unwrap();
    let announce = intent(&run, &config(), times.tomorrow, false)
        .unwrap()
        .unwrap();
    assert!(announce.role_id.is_none());
    assert_eq!(announce.users, vec!["1", "2", "3"]);
    assert!(announce.text.contains("5 places still open"));
    let attendance = intent(&run, &config(), times.attendance, false)
        .unwrap()
        .unwrap();
    assert!(attendance.role_id.is_none());
    assert_eq!(attendance.users, vec!["1", "2", "3"]);
    assert!(attendance.text.contains("within 3 hours"));
}

#[test]
fn migration_preserves_saved_run_and_starts_with_no_bench_or_reminder() {
    use dandys_world_core::runs::storage::{DATA_VERSION, StoredRun, migrate_v4_document};
    use oracle_contracts::ModuleDocument;
    let run = run(ms(2026, 9, 19, 0));
    let old = StoredRun {
        schema_version: 4,
        run: run.clone(),
        publication: None,
        reminder: None,
        moderator_audit: vec![],
    };
    let migrated = migrate_v4_document(ModuleDocument {
        collection: "runs".into(),
        key: run.id.clone(),
        revision: 7,
        value: serde_json::to_value(old).unwrap(),
    })
    .unwrap();
    assert_eq!(migrated.expected_revision, Some(7));
    let current: StoredRun = serde_json::from_value(migrated.value.unwrap()).unwrap();
    assert_eq!(current.schema_version, DATA_VERSION);
    assert_eq!(current.run, run);
    assert!(current.reminder.is_none());
    assert!(current.run.bench.is_empty());
}

#[test]
fn switching_toons_does_not_evade_attendance_and_bench_keeps_current_toon() {
    use dandys_world_core::runs::domain::bench_absent;
    let mut run = run(ms(2026, 9, 19, 0));
    let expected = run.assignments.clone();
    run.assignments.get_mut("2").unwrap().toon = Some("poppy".into());
    let start = run.schedule.as_ref().unwrap().starts_at;
    let deadline = start as u64 * 1000 - HOUR_MS;
    let next = bench_absent(
        &run,
        &expected,
        &BTreeSet::from(["1".into(), "3".into()]),
        start,
        deadline,
        deadline,
    )
    .unwrap();
    assert_eq!(next.bench["2"].toon, Some("poppy".into()));
    assert!(!next.assignments.contains_key("2"));
}
