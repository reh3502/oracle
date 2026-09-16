//! Read-only Discord member-access check; never starts a gateway or sends messages.
use oracle_core::{CoreService, GuildId, GuildPolicy, UserId, member_read::MemberContext};
use oracle_discord::operations::DiscordOperations;
use std::{collections::BTreeSet, sync::Arc, time::Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err(
            "Usage: check-member-access GUILD USER CHANNEL (DISCORD_TOKEN in environment)".into(),
        );
    }
    let guild = GuildId::new(&args[1])?;
    let user = UserId::new(&args[2])?;
    UserId::new(&args[3])?;
    let token = std::env::var("DISCORD_TOKEN")
        .map_err(|_| "Missing DISCORD_TOKEN")?
        .parse()
        .map_err(|_| "Invalid token format")?;
    let folder = tempfile::tempdir()?;
    let storage = Arc::new(
        oracle_storage::Storage::open(oracle_storage::DatabaseConfig::Sqlite {
            path: folder.path().join("check.sqlite"),
        })
        .await?,
    );
    storage
        .initialize_guilds(std::slice::from_ref(&guild))
        .await?;
    let core = Arc::new(CoreService::new(
        storage.clone(),
        vec![GuildPolicy {
            guild: guild.clone(),
            operators: vec![user.clone()],
        }],
    ));
    let operations = DiscordOperations::new(token, core)?;
    let result = operations
        .refresh_member(&MemberContext {
            guild,
            user,
            channel: args[3].clone(),
            roles: BTreeSet::new(),
            observed_at: Instant::now(),
        })
        .await;
    storage.close().await?;
    match result {
        Ok(_) => println!("PASS: fresh Discord member identity and channel permissions"),
        Err(error) => {
            eprintln!("FAIL: member-check {:?}", error.diagnostic());
            std::process::exit(1);
        }
    }
    Ok(())
}
