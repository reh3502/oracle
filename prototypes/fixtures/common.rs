//! Deliberately small, separately compiled native test modules. No bot features.

use std::{process::Stdio, sync::Arc, time::Duration};

use async_trait::async_trait;
use oracle_process_prototype::rpc::{RpcError, RpcHandler, RpcPeer};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::TcpStream, process::Command, sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

struct Fixture {
    identity: &'static str,
    build: &'static str,
    supports_descendants: bool,
    bad_protocol: bool,
    no_hello: bool,
    bad_init: bool,
    initialization: Mutex<Option<(Value, Value)>>,
    remembered: Mutex<Option<Value>>,
    stop: CancellationToken,
    sockets: Mutex<Vec<JoinHandle<()>>>,
}

impl Fixture {
    async fn check_envelope(&self, params: &Value) -> Result<(), RpcError> {
        let initialization = self.initialization.lock().await;
        let Some((session, generation)) = initialization.as_ref() else {
            return Err(RpcError::Remote("fixture is not initialized".into()));
        };
        if params.get("session") != Some(session)
            || params.get("generation") != Some(generation)
            || params.get("lease").is_none()
            || params.get("scope").is_none()
        {
            return Err(RpcError::Remote("invalid invocation envelope".into()));
        }
        if self.stop.is_cancelled() {
            return Err(RpcError::Remote("fixture is shutting down".into()));
        }
        Ok(())
    }

    async fn stop_sockets(&self) {
        self.stop.cancel();
        for task in self.sockets.lock().await.drain(..) {
            let _ = task.await;
        }
    }
}

#[async_trait]
impl RpcHandler for Fixture {
    async fn handle(
        &self,
        peer: RpcPeer,
        method: String,
        params: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method.as_str() {
            "hello" => {
                if self.no_hello {
                    std::future::pending::<()>().await;
                }
                return Ok(json!({
                    "identity": self.identity,
                    "protocol": if self.bad_protocol { 999 } else { 1 },
                    "build": self.build,
                    "pid": std::process::id()
                }));
            }
            "initialize" => {
                if self.bad_init {
                    return Ok(json!({"initialized": false}));
                }
                let session = params
                    .get("session")
                    .filter(|v| v.is_string())
                    .ok_or_else(|| RpcError::Remote("missing session".into()))?;
                let generation = params
                    .get("generation")
                    .filter(|v| v.is_u64())
                    .ok_or_else(|| RpcError::Remote("missing generation".into()))?;
                let mut initialization = self.initialization.lock().await;
                if initialization.is_some() {
                    return Err(RpcError::Remote("already initialized".into()));
                }
                *initialization = Some((session.clone(), generation.clone()));
                return Ok(json!({"initialized": true}));
            }
            "shutdown" => {
                self.stop_sockets().await;
                return Ok(json!({"shutdown": true}));
            }
            _ => self.check_envelope(&params).await?,
        }
        let input = params.get("input").cloned().unwrap_or(Value::Null);
        match method.as_str() {
            "echo" => Ok(input),
            "remember" => {
                *self.remembered.lock().await = Some(params);
                Ok(json!({"remembered": true}))
            }
            "replay" => {
                let saved = self
                    .remembered
                    .lock()
                    .await
                    .clone()
                    .ok_or_else(|| RpcError::Remote("no remembered invocation".into()))?;
                peer.call("host.echo", saved, Duration::from_secs(5)).await
            }
            "forge" => {
                let field = input
                    .get("field")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::Remote("missing forged field".into()))?;
                if !["scope", "session", "generation", "lease"].contains(&field) {
                    return Err(RpcError::Remote("invalid forged field".into()));
                }
                let mut forged = params.clone();
                forged[field] = input.get("value").cloned().unwrap_or(Value::Null);
                peer.call("host.echo", forged, Duration::from_secs(5)).await
            }
            "stderr.flood" => {
                let bytes = vec![b'x'; 262_144];
                tokio::io::stderr()
                    .write_all(&bytes)
                    .await
                    .map_err(|error| RpcError::Io(error.to_string()))?;
                Ok(json!({"bytes": bytes.len()}))
            }
            "nested" | "event" | "job" => {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(RpcError::Cancelled),
                    result = peer.call("host.echo", params, Duration::from_secs(5)) => result,
                }
            }
            "socket.start" => {
                let addr = loopback_address(&input)?;
                let socket = TcpStream::connect(addr)
                    .await
                    .map_err(|error| RpcError::Io(error.to_string()))?;
                // Lock before checking stop so concurrent shutdown cannot miss this task.
                let mut sockets = self.sockets.lock().await;
                if self.stop.is_cancelled() {
                    return Err(RpcError::Cancelled);
                }
                let stop = self.stop.clone();
                sockets.push(tokio::spawn(heartbeat(socket, stop)));
                Ok(json!({"started": true}))
            }
            "descendant.start" if self.supports_descendants => {
                let addr = loopback_address(&input)?;
                let executable =
                    std::env::current_exe().map_err(|error| RpcError::Io(error.to_string()))?;
                let mut child = Command::new(executable)
                    .arg("--descendant")
                    .arg(addr.to_string())
                    .stdin(Stdio::null())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|error| RpcError::Io(error.to_string()))?;
                let pid = child.id().ok_or(RpcError::Closed)?;
                // Remain in the module's process group; host must kill and reap it.
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                Ok(json!({"pid": pid}))
            }
            "crash" => std::process::exit(71),
            "hang" => {
                cancellation.cancelled().await;
                Err(RpcError::Cancelled)
            }
            _ => Err(RpcError::Remote(format!(
                "unknown fixture method: {method}"
            ))),
        }
    }
}

