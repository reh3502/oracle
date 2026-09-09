//! Structure and configuration command-line presentation.
use crate::control::Request;
use oracle_core::*;
use oracle_operations::ingress::OperationRequest;

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
    use crate::cli::{Cli, Command};
    use clap::Parser;
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
