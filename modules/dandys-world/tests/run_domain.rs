use dandys_world_core::runs::domain::*;
use std::collections::BTreeMap;

fn actor(id: &str) -> Actor {
    Actor {
        guild_id: "100".into(),
        user_id: id.into(),
        manage_all_runs: false,
    }
}
fn catalog() -> EligibilitySnapshot {
    EligibilitySnapshot {
        source_hash: "a".repeat(64),
        source_revisions: BTreeMap::from([("Toons".into(), 42)]),
        toons: BTreeMap::from([
            ("pebble".into(), "Pebble".into()),
            ("poppy".into(), "Poppy".into()),
        ]),
        observed_at: 1_000,
        fresh_until: 20_000,
        disputed: false,
    }
}
fn new(mode: RunMode) -> Run {
    Run::new("ABCD2345".into(), &actor("1"), mode, None, catalog(), 2_000).unwrap()
}
fn step(run: &Run, who: &Actor, command: Command) -> Result<(Run, Outcome), Error> {
    apply(run, who, &command, &catalog(), run.updated_at + 1)
}
fn configured() -> Run {
    step(
        &new(RunMode::Organized),
        &actor("1"),
        Command::EditDraft {
            name: Some("Required Toons".into()),
            allocations: Some(BTreeMap::from([("pebble".into(), 1), ("poppy".into(), 7)])),
            host_toon: Some("pebble".into()),
        },
    )
    .unwrap()
    .0
}
fn published(mode: RunMode) -> Run {
    step(
        &if mode == RunMode::Organized {
            configured()
        } else {
            new(mode)
        },
        &actor("1"),
        Command::Publish,
    )
    .unwrap()
    .0
}
fn confirmation(run: &Run, actor: &Actor) -> Confirmation {
    Confirmation {
        actor_id: actor.user_id.clone(),
        revision: run.desired_card_revision,
    }
}

#[test]
fn publication_includes_host_and_both_modes_stop_at_eight() {
    for mode in [RunMode::Casual, RunMode::Organized] {
        let mut run = published(mode);
        assert_eq!(run.capacity(), 8);
        assert_eq!(run.assignments.len(), 1);
        assert_eq!(
            run.assignments["1"].toon,
            if mode == RunMode::Organized {
                Some("pebble".into())
            } else {
                None
            }
        );
        for user in 2..=8 {
            run = step(
                &run,
                &actor(&user.to_string()),
                Command::Join {
                    toon: Some("poppy".into()),
                },
            )
            .unwrap()
            .0;
        }
        assert_eq!(
            step(
                &run,
                &actor("9"),
                Command::Join {
                    toon: Some("poppy".into())
                }
            ),
            Err(Error::Full)
        );
        assert_eq!(run.assignments.len(), 8);
        if mode == RunMode::Casual {
            assert!(run.allocations.is_none());
        }
    }
}

#[test]
fn casual_choice_is_optional_and_never_a_quota() {
    let mut run = published(RunMode::Casual);
    for user in 2..=8 {
        run = step(
            &run,
            &actor(&user.to_string()),
            Command::Join { toon: None },
        )
        .unwrap()
        .0;
    }
    for user in 1..=8 {
        run = step(
            &run,
            &actor(&user.to_string()),
            Command::Switch {
                toon: Some("pebble".into()),
            },
        )
        .unwrap()
        .0;
    }
    assert!(run.allocations.is_none());
    assert!(
        run.assignments
            .values()
            .all(|a| a.toon.as_deref() == Some("pebble"))
    );
    run = step(&run, &actor("1"), Command::Switch { toon: None })
        .unwrap()
        .0;
    assert_eq!(run.assignments.len(), 8);
    assert!(run.assignments["1"].toon.is_none());
}

#[test]
fn failed_switch_preserves_old_slot_and_join_cannot_switch() {
    let run = step(
        &published(RunMode::Organized),
        &actor("2"),
        Command::Join {
            toon: Some("poppy".into()),
        },
    )
    .unwrap()
    .0;
    let original = run.clone();
    assert_eq!(
        step(
            &run,
            &actor("2"),
            Command::Switch {
                toon: Some("pebble".into())
            }
        ),
        Err(Error::Full)
    );
    assert_eq!(
        step(
            &run,
            &actor("2"),
            Command::Join {
                toon: Some("pebble".into())
            }
        ),
        Err(Error::AlreadyJoined)
    );
    assert_eq!(run, original);
    assert_eq!(run.assignments["2"].toon.as_deref(), Some("poppy"));
    assert_eq!(
        step(
            &run,
            &actor("3"),
            Command::Switch {
                toon: Some("poppy".into())
            }
        ),
        Err(Error::NotJoined)
    );
}

