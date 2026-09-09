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
    async fn execute(
        &self,
        context: &PolicyContext,
        guild: &GuildId,
        request: OperationRequest,
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value> {
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
