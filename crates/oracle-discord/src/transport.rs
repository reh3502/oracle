//! Host-only mutation transport. Every socket write shares the executor's revoke lock.
//! A single bounded mutation lane honors Discord delays across framework writes;
//! Serenity's read client is separate and never owns mutation retries.
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request,
    header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderValue},
};
use hyper_util::rt::TokioIo;
use oracle_core::{Error, ErrorCode, ErrorDetail, Result};
use oracle_operations::executor::{SendGuard, now};
use serde_json::Value;
use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    sync::{Mutex, Semaphore},
    time::Instant,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{self, pki_types::ServerName},
};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(15);

const MAX_BODY: usize = 1024 * 1024;
const MAX_DELAY: Duration = Duration::from_secs(86_400);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

fn error(code: ErrorCode) -> Error {
    Error::new(code)
}

fn valid_write_path(method: &Method, path: &str) -> bool {
    if path.len() > 512 {
        return false;
    }
    // Attendance seeds exactly one Unicode reaction on a known bot message.
    // Keep percent encoding and @ restricted to this exact idempotent route.
    if path.ends_with("/reactions/%E2%9C%85/@me") {
        let parts: Vec<_> = path.split('/').collect();
        return method == Method::PUT
            && parts.len() == 10
            && parts[0].is_empty()
            && parts[1..4] == ["api", "v10", "channels"]
            && parts[5] == "messages"
            && [parts[4], parts[6]].iter().all(|id| {
                !id.is_empty()
                    && id.bytes().all(|b| b.is_ascii_digit())
                    && id
                        .parse::<u64>()
                        .is_ok_and(|parsed| parsed > 0 && parsed.to_string() == *id)
            });
    }
    if path.starts_with("/api/v10/webhooks/") {
        let parts: Vec<_> = path.split('/').collect();
        return method == Method::PATCH
            && parts.len() == 8
            && parts[6] == "messages"
            && parts[7] == "@original"
            && parts[4]
                .parse::<u64>()
                .ok()
                .and_then(|id| crate::member_transport::reply_path(id, parts[5]).ok())
                .as_deref()
                == Some(path);
    }
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) && path.starts_with("/api/v10/")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_-".contains(&b))
}

#[async_trait]
pub trait FreshCheck: Send + Sync {
    /// Re-fetch current authority and preconditions after rate waits, before connecting.
    async fn check(&self) -> Result<()>;
}

#[derive(Debug)]
pub struct WriteResponse {
    pub status: u16,
    pub body: Value,
}

