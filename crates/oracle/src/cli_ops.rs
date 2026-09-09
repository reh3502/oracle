use super::*;
use oracle_operations::{
    executor::StructureExecutor,
    ingress::{HumanOperations, OperationRequest},
};
use tokio_util::sync::CancellationToken;

#[derive(clap::Args)]
pub struct StructureArgs {
    #[arg(long)]
    guild: GuildId,
    #[command(subcommand)]
    command: StructureCommand,
}
#[derive(clap::Subcommand)]
enum StructureCommand {
    Inspect,
    Plan {
        #[arg(long)]
        input: String,
    },
    Show {
        #[arg(long)]
        plan: String,
    },
    Approve {
        #[arg(long)]
        plan: String,
        #[arg(long)]
        hash: String,
    },
    Apply {
        #[arg(long)]
        plan: String,
    },
}
impl StructureArgs {
    pub fn request(self) -> Result<Request> {
        let request = match self.command {
            StructureCommand::Inspect => OperationRequest::Inspect,
            StructureCommand::Plan { input } => OperationRequest::Plan {
                request: serde_json::from_str(&input)
                    .map_err(|_| Error::new(ErrorCode::InvalidInput))?,
            },
            StructureCommand::Show { plan } => OperationRequest::Show { plan },
            StructureCommand::Approve { plan, hash } => OperationRequest::Approve { plan, hash },
            StructureCommand::Apply { plan } => OperationRequest::Apply { plan },
        };
        Ok(Request::Operation {
            guild: self.guild,
            request,
        })
    }
}
#[derive(clap::Args)]
pub struct ConfigurationArgs {
    #[arg(long)]
    guild: GuildId,
    #[arg(long)]
    module: ModuleId,
    #[command(subcommand)]
    command: ConfigurationCommand,
}
#[derive(clap::Subcommand)]
enum ConfigurationCommand {
    Inspect,
    Plan {
        #[arg(long)]
        preset: Option<String>,
        #[arg(long, default_value = "{}")]
        values: String,
    },
    Apply {
        #[arg(long)]
        plan: String,
    },
    Recover,
}
impl ConfigurationArgs {
    pub fn request(self) -> Result<Request> {
        let module = self.module;
        let request = match self.command {
            ConfigurationCommand::Inspect => OperationRequest::ConfigurationInspect { module },
            ConfigurationCommand::Plan { preset, values } => OperationRequest::ConfigurationPlan {
                module,
                preset,
                values: serde_json::from_str(&values)
                    .map_err(|_| Error::new(ErrorCode::InvalidInput))?,
            },
            ConfigurationCommand::Apply { plan } => {
                OperationRequest::ConfigurationApply { module, plan }
            }
            ConfigurationCommand::Recover => OperationRequest::ConfigurationRecover { module },
        };
        Ok(Request::Operation {
            guild: self.guild,
            request,
        })
    }
}
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
        let modules = self.modules.clone();
        let context = context.clone();
        let guild = guild.clone();
        let root = self.operation_tasks.token();
        let request_cancel = cancel.clone();
        let work_cancel = root.child_token();
        let (send, receive) = tokio::sync::oneshot::channel();
        self.operation_tasks
            .spawn("human_operation", async move {
                let work = dispatch(structure, modules, &context, &guild, request, &work_cancel);
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

/// Config-only SDK modules work without Discord. A destination always requires a live adapter.
pub struct OfflinePolicy;
#[async_trait::async_trait]
impl oracle_modules::ConfigurationPolicy for OfflinePolicy {
    async fn validate(
        &self,
        _actor: &PolicyContext,
        _guild: &GuildId,
        _module: &ModuleId,
        values: &serde_json::Value,
    ) -> Result<()> {
        if values.get("destination").is_some() {
            Err(Error::new(ErrorCode::ModuleUnavailable))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn human_cli_preserves_exact_plan_approval_and_configuration() {
        let cli = Cli::try_parse_from([
            "oracle",
            "structure",
            "--guild",
            "123",
            "approve",
            "--plan",
            "saved-plan",
            "--hash",
            "exact-hash",
        ])
        .unwrap();
        let Command::Structure(args) = cli.command else {
            panic!("structure command")
        };
        let Request::Operation { guild, request } = args.request().unwrap() else {
            panic!("operation request")
        };
        assert_eq!(guild.as_str(), "123");
        let OperationRequest::Approve { plan, hash } = request else {
            panic!("approval request")
        };
        assert_eq!((plan.as_str(), hash.as_str()), ("saved-plan", "exact-hash"));
        let cli = Cli::try_parse_from([
            "oracle",
            "module-config",
            "--guild",
            "123",
            "--module",
            "community.activity-log",
            "plan",
            "--preset",
            "moderate/v1",
            "--values",
            r#"{"destination":"456"}"#,
        ])
        .unwrap();
        let Command::Configuration(args) = cli.command else {
            panic!("configuration command")
        };
        let Request::Operation {
            request:
                OperationRequest::ConfigurationPlan {
                    module,
                    preset,
                    values,
                },
            ..
        } = args.request().unwrap()
        else {
            panic!("configuration plan")
        };
        assert_eq!(module.as_str(), "community.activity-log");
        assert_eq!(preset.as_deref(), Some("moderate/v1"));
        assert_eq!(values, serde_json::json!({"destination":"456"}));
    }
    #[tokio::test]
    async fn offline_configuration_cannot_approve_discord_destinations() {
        use oracle_modules::ConfigurationPolicy;
        let guild = GuildId::new("123").unwrap();
        let module = ModuleId::new("sample.config").unwrap();
        OfflinePolicy
            .validate(
                &PolicyContext::LocalOperator,
                &guild,
                &module,
                &serde_json::json!({"enabled":true}),
            )
            .await
            .unwrap();
        for values in [
            serde_json::json!({"destination":"456"}),
            serde_json::json!({"destination":null}),
        ] {
            assert_eq!(
                OfflinePolicy
                    .validate(&PolicyContext::LocalOperator, &guild, &module, &values)
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::ModuleUnavailable
            );
        }
    }
}
