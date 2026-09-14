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
