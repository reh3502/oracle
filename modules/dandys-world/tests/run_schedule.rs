use dandys_world_core::runs::{
    domain::*,
    storage::{self, PublicationIntent, StoredRun},
    ui,
};
use oracle_contracts::ModuleDocument;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const NOW: u64 = 1_800_000_000_000;
fn owner() -> Actor {
    Actor {
        guild_id: "123".into(),
        user_id: "7".into(),
        manage_all_runs: false,
    }
}
fn schedule() -> RunSchedule {
    RunSchedule {
        starts_at: (NOW / 1000) as i64 + 3600,
        duration_minutes: 90,
        timezone: Some("America/New_York".into()),
    }
}
fn draft() -> Run {
    Run::new(
        "abcd2345".into(),
        &owner(),
        RunMode::Casual,
        None,
        EligibilitySnapshot {
            source_hash: "a".repeat(64),
            source_revisions: BTreeMap::from([("Toons".into(), 1)]),
            toons: BTreeMap::from([("pebble".into(), "Pebble".into())]),
            observed_at: NOW - 1000,
            fresh_until: NOW + 86_400_000,
            disputed: false,
        },
        NOW,
    )
    .unwrap()
}
fn change(run: &Run, command: Command) -> Run {
    apply(run, &owner(), &command, &run.eligibility, NOW + 1)
        .unwrap()
        .0
}
fn open() -> Run {
    let run = change(
        &draft(),
        Command::SetSchedule {
            schedule: schedule(),
        },
    );
    change(&run, Command::Publish)
}
fn field<'a>(card: &'a Value, name: &str) -> &'a Value {
    &card["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap()["value"]
}

#[test]
fn new_runs_require_a_future_schedule_at_save_and_publish() {
    let run = draft();
    assert_eq!(
        apply(&run, &owner(), &Command::Publish, &run.eligibility, NOW),
        Err(Error::ScheduleRequired)
    );
    for starts_at in [(NOW / 1000) as i64 - 1, (NOW / 1000) as i64] {
        let expired = RunSchedule {
            starts_at,
            ..schedule()
        };
        assert_eq!(
            apply(
                &run,
                &owner(),
                &Command::SetSchedule {
                    schedule: expired.clone()
                },
                &run.eligibility,
                NOW
            ),
            Err(Error::StartInPast)
        );
        let saved = Run {
            schedule: Some(expired),
            ..run.clone()
        };
        assert_eq!(
            apply(&saved, &owner(), &Command::Publish, &saved.eligibility, NOW),
            Err(Error::StartInPast)
        );
    }
    assert_eq!(open().state, RunState::Open);
    assert_eq!(run.schedule, None);
}

#[test]
fn only_managers_can_schedule_and_terminal_runs_cannot_be_rescheduled() {
    let run = open();
    let stranger = Actor {
        user_id: "8".into(),
        ..owner()
    };
    assert_eq!(
        apply(
            &run,
            &stranger,
            &Command::SetSchedule {
                schedule: schedule()
            },
            &run.eligibility,
            NOW + 1
        ),
        Err(Error::Forbidden)
    );
    let locked = change(&run, Command::Lock);
    let completed = change(&locked, Command::Complete);
    let cancelled = change(
        &run,
        Command::Cancel {
            confirmation: Confirmation {
                actor_id: owner().user_id,
                revision: run.desired_card_revision,
            },
        },
    );
    for terminal in [completed, cancelled] {
        assert_eq!(
            apply(
                &terminal,
                &owner(),
                &Command::SetSchedule {
                    schedule: schedule()
                },
                &terminal.eligibility,
                NOW + 1
            ),
            Err(Error::Closed)
        );
    }
}

#[test]
fn changing_schedule_preserves_signups_and_updates_public_card() {
    let run = open();
    let member = Actor {
        user_id: "8".into(),
        ..owner()
    };
    let run = apply(
        &run,
        &member,
        &Command::Join {
            toon: Some("pebble".into()),
        },
        &run.eligibility,
        NOW + 1,
    )
    .unwrap()
    .0;
    let replacement = RunSchedule {
        starts_at: 1_800_007_200,
        duration_minutes: 125,
        timezone: Some("UTC".into()),
    };
    let updated = change(
        &run,
        Command::SetSchedule {
            schedule: replacement.clone(),
        },
    );
    assert_eq!(updated.assignments, run.assignments);
    assert_eq!(updated.state, run.state);
    assert_eq!(updated.desired_card_revision, run.desired_card_revision + 1);
    assert_eq!(updated.schedule, Some(replacement));
    let (card, _) = ui::public_projection(&updated);
    assert_eq!(field(&card, "Starts"), "<t:1800007200:F>\n<t:1800007200:R>");
    assert_eq!(field(&card, "Estimated duration"), "2 h 5 min");
}

