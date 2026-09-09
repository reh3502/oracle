//! P2 host-owned send boundary. Loopback HTTP is a test transport, not Discord TLS.
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    net::TcpStream,
    sync::{Notify, Semaphore},
    time::timeout,
};

#[derive(Clone, Copy, Debug)]
pub enum Caller {
    Ai,
    Command,
    Job,
}
#[derive(Clone, Debug)]
pub struct Lease {
    id: String,
    guild: String,
    generation: u64,
    epoch: u64,
    deadline: Instant,
}
#[derive(Debug)]
struct Activation {
    epoch: u64,
    admitting: bool,
    authorized: bool,
}
#[derive(Debug)]
struct Gate {
    generation: u64,
    activations: BTreeMap<String, Activation>,
    leases: BTreeSet<String>,
}
#[derive(Clone)]
pub struct Authority(Arc<Mutex<Gate>>, Arc<Notify>);
impl Authority {
    pub fn new(guilds: &[&str]) -> Self {
        Self(
            Arc::new(Mutex::new(Gate {
                generation: 1,
                leases: BTreeSet::new(),
                activations: guilds
                    .iter()
                    .map(|g| {
                        (
                            (*g).into(),
                            Activation {
                                epoch: 1,
                                admitting: true,
                                authorized: true,
                            },
                        )
                    })
                    .collect(),
            })),
            Arc::new(Notify::new()),
        )
    }
    pub fn admit(
        &self,
        guild: &str,
        _caller: Caller,
        ttl: Duration,
    ) -> Result<Lease, &'static str> {
        let mut gate = self.0.lock().unwrap();
        let a = gate.activations.get(guild).ok_or("unavailable")?;
        if !a.admitting || !a.authorized || ttl.is_zero() {
            return Err("unavailable");
        }
        let lease = Lease {
            id: uuid::Uuid::new_v4().to_string(),
            guild: guild.into(),
            generation: gate.generation,
            epoch: a.epoch,
            deadline: Instant::now() + ttl,
        };
        gate.leases.insert(lease.id.clone());
        Ok(lease)
    }
    pub fn quiesce(&self, guild: &str) {
        self.0
            .lock()
            .unwrap()
            .activations
            .get_mut(guild)
            .unwrap()
            .admitting = false;
    }
    pub fn fence(&self, guild: &str) {
        let mut g = self.0.lock().unwrap();
        let a = g.activations.get_mut(guild).unwrap();
        a.admitting = false;
        a.epoch += 1;
        self.1.notify_waiters();
    }
    pub fn revoke(&self, guild: &str) {
        self.0
            .lock()
            .unwrap()
            .activations
            .get_mut(guild)
            .unwrap()
            .authorized = false;
        self.1.notify_waiters();
    }
    pub fn finish(&self, lease: &Lease) {
        self.0.lock().unwrap().leases.remove(&lease.id);
        self.1.notify_waiters();
    }
    /// Escalation restarts the shared generation, including other active guilds.
    pub fn force_restart(&self, target: &str) -> Disruption {
        let mut g = self.0.lock().unwrap();
        let old = g.generation;
        g.generation += 1;
        g.leases.clear();
        let interrupted = g.activations.keys().cloned().collect();
        for (name, a) in &mut g.activations {
            a.epoch += 1;
            a.admitting = name != target;
        }
        self.1.notify_waiters();
        Disruption {
            old_generation: old,
            new_generation: g.generation,
            interrupted_guilds: interrupted,
            deactivated_guild: target.into(),
        }
    }
}
#[derive(Debug, Serialize)]
pub struct Disruption {
    pub old_generation: u64,
    pub new_generation: u64,
    pub interrupted_guilds: Vec<String>,
    pub deactivated_guild: String,
}
fn valid(g: &Gate, l: &Lease) -> bool {
    g.generation == l.generation
        && g.leases.contains(&l.id)
        && Instant::now() < l.deadline
        && g.activations
            .get(&l.guild)
            .is_some_and(|a| a.authorized && a.epoch == l.epoch)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum EffectState {
    UnknownOutcome,
    RateLimited,
    Verified,
    Fenced,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub effect: String,
    pub attempt: usize,
    pub state: EffectState,
}
/// Append and fsync before any potentially observable write. Reopening retains uncertainty.
pub struct Ledger {
    file: File,
    entries: Vec<Entry>,
}
impl Ledger {
    pub fn open(path: &Path) -> io::Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        // Only newline-terminated records can have completed record()+fsync.
        // A torn suffix may even end midway through UTF-8; parse the intact prefix.
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |i| i + 1);
        let entries = bytes[..end]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(serde_json::from_slice)
            .collect::<Result<Vec<Entry>, _>>()?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        if end != bytes.len() {
            file.set_len(end as u64)?;
            file.sync_data()?;
        }
        Ok(Self { file, entries })
    }

    fn record(&mut self, e: Entry) -> io::Result<()> {
        serde_json::to_writer(&mut self.file, &e)?;
        self.file.write_all(b"\n")?;
        self.file.sync_data()?;
        self.entries.push(e);
        Ok(())
    }
    pub fn latest(&self, id: &str) -> Option<EffectState> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.effect == id)
            .map(|e| e.state.clone())
    }
}
type EffectLocks = Arc<Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>>;
#[derive(Clone)]
pub struct Sender {
    pub authority: Authority,
    pub ledger: Arc<Mutex<Ledger>>,
    pub rate: Arc<Semaphore>,
    effects: EffectLocks,
}
impl Sender {
    pub fn new(authority: Authority, ledger: Ledger, permits: usize) -> Self {
        Self {
            authority,
            ledger: Arc::new(Mutex::new(ledger)),
            rate: Arc::new(Semaphore::new(permits)),
            effects: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
    fn effect_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.effects.lock().unwrap();
        locks.retain(|_, value| value.strong_count() != 0);
        if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(id.to_owned(), Arc::downgrade(&lock));
        lock
    }

    /// Every initial request AND 429 retry obtains a fresh rate permit then checks authority
    /// under the same lock as fence. The single nonblocking write occurs while holding it.
    /// No private client queue or hidden retry can send later without this check.
    pub async fn send(
        &self,
        lease: &Lease,
        effect: &str,
        endpoint: SocketAddr,
    ) -> io::Result<EffectState> {
        // Serialize the entire effect, including admission waits and response reads.
        // A cancelled owner releases the lock; its durable uncertainty remains.
        let effect_lock = self.effect_lock(effect);
        let _effect_owner = effect_lock.lock().await;
        // A stored ambiguous result must be reconciled, never blindly repeated.
        if let Some(state) = self.ledger.lock().unwrap().latest(effect)
            && state != EffectState::RateLimited
        {
            return Ok(state);
        }
        for attempt in 0..3 {
            let admitted = timeout(
                lease.deadline.saturating_duration_since(Instant::now()),
                async {
                    loop {
                        // Register before checking so fence cannot occur in a lost-wakeup gap.
                        let changed = self.authority.1.notified();
                        tokio::pin!(changed);
                        changed.as_mut().enable();
                        if !valid(&self.authority.0.lock().unwrap(), lease) {
                            return false;
                        }
                        tokio::select! {
                            _ = changed => continue,
                            permit = self.rate.acquire() => {
                                match permit {
                                    Ok(permit) => { permit.forget(); return true; }
                                    Err(_) => return false,
                                }
                            }
                        }
                    }
                },
            )
            .await
            .unwrap_or(false);
            if !admitted {
                return self.mark(effect, attempt, EffectState::Fenced);
            }
            let socket = match timeout(Duration::from_secs(2), TcpStream::connect(endpoint)).await {
                Ok(Ok(s)) => s,
                _ => return Err(io::Error::other("connect failed before dispatch")),
            };
            let request=b"POST /api/v10/channels/123/messages HTTP/1.1\r\nHost: fixture\r\nContent-Type: application/json\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"content\":\"test\"}";
            loop {
                timeout(Duration::from_secs(2), socket.writable()).await??;
                let gate = self.authority.0.lock().unwrap();
                if !valid(&gate, lease) {
                    return self.mark(effect, attempt, EffectState::Fenced);
                }
                self.mark(effect, attempt, EffectState::UnknownOutcome)?;
                match socket.try_write(request) {
                    Ok(n) if n == request.len() => break,
                    Ok(_) => return Ok(EffectState::UnknownOutcome),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // No bytes accepted. Retain conservative uncertainty in the durable
                        // ledger; this live task alone knows it can retry its write readiness.
                        drop(gate);
                        continue;
                    }
                    Err(_) => return Ok(EffectState::UnknownOutcome),
                }
            }
            let mut bytes = Vec::new();
            match timeout(
                Duration::from_millis(300),
                socket.take(4097).read_to_end(&mut bytes),
            )
            .await
            {
                Ok(Ok(_)) if bytes.len() <= 4096 => {}
                _ => return Ok(EffectState::UnknownOutcome),
            }
            if bytes.starts_with(b"HTTP/1.1 429 ") {
                self.mark(effect, attempt, EffectState::RateLimited)?;
                continue;
            }
            if bytes.starts_with(b"HTTP/1.1 200 ") {
                return self.mark(effect, attempt, EffectState::Verified);
            }
            return Ok(EffectState::UnknownOutcome);
        }
        Ok(EffectState::RateLimited)
    }
    fn mark(&self, effect: &str, attempt: usize, state: EffectState) -> io::Result<EffectState> {
        self.ledger.lock().unwrap().record(Entry {
            effect: effect.into(),
            attempt,
            state: state.clone(),
        })?;
        Ok(state)
    }
}
