//! Pool of idle worker WebSocket connections per API key.
//!
//! Each accepted `/v1/tunnel` WebSocket is owned by an `idle_watcher` task.
//! The watcher keeps the socket alive (idle Ping keepalives, automatic Pong
//! replies) and parks a `oneshot` handle in the registry. When application
//! traffic needs tunneled egress, `TunnelRegistry::connect` hands the raw
//! socket to the caller via the saved channel and the watcher exits.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::oneshot::{Receiver as OneshotRx, Sender as OneshotTx};
use tokio::sync::Notify;
use tokio::time::{interval, sleep_until, Instant};

use super::stream::WsByteStream;

/// Error surfaced while obtaining a tunneled connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelError {
    /// No idle worker available within the acquire timeout.
    NoWorker,
    /// Worker could not reach the remote target.
    RemoteConnectFailed(String),
    /// Worker did not answer `CONNECTED` within the handshake timeout.
    HandshakeTimeout,
    /// Transport-level failure.
    Io(String),
}

impl fmt::Display for TunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TunnelError::NoWorker => write!(f, "no tunnel worker available"),
            TunnelError::RemoteConnectFailed(d) => {
                write!(f, "remote connect failed: {d}")
            }
            TunnelError::HandshakeTimeout => write!(f, "tunnel handshake timeout"),
            TunnelError::Io(d) => write!(f, "tunnel io error: {d}"),
        }
    }
}

impl std::error::Error for TunnelError {}

/// Request delivered to an idle watcher asking it to surrender its socket.
pub(crate) struct ConnectReq {
    pub(crate) resp: OneshotTx<Result<WebSocket, TunnelError>>,
}

struct IdleWorker {
    id: u64,
    tx: OneshotTx<ConnectReq>,
}

/// Identifies a parked worker for safe deregistration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerHandle {
    key: String,
    pub id: u64,
}

type Pool = Arc<Mutex<HashMap<String, VecDeque<IdleWorker>>>>;

/// Registry of idle tunnel workers, keyed by API key.
pub struct TunnelRegistry {
    pool: Pool,
    notify: Notify,
    next_id: AtomicU64,
    max_workers_per_key: usize,
}

impl TunnelRegistry {
    pub fn new(max_workers_per_key: usize) -> Self {
        Self {
            pool: Arc::new(Mutex::new(HashMap::new())),
            notify: Notify::new(),
            next_id: AtomicU64::new(1),
            max_workers_per_key: max_workers_per_key.max(1),
        }
    }