#[test]
fn migration_preserves_legacy_signups_without_inventing_schedule() {
    let mut run = open();
    run.schedule = None;
    let old_revision = run.desired_card_revision;
    let stored = StoredRun {
        schema_version: 3,
        moderator_audit: vec![],
        publication: Some(PublicationIntent {
            key: run.id.clone(),
            desired_revision: old_revision,
            repost_generation: 2,
            destination: "runs".into(),
            created_at: NOW + 1,
            card: json!({"title":"legacy public card"}),
            actions: vec![],
        }),
        run: run.clone(),
    };
    let mut value = serde_json::to_value(stored).unwrap();
    value["run"].as_object_mut().unwrap().remove("schedule");
    let write = storage::migrate_v3_document(ModuleDocument {
        collection: "runs".into(),
        key: run.id.clone(),
        revision: 19,
        value,
    })
    .unwrap();
    assert_eq!(write.expected_revision, Some(19));
    let migrated: StoredRun = serde_json::from_value(write.value.clone().unwrap()).unwrap();
    assert_eq!(migrated.schema_version, 4);
    assert_eq!(migrated.run.schedule, None);
    assert_eq!(migrated.run.assignments, run.assignments);
    assert_eq!(migrated.run.eligibility, run.eligibility);
    assert_eq!(migrated.run.state, RunState::Open);
    assert_eq!(migrated.run.desired_card_revision, old_revision + 1);
    let publication = migrated.publication.unwrap();
    assert_eq!(publication.desired_revision, old_revision + 1);
    assert_eq!(publication.repost_generation, 2);
    assert_eq!(field(&publication.card, "Starts"), "Not set");
    assert_eq!(field(&publication.card, "Estimated duration"), "Not set");
    assert_eq!(publication.card, ui::public_projection(&migrated.run).0);
    assert!(matches!(
        storage::migrate_v3_document(ModuleDocument {
            collection: "runs".into(),
            key: run.id,
            revision: 20,
            value: write.value.unwrap()
        }),
        Err(storage::Error::Corrupt)
    ));
}

#[test]
fn draft_and_maintenance_migrations_preserve_data_and_reject_wrong_versions() {
    let run = draft();
    let stored = StoredRun {
        schema_version: 3,
        moderator_audit: vec![],
        publication: None,
        run: run.clone(),
    };
    let write = storage::migrate_v3_document(ModuleDocument {
        collection: "runs".into(),
        key: run.id.clone(),
        revision: 4,
        value: serde_json::to_value(stored).unwrap(),
    })
    .unwrap();
    let migrated: StoredRun = serde_json::from_value(write.value.unwrap()).unwrap();
    assert_eq!(migrated.run, run);
    assert_eq!(migrated.publication, None);
    let value = json!({"version":3,"cursor":"unchanged"});
    let upgraded = storage::migrate_v3_document(ModuleDocument {
        collection: "run_index".into(),
        key: "maintenance".into(),
        revision: 8,
        value,
    })
    .unwrap();
    assert_eq!(
        upgraded.value,
        Some(json!({"version":4,"cursor":"unchanged"}))
    );
    assert!(matches!(
        storage::migrate_v3_document(ModuleDocument {
            collection: "run_index".into(),
            key: "maintenance".into(),
            revision: 9,
            value: upgraded.value.unwrap()
        }),
        Err(storage::Error::Corrupt)
    ));
}

#[test]
fn editing_duration_preserves_a_pasted_start_with_seconds() {
    let mut run = open();
    run.schedule.as_mut().unwrap().starts_at = 1_800_003_630;
    let stored = StoredRun {
        schema_version: storage::DATA_VERSION,
        moderator_audit: vec![],
        publication: None,
        run,
    };
    let reply = ui::render(
        &stored,
        &owner(),
        &ui::Input::show(&stored.run.id, ui::View::Manage),
        None,
    );
    let prompt = &reply["buttons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|button| button["input"]["action"] == "set_schedule")
        .unwrap()["prompt"];
    let parsed = dandys_world_core::runs::schedule::parse_start(
        prompt["value"].as_str().unwrap(),
        prompt["additional_fields"][0]["value"].as_str(),
    )
    .unwrap();
    assert_eq!(parsed.starts_at, 1_800_003_630);
    let updated = change(
        &stored.run,
        Command::SetSchedule {
            schedule: RunSchedule {
                starts_at: parsed.starts_at,
                duration_minutes: 120,
                timezone: parsed.timezone,
            },
        },
    );
    assert_eq!(updated.schedule.unwrap().starts_at, 1_800_003_630);
}