/// No Debug implementation: the credential must never enter diagnostics.
pub struct DiscordWriteClient {
    authorization: HeaderValue,
    tls: TlsConnector,
    lane: Mutex<()>,
    next_ready: Mutex<Instant>,
    capacity: Semaphore,
    #[cfg(test)]
    endpoint: Option<std::net::SocketAddr>,
}
impl DiscordWriteClient {
    pub fn new(token: String) -> Result<Self> {
        if token.trim().is_empty() {
            return Err(error(ErrorCode::InvalidInput));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bot {token}"))
            .map_err(|_| error(ErrorCode::InvalidInput))?;
        authorization.set_sensitive(true);
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .map_err(|_| error(ErrorCode::Io))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            authorization,
            tls: TlsConnector::from(Arc::new(config)),
            lane: Mutex::new(()),
            next_ready: Mutex::new(Instant::now()),
            capacity: Semaphore::new(64),
            #[cfg(test)]
            endpoint: None,
        })
    }

    pub async fn execute(
        &self,
        method: Method,
        path: &str,
        body: &Value,
        guard: &SendGuard,
        fresh: &dyn FreshCheck,
    ) -> Result<WriteResponse> {
        if !valid_write_path(&method, path) {
            return Err(error(ErrorCode::InvalidInput));
        }
        let payload = serde_json::to_vec(body).map_err(|_| error(ErrorCode::InvalidInput))?;
        if payload.len() > MAX_BODY {
            return Err(error(ErrorCode::QuotaExceeded));
        }
        let _capacity = self
            .capacity
            .try_acquire()
            .map_err(|_| error(ErrorCode::QuotaExceeded))?;
        let cancellation = guard.cancellation();
        let deadline = Duration::from_secs(guard.expires_at().saturating_sub(now()));
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(error(ErrorCode::Cancelled)),
            _ = tokio::time::sleep(deadline) => Err(error(ErrorCode::Cancelled)),
            result = self.execute_inner(method, path, payload, guard, fresh) => result,
        }
    }

    async fn execute_inner(
        &self,
        method: Method,
        path: &str,
        payload: Vec<u8>,
        guard: &SendGuard,
        fresh: &dyn FreshCheck,
    ) -> Result<WriteResponse> {
        let _lane = self.lane.lock().await;
        // Initial request plus at most three retries, exclusively for definite 429 replies.
        for attempt in 0..=3 {
            let ready = *self.next_ready.lock().await;
            tokio::time::sleep_until(ready).await;
            guard.dispatch(|| Ok(()))?;
            let (response, rate_delay) = self
                .send_once(
                    method.clone(),
                    path,
                    payload.clone(),
                    guard,
                    fresh,
                    NETWORK_TIMEOUT,
                )
                .await?;
            if let Some(delay) = rate_delay {
                let mut ready = self.next_ready.lock().await;
                *ready = (*ready).max(Instant::now() + delay);
            }
            if response.status != 429 {
                return Ok(response);
            }
            // Share global and route advice even when this request exhausts its retry budget.
            let retry_delay = response
                .body
                .get("retry_after")
                .and_then(Value::as_f64)
                .and_then(delay)
                .or(rate_delay);
            if let Some(delay) = retry_delay {
                let mut ready = self.next_ready.lock().await;
                *ready = (*ready).max(Instant::now() + delay);
            }
            if attempt == 3 || retry_delay.is_none_or(|delay| delay > MAX_RETRY_DELAY) {
                return Ok(response);
            }
        }
        unreachable!("bounded retry loop always returns")
    }

    async fn send_once(
        &self,
        method: Method,
        path: &str,
        payload: Vec<u8>,
        guard: &SendGuard,
        fresh: &dyn FreshCheck,
        network_timeout: Duration,
    ) -> Result<(WriteResponse, Option<Duration>)> {
        // Authority reads can wait for a minute-long Discord bucket reset. Do
        // not open the mutation connection until those reads finish: an idle
        // connection may otherwise be closed before its first HTTP request.
        fresh.check().await?;
        guard.dispatch(|| Ok(()))?;
        let deadline = Instant::now() + network_timeout;
        tokio::time::timeout_at(
            deadline,
            self.connect_and_request(method, path, payload, guard, deadline),
        )
        .await
        .map_err(|_| Error::with_detail(ErrorCode::UnknownOutcome, ErrorDetail::NetworkTimeout))?
    }

    async fn connect_and_request(
        &self,
        method: Method,
        path: &str,
        payload: Vec<u8>,
        guard: &SendGuard,
        deadline: Instant,
    ) -> Result<(WriteResponse, Option<Duration>)> {
        #[cfg(test)]
        if let Some(endpoint) = self.endpoint {
            let stream = TcpStream::connect(endpoint)
                .await
                .map_err(|_| error(ErrorCode::Io))?;
            return self
                .request(
                    GuardedIo::new(stream, guard.clone(), deadline),
                    method,
                    path,
                    payload,
                    guard,
                    deadline,
                )
                .await;
        }
        let stream = TcpStream::connect(("discord.com", 443))
            .await
            .map_err(|_| error(ErrorCode::Io))?;
        let stream = GuardedIo::new(stream, guard.clone(), deadline);
        let name = ServerName::try_from("discord.com").map_err(|_| error(ErrorCode::Io))?;
        let tls = self
            .tls
            .connect(name, stream)
            .await
            .map_err(|_| error(ErrorCode::Io))?;
        self.request(tls, method, path, payload, guard, deadline)
            .await
    }

    async fn request<T>(
        &self,
        stream: T,
        method: Method,
        path: &str,
        payload: Vec<u8>,
        guard: &SendGuard,
        deadline: Instant,
    ) -> Result<(WriteResponse, Option<Duration>)>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| error(ErrorCode::Io))?;
        let driver = Driver(tokio::spawn(async move {
            let _ = connection.await;
        }));
        guard.dispatch(|| Ok(()))?;
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, "discord.com")
            .header(AUTHORIZATION, self.authorization.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload)))
            .map_err(|_| error(ErrorCode::InvalidInput))?;
        guard.mark_request_started()?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| network_failure(deadline))?;
        let status = response.status().as_u16();
        let headers = response.headers();
        let exhausted = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            == Some("0");
        let retry = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok())
            .and_then(delay);
        let reset = headers
            .get("x-ratelimit-reset-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok())
            .and_then(delay);
        let rate_delay = if status == 429 {
            retry.or(reset)
        } else if exhausted {
            reset
        } else {
            None
        };
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| network_failure(deadline))?;
            if let Ok(data) = frame.into_data() {
                if bytes.len().saturating_add(data.len()) > MAX_BODY {
                    return Err(error(ErrorCode::QuotaExceeded));
                }
                bytes.extend_from_slice(&data);
            }
        }
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(|_| network_failure(deadline))?
        };
        drop(driver);
        Ok((WriteResponse { status, body }, rate_delay))
    }
}
fn delay(seconds: f64) -> Option<Duration> {
    if !seconds.is_finite() || seconds < 0.0 || seconds > MAX_DELAY.as_secs_f64() {
        return None;
    }
    Some(Duration::from_secs_f64(seconds))
}
struct Driver(tokio::task::JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// TLS must wrap this type, so encrypted records also pass through the gate.
struct GuardedIo<T> {
    inner: T,
    guard: SendGuard,
    deadline: Instant,
}
impl<T> GuardedIo<T> {
    fn new(inner: T, guard: SendGuard, deadline: Instant) -> Self {
        Self {
            inner,
            guard,
            deadline,
        }
    }
}
impl<T: AsyncRead + Unpin> AsyncRead for GuardedIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}
fn network_failure(deadline: Instant) -> Error {
    Error::with_detail(
        ErrorCode::UnknownOutcome,
        if Instant::now() >= deadline {
            ErrorDetail::NetworkTimeout
        } else {
            ErrorDetail::HttpFailure
        },
    )
}

