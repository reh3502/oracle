//! Compile explicitly published module routes into guild-scoped Discord commands.
use oracle_core::{Error, ErrorCode, Result};
use oracle_modules::ModuleCatalogEntry;
use oracle_operations::commands::{CommandRoute, DesiredCommand};
use serde_json::json;
use std::collections::BTreeSet;

pub fn compile_catalog(entries: &[ModuleCatalogEntry]) -> Result<Vec<DesiredCommand>> {
    // Discord permits 100 guild chat-input commands. Reserve one for /oracle.
    if entries.len() > 99 {
        return Err(Error::new(ErrorCode::QuotaExceeded));
    }
    let mut namespaces = BTreeSet::new();
    let mut desired = Vec::new();
    for entry in entries {
        let descriptor = &entry.commands;
        if descriptor.namespace == "oracle" || !namespaces.insert(&descriptor.namespace) {
            return Err(Error::new(ErrorCode::Conflict));
        }
        let options: Vec<_> = descriptor
            .routes
            .iter()
            .map(|route| {
                json!({
                    "type":1, "name":route.name, "description":route.description,
                    "options":[{"type":3,"name":"input","description":"Operation input as JSON",
                        "required":route.input_required,"max_length":6000}]
                })
            })
            .collect();
        desired.push(DesiredCommand {
            owner: entry.module.clone(),
            route: Some(CommandRoute {
                session: entry.session.clone(),
                generation: entry.generation,
                epoch: entry.epoch,
            }),
            definition: json!({"type":1,"name":descriptor.namespace,
                "description":descriptor.description,"default_member_permissions":"32",
                "options":options}),
        });
    }
    desired.sort_by(|a, b| a.owner.cmp(&b.owner));
    Ok(desired)
}

pub async fn invoke_published(
    manager: &oracle_modules::ModuleManager,
    reconciler: &oracle_operations::commands::CommandReconciler,
    actor: &oracle_core::PolicyContext,
    guild: &oracle_core::GuildId,
    request: oracle_operations::ingress::OperationRequest,
) -> Result<serde_json::Value> {
    use oracle_operations::ingress::OperationRequest;
    let OperationRequest::InvokePublished {
        command_id,
        command_name,
        route,
        input,
    } = request
    else {
        return Err(Error::new(ErrorCode::InvalidInput));
    };
    let binding = reconciler
        .bindings(guild)
        .await?
        .into_iter()
        .find(|binding| binding.id.as_deref() == Some(command_id.as_str()))
        .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
    if binding.definition["name"].as_str() != Some(command_name.as_str()) {
        return Err(Error::new(ErrorCode::ForbiddenScope));
    }
    let identity = binding
        .route
        .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
    let entry = manager
        .catalog(actor, guild)
        .await?
        .into_iter()
        .find(|entry| {
            entry.module == binding.owner
                && entry.session == identity.session
                && entry.generation == identity.generation
                && entry.epoch == identity.epoch
                && entry.commands.namespace == command_name
        })
        .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
    let operation = entry
        .commands
        .routes
        .iter()
        .find(|candidate| candidate.name == route)
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
    manager
        .invoke_bound(
            actor,
            guild,
            &entry.module,
            &operation.operation,
            input,
            &identity.session,
            identity.generation,
            identity.epoch,
        )
        .await
}

struct RegistryFence(oracle_modules::RegistryDispatchPermit);
impl oracle_operations::executor::DispatchFence for RegistryFence {
    fn dispatch(&self, send: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        self.0.dispatch(send)?
    }
}

