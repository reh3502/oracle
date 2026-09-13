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
            .map(oracle_operations::published::compile_route)
            .collect::<Result<_>>()?;
        let member_visible = entry
            .operations
            .iter()
            .any(|op| op.audience == oracle_core::ModuleAudience::MemberRead);
        desired.push(DesiredCommand {
            owner: entry.module.clone(),
            route: Some(CommandRoute {
                session: entry.session.clone(),
                generation: entry.generation,
                epoch: entry.epoch,
            }),
            definition: json!({"type":1,"name":descriptor.namespace,
                "description":descriptor.description,"default_member_permissions":if member_visible {serde_json::Value::Null} else {json!("32")},
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

struct MemberResponseFence {
    policy: oracle_core::member_read::MemberReadPermit,
    registry: oracle_modules::RegistryDispatchPermit,
}
impl oracle_operations::executor::DispatchFence for MemberResponseFence {
    fn dispatch(&self, send: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        // Registry-before-policy matches configuration publication's lock order.
        self.registry.dispatch(|| self.policy.dispatch(send))??
    }
}

fn validate_card_actions(
    card: &oracle_operations::published::CardPresentation,
    entry: &ModuleCatalogEntry,
) -> Result<()> {
    use oracle_core::{ModuleAudience, ModuleCommandInput, ModuleCommandOptionType};
    let actions = card
        .buttons
        .iter()
        .map(|a| (&a.route, &a.options, a.prompt.as_ref()))
        .chain(card.choices.iter().map(|a| (&a.route, &a.options, None)));
    for (name, options, prompt) in actions {
        let route = entry
            .commands
            .routes
            .iter()
            .find(|r| &r.name == name)
            .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
        if !entry
            .operations
            .iter()
            .any(|op| op.name == route.operation && op.audience == ModuleAudience::MemberRead)
        {
            return Err(Error::new(ErrorCode::ForbiddenPermission));
        }
        let mut supplied = options.clone();
        if let Some(prompt) = prompt {
            let Some(ModuleCommandInput::Typed {
                options: descriptors,
            }) = &route.input
            else {
                return Err(Error::new(ErrorCode::InvalidInput));
            };
            let descriptor = descriptors
                .iter()
                .find(|d| d.name == prompt.option)
                .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
            let ModuleCommandOptionType::String {
                min_length,
                max_length,
                choices,
            } = &descriptor.value_type
            else {
                return Err(Error::new(ErrorCode::InvalidInput));
            };
            if !choices.is_empty()
                || u32::from(prompt.max_length) > *max_length
                || u32::from(prompt.max_length) < *min_length
                || supplied.contains_key(&prompt.option)
            {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
            supplied.insert(
                prompt.option.clone(),
                serde_json::Value::String("x".repeat((*min_length).max(1) as usize)),
            );
        }
        oracle_operations::published::decode_input(route, &supplied)?;
    }
    Ok(())
}

fn interaction_binding(entry: &ModuleCatalogEntry) -> String {
    format!(
        "{}/{}/{}/{}",
        entry.module, entry.session, entry.generation, entry.epoch
    )
}

async fn resolve_published_entry(
    manager: &oracle_modules::ModuleManager,
    reconciler: &oracle_operations::commands::CommandReconciler,
    actor: &oracle_core::PolicyContext,
    member: &oracle_core::member_read::MemberContext,
    guild: &oracle_core::GuildId,
    request: &oracle_operations::ingress::PublishedRequest,
) -> Result<ModuleCatalogEntry> {
    use oracle_core::PolicyContext;
    if !matches!(actor, PolicyContext::Discord {guild: g, user, ..} if g == guild && g == &member.guild && user == &member.user)
    {
        return Err(Error::new(ErrorCode::ForbiddenScope));
    }
    if request.member_only && request.expected_binding.is_none() {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    let binding = reconciler
        .bindings(guild)
        .await?
        .into_iter()
        .find(|binding| binding.id.as_deref() == Some(&request.command_id))
        .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
    if binding.definition["name"].as_str() != Some(&request.command_name) {
        return Err(Error::new(ErrorCode::ForbiddenScope));
    }
    let identity = binding
        .route
        .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
    let entries = match manager.catalog(actor, guild).await {
        Ok(entries) => entries,
        Err(error) if error.code == ErrorCode::ForbiddenPermission => {
            manager.member_catalog(member, guild).await?.entries
        }
        Err(error) => return Err(error),
    };
    let entry = entries
        .into_iter()
        .find(|entry| {
            entry.module == binding.owner
                && entry.session == identity.session
                && entry.generation == identity.generation
                && entry.epoch == identity.epoch
                && entry.commands.namespace == request.command_name
        })
        .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
    if request
        .expected_binding
        .as_ref()
        .is_some_and(|expected| expected != &interaction_binding(&entry))
    {
        return Err(Error::new(ErrorCode::ModuleUnavailable));
    }
    Ok(entry)
}

pub async fn published_uses_member_identity(
    manager: &oracle_modules::ModuleManager,
    reconciler: &oracle_operations::commands::CommandReconciler,
    actor: &oracle_core::PolicyContext,
    member: &oracle_core::member_read::MemberContext,
    guild: &oracle_core::GuildId,
    request: &oracle_operations::ingress::PublishedRequest,
) -> Result<bool> {
    let entry = resolve_published_entry(manager, reconciler, actor, member, guild, request).await?;
    let route = entry
        .commands
        .routes
        .iter()
        .find(|r| r.name == request.route)
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
    let operation = entry
        .operations
        .iter()
        .find(|op| op.name == route.operation)
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
    Ok(operation.audience == oracle_core::ModuleAudience::MemberRead)
}

pub async fn invoke_published_options(
    manager: &oracle_modules::ModuleManager,
    reconciler: &oracle_operations::commands::CommandReconciler,
    actor: &oracle_core::PolicyContext,
    member: &oracle_core::member_read::MemberContext,
    guild: &oracle_core::GuildId,
    request: oracle_operations::ingress::PublishedRequest,
) -> Result<oracle_operations::ingress::PublishedReply> {
    use oracle_core::ModuleAudience;
    use oracle_operations::{
        ingress::PublishedReply,
        published::{decode_input, render_card_with_images, render_presentation},
    };
    let entry =
        resolve_published_entry(manager, reconciler, actor, member, guild, &request).await?;
    let route = entry
        .commands
        .routes
        .iter()
        .find(|route| route.name == request.route)
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
    let operation = entry
        .operations
        .iter()
        .find(|op| op.name == route.operation)
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
    if request.member_only && operation.audience != ModuleAudience::MemberRead {
        return Err(Error::new(ErrorCode::ForbiddenPermission));
    }
    let input = decode_input(route, &request.options)?;
    let settings = manager.runtime_settings(&entry.module);
    let prefix = settings
        .as_ref()
        .and_then(|settings| settings.citation_prefix.as_deref());
    let image_prefix = settings
        .as_ref()
        .and_then(|settings| settings.image_prefix.as_deref());
    if operation.audience == ModuleAudience::MemberRead {
        let invocation = manager
            .invoke_member_bound(
                member,
                guild,
                &entry.module,
                &operation.name,
                input,
                &entry.session,
                entry.generation,
                entry.epoch,
            )
            .await?;
        let card = render_card_with_images(route, &invocation.value, prefix, image_prefix)?;
        if let Some(card) = &card {
            validate_card_actions(card, &entry)?;
        }
        let text = if card.is_some() {
            Some("Your answer is in the card below.".to_owned())
        } else {
            render_presentation(route, &invocation.value, prefix)?
        };
        if text.is_none() {
            return Err(Error::new(ErrorCode::Compatibility));
        }
        Ok(PublishedReply {
            card,
            binding: Some(interaction_binding(&entry)),
            control_fence: Some(std::sync::Arc::new(RegistryFence(
                invocation.registry.clone(),
            ))),
            value: invocation.value,
            text,
            policy: Some(invocation.policy.clone()),
            fence: Some(std::sync::Arc::new(MemberResponseFence {
                policy: invocation.policy,
                registry: invocation.registry,
            })),
        })
    } else {
        let revision = manager.registry_revision();
        let value = manager
            .invoke_bound(
                actor,
                guild,
                &entry.module,
                &operation.name,
                input,
                &entry.session,
                entry.generation,
                entry.epoch,
            )
            .await?;
        let card = render_card_with_images(route, &value, prefix, image_prefix)?;
        // Interactive presentation is reserved for member-read routes.
        if card.is_some() {
            return Err(Error::new(ErrorCode::Compatibility));
        }
        let text = render_presentation(route, &value, prefix)?;
        Ok(PublishedReply {
            card: None,
            binding: None,
            control_fence: None,
            value,
            text,
            policy: None,
            fence: Some(std::sync::Arc::new(RegistryFence(
                manager.registry_permit(revision),
            ))),
        })
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
                    input: None,
                    presentation: None,
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