#[test]
fn repeats_are_noops_and_do_not_refresh_timestamps_or_revision() {
    let run = step(
        &published(RunMode::Casual),
        &actor("2"),
        Command::Join { toon: None },
    )
    .unwrap()
    .0;
    let (same, outcome) = step(&run, &actor("2"), Command::Join { toon: None }).unwrap();
    assert_eq!(same, run);
    assert!(!outcome.changed);
    let run = step(&run, &actor("2"), Command::Leave).unwrap().0;
    assert_eq!(step(&run, &actor("2"), Command::Leave).unwrap().0, run);
    assert_eq!(run.owner_id, "1");
}

#[test]
fn all_requires_actor_revision_confirmation_and_clears_only_after_success() {
    let run = configured();
    assert_eq!(
        step(
            &run,
            &actor("1"),
            Command::SetMode {
                mode: RunMode::Casual,
                confirmation: None
            }
        ),
        Err(Error::ConfirmationRequired)
    );
    assert_eq!(run.allocations.as_ref().unwrap().len(), 2);
    let bad = Confirmation {
        actor_id: "2".into(),
        revision: run.desired_card_revision,
    };
    assert_eq!(
        step(
            &run,
            &actor("1"),
            Command::SetMode {
                mode: RunMode::Casual,
                confirmation: Some(bad)
            }
        ),
        Err(Error::StaleConfirmation)
    );
    let run = step(
        &run,
        &actor("1"),
        Command::SetMode {
            mode: RunMode::Casual,
            confirmation: Some(confirmation(&run, &actor("1"))),
        },
    )
    .unwrap()
    .0;
    assert!(run.allocations.is_none());
    assert!(run.host_toon.is_none());
    assert_eq!(run.capacity(), 8);
    let run = step(&run, &actor("1"), Command::Publish).unwrap().0;
    assert_eq!(
        step(
            &run,
            &actor("1"),
            Command::SetMode {
                mode: RunMode::Organized,
                confirmation: None
            }
        ),
        Err(Error::ModeImmutable)
    );
    let renamed = step(
        &run,
        &actor("1"),
        Command::Rename {
            name: "organized".into(),
        },
    )
    .unwrap()
    .0;
    assert_eq!(renamed.mode, RunMode::Casual);
    assert!(renamed.allocations.is_none());
}

#[test]
fn publishing_requires_current_approved_selected_toons_and_keeps_pinned_evidence() {
    let run = configured();
    let mut current = catalog();
    current.toons.remove("pebble");
    assert_eq!(
        apply(&run, &actor("1"), &Command::Publish, &current, 3_000),
        Err(Error::CatalogChanged)
    );
    current = catalog();
    current.disputed = true;
    assert_eq!(
        apply(&run, &actor("1"), &Command::Publish, &current, 3_000),
        Err(Error::CatalogUnavailable)
    );
    current.disputed = false;
    current.fresh_until = 3_000;
    assert_eq!(
        apply(&run, &actor("1"), &Command::Publish, &current, 3_000),
        Err(Error::CatalogUnavailable)
    );
    current = catalog();
    current
        .toons
        .insert("pebble".into(), "Renamed Pebble".into());
    current.source_hash = "b".repeat(64);
    let run = apply(&run, &actor("1"), &Command::Publish, &current, 3_000)
        .unwrap()
        .0;
    assert_eq!(run.eligibility, catalog());
    current.toons.remove("poppy");
    current.disputed = true;
    let run = apply(
        &run,
        &actor("2"),
        &Command::Join {
            toon: Some("poppy".into()),
        },
        &current,
        50_000,
    )
    .unwrap()
    .0;
    assert_eq!(run.assignments["2"].toon.as_deref(), Some("poppy"));
    assert_eq!(run.eligibility.toons["pebble"], "Pebble");
}

#[test]
fn organized_requires_allocated_host_toon_and_rejects_shrink_below_occupancy() {
    let mut draft = configured();
    draft.host_toon = None;
    assert_eq!(
        step(&draft, &actor("1"), Command::Publish),
        Err(Error::ToonRequired)
    );
    draft.host_toon = Some("poppy".into());
    draft.allocations = Some(BTreeMap::from([("pebble".into(), 8)]));
    assert_eq!(
        step(&draft, &actor("1"), Command::Publish),
        Err(Error::UnallocatedToon)
    );
    let run = published(RunMode::Organized);
    assert_eq!(
        step(&run, &actor("2"), Command::Join { toon: None }),
        Err(Error::ToonRequired)
    );
    assert_eq!(
        step(
            &run,
            &actor("1"),
            Command::SetAllocations {
                allocations: BTreeMap::from([("poppy".into(), 8)])
            }
        ),
        Err(Error::BelowOccupancy)
    );
    assert_eq!(
        step(
            &run,
            &actor("1"),
            Command::SetAllocations {
                allocations: BTreeMap::from([("pebble".into(), 1), ("poppy".into(), 8)])
            }
        ),
        Err(Error::InvalidInput)
    );
    let run = step(&run, &actor("1"), Command::Leave).unwrap().0;
    let run = step(
        &run,
        &actor("1"),
        Command::SetAllocations {
            allocations: BTreeMap::from([("poppy".into(), 8)]),
        },
    )
    .unwrap()
    .0;
    assert!(run.assignments.is_empty());
    assert_eq!(run.owner_id, "1");
}

