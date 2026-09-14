use dandys_world_core::{
    model::*,
    runs::{
        domain::{Error, MAX_PLAYERS},
        eligibility::*,
    },
    snapshot::Snapshot,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const NOW: u64 = 1_800_000_000_000;
fn source(page_id: u64, title: &str, raw: &str) -> Source {
    Source {
        id: format!("page:{page_id}"),
        page_id,
        title: title.into(),
        url: format!("{SOURCE_ORIGIN}/wiki/{}", title.replace(' ', "_")),
        revision_id: page_id + 1,
        revision_timestamp: "2026-09-12T00:00:00Z".into(),
        validated_at_ms: NOW - 100,
        content_sha256: format!("{:x}", Sha256::digest(raw.as_bytes())),
        license: "CC-BY-SA-3.0".into(),
        license_url: "https://creativecommons.org/licenses/by-sa/3.0/".into(),
    }
}
fn entity(page_id: u64, kind: Kind, name: &str) -> Entity {
    Entity {
        id: format!("page:{page_id}"),
        kind,
        name: name.into(),
        aliases: vec![],
        availability: EvidenceState::Supported,
        warnings: vec![],
        facts: vec![],
        relationships: vec![],
    }
}
fn fixture() -> CatalogData {
    let raw = [
        (
            6643,
            "Template:RegularToons",
            "{{ToonBox|Poppy|type=regular}}",
        ),
        (
            31108,
            "Template:MCToons",
            "{{ToonBox|Pebble|type=main}}{{ToonBox|Bobette|type=christmasmain}}",
        ),
        (
            8886,
            "Template:EventToons",
            "{{ToonBox|Bobette|type=christmasmain}}{{ToonBox|Coal|type=christmas}}",
        ),
        (
            17070,
            "Template:UnobtainableToons",
            "{{ToonBox|Dandy|type=lethal}}{{ToonBox|Dyle|type=lethal}}",
        ),
    ];
    let mut sources: Vec<_> = raw
        .iter()
        .map(|(id, title, raw)| source(*id, title, raw))
        .collect();
    sources.extend([
        source(151, "Toons", "roster"),
        source(152, "Dandy's World (Game)", "eight players"),
        source(1545, "Template:ToonAmount", "4"),
        source(1, "Poppy", "poppy"),
        source(2, "Pebble", "pebble"),
        source(3, "Bobette", "bobette"),
        source(4, "Coal", "coal"),
        source(5, "Dandy", "dandy"),
        source(6, "Dyle", "dyle"),
        source(7, "Twisted Poppy", "twisted"),
        source(8, "Dev Toon", "developer-only"),
    ]);
    let mut toons = entity(151, Kind::Mechanic, "Toons");
    toons.facts.push(Fact {
        id: "roster".into(),
        key: "overview".into(),
        text: "Attributed complete roster".into(),
        value: serde_json::json!("roster"),
        unit: None,
        conditions: vec![],
        state: EvidenceState::Supported,
        citations: raw
            .iter()
            .map(|(id, _, raw)| Citation {
                source_id: format!("page:{id}"),
                section: "Template definition".into(),
                quote: (*raw).into(),
            })
            .collect(),
    });
    let mut bobette = entity(3, Kind::Toon, "Bobette");
    bobette.warnings.push("Wiki marks this page limited".into());
    let mut dev = entity(8, Kind::Toon, "Dev Toon");
    dev.warnings.push("Developer-only Toon".into());
    let entities = vec![
        toons,
        entity(152, Kind::Topic, "Dandy's World (Game)"),
        entity(1, Kind::Toon, "Poppy"),
        entity(2, Kind::Toon, "Pebble"),
        bobette,
        entity(4, Kind::Toon, "Coal"),
        entity(5, Kind::Npc, "Dandy"),
        entity(6, Kind::Npc, "Dyle"),
        entity(7, Kind::Twisted, "Twisted Poppy"),
        dev,
    ];
    let mut data = CatalogData {
        schema_version: SCHEMA_VERSION,
        adapter_version: "dw-wiki/1.0".into(),
        source_origin: SOURCE_ORIGIN.into(),
        crawl_started_at: "2026-09-12T00:00:00Z".into(),
        crawl_completed_at: "2026-09-12T00:00:01Z".into(),
        sources,
        entities,
        coverage: Coverage {
            discovered_pages: 0,
            imported_pages: 0,
            namespace_counts: BTreeMap::new(),
            nonredirect_articles: 0,
            redirects: 0,
            entities_by_kind: BTreeMap::new(),
            excluded: vec![],
            unresolved_redirects: vec![],
            warnings: vec![],
        },
        images: BTreeMap::new(),
    };
    recount(&mut data);
    data
}
fn recount(data: &mut CatalogData) {
    let templates = data
        .sources
        .iter()
        .filter(|s| s.title.starts_with("Template:"))
        .count();
    data.coverage.discovered_pages = data.sources.len();
    data.coverage.imported_pages = data.sources.len();
    data.coverage.nonredirect_articles = data.sources.len() - templates;
    data.coverage.namespace_counts = BTreeMap::from([
        ("articles".into(), data.sources.len() - templates),
        ("templates".into(), templates),
    ]);
    data.coverage.entities_by_kind.clear();
    for e in &data.entities {
        *data
            .coverage
            .entities_by_kind
            .entry(
                serde_json::to_value(e.kind)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )
            .or_default() += 1;
    }
}
fn build(
    data: &CatalogData,
) -> Result<dandys_world_core::runs::domain::EligibilitySnapshot, Error> {
    from_catalog(&"a".repeat(64), data, NOW)
}

#[test]
fn seasonal_union_is_complete_deduplicated_and_excludes_nonplayable_entities() {
    let evidence = build(&fixture()).unwrap();
    assert_eq!(
        evidence.toons,
        BTreeMap::from([
            ("page:1".into(), "Poppy".into()),
            ("page:2".into(), "Pebble".into()),
            ("page:3".into(), "Bobette".into()),
            ("page:4".into(), "Coal".into())
        ])
    );
    assert_eq!(evidence.source_revisions.len(), 7);
    assert_eq!(evidence.source_revisions["Template:MCToons"], 31109);
    assert_eq!(evidence.source_hash, "a".repeat(64));
    assert_eq!(evidence.observed_at, NOW - 100);
    assert_eq!(evidence.fresh_until, NOW - 100 + MAX_SOURCE_AGE_MS);
    assert!(!evidence.disputed);
    assert_eq!(MAX_PLAYERS, 8);
}

#[test]
fn missing_any_required_source_and_tiny_wiki_only_catalogs_are_unavailable() {
    for title in [
        "Toons",
        "Dandy's World (Game)",
        "Template:RegularToons",
        "Template:MCToons",
        "Template:EventToons",
        "Template:UnobtainableToons",
        "Template:ToonAmount",
    ] {
        let mut data = fixture();
        data.sources.retain(|s| s.title != title);
        recount(&mut data);
        assert_eq!(build(&data), Err(Error::CatalogUnavailable), "{title}");
    }
    let mut tiny = fixture();
    tiny.entities.retain(|e| e.name == "Poppy");
    tiny.sources.retain(|s| s.title == "Poppy");
    recount(&mut tiny);
    dandys_world_core::snapshot::validate(&tiny).unwrap();
    assert_eq!(build(&tiny), Err(Error::CatalogUnavailable));
}

#[test]
fn no_read_fabricates_freshness_and_required_or_article_future_stale_sources_fail() {
    for title in [
        "Template:EventToons",
        "Toons",
        "Dandy's World (Game)",
        "Poppy",
    ] {
        for time in [0, NOW + 1, NOW - MAX_SOURCE_AGE_MS] {
            let mut data = fixture();
            data.sources
                .iter_mut()
                .find(|s| s.title == title)
                .unwrap()
                .validated_at_ms = time;
            assert_eq!(
                build(&data),
                Err(Error::CatalogUnavailable),
                "{title} {time}"
            );
        }
    }
    let mut data = fixture();
    data.sources
        .iter_mut()
        .find(|s| s.title == "Poppy")
        .unwrap()
        .validated_at_ms = NOW - 500;
    let first = build(&data).unwrap();
    let later = from_catalog(&"a".repeat(64), &data, NOW + 10_000).unwrap();
    assert_eq!(first, later);
    assert_eq!(first.observed_at, NOW - 500);
    assert_eq!(
        from_catalog(&"a".repeat(64), &data, first.fresh_until),
        Err(Error::CatalogUnavailable)
    );
}

#[test]
fn missing_extra_or_disputed_roster_members_cannot_silently_change_eligibility() {
    for availability in [
        EvidenceState::Unknown,
        EvidenceState::Conflicting,
        EvidenceState::Historical,
        EvidenceState::Unverified,
    ] {
        let mut data = fixture();
        data.entities
            .iter_mut()
            .find(|e| e.name == "Poppy")
            .unwrap()
            .availability = availability;
        assert_eq!(build(&data), Err(Error::CatalogUnavailable));
    }
    for warning in [
        "Unreleased Toon",
        "Scrapped character",
        "Disputed eligibility",
        "developer-only",
    ] {
        let mut data = fixture();
        data.entities
            .iter_mut()
            .find(|e| e.name == "Poppy")
            .unwrap()
            .warnings
            .push(warning.into());
        assert_eq!(build(&data), Err(Error::CatalogUnavailable));
    }
    let mut data = fixture();
    data.entities.retain(|e| e.name != "Poppy");
    recount(&mut data);
    assert_eq!(build(&data), Err(Error::CatalogUnavailable));
    let mut data = fixture();
    data.entities
        .iter_mut()
        .find(|e| e.name == "Dev Toon")
        .unwrap()
        .warnings
        .clear();
    assert_eq!(build(&data), Err(Error::CatalogUnavailable));
}

#[test]
fn source_headers_without_hash_verified_supported_template_definitions_are_insufficient() {
    let mut data = fixture();
    data.entities
        .iter_mut()
        .find(|e| e.name == "Toons")
        .unwrap()
        .facts
        .clear();
    assert_eq!(build(&data), Err(Error::CatalogUnavailable));
    for state in [
        EvidenceState::Unknown,
        EvidenceState::Conflicting,
        EvidenceState::Unverified,
    ] {
        let mut data = fixture();
        data.entities
            .iter_mut()
            .find(|e| e.name == "Toons")
            .unwrap()
            .facts[0]
            .state = state;
        assert_eq!(build(&data), Err(Error::CatalogUnavailable));
    }
    let mut data = fixture();
    data.entities
        .iter_mut()
        .find(|e| e.name == "Toons")
        .unwrap()
        .facts[0]
        .citations[0]
        .quote
        .push_str("{{ToonBox|Dev Toon|type=regular}}");
    assert_eq!(build(&data), Err(Error::CatalogUnavailable));
    let mut data = fixture();
    data.sources
        .iter_mut()
        .find(|s| s.title == "Template:RegularToons")
        .unwrap()
        .page_id = 999;
    assert_eq!(build(&data), Err(Error::CatalogUnavailable));
}

#[test]
fn exact_snapshot_hash_and_revision_pins_roundtrip_without_assuming_current_time() {
    let data = fixture();
    let bytes = serde_json::to_vec(&data).unwrap();
    let snapshot = Snapshot::from_bytes(&bytes).unwrap();
    let evidence = from_catalog(&snapshot.id, &snapshot.data, NOW).unwrap();
    assert_eq!(
        evidence.source_hash,
        format!("{:x}", Sha256::digest(&bytes))
    );
    let saved = serde_json::to_vec(&evidence).unwrap();
    assert_eq!(
        serde_json::from_slice::<dandys_world_core::runs::domain::EligibilitySnapshot>(&saved)
            .unwrap(),
        evidence
    );
    assert_eq!(
        from_catalog("made-up-id", &data, NOW),
        Err(Error::CatalogUnavailable)
    );
}

/// A real normalized import is local agent material, not a tracked fixture.
#[test]
#[ignore = "set DW_RUN_ELIGIBILITY_CATALOG to a qualified complete normalized catalog"]
fn qualified_complete_catalog_matches_real_roster() {
    let path = std::env::var("DW_RUN_ELIGIBILITY_CATALOG").expect("catalog path required");
    let bytes = std::fs::read(path).unwrap();
    let snapshot = Snapshot::from_bytes(&bytes).unwrap();
    let now = snapshot
        .data
        .sources
        .iter()
        .map(|s| s.validated_at_ms)
        .max()
        .unwrap()
        + 1_000;
    let evidence = from_catalog(&snapshot.id, &snapshot.data, now).unwrap();
    assert_eq!(evidence.toons.len(), 40);
    assert!(evidence.toons.values().any(|name| name == "Coal"));
    assert!(evidence.toons.values().any(|name| name == "Bobette"));
    assert!(
        !evidence
            .toons
            .values()
            .any(|name| matches!(name.as_str(), "Dandy" | "Dyle"))
    );
    assert_eq!(evidence.source_hash, snapshot.id);
}

#[test]
fn disputed_statistics_do_not_invent_an_eligibility_dispute() {
    let mut data = fixture();
    data.entities.iter_mut().find(|e| e.name == "Poppy").unwrap().warnings.push(
        "The same wiki field gives a 48% maximum and a tooltip claiming approximately 59.4%; the maximum is disputed.".into());
    assert_eq!(build(&data).unwrap().toons.len(), 4);
}

#[test]
fn invalid_or_future_revision_and_crawl_timestamps_are_unavailable() {
    for timestamp in [
        "2999-01-01T00:00:00Z",
        "2026-02-30T00:00:00Z",
        "not a timestamp",
    ] {
        let mut data = fixture();
        data.sources
            .iter_mut()
            .find(|s| s.title == "Poppy")
            .unwrap()
            .revision_timestamp = timestamp.into();
        assert_eq!(build(&data), Err(Error::CatalogUnavailable));
        let mut data = fixture();
        data.crawl_completed_at = timestamp.into();
        assert_eq!(build(&data), Err(Error::CatalogUnavailable));
    }
    let mut data = fixture();
    data.crawl_started_at = "2026-09-13T00:00:00Z".into();
    assert_eq!(build(&data), Err(Error::CatalogUnavailable));
}
