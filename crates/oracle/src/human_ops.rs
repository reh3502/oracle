//! Shared human operation dispatch and its tracked cancellation scope.
use crate::{command_runtime, host::Host, value};
use oracle_core::*;
use oracle_modules::ModuleManager;
use oracle_operations::{
    executor::StructureExecutor,
    ingress::{HumanOperations, OperationRequest},
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[async_trait::async_trait]
impl HumanOperations for Host {
    async fn resolve_run_resume(
        &self,
        context: &PolicyContext,
        member: &oracle_core::member_read::MemberContext,
        run_id: &str,
        interaction_id: &str,
        cancel: &CancellationToken,
    ) -> Result<oracle_operations::ingress::PublishedRequest> {
        validate_resume_identity(context, member, run_id, interaction_id)?;
        tokio::select! {biased;
            _ = cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
            _ = tokio::time::sleep(Duration::from_secs(5)) => Err(Error::new(ErrorCode::ModuleUnavailable)),
            result = async {
                let entries = self.modules.member_catalog(member, &member.guild).await?.entries;
                let entry = entries.into_iter().find(|entry| entry.module.as_str() == "community.dandys-world")
                    .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
                let document = self.storage.document_get(&entry.module, &member.guild, "runs", run_id).await?
                    .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| Error::new(ErrorCode::Integrity))?.as_millis();
                let now = u64::try_from(now).map_err(|_| Error::new(ErrorCode::Integrity))?;
                validate_resume_run(&document.value, &member.guild, run_id, now)?;
                let reconciler = self.command_sync.get().ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
                let bindings = reconciler.bindings(&member.guild).await?;
                run_resume_request(&entry, &bindings, run_id, interaction_id)
            } => result,
        }
    }
    async fn resolve_shared(
        &self,
        _context: &PolicyContext,
        member: &oracle_core::member_read::MemberContext,
        request: oracle_operations::ingress::SharedControlRequest,
        cancel: &CancellationToken,
    ) -> Result<oracle_operations::ingress::PublishedRequest> {
        use oracle_operations::ingress::{PrivateAction, PublishedRequest};
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        let shared = self
            .shared_cards
            .get()
            .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
        let record = shared
            .journal
            .resolve_control(&member.guild, &request.custom_id)
            .await?;
        let action = record.verify_control(
            &member.guild,
            &member.channel,
            &request.message_id,
            &request.application_id,
            &request.author_id,
            &request.custom_id,
        )?;
        let entries = self
            .modules
            .member_catalog(member, &member.guild)
            .await?
            .entries;
        let entry = entries
            .into_iter()
            .find(|entry| entry.module == record.module)
            .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
        let reconciler = self
            .command_sync
            .get()
            .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
        let bindings = reconciler.bindings(&member.guild).await?;
        let binding = bindings
            .into_iter()
            .find(|binding| {
                binding.owner == entry.module
                    && binding.id.is_some()
                    && binding.definition["name"].as_str()
                        == Some(entry.commands.namespace.as_str())
                    && binding.route.as_ref().is_some_and(|route| {
                        route.session == entry.session
                            && route.generation == entry.generation
                            && route.epoch == entry.epoch
                    })
            })
            .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
        let route = entry
            .commands
            .routes
            .iter()
            .find(|route| {
                matches!(
                    route.presentation,
                    Some(ModulePresentation::PrivateCardV2 { .. })
                )
            })
            .ok_or_else(|| Error::new(ErrorCode::ForbiddenPermission))?;
        Ok(PublishedRequest {
            interaction_id: Some(request.interaction_id),
            private_action: Some(PrivateAction {
                operation: action.operation,
                input: action.input,
            }),
            expected_binding: Some(format!(
                "{}/{}/{}/{}",
                entry.module, entry.session, entry.generation, entry.epoch
            )),
            member_only: false,
            command_id: binding.id.ok_or_else(|| Error::new(ErrorCode::Integrity))?,
            command_name: entry.commands.namespace.clone(),
            route: route.name.clone(),
            options: serde_json::Map::new(),
        })
    }
    async fn published_uses_member_identity(
        &self,
        context: &PolicyContext,
        member: &oracle_core::member_read::MemberContext,
        guild: &GuildId,
        request: &oracle_operations::ingress::PublishedRequest,
    ) -> Result<bool> {
        let commands = self
            .command_sync
            .get()
            .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
        command_runtime::published_uses_member_identity(
            &self.modules,
            commands,
            context,
            member,
            guild,
            request,
        )
        .await
    }
    async fn execute_published(
        &self,
        context: &PolicyContext,
        member: &oracle_core::member_read::MemberContext,
        guild: &GuildId,
        request: oracle_operations::ingress::PublishedRequest,
        cancel: &CancellationToken,
    ) -> Result<oracle_operations::ingress::PublishedReply> {
        let commands = self
            .command_sync
            .get()
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
        let modules = self.modules.clone();
        let (context, member, guild) = (context.clone(), member.clone(), guild.clone());
        let root = self.operation_tasks.token();
        let cancel = cancel.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        self.operation_tasks.spawn("published_command", async move {
            let result = tokio::select! {biased;
                _ = root.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
                _ = cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
                result = command_runtime::invoke_published_options(&modules, &commands, &context, &member, &guild, request) => result,
            };
            let _ = send.send(result);
            Ok(())
        }).map_err(|_| Error::new(ErrorCode::Cancelled))?;
        receive
            .await
            .map_err(|_| Error::new(ErrorCode::Cancelled))?
    }
    fn ai_available(&self) -> bool {
        self.ai.get().is_some()
    }
    async fn execute(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        request: OperationRequest,
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value> {
        if let OperationRequest::Agent { request } = request {
            return crate::ai::human(self, context, guild, request).await;
        }
        let structure = self.operations.get().cloned();
        let commands = self.command_sync.get().cloned();
        let modules = self.modules.clone();
        let context = context.clone();
        let guild = guild.clone();
        let root = self.operation_tasks.token();
        let request_cancel = cancel.clone();
        let work_cancel = root.child_token();
        let (send, receive) = tokio::sync::oneshot::channel();
        self.operation_tasks
            .spawn("human_operation", async move {
                let work = dispatch(
                    structure,
                    commands,
                    modules,
                    &context,
                    &guild,
                    request,
                    &work_cancel,
                );
                tokio::pin!(work);
                let result = tokio::select! {biased;
                 _=root.cancelled()=>{work_cancel.cancel();work.await},
                 _=request_cancel.cancelled()=>{work_cancel.cancel();work.await},
                 result=&mut work=>result,
                };
                let _ = send.send(result);
                Ok(())
            })
            .map_err(|_| Error::new(ErrorCode::Cancelled))?;
        receive
            .await
            .map_err(|_| Error::new(ErrorCode::Cancelled))?
    }
}
async fn dispatch(
    structure: Option<Arc<StructureExecutor>>,
    commands: Option<Arc<oracle_operations::commands::CommandReconciler>>,
    modules: Arc<ModuleManager>,
    context: &PolicyContext,
    guild: &GuildId,
    request: OperationRequest,
    cancel: &CancellationToken,
) -> Result<serde_json::Value> {
    if cancel.is_cancelled() {
        return Err(Error::new(ErrorCode::Cancelled));
    }
    let service = || {
        structure
            .as_ref()
            .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))
    };
    match request {
        OperationRequest::Agent { .. } => Err(Error::new(ErrorCode::InvalidInput)),
        request @ OperationRequest::InvokePublished { .. } => {
            let commands = commands.ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
            tokio::select! { biased;
                _ = cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
                result = command_runtime::invoke_published(&modules, &commands, context, guild, request) => result,
            }
        }
        OperationRequest::Inspect => value(service()?.inspect(context, guild).await?),
        OperationRequest::Plan { request } => {
            value(service()?.plan(context, guild, &request).await?)
        }
        OperationRequest::Show { plan } => {
            value(service()?.read_plan(context, guild, &plan).await?)
        }
        OperationRequest::Approve { plan, hash } => {
            value(service()?.approve(context, guild, &plan, &hash).await?)
        }
        OperationRequest::Apply { plan } => {
            value(service()?.apply(context, guild, &plan, cancel).await?)
        }
        OperationRequest::ConfigurationInspect { module } => value(
            modules
                .configuration_inspect(context, guild, &module)
                .await?,
        ),
        OperationRequest::ConfigurationPlan {
            module,
            preset,
            values,
        } => value(
            modules
                .configuration_plan(
                    context,
                    guild,
                    &module,
                    preset.as_deref(),
                    values,
                    Duration::from_secs(900),
                )
                .await?,
        ),
        OperationRequest::ConfigurationApply { module, plan } => tokio::select! {biased;
         _=cancel.cancelled()=>Err(Error::new(ErrorCode::Cancelled)),
         receipt=modules.configuration_apply(context,guild,&module,&plan)=>value(receipt?),
        },
        OperationRequest::ConfigurationRecover { module } => tokio::select! {biased;
         _=cancel.cancelled()=>Err(Error::new(ErrorCode::Cancelled)),
         receipt=modules.configuration_recover(context,guild,&module)=>value(receipt?),
        },
    }
}