#[test]
fn lifecycle_owner_moderator_and_cross_guild_rules_are_enforced() {
    let run = published(RunMode::Casual);
    assert_eq!(
        step(&run, &actor("2"), Command::Lock),
        Err(Error::Forbidden)
    );
    let mut moderator = actor("2");
    moderator.manage_all_runs = true;
    moderator.guild_id = "200".into();
    assert_eq!(step(&run, &moderator, Command::Lock), Err(Error::Forbidden));
    assert_eq!(
        step(&run, &moderator, Command::Join { toon: None }),
        Err(Error::Forbidden)
    );
    moderator.guild_id = "100".into();
    let locked = step(&run, &moderator, Command::Lock).unwrap().0;
    assert_eq!(
        step(&locked, &actor("3"), Command::Join { toon: None }),
        Err(Error::Closed)
    );
    let left = step(&locked, &actor("1"), Command::Leave).unwrap().0;
    assert_eq!(left.state, RunState::Locked);
    assert_eq!(left.owner_id, "1");
    let open = step(&left, &actor("1"), Command::Reopen).unwrap().0;
    assert_eq!(open.state, RunState::Open);
    assert_eq!(
        step(&open, &actor("1"), Command::Complete),
        Err(Error::InvalidTransition)
    );
    let terminal = step(&locked, &moderator, Command::Complete).unwrap().0;
    assert!(terminal.state.is_terminal());
    assert_eq!(terminal.terminal_at, Some(terminal.updated_at));
    assert_eq!(
        step(&terminal, &actor("1"), Command::Reopen),
        Err(Error::InvalidTransition)
    );
    assert_eq!(
        step(&terminal, &actor("1"), Command::Leave),
        Err(Error::Closed)
    );
}

#[test]
fn destructive_actions_require_fresh_actor_bound_confirmation() {
    let run = published(RunMode::Casual);
    let approval = confirmation(&run, &actor("1"));
    let newer = step(&run, &actor("2"), Command::Join { toon: None })
        .unwrap()
        .0;
    assert_eq!(
        step(
            &newer,
            &actor("1"),
            Command::Cancel {
                confirmation: approval.clone()
            }
        ),
        Err(Error::StaleConfirmation)
    );
    assert_eq!(
        step(
            &newer,
            &actor("1"),
            Command::Remove {
                member_id: "2".into(),
                confirmation: approval
            }
        ),
        Err(Error::StaleConfirmation)
    );
    let removed = step(
        &newer,
        &actor("1"),
        Command::Remove {
            member_id: "2".into(),
            confirmation: confirmation(&newer, &actor("1")),
        },
    )
    .unwrap()
    .0;
    assert!(!removed.assignments.contains_key("2"));
    let cancelled = step(
        &removed,
        &actor("1"),
        Command::Cancel {
            confirmation: confirmation(&removed, &actor("1")),
        },
    )
    .unwrap()
    .0;
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(
        step(&cancelled, &actor("2"), Command::Join { toon: None }),
        Err(Error::Closed)
    );
}

#[test]
fn strict_input_and_corrupt_aggregate_bounds() {
    for payload in [
        r#"{"action":"join","toon":null,"actor":"1"}"#,
        r#"{"action":"leave","guild_id":"100"}"#,
        r#"{"action":"cancel","confirmation":{"actor_id":"1","revision":1,"privileged":true}}"#,
    ] {
        assert!(serde_json::from_str::<Command>(payload).is_err());
    }
    assert_eq!(normalize_run_id(" abcd2345 ").unwrap(), "ABCD2345");
    for id in ["ABCD234I", "ABCD2340", "ABC2345", "ABCDEFGHJ"] {
        assert!(normalize_run_id(id).is_err());
    }
    for id in ["", "01", "0", "-1", "18446744073709551616", "1\n"] {
        assert!(!valid_member_id(id));
    }
    let mut run = published(RunMode::Casual);
    run.allocations = Some(BTreeMap::new());
    assert_eq!(run.validate(), Err(Error::InvalidInput));
    assert_eq!(
        step(
            &published(RunMode::Casual),
            &actor("1"),
            Command::Rename {
                name: "x".repeat(81)
            }
        ),
        Err(Error::InvalidInput)
    );
}

