//! Real local HTTP responses drive Serenity's actual rate limiter; no Discord calls.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

struct Server {
    address: std::net::SocketAddr,
    certificate: reqwest::Certificate,
    requests: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(reset: Duration, stall: bool) -> Self {
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
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(stream).await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 1024];
                    let size = stream.read(&mut chunk).await.unwrap();
                    if size == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..size]);
                    if request.windows(4).any(|v| v == b"\r\n\r\n") {
                        break;
                    }
                }
                assert!(
                    String::from_utf8_lossy(&request)
                        .starts_with("GET /api/v10/guilds/123/channels ")
                );
                let index = count.fetch_add(1, Ordering::SeqCst);
                if stall {
                    // Keep the socket open, with no response headers or body.
                    std::future::pending::<()>().await;
                }
                let body = if index == 0 {
                    "[]"
                } else {
                    r#"[{"id":"456","guild_id":"123","name":"fresh","type":0}]"#
                };
                let reset_secs = if index == 0 { reset.as_secs_f64() } else { 0.0 };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nx-ratelimit-limit: 1\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset-after: {reset_secs}\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self {
            address,
            certificate,
            requests,
            task,
        }
    }
    fn client(&self, timeout: Duration) -> discord::Http {
        discord::HttpBuilder::without_token()
            .client(
                read_client_builder(timeout)
                    .no_proxy()
                    .resolve("discord.com", self.address)
                    .add_root_certificate(self.certificate.clone())
                    .build()
                    .unwrap(),
            )
            .build()
    }
}

#[tokio::test]
async fn rate_limit_wait_outlives_network_timeout_and_returns_fresh_channels() {
    let server = Server::new(Duration::from_secs(16), false).await;
    let http = server.client(NETWORK_TIMEOUT);
    let guild = discord::GuildId::new(123);
    assert!(read(http.get_channels(guild)).await.unwrap().is_empty());
    // Reproduce the old production deadline: it expires in Serenity's bucket
    // sleep, before an HTTP request even exists.
    assert!(
        tokio::time::timeout(NETWORK_TIMEOUT, http.get_channels(guild))
            .await
            .is_err()
    );
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    // Start with a full reset interval again, so the production wrapper itself
    // must survive longer than its old 15s budget (not merely the remainder).
    let server = Server::new(Duration::from_secs(16), false).await;
    let http = server.client(NETWORK_TIMEOUT);
    assert!(read(http.get_channels(guild)).await.unwrap().is_empty());
    let fresh = read(http.get_channels(guild)).await.unwrap();
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh.iter().next().unwrap().base.name.as_str(), "fresh");
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn stalled_network_is_bounded_independently_of_queue_budget() {
    let server = Server::new(Duration::ZERO, true).await;
    let http = server.client(Duration::from_millis(100));
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        read(http.get_channels(discord::GuildId::new(123))),
    )
    .await
    .expect("network timeout must finish long before the 90s queue budget");
    assert!(matches!(result, Err(e) if e.code == ErrorCode::Io));
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn queue_budget_expiry_cancels_the_read_without_later_network_work() {
    let server = Server::new(Duration::from_millis(500), false).await;
    let http = server.client(NETWORK_TIMEOUT);
    let guild = discord::GuildId::new(123);
    read(http.get_channels(guild)).await.unwrap();
    let result = read_with_budget(http.get_channels(guild), Duration::from_millis(50)).await;
    assert!(matches!(result, Err(e) if e.code == ErrorCode::Io));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    // Dropping the timed-out future leaves the client usable for a later fresh read.
    assert_eq!(read(http.get_channels(guild)).await.unwrap().len(), 1);
}

#[tokio::test]
async fn caller_cancellation_drops_a_queued_read() {
    let server = Server::new(Duration::from_millis(500), false).await;
    let http = server.client(NETWORK_TIMEOUT);
    let guild = discord::GuildId::new(123);
    read(http.get_channels(guild)).await.unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let (_, result) = tokio::join!(
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        },
        async {
            tokio::select! {
                value = read(http.get_channels(guild)) => Some(value),
                _ = cancel.cancelled() => None,
            }
        }
    );
    assert!(result.is_none());
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}