/// This boundary accepts authenticated Discord identity only. Current module
/// invocation still enforces owner/moderator access, including draft visibility.
fn validate_resume_identity(
    context: &PolicyContext,
    member: &oracle_core::member_read::MemberContext,
    run_id: &str,
    interaction_id: &str,
) -> Result<()> {
    if !matches!(context, PolicyContext::Discord { guild, user, .. } if guild == &member.guild && user == &member.user)
        || member.observed_at.elapsed() > Duration::from_secs(10)
    {
        return Err(Error::new(ErrorCode::ForbiddenPermission));
    }
    if run_id.len() != 8
        || !run_id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        || UserId::new(interaction_id).is_err()
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}

fn validate_resume_run(
    document: &serde_json::Value,
    guild: &GuildId,
    run_id: &str,
    now: u64,
) -> Result<()> {
    let run = &document["run"];
    if run["id"].as_str() != Some(run_id) || run["guild_id"].as_str() != Some(guild.as_str()) {
        return Err(Error::new(ErrorCode::ForbiddenScope));
    }
    let state = run["state"]
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
    if !matches!(
        state,
        "draft" | "open" | "locked" | "completed" | "cancelled"
    ) {
        return Err(Error::new(ErrorCode::Integrity));
    }
    match run.get("schedule").filter(|schedule| !schedule.is_null()) {
        Some(schedule) => {
            let start = schedule["starts_at"]
                .as_u64()
                .filter(|start| *start <= 253_402_300_799)
                .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
            let duration = schedule["duration_minutes"]
                .as_u64()
                .filter(|minutes| (1..=1440).contains(minutes))
                .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
            let end = start
                .checked_add(duration * 60)
                .and_then(|seconds| seconds.checked_mul(1000))
                .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
            if now >= end {
                return Err(Error::new(ErrorCode::NotFound));
            }
        }
        None if state == "draft" => {
            let edited = run["last_owner_edit_at"]
                .as_u64()
                .filter(|edited| *edited <= now)
                .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
            if now - edited >= 86_400_000 {
                return Err(Error::new(ErrorCode::NotFound));
            }
        }
        // Older unscheduled runs have no scheduled cutoff. Their persisted domain
        // state and normal run_ui permissions remain authoritative.
        None => {}
    }
    Ok(())
}