pub async fn run(
    host: std::sync::Arc<super::Host>,
    guilds: Vec<oracle_core::GuildId>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use std::{sync::Arc, time::Duration};
    let reconciler = host
        .command_sync
        .get()
        .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
    let mut changes = host.modules.registry_changes();
    'refresh: loop {
        let revision = *changes.borrow_and_update();
        // Collapse rapid load/activation changes before reading Discord's registry.
        tokio::select! { biased;
            _ = cancel.cancelled() => return Ok(()),
            changed = changes.changed() => {
                changed.map_err(|_| Error::new(ErrorCode::Cancelled))?;
                continue 'refresh;
            },
            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
        let mut retry = false;
        for guild in &guilds {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if host.modules.registry_revision() != revision {
                continue 'refresh;
            }
            host.command_status.lock().unwrap().insert(
                guild.clone(),
                json!({"state":"pending","revision":revision}),
            );
            let outcome = match prepare(&host, guild).await {
                Err(error) => Err(error),
                Ok((snapshot_revision, desired)) => {
                    let work_cancel = cancel.child_token();
                    let fence = Arc::new(RegistryFence(
                        host.modules.registry_permit(snapshot_revision),
                    ));
                    let work = reconciler.reconcile_fenced(
                        guild,
                        &desired,
                        &work_cancel,
                        oracle_operations::executor::now() + 60,
                        fence,
                    );
                    tokio::pin!(work);
                    tokio::select! { biased;
                        _ = cancel.cancelled() => { work_cancel.cancel(); let _ = work.await; return Ok(()); },
                        changed = changes.changed() => {
                            work_cancel.cancel(); let _ = work.await;
                            changed.map_err(|_| Error::new(ErrorCode::Cancelled))?;
                            continue 'refresh;
                        },
                        result = &mut work => result,
                    }
                }
            };
            let status = match outcome {
                Ok(report) => json!({"state":"synchronized","revision":revision,"report":report}),
                Err(error) => {
                    retry = true;
                    tracing::warn!(guild = %guild, error = ?error.code, "module command publication pending");
                    json!({"state":"pending","revision":revision,"error":error.code})
                }
            };
            host.command_status
                .lock()
                .unwrap()
                .insert(guild.clone(), status);
        }
        if retry {
            tokio::select! { biased;
                _ = cancel.cancelled() => return Ok(()),
                changed = changes.changed() => { changed.map_err(|_| Error::new(ErrorCode::Cancelled))?; },
                _ = tokio::time::sleep(Duration::from_secs(30)) => {},
            }
        } else {
            tokio::select! { biased;
                _ = cancel.cancelled() => return Ok(()),
                changed = changes.changed() => { changed.map_err(|_| Error::new(ErrorCode::Cancelled))?; },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oracle_core::{ModuleCommandRoute, ModuleCommands, ModuleId};
    fn entry(namespace: &str) -> ModuleCatalogEntry {
        ModuleCatalogEntry {
            module: ModuleId::new(format!("sample.{namespace}")).unwrap(),
            session: "session-a".into(),
            generation: 7,
            epoch: 9,
            operations: vec![],
            commands: ModuleCommands {
                namespace: namespace.into(),
                description: "Sample module".into(),
                routes: vec![ModuleCommandRoute {
                    name: "status".into(),
                    description: "Read module status".into(),
                    operation: "private_status_operation".into(),
                    input_required: false,
                }],
            },
        }
    }
    #[test]
    fn declared_routes_preserve_identity_and_omit_internal_operation_names() {
        let compiled = compile_catalog(&[entry("sample")]).unwrap();
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].definition["name"], "sample");
        assert_eq!(compiled[0].definition["options"][0]["name"], "status");
        assert!(
            !compiled[0]
                .definition
                .to_string()
                .contains("private_status_operation")
        );
        assert_eq!(
            compiled[0].route,
            Some(CommandRoute {
                session: "session-a".into(),
                generation: 7,
                epoch: 9
            })
        );
        assert_eq!(compiled[0].definition["default_member_permissions"], "32");
    }
    #[test]
    fn reserved_names_collisions_and_scope_limit_fail_before_publication() {
        assert_eq!(
            compile_catalog(&[entry("oracle")]).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            compile_catalog(&[entry("same"), entry("same")])
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            compile_catalog(&vec![entry("many"); 100]).unwrap_err().code,
            ErrorCode::QuotaExceeded
        );
    }
}

#[cfg(test)]
#[path = "command_runtime_tests.rs"]
mod integration_tests;

async fn prepare(
    host: &super::Host,
    guild: &oracle_core::GuildId,
) -> Result<(u64, Vec<DesiredCommand>)> {
    let snapshot = host
        .modules
        .catalog_snapshot(&oracle_core::PolicyContext::LocalOperator, guild)
        .await?;
    let mut desired = compile_catalog(&snapshot.entries)?;
    let bootstrap = oracle_discord::bootstrap_commands::desired()?;
    let known = oracle_discord::bootstrap_commands::known_definitions()?;
    host.command_sync
        .get()
        .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?
        .adopt_known(guild, &bootstrap.owner, &known)
        .await?;
    desired.push(bootstrap);
    Ok((snapshot.revision, desired))
}

pub async fn publish_once(host: &super::Host) -> Result<serde_json::Value> {
    use std::sync::Arc;
    let reconciler = host
        .command_sync
        .get()
        .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
    let guilds = host
        .core
        .status(&oracle_core::PolicyContext::LocalOperator, None)
        .await?
        .guilds;
    let mut reports = Vec::new();
    let cancel = host.operation_tasks.token();
    for guild in guilds {
        let (revision, desired) = prepare(host, &guild.guild).await?;
        let report = reconciler
            .reconcile_fenced(
                &guild.guild,
                &desired,
                &cancel,
                oracle_operations::executor::now() + 60,
                Arc::new(RegistryFence(host.modules.registry_permit(revision))),
            )
            .await?;
        reports.push(json!({"guild":guild.guild,"revision":revision,"report":report}));
    }
    Ok(json!(reports))
}
