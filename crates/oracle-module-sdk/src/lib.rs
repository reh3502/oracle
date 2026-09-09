//! Native module author API. The host remains the authority for identity, grants and storage.
//! stdout is reserved for framed RPC. Use tracked scopes for background work.
#![forbid(unsafe_code)]
use async_trait::async_trait;
use oracle_contracts::{DocumentWrite, GuildId, ModuleDocument, ModuleManifest};
pub use oracle_rpc::RpcError;
use oracle_rpc::{RpcHandler, RpcPeer};
pub use oracle_task_scope::{HostTasks, SpawnError, TaskError, TaskId, TaskStats};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite};
pub use tokio_util::sync::CancellationToken;

pub type Result<T> = std::result::Result<T, RpcError>;
const GRACE: Duration = Duration::from_secs(2);
fn denied() -> RpcError {
    RpcError::Remote("module lifecycle or authority rejected".into())
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| RpcError::Protocol("invalid module request".into()))
}
fn encode<T: serde::Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| RpcError::Protocol("invalid module response".into()))
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Normal,
    Migration,
}

/// A tracked scope exposes no host client and no storage authority.
#[derive(Clone)]
pub struct TaskScope {
    tasks: Arc<HostTasks>,
    cancel: CancellationToken,
    accepting: Arc<Mutex<bool>>,
}
impl TaskScope {
    fn new() -> Self {
        Self {
            tasks: Arc::new(HostTasks::new()),
            cancel: CancellationToken::new(),
            accepting: Arc::new(Mutex::new(true)),
        }
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.child_token()
    }
    fn seal(&self) {
        *self.accepting.lock().unwrap() = false;
        self.cancel.cancel();
    }
    pub fn spawn<F>(&self, name: &'static str, future: F) -> std::result::Result<TaskId, SpawnError>
    where
        F: Future<Output = std::result::Result<(), TaskError>> + Send + 'static,
    {
        let accepting = self.accepting.lock().unwrap();
        if !*accepting {
            return Err(SpawnError::NotAccepting);
        }
        self.tasks.spawn(name, future)
    }
    pub fn stats(&self) -> TaskStats {
        self.tasks.stats()
    }
}
#[derive(Clone)]
pub struct GuildContext {
    pub guild: GuildId,
    pub epoch: u64,
    pub generation: u64,
    pub tasks: TaskScope,
}

/// Opaque invocation lease: callers cannot replace its identity or guild envelope.
/// Clones expire when the inbound invocation ends or its guild is quiesced.
#[derive(Clone)]
pub struct CallContext {
    peer: RpcPeer,
    invocation: Value,
    guild: GuildId,
    generation: u64,
    epoch: u64,
    cancel: CancellationToken,
    guild_cancel: CancellationToken,
}
impl CallContext {
    pub fn guild(&self) -> &GuildId {
        &self.guild
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.child_token()
    }
    async fn host<T: DeserializeOwned>(&self, method: &str, mut args: Value) -> Result<T> {
        if self.cancel.is_cancelled() || self.guild_cancel.is_cancelled() {
            return Err(RpcError::Cancelled);
        }
        args.as_object_mut()
            .ok_or_else(denied)?
            .insert("invocation".into(), self.invocation.clone());
        let response = tokio::select! {biased;
            _=self.guild_cancel.cancelled()=>return Err(RpcError::Cancelled),
            value=self.peer.call_cancelled(method,args,Duration::from_secs(30),self.cancel.clone())=>value?
        };
        decode(response)
    }
    pub async fn document_get(
        &self,
        collection: &str,
        key: &str,
    ) -> Result<Option<ModuleDocument>> {
        self.host(
            "host.document_get",
            json!({"collection":collection,"key":key}),
        )
        .await
    }
    pub async fn document_batch(&self, writes: Vec<DocumentWrite>) -> Result<Vec<ModuleDocument>> {
        self.host("host.document_batch", json!({"writes":writes}))
            .await
    }
    /// Request the host's journaled diagnostic echo using this invocation's opaque lease.
    /// The host checks the operation's `host.echo` grant before dispatch.
    pub async fn echo(&self, purpose: &str, body: Value) -> Result<Value> {
        self.host("host.echo", json!({"purpose":purpose,"body":body}))
            .await
    }
    pub async fn contract_invoke(&self, contract: &str, input: Value) -> Result<Value> {
        self.host(
            "host.contract_invoke",
            json!({"contract":contract,"input":input}),
        )
        .await
    }
}
#[async_trait]
pub trait Module: Send + Sync + 'static {
    fn manifest(&self) -> ModuleManifest;
    async fn initialize(&self, _mode: Mode, _global: TaskScope) -> Result<()> {
        Ok(())
    }
    async fn activate(&self, _context: GuildContext) -> Result<()> {
        Ok(())
    }
    async fn deactivate(&self, _guild: &GuildId, _epoch: u64) -> Result<()> {
        Ok(())
    }
    async fn invoke(&self, context: CallContext, operation: &str, input: Value) -> Result<Value>;
    /// Pure transform only: no CallContext exists in migration mode.
    async fn migrate(
        &self,
        _operation: &str,
        _from: u32,
        _to: u32,
        _documents: Vec<ModuleDocument>,
    ) -> Result<Vec<DocumentWrite>> {
        Err(denied())
    }
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    protocol_major: u32,
    protocol_minor: u32,
    session: String,
    generation: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Initialize {
    session: String,
    generation: u64,
    mode: Mode,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Activation {
    guild: GuildId,
    epoch: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invocation {
    invocation: Value,
    session: String,
    generation: u64,
    guild: GuildId,
    epoch: u64,
    operation: String,
    input: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Migration {
    operation: String,
    from: u32,
    to: u32,
    documents: Vec<ModuleDocument>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Quiesce {
    guild: Option<GuildId>,
}
struct GuildState {
    context: GuildContext,
    active: bool,
}
#[derive(Default)]
struct State {
    hello: Option<(String, u64)>,
    mode: Option<Mode>,
    stopping: bool,
    guilds: BTreeMap<GuildId, GuildState>,
}
struct Driver<M: Module> {
    module: Arc<M>,
    state: Mutex<State>,
    control: tokio::sync::Mutex<()>,
    global: TaskScope,
}
impl<M: Module> Driver<M> {
    fn new(module: Arc<M>) -> Self {
        Self {
            module,
            state: Mutex::new(State::default()),
            control: tokio::sync::Mutex::new(()),
            global: TaskScope::new(),
        }
    }
    async fn drain(&self, guild: Option<&GuildId>) -> Result<()> {
        let scopes = {
            let mut state = self.state.lock().unwrap();
            if let Some(guild) = guild {
                let item = state.guilds.get_mut(guild).ok_or_else(denied)?;
                item.active = false;
                vec![item.context.tasks.clone()]
            } else {
                state.stopping = true;
                state
                    .guilds
                    .values_mut()
                    .map(|item| {
                        item.active = false;
                        item.context.tasks.clone()
                    })
                    .chain(std::iter::once(self.global.clone()))
                    .collect()
            }
        };
        for scope in &scopes {
            scope.seal();
        }
        let deadline = tokio::time::Instant::now() + GRACE;
        for scope in scopes {
            scope
                .tasks
                .shutdown(deadline.saturating_duration_since(tokio::time::Instant::now()))
                .await;
        }
        Ok(())
    }
}
#[async_trait]
impl<M: Module> RpcHandler for Driver<M> {
    async fn handle(
        &self,
        peer: RpcPeer,
        method: String,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        if method == "operation.invoke" {
            let request: Invocation = decode(params)?;
            let guild_cancel = {
                let state = self.state.lock().unwrap();
                if state.stopping
                    || state.mode != Some(Mode::Normal)
                    || state.hello.as_ref() != Some(&(request.session.clone(), request.generation))
                {
                    return Err(denied());
                }
                let guild = state
                    .guilds
                    .get(&request.guild)
                    .filter(|g| g.active && g.context.epoch == request.epoch)
                    .ok_or_else(denied)?;
                guild.context.tasks.cancellation()
            };
            let context = CallContext {
                peer,
                invocation: request.invocation,
                guild: request.guild,
                generation: request.generation,
                epoch: request.epoch,
                cancel: cancel.clone(),
                guild_cancel: guild_cancel.clone(),
            };
            return tokio::select! {biased;_=guild_cancel.cancelled()=>Err(RpcError::Cancelled),_=cancel.cancelled()=>Err(RpcError::Cancelled),value=self.module.invoke(context,&request.operation,request.input)=>value};
        }
        // Hooks serialize state changes, but never hold the state mutex over module code.
        let _control = self.control.lock().await;
        match method.as_str() {
            "hello" => {
                let request: Hello = decode(params)?;
                let manifest = self.module.manifest();
                if request.protocol_major != 1
                    || request.protocol_minor < manifest.protocol_minor_min
                    || request.session.is_empty()
                {
                    return Err(denied());
                }
                let mut state = self.state.lock().unwrap();
                if state.hello.is_some() || state.stopping {
                    return Err(denied());
                }
                state.hello = Some((request.session, request.generation));
                Ok(json!({"protocol_major":1,"protocol_minor":0,"manifest":manifest}))
            }
            "initialize" => {
                let request: Initialize = decode(params)?;
                {
                    let state = self.state.lock().unwrap();
                    if state.mode.is_some()
                        || state.stopping
                        || state.hello.as_ref() != Some(&(request.session, request.generation))
                    {
                        return Err(denied());
                    }
                }
                self.module
                    .initialize(request.mode, self.global.clone())
                    .await?;
                self.state.lock().unwrap().mode = Some(request.mode);
                Ok(json!({"initialized":true}))
            }
            "activate" => {
                let request: Activation = decode(params)?;
                let context = {
                    let mut state = self.state.lock().unwrap();
                    if state.stopping || state.mode != Some(Mode::Normal) {
                        return Err(denied());
                    }
                    if state
                        .guilds
                        .get(&request.guild)
                        .is_some_and(|g| g.active || g.context.epoch >= request.epoch)
                    {
                        return Err(denied());
                    }
                    let context = GuildContext {
                        guild: request.guild.clone(),
                        epoch: request.epoch,
                        generation: state.hello.as_ref().ok_or_else(denied)?.1,
                        tasks: TaskScope::new(),
                    };
                    state.guilds.insert(
                        request.guild,
                        GuildState {
                            context: context.clone(),
                            active: false,
                        },
                    );
                    context
                };
                if let Err(error) = self.module.activate(context.clone()).await {
                    self.drain(Some(&context.guild)).await?;
                    return Err(error);
                }
                self.state
                    .lock()
                    .unwrap()
                    .guilds
                    .get_mut(&context.guild)
                    .unwrap()
                    .active = true;
                Ok(json!({"activated":true}))
            }
            "deactivate" => {
                let request: Activation = decode(params)?;
                {
                    let state = self.state.lock().unwrap();
                    if !state
                        .guilds
                        .get(&request.guild)
                        .is_some_and(|g| g.context.epoch == request.epoch)
                    {
                        return Err(denied());
                    }
                }
                self.drain(Some(&request.guild)).await?;
                self.module
                    .deactivate(&request.guild, request.epoch)
                    .await?;
                Ok(json!({"deactivated":true}))
            }
            "quiesce" => {
                let request: Quiesce = decode(params)?;
                self.drain(request.guild.as_ref()).await?;
                Ok(json!({"quiesced":true}))
            }
            "migration.transform" => {
                let request: Migration = decode(params)?;
                {
                    let state = self.state.lock().unwrap();
                    if state.mode != Some(Mode::Migration) || state.stopping {
                        return Err(denied());
                    }
                }
                let writes = self
                    .module
                    .migrate(
                        &request.operation,
                        request.from,
                        request.to,
                        request.documents,
                    )
                    .await?;
                encode(writes)
            }
            "health" => {
                let state = self.state.lock().unwrap();
                let guilds:BTreeMap<_,_>=state.guilds.iter().map(|(id,g)|(id.clone(),json!({"epoch":g.context.epoch,"active":g.active,"tasks":g.context.tasks.stats()}))).collect();
                Ok(
                    json!({"generation":state.hello.as_ref().map(|(_,g)|g),"global":self.global.stats(),"guilds":guilds}),
                )
            }
            "shutdown" => {
                self.drain(None).await?;
                self.module.shutdown().await?;
                Ok(json!({"acknowledged":true}))
            }
            _ => Err(RpcError::Remote("unknown module method".into())),
        }
    }
}
/// The host closes its input after the shutdown response. EOF also drains all scopes.
/// The transport owns and joins invocation handlers; scopes own module background work.
pub async fn serve_streams<M, R, W>(module: Arc<M>, read: R, write: W) -> Result<()>
where
    M: Module,
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let driver = Arc::new(Driver::new(module));
    let peer = RpcPeer::new(read, write, driver.clone());
    peer.wait_closed().await;
    driver.drain(None).await?;
    Ok(())
}
pub async fn serve_stdio<M: Module>(module: Arc<M>) -> Result<()> {
    std::panic::set_hook(Box::new(|_| eprintln!("module task panicked")));
    serve_streams(module, tokio::io::stdin(), tokio::io::stdout()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Probe {
        saved: Mutex<Option<CallContext>>,
        entered: tokio::sync::Notify,
    }
    #[async_trait]
    impl Module for Probe {
        fn manifest(&self) -> ModuleManifest {
            decode(json!({"manifest_version":1,"id":"fixture.probe","version":"1.0.0","target":"x86_64-unknown-linux-gnu","protocol_major":1,"protocol_minor_min":0,"host_api":"^0.1","data_version":1,"readable_data_versions":[1],"operations":[]})).unwrap()
        }
        async fn initialize(&self, mode: Mode, scope: TaskScope) -> Result<()> {
            if mode == Mode::Normal {
                let cancel = scope.cancellation();
                scope
                    .spawn("global", async move {
                        cancel.cancelled().await;
                        Ok(())
                    })
                    .unwrap();
            }
            Ok(())
        }
        async fn activate(&self, context: GuildContext) -> Result<()> {
            let token = context.tasks.cancellation();
            context
                .tasks
                .spawn("guild", async move {
                    token.cancelled().await;
                    Ok(())
                })
                .unwrap();
            Ok(())
        }
        async fn invoke(
            &self,
            context: CallContext,
            operation: &str,
            _input: Value,
        ) -> Result<Value> {
            match operation {
                "document" => encode(context.document_get("items", "main").await?),
                "batch" => encode(
                    context
                        .document_batch(vec![DocumentWrite {
                            collection: "items".into(),
                            key: "main".into(),
                            expected_revision: None,
                            value: Some(json!({"n":1})),
                        }])
                        .await?,
                ),
                "contract" => context.contract_invoke("counter/v1", json!({})).await,
                "host_echo" => context.echo("probe-echo", json!({"nested":[1,null]})).await,
                "echo_null" => context.echo("probe-null", Value::Null).await,
                "replay_echo" => {
                    let saved = self.saved.lock().unwrap().clone().unwrap();
                    saved.echo("expired", Value::Null).await
                }
                "remember" => {
                    *self.saved.lock().unwrap() = Some(context);
                    Ok(Value::Null)
                }
                "replay" => {
                    let saved = self.saved.lock().unwrap().clone().unwrap();
                    encode(saved.document_get("items", "main").await?)
                }
                "wait" => {
                    self.entered.notify_one();
                    context.cancellation().cancelled().await;
                    Err(RpcError::Cancelled)
                }
                _ => Ok(
                    json!({"guild":context.guild(),"generation":context.generation(),"epoch":context.epoch()}),
                ),
            }
        }
        async fn migrate(
            &self,
            _operation: &str,
            _from: u32,
            _to: u32,
            documents: Vec<ModuleDocument>,
        ) -> Result<Vec<DocumentWrite>> {
            Ok(documents
                .into_iter()
                .map(|d| DocumentWrite {
                    collection: d.collection,
                    key: d.key,
                    expected_revision: Some(d.revision),
                    value: Some(d.value),
                })
                .collect())
        }
    }
    struct Host {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl RpcHandler for Host {
        async fn handle(
            &self,
            _peer: RpcPeer,
            method: String,
            params: Value,
            _cancel: CancellationToken,
        ) -> Result<Value> {
            assert_eq!(params["invocation"], "opaque-lease");
            assert!(params.get("guild").is_none());
            assert!(params.get("session").is_none());
            self.calls.fetch_add(1, Ordering::SeqCst);
            match method.as_str() {
                "host.document_get" => {
                    Ok(json!({"collection":"items","key":"main","value":{"n":1},"revision":1}))
                }
                "host.document_batch" => Ok(json!([])),
                "host.echo" => {
                    assert_eq!(params.as_object().unwrap().len(), 3);
                    assert!(params.get("args").is_none());
                    assert!(matches!(
                        params["purpose"].as_str(),
                        Some("probe-echo" | "probe-null")
                    ));
                    assert!(params.get("body").is_some());
                    Ok(params["body"].clone())
                }
                "host.contract_invoke" => {
                    assert_eq!(params["contract"], "counter/v1");
                    Ok(json!({"value":9}))
                }
                _ => Err(denied()),
            }
        }
    }
    struct Harness {
        peer: RpcPeer,
        driver: Arc<Driver<Probe>>,
        server: RpcPeer,
        host: Arc<Host>,
    }
    impl Harness {
        fn new() -> Self {
            let driver = Arc::new(Driver::new(Arc::new(Probe {
                saved: Mutex::new(None),
                entered: tokio::sync::Notify::new(),
            })));
            let host = Arc::new(Host {
                calls: AtomicUsize::new(0),
            });
            let (a, b) = tokio::io::duplex(65536);
            let (ar, aw) = tokio::io::split(a);
            let (br, bw) = tokio::io::split(b);
            Self {
                peer: RpcPeer::new(ar, aw, host.clone()),
                driver: driver.clone(),
                server: RpcPeer::new(br, bw, driver),
                host,
            }
        }
        async fn call(&self, method: &str, params: Value) -> Result<Value> {
            self.peer.call(method, params, Duration::from_secs(3)).await
        }
        async fn initialize(&self, mode: &str) {
            self.call("hello",json!({"protocol_major":1,"protocol_minor":0,"session":"session-one","generation":7})).await.unwrap();
            self.call(
                "initialize",
                json!({"session":"session-one","generation":7,"mode":mode}),
            )
            .await
            .unwrap();
        }
        async fn activate(&self, guild: &str, epoch: u64) {
            self.call("activate", json!({"guild":guild,"epoch":epoch}))
                .await
                .unwrap();
        }
        fn invocation(operation: &str) -> Value {
            json!({"invocation":"opaque-lease","session":"session-one","generation":7,"guild":"100","epoch":2,"operation":operation,"input":{}})
        }
        async fn close(self) {
            self.call("shutdown", json!({})).await.unwrap();
            self.peer.close().await;
            self.server.close().await;
        }
    }
    #[tokio::test]
    async fn authority_rejection_precedes_callbacks_and_typed_clients_preserve_opaque_lease() {
        let h = Harness::new();
        assert!(
            h.call("operation.invoke", Harness::invocation("document"))
                .await
                .is_err()
        );
        h.initialize("normal").await;
        h.activate("100", 2).await;
        for (key, value) in [
            ("session", json!("other")),
            ("generation", json!(8)),
            ("guild", json!("200")),
            ("epoch", json!(1)),
        ] {
            let mut params = Harness::invocation("document");
            params[key] = value;
            assert!(h.call("operation.invoke", params).await.is_err());
        }
        assert_eq!(h.host.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            h.call("operation.invoke", Harness::invocation("document"))
                .await
                .unwrap()["revision"],
            1
        );
        assert_eq!(
            h.call("operation.invoke", Harness::invocation("batch"))
                .await
                .unwrap(),
            json!([])
        );
        assert_eq!(
            h.call("operation.invoke", Harness::invocation("contract"))
                .await
                .unwrap()["value"],
            9
        );
        h.close().await;
    }
    #[tokio::test]
    async fn echo_callback_preserves_flat_envelope_and_expires_with_invocation() {
        let h = Harness::new();
        h.initialize("normal").await;
        h.activate("100", 2).await;
        assert_eq!(
            h.call("operation.invoke", Harness::invocation("host_echo"))
                .await
                .unwrap(),
            json!({"nested":[1,null]})
        );
        assert_eq!(
            h.call("operation.invoke", Harness::invocation("echo_null"))
                .await
                .unwrap(),
            Value::Null
        );
        assert_eq!(h.host.calls.load(Ordering::SeqCst), 2);
        h.call("operation.invoke", Harness::invocation("remember"))
            .await
            .unwrap();
        assert_eq!(
            h.call("operation.invoke", Harness::invocation("replay_echo"))
                .await,
            Err(RpcError::Cancelled)
        );
        assert_eq!(h.host.calls.load(Ordering::SeqCst), 2);
        h.close().await;
    }
    #[tokio::test]
    async fn a_saved_context_expires_when_its_invocation_returns() {
        let h = Harness::new();
        h.initialize("normal").await;
        h.activate("100", 2).await;
        h.call("operation.invoke", Harness::invocation("remember"))
            .await
            .unwrap();
        assert_eq!(
            h.call("operation.invoke", Harness::invocation("replay"))
                .await,
            Err(RpcError::Cancelled)
        );
        assert_eq!(h.host.calls.load(Ordering::SeqCst), 0);
        h.close().await;
    }
    #[tokio::test]
    async fn quiesce_joins_one_guild_and_rejects_old_epoch_without_stopping_other_guild() {
        let h = Harness::new();
        h.initialize("normal").await;
        h.activate("100", 2).await;
        h.activate("200", 5).await;
        let params = Harness::invocation("wait");
        let peer = h.peer.clone();
        let call = tokio::spawn(async move {
            peer.call("operation.invoke", params, Duration::from_secs(3))
                .await
        });
        h.driver.module.entered.notified().await;
        h.call("quiesce", json!({"guild":"100"})).await.unwrap();
        assert_eq!(call.await.unwrap(), Err(RpcError::Cancelled));
        let health = h.call("health", json!({})).await.unwrap();
        assert_eq!(health["guilds"]["100"]["tasks"]["counts"]["running"], 0);
        assert_eq!(health["guilds"]["200"]["tasks"]["counts"]["running"], 1);
        assert!(
            h.call("operation.invoke", Harness::invocation("echo"))
                .await
                .is_err()
        );
        assert!(
            h.call("activate", json!({"guild":"100","epoch":2}))
                .await
                .is_err()
        );
        h.activate("100", 3).await;
        let mut params = Harness::invocation("echo");
        params["epoch"] = json!(3);
        assert_eq!(
            h.call("operation.invoke", params).await.unwrap()["epoch"],
            3
        );
        h.call("shutdown", json!({})).await.unwrap();
        let health = h.call("health", json!({})).await.unwrap();
        assert_eq!(health["global"]["counts"]["running"], 0);
        assert_eq!(health["guilds"]["200"]["tasks"]["counts"]["running"], 0);
        h.close().await;
    }
    #[tokio::test]
    async fn migration_mode_has_no_activation_or_ordinary_invocations() {
        let h = Harness::new();
        h.initialize("migration").await;
        assert!(
            h.call("activate", json!({"guild":"100","epoch":2}))
                .await
                .is_err()
        );
        assert!(
            h.call("operation.invoke", Harness::invocation("document"))
                .await
                .is_err()
        );
        let value=h.call("migration.transform",json!({"operation":"transform","from":1,"to":2,"documents":[{"collection":"items","key":"main","value":{"n":3},"revision":4}]})).await.unwrap();
        assert_eq!(value[0]["expected_revision"], 4);
        assert_eq!(value[0]["key"], "main");
        assert_eq!(h.host.calls.load(Ordering::SeqCst), 0);
        h.close().await;
        let h = Harness::new();
        h.initialize("normal").await;
        assert!(
            h.call(
                "migration.transform",
                json!({"operation":"transform","from":1,"to":2,"documents":[]})
            )
            .await
            .is_err()
        );
        h.close().await;
    }
    #[tokio::test]
    async fn sealed_scope_rejects_retained_spawn_handles_and_joins_noncooperative_futures() {
        let scope = TaskScope::new();
        let retained = scope.clone();
        scope.spawn("pending", std::future::pending()).unwrap();
        scope.seal();
        assert_eq!(
            retained.spawn("late", async { Ok(()) }),
            Err(SpawnError::NotAccepting)
        );
        assert!(retained.cancellation().is_cancelled());
        let summary = scope.tasks.shutdown(Duration::ZERO).await;
        assert_eq!(summary.stats.counts.running, 0);
        assert_eq!(summary.stats.counts.aborted, 1);
    }
}