fn run_resume_request(
    entry: &oracle_modules::ModuleCatalogEntry,
    bindings: &[oracle_operations::commands::CommandBinding],
    run_id: &str,
    interaction_id: &str,
) -> Result<oracle_operations::ingress::PublishedRequest> {
    use oracle_operations::ingress::{PrivateAction, PublishedRequest};
    if entry.module.as_str() != "community.dandys-world"
        || entry.commands.namespace != "dw"
        || !entry.operations.iter().any(|operation| {
            operation.name == "run_ui" && operation.audience == ModuleAudience::MemberMutation
        })
    {
        return Err(Error::new(ErrorCode::ForbiddenPermission));
    }
    let route = entry
        .commands
        .routes
        .iter()
        .find(|route| {
            route.name == "run"
                && route.operation == "run_view"
                && matches!(
                    route.presentation,
                    Some(ModulePresentation::PrivateCardV2 { .. })
                )
        })
        .ok_or_else(|| Error::new(ErrorCode::ForbiddenPermission))?;
    let binding = bindings
        .iter()
        .find(|binding| {
            binding.owner == entry.module
                && binding.id.is_some()
                && !binding.deleted
                && binding.pending.is_none()
                && binding.definition["name"] == entry.commands.namespace
                && binding.route.as_ref().is_some_and(|route| {
                    route.session == entry.session
                        && route.generation == entry.generation
                        && route.epoch == entry.epoch
                })
        })
        .ok_or_else(|| Error::new(ErrorCode::ModuleUnavailable))?;
    Ok(PublishedRequest {
        interaction_id: Some(interaction_id.into()),
        private_action: Some(PrivateAction {
            operation: "run_ui".into(),
            input: serde_json::Map::from_iter([
                ("action".into(), serde_json::json!("view")),
                ("view".into(), serde_json::json!("summary")),
                ("id".into(), serde_json::json!(run_id)),
            ]),
        }),
        expected_binding: Some(format!(
            "{}/{}/{}/{}",
            entry.module, entry.session, entry.generation, entry.epoch
        )),
        member_only: false,
        command_id: binding
            .id
            .clone()
            .ok_or_else(|| Error::new(ErrorCode::Integrity))?,
        command_name: entry.commands.namespace.clone(),
        route: route.name.clone(),
        options: serde_json::Map::new(),
    })
}