    /// Park a newly accepted worker socket. Spawns the idle watcher task and
    /// returns a handle usable for explicit deregistration (the watcher also
    /// deregisters itself on socket death).
    pub fn register(self: &Arc<Self>, key: &str, ws: WebSocket, idle_ping: Duration) -> WorkerHandle {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx): (OneshotTx<ConnectReq>, OneshotRx<ConnectReq>) =
            tokio::sync::oneshot::channel();
        {
            let mut pool = self.pool.lock().unwrap();
            let queue = pool.entry(key.to_string()).or_default();
            // Evict oldest workers beyond the per-key cap.
            while queue.len() >= self.max_workers_per_key {
                match queue.pop_front() {
                    Some(evicted) => {
                        let _ = evicted.tx.send(ConnectReq {
                            resp: tokio::sync::oneshot::channel().0,
                        });
                    }
                    None => break,
                }
            }
            queue.push_back(IdleWorker { id, tx });
        }
        self.notify.notify_waiters();
        let handle = WorkerHandle { key: key.to_string(), id };
        let registry = Arc::clone(self);
        let watched = handle.clone();
        tokio::spawn(async move {
            registry.idle_watcher(watched, ws, rx, idle_ping).await;
        });
        handle
    }

    /// Remove a worker from the pool (no-op if already taken or gone).
    pub fn deregister(&self, handle: &WorkerHandle) {
        let mut pool = self.pool.lock().unwrap();
        if let Some(queue) = pool.get_mut(&handle.key) {
            if let Some(pos) = queue.iter().position(|w| w.id == handle.id) {
                queue.remove(pos);
            }
            if queue.is_empty() {
                pool.remove(&handle.key);
            }
        }
    }

    /// Number of idle workers for a key.
    pub fn idle_count(&self, key: &str) -> usize {
        self.pool.lock().unwrap().get(key).map_or(0, |q| q.len())
    }

    /// Acquire a worker for `key` and run the `CONNECT {host} {port}`
    /// handshake, returning a byte stream bridged to the remote target.
    pub async fn connect(
        &self,
        key: &str,
        host: &str,
        port: u16,
        handshake_timeout: Duration,
        acquire_timeout: Duration,
    ) -> Result<WsByteStream, TunnelError> {
        let deadline = Instant::now() + acquire_timeout;
        loop {
            // LIFO: pop the most recently parked worker.
            let worker = {
                let mut pool = self.pool.lock().unwrap();
                let mut worker = None;
                let mut empty = false;
                if let Some(q) = pool.get_mut(key) {
                    if !q.is_empty() {
                        worker = q.pop_back();
                    } else {
                        empty = true;
                    }
                }
                if empty {
                    pool.remove(key);
                }
                worker
            };
            match worker {
                Some(w) => {
                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                    if w.tx.send(ConnectReq { resp: resp_tx }).is_err() {
                        // Watcher already gone; try the next worker.
                        if self.idle_count(key) == 0 && Instant::now() >= deadline {
                            return Err(TunnelError::NoWorker);
                        }
                        continue;
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let ws = tokio::select! {
                        res = resp_rx => {
                            match res {
                                Ok(Ok(ws)) => ws,
                                Ok(Err(e)) => return Err(e),
                                Err(_) => return Err(TunnelError::NoWorker),
                            }
                        }
                        _ = sleep_until(Instant::now() + remaining.max(Duration::from_millis(1))) => {
                            return Err(TunnelError::NoWorker);
                        }
                    };
                    // Handshake failures are final for this request: the
                    // worker already left the pool.
                    return WsByteStream::establish(ws, host, port, handshake_timeout).await;
                }
                None => {
                    if Instant::now() >= deadline {
                        return Err(TunnelError::NoWorker);
                    }
                    // Register interest in notifications before re-checking
                    // the pool (polling the future parks the waiter; the
                    // double-check closes the check/enable race).
                    let mut notified = std::pin::pin!(self.notify.notified());
                    let _ = futures_util::poll!(&mut notified);
                    if self.idle_count(key) > 0 {
                        continue;
                    }
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = sleep_until(deadline) => return Err(TunnelError::NoWorker),
                    }
                }
            }
        }
    }

    /// Owns an idle worker socket until it is handed over or dies.
    async fn idle_watcher(
        &self,
        handle: WorkerHandle,
        mut ws: WebSocket,
        mut rx: OneshotRx<ConnectReq>,
        idle_ping: Duration,
    ) {
        // TakeOnceCell-style guard: deregister on any exit path except
        // successful handover (where connect() already removed us).
        let mut handed_off = false;
        {
            let mut ping = interval(idle_ping);
            ping.tick().await; // consume the immediate first tick
            loop {
                tokio::select! {
                    biased;
                    req = &mut rx => {
                        match req {
                            Ok(req) => {
                                handed_off = true;
                                let _ = req.resp.send(Ok(ws));
                            }
                            Err(_) => {}
                        }
                        break;
                    }
                    _ = ping.tick() => {
                        if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                            break;
                        }
                    }
                    msg = ws.next() => {
                        match msg {
                            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                            // tungstenite replies Pong to Ping automatically;
                            // flush so the queued reply actually goes out.
                            Some(Ok(Message::Ping(_))) => {
                                if ws.flush().await.is_err() {
                                    break;
                                }
                            }
                            Some(Ok(_)) => {}
                        }
                    }
                }
            }
        }
        if !handed_off {
            self.deregister(&handle);
        }
    }
}
