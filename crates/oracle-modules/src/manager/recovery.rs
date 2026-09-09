//! Host-ticked bounded recovery; no detached restart tasks.
use super::*;
use tokio::time::Instant;

pub(super) struct Recovery {
    attempts: u32,
    next: Instant,
    healthy_since: Option<Instant>,
    error: ErrorCode,
}
impl Recovery {
    fn new() -> Self {
        Self {
            attempts: 0,
            next: Instant::now() + Duration::from_secs(1),
            healthy_since: None,
            error: ErrorCode::ModuleUnavailable,
        }
    }
}
fn required_edges(
    manifest: &ModuleManifest,
    activations: &BTreeMap<GuildId, crate::generation::Activation>,
) -> BTreeSet<ModuleId> {
    activations
        .values()
        .flat_map(|active| {
            manifest
                .consumes
                .iter()
                .filter(|contract| !contract.optional)
                .filter_map(|contract| active.bindings.get(&contract.name).cloned())
        })
        .collect()
}
fn dependent_closure(
    failed: &mut BTreeSet<ModuleId>,
    edges: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
) {
    loop {
        let before = failed.len();
        for (consumer, providers) in edges {
            if providers.iter().any(|provider| failed.contains(provider)) {
                failed.insert(consumer.clone());
            }
        }
        if failed.len() == before {
            break;
        }
    }
}
impl ModuleManager {
    /// Required dependencies must already be active in every desired active guild.
    /// Waiting on providers does not consume this module's process-crash budget.
    fn recovery_dependencies(
        &self,
        installed: &InstalledModule,
        activations: &[DesiredActivation],
    ) -> Result<()> {
        let manifest = &installed.package.manifest;
        for desired in activations
            .iter()
            .filter(|a| a.active && a.module == manifest.id)
        {
            let (mut providers, mut graph) = self.guild_graph(&desired.guild);
            providers.remove(&manifest.id);
            graph.remove(&manifest.id);
            let bindings = dependencies::resolve(manifest, &providers, &desired.bindings)?;
            providers.insert(manifest.id.clone(), manifest.clone());
            graph.insert(manifest.id.clone(), bindings);
            dependencies::validate_graph(&providers, &graph)?;
        }
        Ok(())
    }
    /// At most three failed process attempts occur in a crash streak; sixty healthy
    /// seconds replenish the budget. Required-consumer cascades retain durable intent.
    pub async fn tick(self: &Arc<Self>) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        let generations: BTreeMap<_, _> = self
            .registry
            .read()
            .unwrap()
            .iter()
            .map(|(id, g)| (id.clone(), g.clone()))
            .collect();
        let original_dead: BTreeSet<_> = generations
            .iter()
            .filter(|(_, g)| !g.process().is_alive())
            .map(|(id, _)| id.clone())
            .collect();
        let edges = generations
            .iter()
            .map(|(id, g)| {
                (
                    id.clone(),
                    required_edges(
                        &g.installed.package.manifest,
                        &g.activations.lock().unwrap(),
                    ),
                )
            })
            .collect();
        let mut failed = original_dead.clone();
        dependent_closure(&mut failed, &edges);
        // Fence every dependent before the first await. Keep the registry tombstones
        // until actual cleanup finishes so cancellation cannot lose old-process ownership.
        for id in &failed {
            let generation = &generations[id];
            generation.gate.fence(None);
            generation.process().request_stop();
            let mut recovery = self.recovery.lock().unwrap();
            let state = recovery.entry(id.clone()).or_insert_with(Recovery::new);
            let previously_waiting =
                state.healthy_since.is_none() && state.error == ErrorCode::DependencyUnavailable;
            state.error = if !original_dead.contains(id) || previously_waiting {
                ErrorCode::DependencyUnavailable
            } else {
                ErrorCode::ModuleUnavailable
            };
            state.healthy_since = None;
            state.next = Instant::now() + Duration::from_secs(1u64 << state.attempts.min(3));
        }
        self.publish_counts();
        for id in failed {
            let stopped = generations[&id].force_stop().await?;
            if stopped.cleanup_error.is_some() {
                return Err(Error::new(ErrorCode::Io));
            }
            self.registry_remove(&id);
        }
        let desired = self.repository.desired_modules().await?;
        let activations = self.repository.desired_activations().await?;
        let ready: Vec<_> = {
            let now = Instant::now();
            let mut recovery = self.recovery.lock().unwrap();
            recovery.retain(|id, state| {
                desired.iter().any(|d| &d.module == id && d.loaded)
                    && !state
                        .healthy_since
                        .is_some_and(|since| now.duration_since(since) >= Duration::from_secs(60))
            });
            recovery
                .iter()
                .filter(|(_, state)| {
                    state.healthy_since.is_none() && state.attempts < 3 && state.next <= now
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ready {
            let Some(intent) = desired.iter().find(|d| d.module == id && d.loaded) else {
                continue;
            };
            let dependency_check = match self.installation(&intent.digest).await {
                Ok(installed) => self.recovery_dependencies(&installed, &activations),
                Err(error) => Err(error),
            };
            if dependency_check
                .as_ref()
                .is_err_and(|error| error.code == ErrorCode::DependencyUnavailable)
            {
                if let Some(state) = self.recovery.lock().unwrap().get_mut(&id) {
                    state.error = ErrorCode::DependencyUnavailable;
                    state.next = Instant::now() + Duration::from_secs(1);
                }
                continue;
            }
            // Real spawn/compatibility failures consume the attempt before awaiting.
            {
                let mut recovery = self.recovery.lock().unwrap();
                let state = recovery.get_mut(&id).unwrap();
                state.attempts += 1;
                state.next = Instant::now() + Duration::from_secs(1u64 << state.attempts);
            }
            let outcome: Result<()> = async {
                dependency_check?;
                self.load_locked(&intent.digest).await?;
                for activation in activations.iter().filter(|a| a.module == id && a.active) {
                    self.activate_locked(&PolicyContext::LocalOperator, activation.clone())
                        .await?;
                }
                Ok(())
            }
            .await;
            if let Err(error) = outcome {
                // Preserve an uncallable tombstone if cleanup is cancelled or fails.
                let generation = self.registry.read().unwrap().get(&id).cloned();
                if let Some(generation) = generation {
                    generation.gate.fence(None);
                    generation.process().request_stop();
                    let stopped = generation.force_stop().await?;
                    if stopped.cleanup_error.is_some() {
                        return Err(Error::new(ErrorCode::Io));
                    }
                    self.registry_remove(&id);
                }
                if let Some(state) = self.recovery.lock().unwrap().get_mut(&id) {
                    state.error = error.code;
                }
            } else if let Some(state) = self.recovery.lock().unwrap().get_mut(&id) {
                state.healthy_since = Some(Instant::now());
            }
        }
        self.publish_counts();
        Ok(())
    }
    pub(super) fn recovery_health(&self) -> BTreeMap<ModuleId, Value> {
        self.recovery.lock().unwrap().iter().map(|(id,state)|(id.clone(),serde_json::json!({"available":state.healthy_since.is_some(),"restart_attempts":state.attempts,"quarantined":state.healthy_since.is_none()&&state.attempts>=3,"retry_after_ms":state.next.saturating_duration_since(Instant::now()).as_millis() as u64,"error":state.error}))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(name: &str) -> ModuleId {
        name.parse().unwrap()
    }
    #[test]
    fn required_failure_closure_is_transitive_and_preserves_unrelated_modules() {
        let edges = BTreeMap::from([
            (id("consumer"), BTreeSet::from([id("provider")])),
            (id("downstream"), BTreeSet::from([id("consumer")])),
            (id("unrelated"), BTreeSet::new()),
        ]);
        let mut failed = BTreeSet::from([id("provider")]);
        dependent_closure(&mut failed, &edges);
        assert_eq!(
            failed,
            BTreeSet::from([id("provider"), id("consumer"), id("downstream")])
        );
    }
    #[test]
    fn optional_bindings_do_not_cascade_but_required_binding_in_any_guild_does() {
        let manifest:ModuleManifest=serde_json::from_value(serde_json::json!({"manifest_version":1,"id":"consumer","version":"1.0.0","target":"test","protocol_major":1,"protocol_minor_min":0,"host_api":"^1","data_version":1,"readable_data_versions":[1],"operations":[],"consumes":[{"name":"required","version":"^1","optional":false},{"name":"optional","version":"^1","optional":true}]})).unwrap();
        let activations = BTreeMap::from([
            (
                "123".parse().unwrap(),
                crate::generation::Activation {
                    epoch: 1,
                    grants: BTreeSet::new(),
                    bindings: BTreeMap::from([("optional".into(), id("optional-provider"))]),
                },
            ),
            (
                "456".parse().unwrap(),
                crate::generation::Activation {
                    epoch: 2,
                    grants: BTreeSet::new(),
                    bindings: BTreeMap::from([("required".into(), id("required-provider"))]),
                },
            ),
        ]);
        assert_eq!(
            required_edges(&manifest, &activations),
            BTreeSet::from([id("required-provider")])
        );
    }
}