// Check under the same dispatch lock as the socket write, including TLS records.
// Dropping the request aborts its driver, but this also fences a driver that is
// scheduled late before it gets to observe that abort.
fn within_deadline<T>(
    deadline: Instant,
    write: impl FnOnce() -> Poll<io::Result<T>>,
) -> Result<Poll<io::Result<T>>> {
    if Instant::now() >= deadline {
        return Err(network_failure(deadline));
    }
    Ok(write())
}

fn fenced<T>(result: Result<Poll<io::Result<T>>>) -> Poll<io::Result<T>> {
    result.unwrap_or_else(|_| {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "operation fenced",
        )))
    })
}
impl<T: AsyncWrite + Unpin> AsyncWrite for GuardedIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        fenced(this.guard.dispatch(|| {
            within_deadline(this.deadline, || {
                Pin::new(&mut this.inner).poll_write(cx, buf)
            })
        }))
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        fenced(this.guard.dispatch(|| {
            within_deadline(this.deadline, || {
                Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
            })
        }))
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        fenced(this.guard.dispatch(|| {
            within_deadline(this.deadline, || Pin::new(&mut this.inner).poll_flush(cx))
        }))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        fenced(this.guard.dispatch(|| {
            within_deadline(this.deadline, || {
                Pin::new(&mut this.inner).poll_shutdown(cx)
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use tokio_util::sync::CancellationToken;
    struct Fresh(bool);
    #[async_trait]
    impl FreshCheck for Fresh {
        async fn check(&self) -> Result<()> {
            if self.0 {
                Ok(())
            } else {
                Err(error(ErrorCode::Conflict))
            }
        }
    }
    fn guard() -> SendGuard {
        SendGuard::new(CancellationToken::new(), now() + 10)
    }
    fn client(address: std::net::SocketAddr) -> DiscordWriteClient {
        let mut client = DiscordWriteClient::new("test-secret".into()).unwrap();
        client.endpoint = Some(address);
        client
    }
    async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut byte = [0u8; 1];
        while !bytes.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await.unwrap() == 0 {
                return bytes;
            }
            bytes.push(byte[0]);
        }
        let headers = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
        let length = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-length:")
                    .and_then(|s| s.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        bytes.extend(body);
        bytes
    }
    async fn reply(stream: &mut TcpStream, status: u16, body: &str) {
        stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    }
    #[test]
    fn attendance_reaction_route_is_exact_and_put_only() {
        let path = "/api/v10/channels/123/messages/456/reactions/%E2%9C%85/@me";
        assert!(valid_write_path(&Method::PUT, path));
        for method in [Method::POST, Method::PATCH, Method::DELETE, Method::GET] {
            assert!(!valid_write_path(&method, path));
        }
        for path in [
            "/api/v10/channels/0/messages/456/reactions/%E2%9C%85/@me",
            "/api/v10/channels/0123/messages/456/reactions/%E2%9C%85/@me",
            "/api/v10/channels/123/messages/../reactions/%E2%9C%85/@me",
            "/api/v10/channels/123/messages/456/reactions/%E2%9C%85/789",
            "/api/v10/channels/123/messages/456/reactions/%E2%9C%85/@me?x=1",
            "/api/v10/channels/123/messages/456/reactions/%2F/@me",
            "x/api/v10/channels/123/messages/456/reactions/%E2%9C%85/@me",
        ] {
            assert!(!valid_write_path(&Method::PUT, path), "{path}");
        }
    }
    #[test]
    fn interaction_reply_endpoint_is_exact_and_patch_only() {
        let path = "/api/v10/webhooks/123/abc.DEF_-123/messages/@original";
        assert!(valid_write_path(&Method::PATCH, path));
        assert!(!valid_write_path(&Method::POST, path));
        for path in [
            "/api/v10/webhooks/123/token/messages/456",
            "/api/v10/webhooks/123/token/messages/@original?wait=true",
            "/api/v10/webhooks/123/../messages/@original",
            "/api/v10/webhooks/0123/token/messages/@original",
            "/api/v10/channels/123/messages/@original",
        ] {
            assert!(!valid_write_path(&Method::PATCH, path));
        }
    }
    #[tokio::test]
    async fn preflight_failure_proves_no_request_started() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let gate = guard();
        assert!(
            client
                .execute(
                    Method::POST,
                    "/api/v10/channels/123/messages",
                    &serde_json::json!({}),
                    &gate,
                    &Fresh(false)
                )
                .await
                .is_err()
        );
        assert!(!gate.request_started());
        assert!(!gate.clone().request_started());
    }
    struct ReplyRegistry(std::sync::atomic::AtomicBool);
    impl oracle_operations::executor::DispatchFence for ReplyRegistry {
        fn dispatch(&self, send: &mut dyn FnMut() -> Result<()>) -> Result<()> {
            if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(error(ErrorCode::Cancelled));
            }
            send()
        }
    }
    struct CountFresh(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl FreshCheck for CountFresh {
        async fn check(&self) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }
    #[tokio::test]
    async fn interaction_reply_retry_refreshes_and_preserves_safe_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = String::from_utf8(read_request(&mut stream).await).unwrap();
                assert!(request.starts_with(
                    "PATCH /api/v10/webhooks/123/abc.def/messages/@original HTTP/1.1"
                ));
                let body: Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                assert_eq!(
                    body,
                    json!({"content":"reply","allowed_mentions":{"parse":[]},"flags":68})
                );
                if attempt == 0 {
                    reply(&mut stream, 429, r#"{"retry_after":0.01}"#).await;
                } else {
                    reply(&mut stream, 200, "{}").await;
                }
            }
        });
        let fresh = CountFresh(std::sync::atomic::AtomicUsize::new(0));
        let gate = guard();
        let result = client
            .execute(
                Method::PATCH,
                "/api/v10/webhooks/123/abc.def/messages/@original",
                &json!({"content":"reply","allowed_mentions":{"parse":[]},"flags":68}),
                &gate,
                &fresh,
            )
            .await
            .unwrap();
        assert_eq!(result.status, 200);
        assert!(gate.clone().request_started());
        assert_eq!(fresh.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        server.await.unwrap();
    }
    #[tokio::test]
    async fn interaction_reply_retry_checks_registry_revocation_before_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let registry = Arc::new(ReplyRegistry(std::sync::atomic::AtomicBool::new(false)));
        let guard = SendGuard::with_fence(CancellationToken::new(), now() + 10, registry.clone());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(!read_request(&mut stream).await.is_empty());
            reply(&mut stream, 429, r#"{"retry_after":0.05}"#).await;
            registry.0.store(true, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });
        let result = client
            .execute(
                Method::PATCH,
                "/api/v10/webhooks/123/abc.def/messages/@original",
                &json!({"content":"reply"}),
                &guard,
                &Fresh(true),
            )
            .await
            .unwrap_err();
        assert_eq!(result.code, ErrorCode::Cancelled);
        server.await.unwrap();
    }
    struct SlowFresh;
    #[async_trait]
    impl FreshCheck for SlowFresh {
        async fn check(&self) -> Result<()> {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(())
        }
    }
    #[tokio::test]
    async fn delayed_authority_does_not_leave_an_idle_write_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request =
                tokio::time::timeout(Duration::from_millis(50), read_request(&mut stream)).await;
            if request.is_err() {
                // Model Discord closing a newly opened connection that has sat idle.
                return false;
            }
            reply(&mut stream, 200, "{}").await;
            true
        });
        let result = client
            .execute(
                Method::POST,
                "/api/v10/guilds/100/channels",
                &json!({}),
                &guard(),
                &SlowFresh,
            )
            .await;
        assert!(
            server.await.unwrap(),
            "write connection idled during authority refresh"
        );
        assert_eq!(result.unwrap().status, 200);
    }
    #[tokio::test]
    async fn freshness_wait_is_outside_network_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            reply(&mut stream, 200, "{}").await;
        });
        let response = client
            .send_once(
                Method::POST,
                "/api/v10/guilds/100/channels",
                vec![],
                &guard(),
                &SlowFresh,
                Duration::from_millis(50),
            )
            .await
            .unwrap();
        assert_eq!(response.0.status, 200);
        server.await.unwrap();
    }
    #[tokio::test]
    async fn stalled_response_body_has_unknown_network_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(!read_request(&mut stream).await.is_empty());
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{")
                .await
                .unwrap();
            let mut remaining = Vec::new();
            // The network deadline drops and aborts the driver, closing this socket.
            tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut remaining))
                .await
                .unwrap()
                .unwrap();
            assert!(remaining.is_empty());
        });
        let error = client
            .send_once(
                Method::POST,
                "/api/v10/guilds/100/channels",
                vec![],
                &guard(),
                &Fresh(true),
                Duration::from_millis(50),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::UnknownOutcome);
        assert_eq!(error.diagnostic().detail, Some(ErrorDetail::NetworkTimeout));
        server.await.unwrap();
    }
    #[tokio::test]
    async fn lost_response_is_diagnostic_and_never_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(!read_request(&mut stream).await.is_empty());
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let gate = guard();
        let error = client
            .execute(
                Method::POST,
                "/api/v10/guilds/100/channels",
                &json!({}),
                &gate,
                &Fresh(true),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::UnknownOutcome);
        assert!(gate.request_started());
        assert_eq!(error.diagnostic().detail, Some(ErrorDetail::HttpFailure));
        server.await.unwrap();
    }
    #[tokio::test]
    async fn network_deadline_fences_late_socket_writes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let mut stream =
            GuardedIo::new(stream, guard(), Instant::now() + Duration::from_millis(30));
        stream.write_all(b"first").await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(stream.write_all(b"late").await.is_err());
        assert!(
            stream
                .write_vectored(&[io::IoSlice::new(b"late")])
                .await
                .is_err()
        );
        assert!(stream.flush().await.is_err());
        drop(stream);
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"first");
    }
    #[tokio::test]
    async fn cancellation_during_freshness_wait_opens_no_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let guard = guard();
        let cancel = guard.clone();
        let task = tokio::spawn(async move {
            client
                .execute(
                    Method::POST,
                    "/api/v10/guilds/100/channels",
                    &json!({}),
                    &guard,
                    &SlowFresh,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.revoke();
        assert_eq!(task.await.unwrap().unwrap_err().code, ErrorCode::Cancelled);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn queued_revocation_sends_no_http_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        *client.next_ready.lock().await = Instant::now() + Duration::from_millis(80);
        let guard = guard();
        let revoke = guard.clone();
        let task = tokio::spawn(async move {
            client
                .execute(
                    Method::POST,
                    "/api/v10/guilds/100/channels",
                    &json!({}),
                    &guard,
                    &Fresh(true),
                )
                .await
        });
        tokio::task::yield_now().await;
        revoke.revoke();
        assert_eq!(task.await.unwrap().unwrap_err().code, ErrorCode::Cancelled);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn stale_authority_opens_no_write_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        assert_eq!(
            client
                .execute(
                    Method::POST,
                    "/api/v10/guilds/100/channels",
                    &json!({}),
                    &guard(),
                    &Fresh(false)
                )
                .await
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn definite_429_retry_is_fenced_before_second_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let guard = guard();
        let revoke = guard.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(!read_request(&mut stream).await.is_empty());
            reply(&mut stream, 429, r#"{"retry_after":0.08,"global":true}"#).await;
            revoke.revoke();
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(200), listener.accept())
                    .await
                    .is_err()
            );
        });
        assert_eq!(
            client
                .execute(
                    Method::POST,
                    "/api/v10/guilds/100/channels",
                    &json!({}),
                    &guard,
                    &Fresh(true)
                )
                .await
                .unwrap_err()
                .code,
            ErrorCode::Cancelled
        );
        server.await.unwrap();
    }
    #[tokio::test]
    async fn partial_tcp_write_cannot_continue_after_fence() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let guard = guard();
        let mut io = GuardedIo::new(stream, guard.clone(), Instant::now() + NETWORK_TIMEOUT);
        io.write_all(b"first").await.unwrap();
        let mut first = [0; 5];
        peer.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"first");
        guard.revoke();
        assert!(io.write_all(b"second").await.is_err());
        assert!(
            io.write_vectored(&[io::IoSlice::new(b"third")])
                .await
                .is_err()
        );
        assert!(io.flush().await.is_err());
        drop(io);
        let mut rest = Vec::new();
        peer.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }
    #[tokio::test]
    async fn retry_refreshes_authority_and_5xx_is_not_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for status in [429, 503] {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                reply(
                    &mut stream,
                    status,
                    if status == 429 {
                        r#"{"retry_after":0.001}"#
                    } else {
                        r#"{"error":"unavailable"}"#
                    },
                )
                .await;
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        struct Count(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl FreshCheck for Count {
            async fn check(&self) -> Result<()> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }
        let fresh = Count(std::sync::atomic::AtomicUsize::new(0));
        assert_eq!(
            client
                .execute(
                    Method::POST,
                    "/api/v10/guilds/100/channels",
                    &json!({}),
                    &guard(),
                    &fresh
                )
                .await
                .unwrap()
                .status,
            503
        );
        assert_eq!(fresh.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        server.await.unwrap();
    }
    #[tokio::test]
    async fn arbitrary_destinations_and_expired_requests_are_rejected() {
        let client = client("127.0.0.1:1".parse().unwrap());
        for path in [
            "https://example.com/",
            "//example.com/",
            "/api/v10/../secrets",
            "/api/v10/channels?token=x",
        ] {
            assert_eq!(
                client
                    .execute(Method::POST, path, &json!({}), &guard(), &Fresh(true))
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidInput
            );
        }
        assert_eq!(
            client
                .execute(
                    Method::POST,
                    "/api/v10/channels/1",
                    &json!({}),
                    &SendGuard::new(CancellationToken::new(), now()),
                    &Fresh(true)
                )
                .await
                .unwrap_err()
                .code,
            ErrorCode::Cancelled
        );
    }
    #[tokio::test]
    async fn retry_budget_and_response_size_are_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                reply(&mut stream, 429, r#"{"retry_after":0.001}"#).await;
            }
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            let body = "x".repeat(MAX_BODY + 1);
            let headers = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            stream.write_all(headers.as_bytes()).await.unwrap();
            let _ = stream.write_all(body.as_bytes()).await;
        });
        assert_eq!(
            client
                .execute(
                    Method::POST,
                    "/api/v10/channels/1",
                    &json!({}),
                    &guard(),
                    &Fresh(true)
                )
                .await
                .unwrap()
                .status,
            429
        );
        assert_eq!(
            client
                .execute(
                    Method::POST,
                    "/api/v10/channels/1",
                    &json!({}),
                    &guard(),
                    &Fresh(true)
                )
                .await
                .unwrap_err()
                .code,
            ErrorCode::QuotaExceeded
        );
        server.await.unwrap();
    }
    #[tokio::test]
    async fn external_module_fence_guards_actual_socket_writes() {
        use oracle_operations::executor::DispatchFence;
        struct Fence(Arc<std::sync::Mutex<bool>>);
        impl DispatchFence for Fence {
            fn dispatch(&self, send: &mut dyn FnMut() -> Result<()>) -> Result<()> {
                let active = self.0.lock().unwrap();
                if !*active {
                    return Err(error(ErrorCode::ModuleUnavailable));
                }
                send()
            }
        }
        let active = Arc::new(std::sync::Mutex::new(true));
        let guard = SendGuard::with_fence(
            CancellationToken::new(),
            now() + 10,
            Arc::new(Fence(active.clone())),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let mut io = GuardedIo::new(stream, guard, Instant::now() + NETWORK_TIMEOUT);
        io.write_all(b"first").await.unwrap();
        let mut first = [0; 5];
        peer.read_exact(&mut first).await.unwrap();
        *active.lock().unwrap() = false;
        assert!(io.write_all(b"second").await.is_err());
        drop(io);
        let mut remainder = Vec::new();
        peer.read_to_end(&mut remainder).await.unwrap();
        assert_eq!(&first, b"first");
        assert!(remainder.is_empty());
    }
}
