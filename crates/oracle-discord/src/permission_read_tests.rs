//! Exercise Serenity HTTP decoding and fresh scoped authority without an inventory read.
use super::*;
use oracle_modules::ConfigurationPolicy;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct Fixture {
    operations: DiscordOperations,
    paths: Arc<Mutex<Vec<String>>>,
    deny: Arc<AtomicBool>,
    wrong_guild: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    _folder: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let folder = tempfile::tempdir().unwrap();
        let storage = Arc::new(
            oracle_storage::Storage::open(oracle_storage::DatabaseConfig::Sqlite {
                path: folder.path().join("authority.sqlite"),
            })
            .await
            .unwrap(),
        );
        let guild = GuildId::new("123").unwrap();
        storage
            .initialize_guilds(std::slice::from_ref(&guild))
            .await
            .unwrap();
        let core = Arc::new(CoreService::new(
            storage,
            vec![oracle_core::GuildPolicy {
                guild,
                operators: vec!["789".parse().unwrap()],
            }],
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let generated = rcgen::generate_simple_self_signed(vec!["discord.com".into()]).unwrap();
        let certificate = reqwest::Certificate::from_der(generated.cert.der()).unwrap();
        let tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![generated.cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der())
                    .into(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let requests = paths.clone();
        let deny = Arc::new(AtomicBool::new(false));
        let denied = deny.clone();
        let wrong_guild = Arc::new(AtomicBool::new(false));
        let wrong = wrong_guild.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(stream).await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 1024];
                    let count = stream.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|v| v == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap().to_owned();
                requests.lock().unwrap().push(path.clone());
                let body = match path.as_str() {
                    "/api/v10/channels/222" => json!({"id":"222","guild_id":if wrong.load(Ordering::SeqCst) {"999"} else {"123"},"type":0,"name":"runs","position":0,"permission_overwrites":[{"id":"123","type":0,"allow":"0","deny":if denied.load(Ordering::SeqCst) {"1024"} else {"0"}}]}),
                    "/api/v10/guilds/123" => json!({"id":"123","name":"Fixture","owner_id":"999","verification_level":0,"default_message_notifications":0,"explicit_content_filter":0,"roles":[{"id":"123","name":"@everyone","permissions":"85056","position":0,"color":0,"colors":{"primary_color":0,"secondary_color":null,"tertiary_color":null},"hoist":false,"managed":false,"mentionable":false}],"emojis":[],"features":[],"mfa_level":0,"system_channel_flags":0,"premium_tier":0,"preferred_locale":"en-US","nsfw_level":0,"stickers":[],"premium_progress_bar_enabled":false}),
                    "/api/v10/guilds/123/members/456" | "/api/v10/guilds/123/members/789" => {
                        let mut member = discord::Member::default();
                        member.guild_id = discord::GuildId::new(123);
                        member.user.id = discord::UserId::new(if path.ends_with("789") {789} else {456});
                        serde_json::to_value(member).unwrap()
                    },
                    // The old code fails here: an exhausted channel inventory must
                    // not prevent fresh permission reads for a known destination.
                    _ => {
                        stream.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await.unwrap();
                        continue;
                    }
                }.to_string();
                if path == "/api/v10/guilds/123" {
                    serde_json::from_str::<discord::PartialGuild>(&body).unwrap();
                }
                if path == "/api/v10/channels/222" {
                    serde_json::from_str::<discord::Channel>(&body).unwrap();
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let http = discord::HttpBuilder::without_token()
            .client(
                read_client_builder(NETWORK_TIMEOUT)
                    .no_proxy()
                    .resolve("discord.com", address)
                    .add_root_certificate(certificate)
                    .build()
                    .unwrap(),
            )
            .build();
        Self {
            operations: DiscordOperations {
                core,
                http: Arc::new(http),
                writer: Arc::new(DiscordWriteClient::new("fixture-token".into()).unwrap()),
                bot: OnceCell::new_with(Some(discord::UserId::new(456))),
                application: OnceCell::new(),
            },
            paths,
            deny,
            wrong_guild,
            task,
            _folder: folder,
        }
    }
}

#[tokio::test]
async fn known_destination_checks_fresh_permissions_without_channel_inventory() {
    let fixture = Fixture::new().await;
    let guild = "123".parse().unwrap();
    let context = PolicyContext::LocalOperator;
    let first = fixture
        .operations
        .channel_mutation_authority(&context, &guild, "222")
        .await
        .unwrap();
    assert_eq!(first.channels.len(), 1);
    assert!(!first.complete);
    fixture.deny.store(true, Ordering::SeqCst);
    let denied = fixture
        .operations
        .channel_mutation_authority(&context, &guild, "222")
        .await
        .unwrap();
    assert!(
        denied.channels.is_empty(),
        "permission revocation must not use cached overwrites"
    );
    fixture.wrong_guild.store(true, Ordering::SeqCst);
    assert_eq!(
        fixture
            .operations
            .channel_mutation_authority(&context, &guild, "222")
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenScope
    );
    let paths = fixture.paths.lock().unwrap();
    assert_eq!(
        paths
            .iter()
            .filter(|p| p.as_str() == "/api/v10/channels/222")
            .count(),
        3
    );
    assert!(!paths.iter().any(|p| p.ends_with("/channels")));
}

#[tokio::test]
async fn empty_configuration_checks_guild_authority_without_channel_inventory() {
    let fixture = Fixture::new().await;
    fixture
        .operations
        .validate(
            &PolicyContext::LocalOperator,
            &"123".parse().unwrap(),
            &"sample.runs".parse().unwrap(),
            &json!({}),
        )
        .await
        .unwrap();
    assert!(
        !fixture
            .paths
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.contains("channels"))
    );
}

#[tokio::test]
async fn guild_check_rejects_stale_manage_guild_claim() {
    let fixture = Fixture::new().await;
    let context = PolicyContext::Discord {
        guild: "123".parse().unwrap(),
        user: "789".parse().unwrap(),
        manage_guild: true,
    };
    assert_eq!(
        fixture
            .operations
            .guild_mutation_authority(&context, &"123".parse().unwrap())
            .await
            .unwrap_err()
            .code,
        ErrorCode::ForbiddenPermission
    );
}
