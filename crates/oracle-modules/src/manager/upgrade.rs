//! Forward-only upgrade: stop the old writer, persist incoming intent, then migrate.
use super::*;

impl ModuleManager {
    pub async fn upgrade(
        self: &Arc<Self>,
        module: &ModuleId,
        digest: &str,
        grace: Duration,
    ) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        let incoming = self.installation(digest).await?;
        if &incoming.package.manifest.id != module {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        self.artifacts.verify(&incoming)?;
        let old = self.registry.read().unwrap().get(module).cloned();
        let desired = self
            .repository
            .desired_modules()
            .await?
            .into_iter()
            .find(|d| &d.module == module);
        let resume = old.is_none()
            && desired
                .as_ref()
                .is_some_and(|d| d.loaded && d.digest == digest);
        let previous = match &old {
            Some(old) => old.installed.clone(),
            None => {
                self.installation(
                    &desired
                        .as_ref()
                        .ok_or_else(|| Error::new(ErrorCode::NotFound))?
                        .digest,
                )
                .await?
            }
        };
        check_forward(
            &previous.package.manifest,
            &incoming.package.manifest,
            resume,
        )?;
        self.ensure_no_dependents(module, None)?;
        let activations: Vec<_> = self
            .repository
            .desired_activations()
            .await?
            .into_iter()
            .filter(|a| &a.module == module)
            .collect();
        for activation in &activations {
            check_grants(
                &previous.package.manifest,
                &incoming.package.manifest,
                &activation.grants,
            )?;
            let progress = self
                .repository
                .migration_status(module, &activation.guild)
                .await?;
            check_migration(&incoming, &progress)?;
            // Resolve known dependency failures while the old writer is still available.
            if activation.active {
                let (mut providers, mut graph) = self.guild_graph(&activation.guild);
                providers.remove(module);
                graph.remove(module);
                let bindings = dependencies::resolve(
                    &incoming.package.manifest,
                    &providers,
                    &activation.bindings,
                )?;
                providers.insert(module.clone(), incoming.package.manifest.clone());
                graph.insert(module.clone(), bindings);
                dependencies::validate_graph(&providers, &graph)?;
            }
        }
        if let Some(old) = old {
            // A cancelled drain cannot orphan an invisible but still-running old process.
            let mut cleanup = OldWriterGuard {
                manager: self,
                module: module.clone(),
                generation: old.clone(),
                stopped: false,
            };
            old.gate.close(None);
            self.registry.write().unwrap().remove(module);
            self.publish_counts();
            let drained = old.quiesce(None, grace).await.unwrap_or(false);
            let stopped = if drained {
                match old.stop(Duration::from_secs(2)).await {
                    Ok(report) => report,
                    Err(_) => old.force_stop().await?,
                }
            } else {
                old.force_stop().await?
            };
            if stopped.cleanup_error.is_some() {
                return Err(Error::new(ErrorCode::Io));
            }
            cleanup.stopped = true;
        }
        // Once data advancement can begin, restart must select only the incoming artifact.
        // Failure deliberately retains this intent; there is no automatic old-writer rollback.
        self.repository
            .set_module_desired(&DesiredModule {
                module: module.clone(),
                digest: digest.into(),
                loaded: true,
            })
            .await?;
        for activation in &activations {
            self.migrate(&incoming, &activation.guild).await?;
        }
        self.load_locked(digest).await?;
        for activation in activations.into_iter().filter(|a| a.active) {
            self.activate_locked(&PolicyContext::LocalOperator, activation)
                .await?;
        }
        Ok(())
    }
}
// Preserve cleanup ownership across cancellation before the durable incoming intent.
// This tombstone is uncallable, but lets a retry wait for the same old process.
struct OldWriterGuard<'a> {
    manager: &'a ModuleManager,
    module: ModuleId,
    generation: Arc<Generation>,
    stopped: bool,
}
impl Drop for OldWriterGuard<'_> {
    fn drop(&mut self) {
        if !self.stopped {
            self.generation.gate.fence(None);
            self.generation.process().request_stop();
            self.manager
                .registry
                .write()
                .unwrap()
                .insert(self.module.clone(), self.generation.clone());
            self.manager.publish_counts();
        }
    }
}
fn check_forward(previous: &ModuleManifest, incoming: &ModuleManifest, resume: bool) -> Result<()> {
    let old = semver::Version::parse(&previous.version)
        .map_err(|_| Error::new(ErrorCode::Compatibility))?;
    let new = semver::Version::parse(&incoming.version)
        .map_err(|_| Error::new(ErrorCode::Compatibility))?;
    if (!resume && new <= old)
        || (resume && new != old)
        || incoming.data_version < previous.data_version
    {
        return Err(Error::new(ErrorCode::DataVersionMismatch));
    }
    Ok(())
}
fn check_grants(
    previous: &ModuleManifest,
    incoming: &ModuleManifest,
    grants: &[String],
) -> Result<()> {
    if grants
        .iter()
        .any(|cap| !incoming.capabilities.contains(cap))
    {
        return Err(Error::new(ErrorCode::ForbiddenPermission));
    }
    for operation in &incoming.operations {
        let previous_operation = previous
            .operations
            .iter()
            .find(|old| old.name == operation.name);
        for capability in &operation.capabilities {
            let already_required =
                previous_operation.is_some_and(|old| old.capabilities.contains(capability));
            if !already_required && !grants.contains(capability) {
                return Err(Error::new(ErrorCode::ForbiddenPermission));
            }
        }
    }
    Ok(())
}
fn check_migration(incoming: &InstalledModule, progress: &MigrationProgress) -> Result<()> {
    let manifest = &incoming.package.manifest;
    if progress.data_version > manifest.data_version {
        return Err(Error::new(ErrorCode::DataVersionMismatch));
    }
    if let Some(target) = progress.target_version {
        if progress.artifact_digest.as_deref() != Some(incoming.digest.as_str())
            || target <= progress.data_version
            || target > manifest.data_version
        {
            return Err(Error::new(ErrorCode::MigrationMismatch));
        }
        if progress.data_version != 0
            && !manifest
                .migrations
                .iter()
                .any(|m| m.from == progress.data_version && m.to == target)
        {
            return Err(Error::new(ErrorCode::MigrationMismatch));
        }
    }
    // Version zero is an empty namespace bootstrap, handled transactionally by migrate.
    let mut version = progress.data_version;
    while version != 0 && version < manifest.data_version {
        let step = manifest
            .migrations
            .iter()
            .find(|step| step.from == version)
            .ok_or_else(|| Error::new(ErrorCode::DataVersionMismatch))?;
        if version.checked_add(1) != Some(step.to) || step.to > manifest.data_version {
            return Err(Error::new(ErrorCode::DataVersionMismatch));
        }
        version = step.to;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifests() -> (ModuleManifest, ModuleManifest) {
        let old = serde_json::from_str(include_str!(
            "../../../../examples/modules/counter/manifest.json"
        ))
        .unwrap();
        let new = serde_json::from_str(include_str!(
            "../../../../examples/modules/counter/manifest-v2.json"
        ))
        .unwrap();
        (old, new)
    }
    #[test]
    fn downgrade_same_version_and_silent_capability_expansion_fail_preflight() {
        let (old, mut new) = manifests();
        assert!(check_forward(&old, &new, false).is_ok());
        assert!(check_forward(&new, &old, false).is_err());
        assert!(check_forward(&new, &new, false).is_err());
        assert!(check_forward(&new, &new, true).is_ok());
        assert!(check_grants(&old, &new, &["storage.own".into()]).is_ok());
        new.operations[0]
            .capabilities
            .push("contracts.invoke".into());
        assert!(check_grants(&old, &new, &["storage.own".into()]).is_err());
    }
    #[test]
    fn existing_ungranted_operations_stay_ungranted_across_upgrade() {
        let (old, new) = manifests();
        assert!(
            old.operations
                .iter()
                .any(|op| op.capabilities.contains(&"host.echo".into()))
        );
        assert!(
            new.operations
                .iter()
                .any(|op| op.capabilities.contains(&"host.echo".into()))
        );
        assert!(check_grants(&old, &new, &["storage.own".into()]).is_ok());
        let mut changed = new.clone();
        changed
            .operations
            .iter_mut()
            .find(|op| op.name == "increment")
            .unwrap()
            .capabilities
            .push("host.echo".into());
        assert!(check_grants(&old, &changed, &["storage.own".into()]).is_err());
        assert!(check_grants(&old, &changed, &["storage.own".into(), "host.echo".into()]).is_ok());
        let mut added = new.clone();
        let mut operation = added
            .operations
            .iter()
            .find(|op| op.capabilities.contains(&"host.echo".into()))
            .unwrap()
            .clone();
        operation.name = "new_echo".into();
        added.operations.push(operation);
        assert!(check_grants(&old, &added, &["storage.own".into()]).is_err());
    }
    #[test]
    fn missing_forward_step_or_other_artifact_checkpoint_fails_preflight() {
        let (_, manifest) = manifests();
        let mut incoming = InstalledModule {
            digest: "a".repeat(64),
            package: ModulePackage {
                manifest,
                entrypoint: "module".into(),
                files: BTreeMap::new(),
                source_revision: "test".into(),
                toolchain: "test".into(),
                license: "test".into(),
            },
        };
        let mut progress = MigrationProgress {
            module: incoming.package.manifest.id.clone(),
            guild: "123".parse().unwrap(),
            data_version: 1,
            target_version: None,
            artifact_digest: None,
            cursor: None,
        };
        assert!(check_migration(&incoming, &progress).is_ok());
        progress.target_version = Some(2);
        progress.artifact_digest = Some("b".repeat(64));
        assert!(check_migration(&incoming, &progress).is_err());
        progress.artifact_digest = Some(incoming.digest.clone());
        assert!(check_migration(&incoming, &progress).is_ok());
        incoming.package.manifest.migrations.clear();
        assert!(check_migration(&incoming, &progress).is_err());
    }
}
