//! Bounded Oracle v1 framing over a pair of byte streams.
#![forbid(unsafe_code)]
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

pub const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_BODY: usize = 512 * 1024;
pub const MAX_PENDING: usize = 64;
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum RpcError {
    #[error("connection closed")]
    Closed,
    #[error("transport I/O: {0}")]
    Io(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("payload exceeds limit")]
    TooLarge,
    #[error("request capacity exhausted")]
    Overloaded,
    #[error("deadline exceeded")]
    DeadlineExceeded,
    #[error("request cancelled")]
    Cancelled,
    #[error("remote operation: {0}")]
    Remote(String),
}
#[async_trait]
pub trait RpcHandler: Send + Sync + 'static {
    async fn handle(
        &self,
        peer: RpcPeer,
        method: String,
        params: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, RpcError>;
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Frame {
    Request {
        protocol: u8,
        id: u64,
        method: String,
        params: Value,
        timeout_ms: u64,
    },
    Response {
        protocol: u8,
        id: u64,
        result: Result<Value, RpcError>,
    },
    Cancel {
        protocol: u8,
        id: u64,
    },
}
impl Frame {
    fn protocol(&self) -> u8 {
        match self {
            Self::Request { protocol, .. }
            | Self::Response { protocol, .. }
            | Self::Cancel { protocol, .. } => *protocol,
        }
    }
    fn validate(&self) -> Result<(), RpcError> {
        if self.protocol() != 1 {
            return Err(RpcError::Protocol("unsupported protocol major".into()));
        }
        let body = match self {
            Self::Request { params, method, .. } => {
                if method.len() > 256 {
                    return Err(RpcError::TooLarge);
                }
                Some(params)
            }
            Self::Response {
                result: Ok(value), ..
            } => Some(value),
            _ => None,
        };
        if let Some(body) = body
            && serde_json::to_vec(body)
                .map_err(|e| RpcError::Protocol(e.to_string()))?
                .len()
                > MAX_BODY
        {
            return Err(RpcError::TooLarge);
        }
        Ok(())
    }
}
struct Outgoing {
    bytes: Vec<u8>,
    state: Option<Arc<AtomicU8>>,
}
fn encode(frame: Frame) -> Result<Outgoing, RpcError> {
    frame.validate()?;
    let bytes = serde_json::to_vec(&frame).map_err(|e| RpcError::Protocol(e.to_string()))?;
    if bytes.len() > MAX_FRAME {
        return Err(RpcError::TooLarge);
    }
    Ok(Outgoing { bytes, state: None })
}
type Reply = oneshot::Sender<Result<Value, RpcError>>;
struct State {
    stop: CancellationToken,
    error: Mutex<Option<RpcError>>,
    pending: Mutex<HashMap<u64, Reply>>,
    data: mpsc::Sender<Outgoing>,
    control: mpsc::Sender<Outgoing>,
    slots: Arc<Semaphore>,
    next: AtomicU64,
    done: watch::Sender<bool>,
}
impl State {
    fn fail(&self, error: RpcError) {
        let mut terminal = self.error.lock().unwrap();
        if terminal.is_none() {
            *terminal = Some(error.clone());
        }
        let error = terminal.as_ref().unwrap().clone();
        self.stop.cancel();
        for (_, reply) in self.pending.lock().unwrap().drain() {
            let _ = reply.send(Err(error.clone()));
        }
    }
    fn terminal(&self) -> RpcError {
        self.error
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(RpcError::Closed)
    }
    fn control(&self, frame: Frame) {
        match encode(frame) {
            Ok(frame) => {
                if self.control.try_send(frame).is_err() {
                    self.fail(RpcError::Overloaded)
                }
            }
            Err(e) => self.fail(e),
        }
    }
}
struct Owner {
    state: Arc<State>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.state.fail(RpcError::Closed);
    }
}
#[derive(Clone)]
pub struct RpcPeer {
    state: Arc<State>,
    _owner: Option<Arc<Owner>>,
}
struct PendingGuard {
    state: Arc<State>,
    id: u64,
    sent: Arc<AtomicU8>,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self
            .state
            .pending
            .lock()
            .unwrap()
            .remove(&self.id)
            .is_some()
            && self.sent.swap(2, Ordering::SeqCst) == 1
        {
            self.state.control(Frame::Cancel {
                protocol: 1,
                id: self.id,
            });
        }
    }
}
impl RpcPeer {
    pub fn new<R, W>(read: R, write: W, handler: Arc<dyn RpcHandler>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (data_tx, data_rx) = mpsc::channel(MAX_PENDING);
        let (control_tx, control_rx) = mpsc::channel(MAX_PENDING * 3);
        let (done, _) = watch::channel(false);
        let state = Arc::new(State {
            stop: CancellationToken::new(),
            error: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            data: data_tx,
            control: control_tx,
            slots: Arc::new(Semaphore::new(MAX_PENDING)),
            next: AtomicU64::new(1),
            done,
        });
        let owner = Arc::new(Owner {
            state: state.clone(),
        });
        let weak = Arc::downgrade(&owner);
        // Supervisor owns the two drivers, joins them, and publishes terminal completion.
        // Drivers only hold state and a weak Owner, so dropping the last external peer stops idle I/O.
        tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            let reader_state = state.clone();
            tasks.spawn(async move { reader(read, reader_state, weak, handler).await });
            let writer_state = state.clone();
            tasks.spawn(async move { writer(write, writer_state, data_rx, control_rx).await });
            if let Some(result) = tasks.join_next().await {
                let error = match result {
                    Ok(Err(e)) => e,
                    Ok(Ok(())) => RpcError::Closed,
                    Err(e) => RpcError::Protocol(format!("driver panicked: {e}")),
                };
                state.fail(error);
            }
            while tasks.join_next().await.is_some() {}
            state.done.send_replace(true);
        });
        Self {
            state: owner.state.clone(),
            _owner: Some(owner),
        }
    }
    pub async fn call(
        &self,
        method: impl Into<String>,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        self.call_cancelled(method, params, timeout, CancellationToken::new())
            .await
    }
    /// Explicit cancellation alias shared by process and SDK callers.
    pub async fn call_with_cancel(
        &self,
        method: impl Into<String>,
        params: Value,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<Value, RpcError> {
        self.call_cancelled(method, params, timeout, cancellation)
            .await
    }
    pub async fn call_cancelled(
        &self,
        method: impl Into<String>,
        params: Value,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<Value, RpcError> {
        let s = &self.state;
        if s.stop.is_cancelled() {
            return Err(s.terminal());
        }
        // Reject work already cancelled or expired before it can reach the writer.
        // Cancellation after admission remains best effort across the transport.
        if cancellation.is_cancelled() {
            return Err(RpcError::Cancelled);
        }
        if timeout.is_zero() {
            return Err(RpcError::DeadlineExceeded);
        }
        let _permit = s
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| RpcError::Overloaded)?;
        let id = s
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
            .map_err(|_| RpcError::Protocol("request IDs exhausted".into()))?;
        let mut outgoing = encode(Frame::Request {
            protocol: 1,
            id,
            method: method.into(),
            params,
            timeout_ms: timeout.as_millis().min(u64::MAX as u128) as u64,
        })?;
        let sent = Arc::new(AtomicU8::new(0));
        outgoing.state = Some(sent.clone());
        let (tx, rx) = oneshot::channel();
        s.pending.lock().unwrap().insert(id, tx);
        let _guard = PendingGuard {
            state: s.clone(),
            id,
            sent,
        };
        s.data
            .try_send(outgoing)
            .map_err(|_| RpcError::Overloaded)?;
        tokio::select! { biased;
            _=s.stop.cancelled()=>Err(s.terminal()),
            _=cancellation.cancelled()=>Err(RpcError::Cancelled),
            result=tokio::time::timeout(timeout,rx)=> match result {Ok(Ok(result))=>result,Ok(Err(_))=>Err(s.terminal()),Err(_)=>Err(RpcError::DeadlineExceeded)}
        }
    }
    /// Stop transport and await both drivers and all request handlers. Call from the owner,
    /// never from an inbound handler (which is itself part of the work being joined).
    pub async fn close(&self) {
        self.state.fail(RpcError::Closed);
        self.wait_closed().await;
    }
    pub async fn wait_closed(&self) {
        let mut done = self.state.done.subscribe();
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                break;
            }
        }
    }
}
async fn read_frame<R: AsyncRead + Unpin>(read: &mut R) -> Result<Frame, RpcError> {
    let len = read
        .read_u32()
        .await
        .map_err(|e| RpcError::Io(e.to_string()))? as usize;
    if len > MAX_FRAME || len == 0 {
        return Err(RpcError::TooLarge);
    }
    let mut bytes = vec![0; len];
    read.read_exact(&mut bytes)
        .await
        .map_err(|e| RpcError::Io(e.to_string()))?;
    let frame: Frame =
        serde_json::from_slice(&bytes).map_err(|e| RpcError::Protocol(e.to_string()))?;
    frame.validate()?;
    Ok(frame)
}
async fn writer<W: AsyncWrite + Unpin>(
    mut write: W,
    state: Arc<State>,
    mut data: mpsc::Receiver<Outgoing>,
    mut control: mpsc::Receiver<Outgoing>,
) -> Result<(), RpcError> {
    loop {
        let outgoing = tokio::select! {biased; _=state.stop.cancelled()=>return Ok(()),Some(frame)=control.recv()=>frame,Some(frame)=data.recv()=>frame};
        if let Some(sent) = outgoing.state
            && sent
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            continue;
        }
        tokio::select! {biased; _=state.stop.cancelled()=>return Ok(()), result=async {
            write.write_u32(outgoing.bytes.len() as u32).await?;
            write.write_all(&outgoing.bytes).await?;
            write.flush().await
        } => result.map_err(|e|RpcError::Io(e.to_string()))? }
    }
}
async fn reader<R: AsyncRead + Unpin>(
    mut read: R,
    state: Arc<State>,
    owner: Weak<Owner>,
    handler: Arc<dyn RpcHandler>,
) -> Result<(), RpcError> {
    let mut handlers = JoinSet::new();
    let mut inbound: HashMap<u64, CancellationToken> = HashMap::new();
    let outcome=async {
        loop {
            // Keep this read future pinned while reaping handlers: read_exact is not cancel safe.
            let read_next=read_frame(&mut read); tokio::pin!(read_next);
            let frame=loop {tokio::select! {biased;
                _=state.stop.cancelled()=>return Ok(()),
                completed=handlers.join_next(),if !handlers.is_empty()=>{match completed {Some(Ok(id))=>{inbound.remove(&id);},Some(Err(e))=>return Err(RpcError::Protocol(format!("handler panicked: {e}"))),None=>{}}},
                frame=&mut read_next=>break frame?
            }};
            match frame {
                Frame::Response{id,result,..}=>{if let Some(tx)=state.pending.lock().unwrap().remove(&id){let _=tx.send(result);}},
                Frame::Cancel{id,..}=>{if let Some(token)=inbound.get(&id){token.cancel()}},
                Frame::Request{id,method,params,timeout_ms,..}=>{
                    if inbound.contains_key(&id){return Err(RpcError::Protocol("duplicate active request ID".into()))}
                    if inbound.len()>=MAX_PENDING {state.control(Frame::Response{protocol:1,id,result:Err(RpcError::Overloaded)});continue}
                    if owner.upgrade().is_none(){return Ok(())}
                    let cancel=state.stop.child_token(); inbound.insert(id,cancel.clone());
                    let handler=handler.clone(); let s=state.clone();
                    handlers.spawn(async move {
                        let result=tokio::select!{biased;
                            _=cancel.cancelled()=>Err(RpcError::Cancelled),
                            result=tokio::time::timeout(Duration::from_millis(timeout_ms),handler.handle(RpcPeer{state:s.clone(), _owner:None},method,params,cancel.clone()))=>result.unwrap_or(Err(RpcError::DeadlineExceeded))
                        };
                        cancel.cancel();
                        let outgoing=encode(Frame::Response{protocol:1,id,result}).unwrap_or_else(|_|encode(Frame::Response{protocol:1,id,result:Err(RpcError::TooLarge)}).unwrap());
                        tokio::select!{biased; _=s.stop.cancelled()=>{},result=s.control.send(outgoing)=>{if result.is_err(){s.fail(RpcError::Closed)}}}
                        id
                    });
                }
            }
        }
    }.await;
    state.fail(outcome.clone().err().unwrap_or(RpcError::Closed));
    handlers.abort_all();
    while handlers.join_next().await.is_some() {}
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct Handler;
    #[async_trait]
    impl RpcHandler for Handler {
        async fn handle(
            &self,
            peer: RpcPeer,
            method: String,
            params: Value,
            _cancel: CancellationToken,
        ) -> Result<Value, RpcError> {
            match method.as_str() {
                "nested" => peer.call("echo", params, Duration::from_secs(1)).await,
                "hang" => std::future::pending().await,
                _ => Ok(params),
            }
        }
    }
    fn pair() -> (RpcPeer, RpcPeer) {
        let (a, b) = tokio::io::duplex(4096);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        (
            RpcPeer::new(ar, aw, Arc::new(Handler)),
            RpcPeer::new(br, bw, Arc::new(Handler)),
        )
    }
    struct CountingHandler(Arc<AtomicU64>);
    #[async_trait]
    impl RpcHandler for CountingHandler {
        async fn handle(
            &self,
            _peer: RpcPeer,
            method: String,
            _params: Value,
            _cancel: CancellationToken,
        ) -> Result<Value, RpcError> {
            if method == "effect" {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            Ok(Value::Null)
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pre_admission_cancellation_and_zero_deadline_never_dispatch() {
        let (a, b) = tokio::io::duplex(4096);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let effects = Arc::new(AtomicU64::new(0));
        let caller = RpcPeer::new(ar, aw, Arc::new(Handler));
        let remote = RpcPeer::new(br, bw, Arc::new(CountingHandler(effects.clone())));
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        for _ in 0..256 {
            assert_eq!(
                caller
                    .call_cancelled(
                        "effect",
                        Value::Null,
                        Duration::from_secs(1),
                        cancelled.clone()
                    )
                    .await,
                Err(RpcError::Cancelled)
            );
            assert_eq!(
                caller.call("effect", Value::Null, Duration::ZERO).await,
                Err(RpcError::DeadlineExceeded)
            );
        }
        // Flush prior requests through the real remote reader before checking effects.
        caller
            .call("barrier", Value::Null, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        caller.close().await;
        remote.close().await;
    }
    #[tokio::test]
    async fn pre_admission_cancellation_and_zero_deadline_take_precedence_over_overload() {
        let (caller, remote) = pair();
        let _capacity = caller
            .state
            .slots
            .clone()
            .acquire_many_owned(MAX_PENDING as u32)
            .await
            .unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            caller
                .call_cancelled("effect", Value::Null, Duration::from_secs(1), cancelled)
                .await,
            Err(RpcError::Cancelled)
        );
        assert_eq!(
            caller.call("effect", Value::Null, Duration::ZERO).await,
            Err(RpcError::DeadlineExceeded)
        );
        caller.close().await;
        remote.close().await;
    }
    #[tokio::test]
    async fn nested_calls_demultiplex_while_handler_waits() {
        let (a, b) = pair();
        assert_eq!(
            a.call("nested", json!({"ok":true}), Duration::from_secs(2))
                .await
                .unwrap(),
            json!({"ok":true})
        );
        a.close().await;
        b.wait_closed().await;
    }
    #[tokio::test]
    async fn limits_apply_to_serialized_body() {
        let (a, b) = pair();
        assert_eq!(
            a.call("echo", json!("x".repeat(MAX_BODY)), Duration::from_secs(1))
                .await,
            Err(RpcError::TooLarge)
        );
        assert!(
            a.call(
                "echo",
                json!("x".repeat(MAX_BODY - 2)),
                Duration::from_secs(2)
            )
            .await
            .is_ok()
        );
        a.close().await;
        b.close().await;
    }
    #[tokio::test]
    async fn cancellation_deadline_and_close_resolve_pending() {
        let (a, b) = pair();
        assert_eq!(
            a.call("hang", Value::Null, Duration::from_millis(10)).await,
            Err(RpcError::DeadlineExceeded)
        );
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            a.call_cancelled("hang", Value::Null, Duration::from_secs(1), token)
                .await,
            Err(RpcError::Cancelled)
        );
        let other = a.clone();
        let pending = tokio::spawn(async move {
            other
                .call("hang", Value::Null, Duration::from_secs(60))
                .await
        });
        tokio::task::yield_now().await;
        a.close().await;
        assert!(pending.await.unwrap().is_err());
        b.close().await;
        assert!(a.state.pending.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn dropping_call_future_cleans_pending_and_cancels_remote() {
        let (a, b) = pair();
        let other = a.clone();
        let task = tokio::spawn(async move {
            other
                .call("hang", Value::Null, Duration::from_secs(60))
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        task.abort();
        let _ = task.await;
        assert!(a.state.pending.lock().unwrap().is_empty());
        assert!(
            a.call("echo", json!(1), Duration::from_secs(1))
                .await
                .is_ok()
        );
        a.close().await;
        b.close().await;
    }
    #[tokio::test]
    async fn outbound_admission_is_bounded() {
        let (a, b) = pair();
        let mut calls = JoinSet::new();
        for _ in 0..MAX_PENDING {
            let peer = a.clone();
            calls.spawn(async move {
                peer.call("hang", Value::Null, Duration::from_secs(60))
                    .await
            });
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while a.state.pending.lock().unwrap().len() < MAX_PENDING {
                tokio::task::yield_now().await
            }
        })
        .await
        .unwrap();
        assert_eq!(
            a.call("echo", Value::Null, Duration::from_secs(1)).await,
            Err(RpcError::Overloaded)
        );
        a.close().await;
        while calls.join_next().await.is_some() {}
        b.close().await;
    }
    #[tokio::test]
    async fn malformed_and_oversized_frames_close_transport() {
        for bytes in [
            ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec(),
            [3u32.to_be_bytes().as_slice(), b"bad"].concat(),
            {
                let body = br#"{"kind":"cancel","protocol":2,"id":1}"#;
                [(body.len() as u32).to_be_bytes().as_slice(), body].concat()
            },
        ] {
            let (a, mut b) = tokio::io::duplex(4096);
            let (ar, aw) = tokio::io::split(a);
            let peer = RpcPeer::new(ar, aw, Arc::new(Handler));
            b.write_all(&bytes).await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), peer.wait_closed())
                .await
                .unwrap();
            assert!(peer.state.stop.is_cancelled());
        }
    }
    #[tokio::test]
    async fn inbound_overload_preserves_reserved_cancellation_and_response_capacity() {
        let (a, mut raw) = tokio::io::duplex(4096);
        let (ar, aw) = tokio::io::split(a);
        let peer = RpcPeer::new(ar, aw, Arc::new(Handler));
        for id in 1..=65 {
            let frame = encode(Frame::Request {
                protocol: 1,
                id,
                method: "hang".into(),
                params: Value::Null,
                timeout_ms: 60_000,
            })
            .unwrap();
            raw.write_u32(frame.bytes.len() as u32).await.unwrap();
            raw.write_all(&frame.bytes).await.unwrap();
        }
        let frame = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut raw))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            frame,
            Frame::Response {
                id: 65,
                result: Err(RpcError::Overloaded),
                ..
            }
        ));
        let cancel = encode(Frame::Cancel { protocol: 1, id: 1 }).unwrap();
        raw.write_u32(cancel.bytes.len() as u32).await.unwrap();
        raw.write_all(&cancel.bytes).await.unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut raw))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            frame,
            Frame::Response {
                id: 1,
                result: Err(RpcError::Cancelled),
                ..
            }
        ));
        peer.close().await;
    }
    #[tokio::test]
    async fn dropping_last_owner_stops_active_handlers() {
        let (a, b) = pair();
        let caller = b.clone();
        let pending = tokio::spawn(async move {
            caller
                .call("hang", Value::Null, Duration::from_secs(60))
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(a);
        tokio::time::timeout(Duration::from_secs(1), b.wait_closed())
            .await
            .unwrap();
        assert!(pending.await.unwrap().is_err());
    }
    #[tokio::test]
    async fn dropping_last_peer_closes_idle_drivers() {
        let (a, b) = pair();
        drop(a);
        tokio::time::timeout(Duration::from_secs(1), b.wait_closed())
            .await
            .unwrap();
    }
}