#[test]
fn persisted_roundtrip_preserves_policy_and_moderators_do_not_extend_draft_expiry() {
    let run = configured();
    let mut moderator = actor("2");
    moderator.manage_all_runs = true;
    let edited = step(
        &run,
        &moderator,
        Command::Rename {
            name: "Moderator title".into(),
        },
    )
    .unwrap()
    .0;
    assert_eq!(edited.last_owner_edit_at, run.last_owner_edit_at);
    let edited = step(
        &edited,
        &actor("1"),
        Command::Rename {
            name: "Owner title".into(),
        },
    )
    .unwrap()
    .0;
    assert_eq!(edited.last_owner_edit_at, edited.updated_at);
    let published = step(&edited, &actor("1"), Command::Publish).unwrap().0;
    let bytes = serde_json::to_vec(&published).unwrap();
    let restored: Run = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored, published);
    restored.validate().unwrap();
}

#[test]
fn successful_switch_releases_exactly_one_slot_and_moderator_removal_keeps_owner() {
    let run = published(RunMode::Organized);
    let first_joined_at = run.assignments["1"].joined_at;
    let switched = step(
        &run,
        &actor("1"),
        Command::Switch {
            toon: Some("poppy".into()),
        },
    )
    .unwrap()
    .0;
    assert_eq!(switched.assignments.len(), 1);
    assert_eq!(switched.assignments["1"].joined_at, first_joined_at);
    let joined = step(
        &switched,
        &actor("2"),
        Command::Join {
            toon: Some("pebble".into()),
        },
    )
    .unwrap()
    .0;
    let mut moderator = actor("3");
    moderator.manage_all_runs = true;
    let removed = step(
        &joined,
        &moderator,
        Command::Remove {
            member_id: "1".into(),
            confirmation: confirmation(&joined, &moderator),
        },
    )
    .unwrap()
    .0;
    assert!(!removed.assignments.contains_key("1"));
    assert_eq!(removed.owner_id, "1");
    assert_eq!(
        step(&removed, &actor("1"), Command::Lock).unwrap().0.state,
        RunState::Locked
    );
}

#[test]
fn oversized_source_evidence_never_creates_an_oversized_aggregate() {
    let mut evidence = catalog();
    evidence.toons.clear();
    for i in 0..80 {
        evidence
            .toons
            .insert(format!("{i:02}{}", "界".repeat(98)), "界".repeat(100));
    }
    evidence.validate().unwrap();
    assert_eq!(
        Run::new(
            "ABCD2345".into(),
            &actor("1"),
            RunMode::Casual,
            None,
            evidence,
            2_000
        ),
        Err(Error::InvalidInput)
    );
    let run = published(RunMode::Organized);
    assert!(serde_json::to_vec(&run).unwrap().len() < MAX_RUN_BYTES);
    let mut corrupted = run;
    corrupted.allocations = Some(BTreeMap::from([
        ("pebble".into(), 255),
        ("poppy".into(), 255),
    ]));
    assert_eq!(corrupted.validate(), Err(Error::InvalidInput));
    assert_eq!(corrupted.capacity(), 255); // Cannot wrap and appear valid to callers.
}

#[test]
fn every_command_roundtrips_and_every_top_level_extra_field_is_rejected() {
    let run = configured();
    let approval = confirmation(&run, &actor("1"));
    let commands = [
        Command::EditDraft {
            name: None,
            allocations: None,
            host_toon: None,
        },
        Command::SetMode {
            mode: RunMode::Casual,
            confirmation: Some(approval.clone()),
        },
        Command::Publish,
        Command::Join { toon: None },
        Command::Switch {
            toon: Some("pebble".into()),
        },
        Command::Leave,
        Command::SetAllocations {
            allocations: BTreeMap::new(),
        },
        Command::Rename {
            name: "title".into(),
        },
        Command::Lock,
        Command::Reopen,
        Command::Complete,
        Command::Cancel {
            confirmation: approval.clone(),
        },
        Command::Remove {
            member_id: "2".into(),
            confirmation: approval,
        },
    ];
    for command in commands {
        let mut json = serde_json::to_value(&command).unwrap();
        assert_eq!(
            serde_json::from_value::<Command>(json.clone()).unwrap(),
            command
        );
        json.as_object_mut()
            .unwrap()
            .insert("guild_id".into(), serde_json::json!("200"));
        assert!(serde_json::from_value::<Command>(json).is_err());
    }
}