fn loopback_address(input: &Value) -> Result<std::net::SocketAddr, RpcError> {
    let addr = input
        .get("addr")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::Remote("missing loopback addr".into()))?
        .parse::<std::net::SocketAddr>()
        .map_err(|error| RpcError::Remote(error.to_string()))?;
    if !addr.ip().is_loopback() {
        return Err(RpcError::Remote("fixture sockets must use loopback".into()));
    }
    Ok(addr)
}

async fn heartbeat(mut socket: TcpStream, stop: CancellationToken) {
    let mut interval = tokio::time::interval(Duration::from_millis(20));
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = interval.tick() => {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    result = socket.write_all(b"heartbeat\n") => if result.is_err() { break; },
                }
            }
        }
    }
}

pub async fn run(identity: &'static str, build: &'static str, supports_descendants: bool) {
    if supports_descendants && std::env::args().nth(1).as_deref() == Some("--descendant") {
        // A controlled adversarial fixture, not the recommended module task model.
        // The supervisor must escalate to SIGKILL for this native descendant.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        let addr = std::env::args().nth(2).expect("descendant address");
        let socket = TcpStream::connect(addr).await.expect("descendant connect");
        heartbeat(socket, CancellationToken::new()).await;
        std::future::pending::<()>().await;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has_flag = |flag: &str| args.iter().any(|arg| arg == flag);
    let stubborn = has_flag("--stubborn");
    if stubborn {
        // Force the leader-kill path even though shutdown acknowledges normally.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }
    let fixture = Arc::new(Fixture {
        identity,
        build,
        supports_descendants,
        bad_protocol: has_flag("--bad-protocol"),
        no_hello: has_flag("--no-hello"),
        bad_init: has_flag("--bad-init"),
        initialization: Mutex::new(None),
        remembered: Mutex::new(None),
        stop: CancellationToken::new(),
        sockets: Mutex::new(Vec::new()),
    });
    let peer = RpcPeer::new(tokio::io::stdin(), tokio::io::stdout(), fixture.clone());
    // The host closes RPC after reading shutdown acknowledgement. EOF also ends a
    // fixture when the host crashes or cancels handshake.
    peer.wait_closed().await;
    fixture.stop_sockets().await;
    if stubborn {
        std::future::pending::<()>().await;
    }
}
