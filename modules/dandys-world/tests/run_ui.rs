use dandys_world_core::runs::{
    domain::*,
    storage::StoredRun,
    ui::{self, Input, View},
};
use serde_json::json;
use std::collections::BTreeMap;
fn owner() -> Actor {
    Actor {
        guild_id: "123".into(),
        user_id: "7".into(),
        manage_all_runs: false,
    }
}
fn draft(mode: RunMode) -> StoredRun {
    let eligibility = EligibilitySnapshot {
        source_hash: "a".repeat(64),
        source_revisions: BTreeMap::from([("Toons".into(), 1)]),
        toons: (0..40)
            .map(|i| (format!("toon{i:02}"), format!("Toon {i:02}")))
            .collect(),
        observed_at: 1000,
        fresh_until: 100000,
        disputed: false,
    };
    let mut run = Run::new("abcd2345".into(), &owner(), mode, None, eligibility, 2000).unwrap();
    run.schedule = Some(RunSchedule {
        starts_at: 4_070_908_800,
        duration_minutes: 90,
        timezone: Some("UTC".into()),
    });
    StoredRun {
        schema_version: 4,
        moderator_audit: vec![],
        publication: None,
        run,
    }
}
fn apply_input(stored: &mut StoredRun, value: serde_json::Value) {
    let input: Input = serde_json::from_value(value).unwrap();
    stored.run = apply(
        &stored.run,
        &owner(),
        &input.command().unwrap(),
        &stored.run.eligibility,
        stored.run.updated_at + 1,
    )
    .unwrap()
    .0;
}
#[test]
fn organized_paged_counts_host_review_and_post_keep_durable_rows() {
    let mut stored = draft(RunMode::Organized);
    let first = ui::render(
        &stored,
        &owner(),
        &Input::show(&stored.run.id, View::Summary),
        None,
    );
    assert_eq!(
        first["buttons"][0]["prompt"]["select"]["choices"]
            .as_array()
            .unwrap()
            .len(),
        25
    );
    apply_input(
        &mut stored,
        json!({"action":"set_count","id":"abcd2345","toon":"toon00","count":"2"}),
    );
    let mut page = Input::show(&stored.run.id, View::Toons);
    page.page = Some(1);
    let second = ui::render(&stored, &owner(), &page, None);
    assert_eq!(
        second["buttons"][0]["prompt"]["select"]["choices"]
            .as_array()
            .unwrap()
            .len(),
        15
    );
    assert_eq!(
        second["buttons"][0]["prompt"]["select"]["label"],
        "Toon · page 2 of 2"
    );
    let mut add = second["buttons"][0]["input"].clone();
    assert_eq!(add["page"], 1);
    add["toon"] = json!("toon30");
    add["count"] = json!("6");
    apply_input(&mut stored, add);
    let mut edit_page = Input::show(&stored.run.id, View::Toon);
    edit_page.page = Some(1);
    edit_page.toon = Some("toon30".into());
    let edit = ui::render(&stored, &owner(), &edit_page, None);
    for label in ["Set count", "Remove Toon"] {
        let button = edit["buttons"]
            .as_array()
            .unwrap()
            .iter()
            .find(|button| button["label"] == label)
            .unwrap();
        assert_eq!(button["input"]["page"], 1);
    }
    apply_input(
        &mut stored,
        json!({"action":"set_host","id":"abcd2345","toon":"toon30"}),
    );
    let review = ui::render(
        &stored,
        &owner(),
        &Input::show(&stored.run.id, View::Review),
        None,
    );
    assert!(
        review["buttons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["input"]["action"] == "post")
    );
    assert_eq!(
        stored.run.allocations.as_ref().unwrap().get("toon00"),
        Some(&2)
    );
    assert_eq!(
        stored.run.allocations.as_ref().unwrap().get("toon30"),
        Some(&6)
    );
    apply_input(&mut stored, json!({"action":"post","id":"abcd2345"}));
    assert_eq!(stored.run.assignments.len(), 1);
    assert_eq!(stored.run.capacity(), 8);
    assert_eq!(stored.run.assignments["7"].toon.as_deref(), Some("toon30"));
    let (card, actions) = ui::public_projection(&stored.run);
    assert_eq!(card["fields"][0]["members"][0]["user_id"], "7");
    assert_eq!(
        actions.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
        vec!["join", "leave", "players"]
    );
}
#[test]
fn casual_never_renders_count_forms_and_all_requires_confirmation() {
    let stored = draft(RunMode::Casual);
    for view in [
        View::Summary,
        View::Toons,
        View::Toon,
        View::Host,
        View::Review,
    ] {
        let card = ui::render(&stored, &owner(), &Input::show(&stored.run.id, view), None);
        let serialized = card.to_string();
        assert!(!serialized.contains("set_count"));
        assert!(!serialized.contains("set_host"));
    }
    let mut organized = draft(RunMode::Organized);
    apply_input(
        &mut organized,
        json!({"action":"set_count","id":"abcd2345","toon":"toon00","count":"8"}),
    );
    let before = organized.clone();
    let input: Input =
        serde_json::from_value(json!({"action":"confirm_all","id":"abcd2345"})).unwrap();
    assert!(matches!(
        apply(
            &organized.run,
            &owner(),
            &input.command().unwrap(),
            &organized.run.eligibility,
            3000
        ),
        Err(Error::ConfirmationRequired)
    ));
    assert_eq!(organized, before);
}

#[test]
fn casual_signup_and_switch_expose_every_requested_toon_across_named_pages() {
    let names = [
        "Blot",
        "Boxten",
        "Brightney",
        "Brusha",
        "Connie",
        "Cosmo",
        "Finn",
        "Flutter",
        "Gigi",
        "Glisten",
        "Goob",
        "Looey",
        "Poppy",
        "Razzle & Dazzle",
        "Rodger",
        "Scraps",
        "Shrimpo",
        "Squirm",
        "Teagan",
        "Tisha",
        "Toodles",
        "Waxwell",
        "Yatta",
        "Astro",
        "Bassie",
        "Bobette",
        "Gourdy",
        "Pebble",
        "Shelly",
        "Sprout",
        "Vee",
        "Cocoa",
        "Eggson",
        "Flyte",
        "Eclipse",
        "Ribecca",
        "Soulvester",
        "Coal",
        "Ginger",
        "Rudie",
    ];
    let mut stored = draft(RunMode::Casual);
    stored.run.eligibility.toons = names
        .iter()
        .enumerate()
        .map(|(i, name)| (format!("toon{i:02}"), (*name).into()))
        .collect();
    apply_input(&mut stored, json!({"action":"post","id":"abcd2345"}));
    let guest = Actor {
        user_id: "8".into(),
        ..owner()
    };
    for view in [View::Join, View::Switch] {
        let mut input = Input::show(&stored.run.id, view);
        let mut seen = std::collections::BTreeSet::new();
        for page in 0..2 {
            input.page = Some(page);
            let card = ui::render(&stored, &guest, &input, None);
            let description = card["card"]["description"].as_str().unwrap();
            assert!(description.contains(&format!("Page {} of 2", page + 1)));
            for choice in card["choices"].as_array().unwrap() {
                assert!(seen.insert(choice["label"].as_str().unwrap().to_owned()));
            }
            let label = if page == 0 {
                "Next Toons"
            } else {
                "Previous Toons"
            };
            let nav = card["buttons"]
                .as_array()
                .unwrap()
                .iter()
                .find(|button| button["label"] == label)
                .unwrap();
            assert_eq!(nav["input"]["page"], json!(1 - page));
        }
        assert_eq!(seen, names.iter().map(|name| (*name).to_owned()).collect());
    }
}
#[test]
fn modal_counts_and_actor_fields_are_strict_and_cards_stay_bounded() {
    for count in ["0", "9", "12", "1.0", "-1", " 2", "٢", ""] {
        let input: Input = serde_json::from_value(
            json!({"action":"set_count","id":"abcd2345","toon":"toon00","count":count}),
        )
        .unwrap();
        assert!(input.command().is_err(), "{count}");
    }
    assert!(
        serde_json::from_value::<Input>(json!({"action":"view","id":"abcd2345","actor_id":"7"}))
            .is_err()
    );
    let stored = draft(RunMode::Organized);
    for view in [
        View::Summary,
        View::Toons,
        View::Toon,
        View::Host,
        View::Review,
        View::Players,
        View::Join,
        View::Switch,
        View::Manage,
        View::Remove,
    ] {
        let card = ui::render(&stored, &owner(), &Input::show(&stored.run.id, view), None);
        assert!(card["choices"].as_array().unwrap().len() <= 25);
        assert!(card["buttons"].as_array().unwrap().len() <= 15);
    }
}

#[test]
fn full_places_are_visible_on_card_but_not_offered_for_signup() {
    let mut stored = draft(RunMode::Organized);
    apply_input(
        &mut stored,
        json!({"action":"set_count","id":"abcd2345","toon":"toon00","count":"1"}),
    );
    apply_input(
        &mut stored,
        json!({"action":"set_count","id":"abcd2345","toon":"toon30","count":"1"}),
    );
    apply_input(
        &mut stored,
        json!({"action":"set_host","id":"abcd2345","toon":"toon00"}),
    );
    apply_input(&mut stored, json!({"action":"post","id":"abcd2345"}));
    let guest = Actor {
        user_id: "8".into(),
        ..owner()
    };
    let join = ui::render(
        &stored,
        &guest,
        &Input::show(&stored.run.id, View::Join),
        None,
    );
    assert_eq!(join["choices"].as_array().unwrap().len(), 1);
    assert_eq!(join["choices"][0]["input"]["toon"], "toon30");
    let (card, _) = ui::public_projection(&stored.run);
    assert!(
        card["fields"][1]["value"]
            .as_str()
            .unwrap()
            .contains("Toon 00 · 1 / 1 filled · 0 available")
    );
    stored.run = apply(
        &stored.run,
        &guest,
        &Command::Join {
            toon: Some("toon30".into()),
        },
        &stored.run.eligibility,
        3000,
    )
    .unwrap()
    .0;
    let third = Actor {
        user_id: "9".into(),
        ..owner()
    };
    let full = ui::render(
        &stored,
        &third,
        &Input::show(&stored.run.id, View::Join),
        None,
    );
    assert!(full["choices"].as_array().unwrap().is_empty());
    assert!(
        full["card"]["description"]
            .as_str()
            .unwrap()
            .contains("full")
    );
    assert!(
        ui::public_projection(&stored.run).0["description"]
            .as_str()
            .unwrap()
            .contains("Full")
    );
}

#[test]
fn ordinary_members_never_receive_management_controls() {
    let stored = draft(RunMode::Organized);
    let guest = Actor {
        user_id: "8".into(),
        ..owner()
    };
    for view in [
        View::Toons,
        View::Toon,
        View::Host,
        View::Review,
        View::Manage,
        View::Remove,
    ] {
        let card = ui::render(&stored, &guest, &Input::show(&stored.run.id, view), None);
        assert!(card["choices"].as_array().unwrap().is_empty());
        assert!(
            card["buttons"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["input"]["action"] == "view")
        );
    }
}
