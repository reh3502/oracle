//! Explicitly invoked disposable-guild smoke gate. Never run as part of offline checks.
use serde_json::{Value, json};
use serenity::all::*;
use std::{
    error::Error,
    future::Future,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
async fn request<T>(future: impl Future<Output = serenity::Result<T>>) -> Result<T> {
    Ok(tokio::time::timeout(Duration::from_secs(30), future).await??)
}
fn persist(path: &PathBuf, report: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(report)?)?;
    Ok(())
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("P3 live gate failed: {error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let mode = std::env::args().nth(1);
    if !matches!(mode.as_deref(), Some("--execute" | "--inspect")) {
        return Err("Live writes disabled. Usage: live --inspect or --execute [report-path]; requires ORACLE_P3_DISCORD_TOKEN, ORACLE_P3_GUILD_ID for an explicitly authorized disposable guild".into());
    }
    let token = Token::from_env("ORACLE_P3_DISCORD_TOKEN")?;
    let guild = GuildId::new(std::env::var("ORACLE_P3_GUILD_ID")?.parse()?);
    let path = PathBuf::from(
        std::env::args()
            .nth(2)
            .unwrap_or_else(|| "artifacts/live.json".into()),
    );
    let http = HttpBuilder::new(token).build();
    let application = request(http.get_current_application_info()).await?.id;
    if let Ok(expected) = std::env::var("ORACLE_P3_APPLICATION_ID")
        && ApplicationId::new(expected.parse()?) != application
    {
        return Err("configured application differs from token application".into());
    }
    http.set_application_id(application);
    let current = request(http.get_current_user()).await?;
    if mode.as_deref() == Some("--inspect") {
        let channels = request(http.get_channels(guild)).await?;
        let commands = request(http.get_guild_commands(guild)).await?;
        println!(
            "{}",
            json!({"status":"read_only_preflight_passed", "application_id":application.to_string(), "guild_id":guild.to_string(), "visible_channels":channels.len(), "guild_commands":commands.len()})
        );
        return Ok(());
    }
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let prefix = format!("oracle-p3-{suffix}");
    let mut channels: Vec<(ChannelId, String, ChannelType, Option<ChannelId>)> = Vec::new();
    let mut command: Option<CommandId> = None;
    let mut report = json!({"serenity_rev":oracle_serenity_baseline::SERENITY_REV,"guild_id":guild.to_string(),"application_id":application.to_string(),"status":"running","owned_channels":[],"owned_command":null,"cleanup_errors":[]});
    persist(&path, &report)?;
    let outcome: Result<()> = async {
        // Reads establish credentials and prove no existing name is overwritten.
        let initial = request(http.get_channels(guild)).await?;
        let commands = request(http.get_guild_commands(guild)).await?;
        if initial.iter().any(|c| c.base.name.starts_with(&prefix))
            || commands.iter().any(|c| c.name.as_str() == prefix)
        {
            return Err("generated test name collides; no mutations made".into());
        }
        let overwrites = vec![
            PermissionOverwrite {
                allow: Permissions::empty(),
                deny: Permissions::VIEW_CHANNEL,
                kind: PermissionOverwriteType::Role(RoleId::new(guild.get())),
            },
            PermissionOverwrite {
                allow: Permissions::VIEW_CHANNEL
                    | Permissions::MANAGE_CHANNELS
                    | Permissions::MANAGE_ROLES
                    | Permissions::SEND_MESSAGES
                    | Permissions::CONNECT,
                deny: Permissions::empty(),
                kind: PermissionOverwriteType::Member(current.id),
            },
        ];
        let mut category = None;
        for (label, kind) in [
            ("category", ChannelType::Category),
            ("text", ChannelType::Text),
            ("voice", ChannelType::Voice),
        ] {
            let name = format!("{prefix}-{label}");
            let mut builder = CreateChannel::new(name.clone())
                .kind(kind)
                .permissions(overwrites.clone());
            if let Some(parent) = category {
                builder = builder.category(parent);
            }
            let created = request(http.create_channel(
                guild,
                &builder,
                Some("Oracle P3 disposable-guild test"),
            ))
            .await?;
            channels.push((created.id, name.clone(), kind, category));
            report["owned_channels"] = json!(
                channels
                    .iter()
                    .map(|(id, name, _, _)| json!({"id":id.to_string(),"name":name}))
                    .collect::<Vec<_>>()
            );
            persist(&path, &report)?;
            if kind == ChannelType::Category {
                category = Some(created.id);
            }
            let readback = request(http.get_channel(created.id.into()))
                .await?
                .guild()
                .ok_or("created resource was not a guild channel")?;
            if readback.base.guild_id != guild
                || readback.base.kind != kind
                || readback.base.name.as_str() != name
                || (readback.permission_overwrites.iter().count() != overwrites.len()
                    || !overwrites
                        .iter()
                        .all(|p| readback.permission_overwrites.contains(p)))
            {
                return Err("channel readback/overwrite verification failed".into());
            }
        }
        let created = request(
            http.create_guild_command(
                guild,
                &CreateCommand::new(prefix.clone())
                    .description("Oracle P3 disposable test")
                    .default_member_permissions(Permissions::MANAGE_GUILD),
            ),
        )
        .await?;
        command = Some(created.id);
        report["owned_command"] = json!(created.id.to_string());
        persist(&path, &report)?;
        let readback = request(http.get_guild_commands(guild)).await?;
        if !readback.iter().any(|c| {
            c.id == created.id
                && c.name.as_str() == prefix
                && c.default_member_permissions == Some(Permissions::MANAGE_GUILD)
        }) {
            return Err("command publication readback failed".into());
        }
        if !commands
            .iter()
            .all(|old| readback.iter().any(|c| c.id == old.id))
        {
            return Err("unrelated command disappeared during test".into());
        }
        Ok(())
    }
    .await;
    let mut cleanup_errors = Vec::new();
    if let Some(id) = command {
        let cleanup: Result<()> = async {
            let commands = request(http.get_guild_commands(guild)).await?;
            if commands
                .iter()
                .any(|c| c.id == id && c.name.as_str() != prefix)
            {
                return Err("test command changed; cleanup refused".into());
            }
            request(http.delete_guild_command(guild, id)).await?;
            if request(http.get_guild_commands(guild))
                .await?
                .iter()
                .any(|c| c.id == id)
            {
                return Err("deleted test command still present".into());
            }
            Ok(())
        }
        .await;
        if let Err(e) = cleanup {
            cleanup_errors.push(e.to_string());
        }
    }
    for (id, name, kind, parent) in channels.iter().rev() {
        let cleanup: Result<()> = async {
            let current = request(http.get_channel((*id).into()))
                .await?
                .guild()
                .ok_or("cleanup target changed type")?;
            if current.base.guild_id != guild
                || current.base.name.as_str() != name
                || current.base.kind != *kind
                || current.parent_id != *parent
                || current.base.last_message_id.is_some()
            {
                return Err(format!("resource {id} changed/populated; cleanup refused").into());
            }
            if *kind == ChannelType::Category
                && request(http.get_channels(guild))
                    .await?
                    .iter()
                    .any(|c| c.parent_id == Some(*id))
            {
                return Err("category has remaining children; cleanup refused".into());
            }
            request(http.delete_channel((*id).into(), Some("Oracle P3 owned test cleanup")))
                .await?;
            if request(http.get_channels(guild)).await?.contains_key(id) {
                return Err("deleted test channel still visible".into());
            }
            Ok(())
        }
        .await;
        if let Err(e) = cleanup {
            cleanup_errors.push(e.to_string());
        }
    }
    report["status"] = json!(if outcome.is_ok() && cleanup_errors.is_empty() {
        "passed"
    } else {
        "failed"
    });
    report["error"] = json!(outcome.as_ref().err().map(|e| e.to_string()));
    report["cleanup_errors"] = json!(cleanup_errors);
    report["checks"] = json!([
        "category/text/voice created and read back",
        "private everyone/bot overwrites read back",
        "guild command publication read back",
        "unrelated command IDs retained",
        "only owned unchanged resources cleaned and absence verified"
    ]);
    persist(&path, &report)?;
    outcome?;
    if !cleanup_errors.is_empty() {
        return Err("owned resource cleanup incomplete; inspect report IDs".into());
    }
    println!("P3 disposable-guild gate passed; report {}", path.display());
    Ok(())
}
