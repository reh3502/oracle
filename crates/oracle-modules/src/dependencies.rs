//! Guild-local dependency resolution. Callers supply only the providers active in
//! that guild; this module never consults global installation or another guild.
use oracle_core::{ConsumedContract, Error, ErrorCode, ModuleId, ModuleManifest, Result};
use semver::{Version, VersionReq};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
fn unavailable() -> Error {
    Error::new(ErrorCode::DependencyUnavailable)
}
fn malformed() -> Error {
    Error::new(ErrorCode::InvalidInput)
}
fn requirement(contract: &ConsumedContract) -> Result<VersionReq> {
    VersionReq::parse(&contract.version).map_err(|_| Error::new(ErrorCode::Compatibility))
}
fn validate_manifest(manifest: &ModuleManifest) -> Result<()> {
    let mut consumes = BTreeSet::new();
    for contract in &manifest.consumes {
        if !consumes.insert(&contract.name) {
            return Err(malformed());
        }
        requirement(contract)?;
    }
    let mut provides = BTreeSet::new();
    for contract in &manifest.provides {
        if !provides.insert(&contract.name) {
            return Err(malformed());
        }
        Version::parse(&contract.version).map_err(|_| Error::new(ErrorCode::Compatibility))?;
    }
    Ok(())
}
fn validate_providers(providers: &BTreeMap<ModuleId, ModuleManifest>) -> Result<()> {
    for (id, manifest) in providers {
        if id != &manifest.id {
            return Err(malformed());
        }
        validate_manifest(manifest)?;
    }
    Ok(())
}
fn compatible(contract: &ConsumedContract, provider: &ModuleManifest) -> Result<bool> {
    let requirement = requirement(contract)?;
    match provider
        .provides
        .iter()
        .find(|provided| provided.name == contract.name)
    {
        Some(provided) => Ok(requirement.matches(
            &Version::parse(&provided.version).map_err(|_| Error::new(ErrorCode::Compatibility))?,
        )),
        None => Ok(false),
    }
}
/// Resolve exact contract names and semver ranges against this guild's active map.
/// An explicit binding must be valid even when its dependency is optional.
pub fn resolve(
    consumer: &ModuleManifest,
    providers: &BTreeMap<ModuleId, ModuleManifest>,
    explicit: &BTreeMap<String, ModuleId>,
) -> Result<BTreeMap<String, ModuleId>> {
    validate_manifest(consumer)?;
    validate_providers(providers)?;
    if explicit
        .keys()
        .any(|name| !consumer.consumes.iter().any(|c| &c.name == name))
    {
        return Err(unavailable());
    }
    let mut resolved = BTreeMap::new();
    for contract in &consumer.consumes {
        if let Some(selected) = explicit.get(&contract.name) {
            let provider = providers.get(selected).ok_or_else(unavailable)?;
            if !compatible(contract, provider)? {
                return Err(unavailable());
            }
            resolved.insert(contract.name.clone(), selected.clone());
            continue;
        }
        let mut candidates = Vec::new();
        for (id, provider) in providers {
            if compatible(contract, provider)? {
                candidates.push(id)
            }
        }
        match candidates.as_slice() {
            [id] => {
                resolved.insert(contract.name.clone(), (*id).clone());
            }
            [] if contract.optional => {}
            _ => return Err(unavailable()),
        }
    }
    Ok(resolved)
}
/// Validate an already-resolved guild graph. Missing required edges, stale
/// providers and changed versions fail closed. Optional bindings are checked but
/// do not constrain required-dependency activation order or participate in cycles.
pub fn validate_graph(
    manifests: &BTreeMap<ModuleId, ModuleManifest>,
    bindings: &BTreeMap<ModuleId, BTreeMap<String, ModuleId>>,
) -> Result<()> {
    validate_providers(manifests)?;
    if bindings.keys().any(|id| !manifests.contains_key(id)) {
        return Err(unavailable());
    }
    let mut outgoing: BTreeMap<ModuleId, BTreeSet<ModuleId>> = BTreeMap::new();
    let mut indegree: BTreeMap<ModuleId, usize> =
        manifests.keys().map(|id| (id.clone(), 0)).collect();
    for (id, manifest) in manifests {
        let selected = bindings.get(id);
        if selected.is_some_and(|map| {
            map.keys()
                .any(|name| !manifest.consumes.iter().any(|c| &c.name == name))
        }) {
            return Err(unavailable());
        }
        for contract in &manifest.consumes {
            let Some(provider_id) = selected.and_then(|map| map.get(&contract.name)) else {
                if contract.optional {
                    continue;
                } else {
                    return Err(unavailable());
                }
            };
            let provider = manifests.get(provider_id).ok_or_else(unavailable)?;
            if !compatible(contract, provider)? {
                return Err(unavailable());
            }
            if !contract.optional
                && outgoing
                    .entry(id.clone())
                    .or_default()
                    .insert(provider_id.clone())
            {
                *indegree.get_mut(provider_id).ok_or_else(unavailable)? += 1;
            }
        }
    }
    // Iterative topological elimination avoids recursion on a large dependency chain.
    let mut ready: VecDeque<_> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut visited = 0;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        if let Some(edges) = outgoing.get(&id) {
            for provider in edges {
                let degree = indegree.get_mut(provider).ok_or_else(unavailable)?;
                *degree -= 1;
                if *degree == 0 {
                    ready.push_back(provider.clone())
                }
            }
        }
    }
    if visited != manifests.len() {
        return Err(unavailable());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oracle_core::ProvidedContract;
    fn module(id: &str) -> ModuleManifest {
        ModuleManifest {
            configuration: None,
            subscriptions: vec![],
            commands: None,
            manifest_version: 1,
            id: ModuleId::new(id).unwrap(),
            version: "1.0.0".into(),
            target: "x86_64-unknown-linux-gnu".into(),
            protocol_major: 1,
            protocol_minor_min: 0,
            host_api: "^1".into(),
            data_version: 1,
            readable_data_versions: vec![1],
            capabilities: vec![],
            required_intents: vec![],
            provides: vec![],
            consumes: vec![],
            operations: vec![],
            collections: vec![],
            migrations: vec![],
        }
    }
    fn provider(id: &str, name: &str, version: &str) -> ModuleManifest {
        let mut m = module(id);
        m.provides.push(ProvidedContract {
            name: name.into(),
            version: version.into(),
            operation: "get".into(),
        });
        m
    }
    fn consumer(id: &str, name: &str, version: &str, optional: bool) -> ModuleManifest {
        let mut m = module(id);
        m.consumes.push(ConsumedContract {
            name: name.into(),
            version: version.into(),
            optional,
        });
        m
    }
    fn map(modules: Vec<ModuleManifest>) -> BTreeMap<ModuleId, ModuleManifest> {
        modules.into_iter().map(|m| (m.id.clone(), m)).collect()
    }
    #[test]
    fn only_supplied_active_providers_can_resolve_a_guild_dependency() {
        let c = consumer("fixture.consumer", "counter/v1", "^1", false);
        let p = provider("fixture.counter", "counter/v1", "1.2.0");
        assert!(resolve(&c, &BTreeMap::new(), &BTreeMap::new()).is_err());
        let guild_a = map(vec![p.clone()]);
        assert_eq!(
            resolve(&c, &guild_a, &BTreeMap::new()).unwrap()["counter/v1"],
            p.id
        );
        let explicit = BTreeMap::from([("counter/v1".into(), p.id.clone())]);
        assert!(resolve(&c, &BTreeMap::new(), &explicit).is_err());
    }
    #[test]
    fn versions_and_names_are_exact_and_ambiguity_requires_a_valid_selection() {
        let c = consumer("fixture.consumer", "counter/v1", "^1", false);
        let a = provider("fixture.a", "counter/v1", "1.0.0");
        let b = provider("fixture.b", "counter/v1", "1.9.0");
        let wrong = provider("fixture.wrong", "counter/v1", "2.0.0");
        assert!(resolve(&c, &map(vec![wrong.clone()]), &BTreeMap::new()).is_err());
        assert!(
            resolve(
                &c,
                &map(vec![provider("fixture.alias", "counter/v2", "1.0.0")]),
                &BTreeMap::new()
            )
            .is_err()
        );
        let active = map(vec![a.clone(), b, wrong.clone()]);
        assert!(resolve(&c, &active, &BTreeMap::new()).is_err());
        assert_eq!(
            resolve(
                &c,
                &active,
                &BTreeMap::from([("counter/v1".into(), a.id.clone())])
            )
            .unwrap()["counter/v1"],
            a.id
        );
        assert!(
            resolve(
                &c,
                &active,
                &BTreeMap::from([("counter/v1".into(), wrong.id)])
            )
            .is_err()
        );
        assert!(resolve(&c, &active, &BTreeMap::from([("other".into(), a.id)])).is_err());
    }
    #[test]
    fn optional_absence_is_omitted_but_invalid_explicit_and_ambiguity_fail() {
        let c = consumer("fixture.consumer", "counter/v1", "^1", true);
        assert!(
            resolve(&c, &BTreeMap::new(), &BTreeMap::new())
                .unwrap()
                .is_empty()
        );
        let a = provider("fixture.a", "counter/v1", "1.0.0");
        let b = provider("fixture.b", "counter/v1", "1.1.0");
        assert!(
            resolve(
                &c,
                &BTreeMap::new(),
                &BTreeMap::from([("counter/v1".into(), a.id.clone())])
            )
            .is_err()
        );
        assert!(resolve(&c, &map(vec![a, b]), &BTreeMap::new()).is_err());
    }
    #[test]
    fn required_cycles_including_self_edges_fail_but_optional_cycle_edges_do_not() {
        let mut a = consumer("fixture.a", "b/v1", "^1", false);
        a.provides = provider("fixture.a", "a/v1", "1.0.0").provides;
        let mut b = consumer("fixture.b", "a/v1", "^1", false);
        b.provides = provider("fixture.b", "b/v1", "1.0.0").provides;
        let bindings = BTreeMap::from([
            (
                a.id.clone(),
                BTreeMap::from([("b/v1".into(), b.id.clone())]),
            ),
            (
                b.id.clone(),
                BTreeMap::from([("a/v1".into(), a.id.clone())]),
            ),
        ]);
        assert!(validate_graph(&map(vec![a.clone(), b.clone()]), &bindings).is_err());
        b.consumes[0].optional = true;
        validate_graph(&map(vec![a.clone(), b]), &bindings).unwrap();
        a.consumes[0].name = "a/v1".into();
        assert!(
            validate_graph(
                &map(vec![a.clone()]),
                &BTreeMap::from([(a.id.clone(), BTreeMap::from([("a/v1".into(), a.id)]))])
            )
            .is_err()
        );
    }
    #[test]
    fn persisted_graph_rejects_missing_stale_and_changed_bindings() {
        let c = consumer("fixture.consumer", "counter/v1", "^1", false);
        let p = provider("fixture.counter", "counter/v1", "1.0.0");
        let valid = BTreeMap::from([(
            c.id.clone(),
            BTreeMap::from([("counter/v1".into(), p.id.clone())]),
        )]);
        let active = map(vec![c.clone(), p.clone()]);
        validate_graph(&active, &valid).unwrap();
        assert!(validate_graph(&active, &BTreeMap::new()).is_err());
        assert!(validate_graph(&map(vec![c.clone()]), &valid).is_err());
        assert!(
            validate_graph(
                &map(vec![
                    c.clone(),
                    provider("fixture.counter", "counter/v1", "2.0.0")
                ]),
                &valid
            )
            .is_err()
        );
        let mut stale = valid.clone();
        stale
            .get_mut(&c.id)
            .unwrap()
            .insert("removed/v1".into(), p.id.clone());
        assert!(validate_graph(&active, &stale).is_err());
        let mut unknown = valid;
        unknown.insert(ModuleId::new("fixture.removed").unwrap(), BTreeMap::new());
        assert!(validate_graph(&active, &unknown).is_err());
    }
    #[test]
    fn malformed_versions_and_mismatched_provider_identity_fail_closed() {
        let c = consumer("fixture.consumer", "counter/v1", "invalid version", false);
        assert_eq!(
            resolve(&c, &BTreeMap::new(), &BTreeMap::new())
                .unwrap_err()
                .code,
            ErrorCode::Compatibility
        );
        let c = consumer("fixture.consumer", "counter/v1", "^1", false);
        let p = provider("fixture.counter", "counter/v1", "1.0.0");
        let bad = BTreeMap::from([(ModuleId::new("fixture.alias").unwrap(), p)]);
        assert_eq!(
            resolve(&c, &bad, &BTreeMap::new()).unwrap_err().code,
            ErrorCode::InvalidInput
        );
    }
}