#[cfg(test)]
mod run_resume_tests {
    use super::*;
    use oracle_modules::ModuleCatalogEntry;
    use oracle_operations::commands::{CommandBinding, CommandRoute, PendingCommand};
    use serde_json::json;
    use std::{collections::BTreeSet, time::Instant};

    fn member() -> oracle_core::member_read::MemberContext {
        oracle_core::member_read::MemberContext {
            guild: "123".parse().unwrap(),
            user: "900".parse().unwrap(),
            channel: "700".into(),
            roles: BTreeSet::new(),
            observed_at: Instant::now(),
        }
    }
    fn context() -> PolicyContext {
        PolicyContext::Discord {
            guild: "123".parse().unwrap(),
            user: "900".parse().unwrap(),
            manage_guild: false,
        }
    }
    fn catalog() -> (ModuleCatalogEntry, CommandBinding) {
        let manifest: ModuleManifest =
            serde_json::from_str(include_str!("../../../archive/dandys-world/manifest.json"))
                .unwrap();
        let entry = ModuleCatalogEntry {
            module: manifest.id,
            session: "current-session".into(),
            generation: 7,
            epoch: 12,
            operations: manifest.operations,
            commands: manifest.commands.unwrap(),
        };
        let binding = CommandBinding {
            owner: entry.module.clone(),
            id: Some("1000".into()),
            definition: json!({"name":"dw"}),
            route: Some(CommandRoute {
                session: entry.session.clone(),
                generation: entry.generation,
                epoch: entry.epoch,
            }),
            pending: None,
            target: None,
            deleted: false,
        };
        (entry, binding)
    }
    #[test]
    fn run_resume_requires_fresh_matching_discord_identity() {
        let actor = member();
        validate_resume_identity(&context(), &actor, "ABCDEFGH", "1000").unwrap();
        assert!(
            validate_resume_identity(&PolicyContext::LocalOperator, &actor, "ABCDEFGH", "1000")
                .is_err()
        );
        let mut stale = actor.clone();
        stale.observed_at = Instant::now() - Duration::from_secs(11);
        assert!(validate_resume_identity(&context(), &stale, "ABCDEFGH", "1000").is_err());
        let mut foreign = actor.clone();
        foreign.guild = "124".parse().unwrap();
        assert!(validate_resume_identity(&context(), &foreign, "ABCDEFGH", "1000").is_err());
        let mut other = actor.clone();
        other.user = "901".parse().unwrap();
        assert!(validate_resume_identity(&context(), &other, "ABCDEFGH", "1000").is_err());
        for (id, interaction) in [
            ("../other", "1000"),
            ("ABCDEFGH", "forged"),
            ("short", "1000"),
        ] {
            assert!(validate_resume_identity(&context(), &actor, id, interaction).is_err());
        }
    }
    #[test]
    fn run_resume_uses_authoritative_schedule_end_and_guild() {
        let guild = "123".parse().unwrap();
        let mut doc = json!({"run":{"id":"ABCDEFGH","guild_id":"123","state":"open","schedule":{"starts_at":1000,"duration_minutes":60}}});
        // Start is seconds; the clock and draft retention fields are milliseconds.
        validate_resume_run(&doc, &guild, "ABCDEFGH", 4_599_999).unwrap();
        assert!(validate_resume_run(&doc, &guild, "ABCDEFGH", 4_600_000).is_err());
        assert!(validate_resume_run(&doc, &guild, "ABCDEFGH", 4_600_001).is_err());
        assert!(validate_resume_run(&doc, &"124".parse().unwrap(), "ABCDEFGH", 1).is_err());
        assert!(validate_resume_run(&doc, &guild, "HGFEDCBA", 1).is_err());
        for state in ["draft", "locked", "completed", "cancelled"] {
            doc["run"]["state"] = json!(state);
            validate_resume_run(&doc, &guild, "ABCDEFGH", 4_599_999).unwrap();
            assert!(validate_resume_run(&doc, &guild, "ABCDEFGH", 4_600_000).is_err());
        }
        for schedule in [
            json!({"starts_at":-1,"duration_minutes":60}),
            json!({"starts_at":1000,"duration_minutes":0}),
            json!({"starts_at":u64::MAX,"duration_minutes":1440}),
            json!({"starts_at":1000}),
        ] {
            doc["run"]["schedule"] = schedule;
            assert!(validate_resume_run(&doc, &guild, "ABCDEFGH", 1).is_err());
        }
    }
    #[test]
    fn unscheduled_draft_resume_observes_existing_retention() {
        let guild = "123".parse().unwrap();
        let doc = json!({"run":{"id":"ABCDEFGH","guild_id":"123","state":"draft","schedule":null,"last_owner_edit_at":1000}});
        validate_resume_run(&doc, &guild, "ABCDEFGH", 86_400_999).unwrap();
        assert!(validate_resume_run(&doc, &guild, "ABCDEFGH", 86_401_000).is_err());
        assert!(validate_resume_run(&doc, &guild, "ABCDEFGH", 999).is_err());
    }
    #[test]
    fn resumed_request_only_reopens_summary_with_current_binding() {
        let (entry, binding) = catalog();
        let request =
            run_resume_request(&entry, std::slice::from_ref(&binding), "ABCDEFGH", "1001").unwrap();
        let action = request.private_action.unwrap();
        assert_eq!(action.operation, "run_ui");
        assert_eq!(
            json!(action.input),
            json!({"action":"view","view":"summary","id":"ABCDEFGH"})
        );
        assert_eq!(request.interaction_id.as_deref(), Some("1001"));
        assert_eq!(
            request.expected_binding.as_deref(),
            Some("community.dandys-world/current-session/7/12")
        );
        assert_eq!(
            (
                request.command_id.as_str(),
                request.command_name.as_str(),
                request.route.as_str()
            ),
            ("1000", "dw", "run")
        );
        assert!(request.options.is_empty());
        for index in 0..6 {
            let mut stale = binding.clone();
            match index {
                0 => stale.route.as_mut().unwrap().session = "old-session".into(),
                1 => stale.route.as_mut().unwrap().generation -= 1,
                2 => stale.route.as_mut().unwrap().epoch -= 1,
                3 => stale.deleted = true,
                4 => stale.pending = Some(PendingCommand::Edit),
                _ => stale.owner = "foreign.module".parse().unwrap(),
            }
            assert!(run_resume_request(&entry, &[stale], "ABCDEFGH", "1001").is_err());
        }
        let mut denied = entry.clone();
        denied
            .operations
            .retain(|operation| operation.name != "run_ui");
        assert!(
            run_resume_request(&denied, std::slice::from_ref(&binding), "ABCDEFGH", "1001")
                .is_err()
        );
        let mut denied = entry;
        denied.commands.routes.retain(|route| route.name != "run");
        assert!(run_resume_request(&denied, &[binding], "ABCDEFGH", "1001").is_err());
    }
}
