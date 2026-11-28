//! Cancel-safe JSON-RPC 2.0 stdio framing and request/notification multiplexer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use bytes::Bytes;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    process::Command,
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_stream::StreamExt;
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    sync::{CancellationToken, DropGuard},
};

use crate::job::{ProcessGroupError, ProcessGroupGuard, configure_command_group};

/// Typed errors produced by a stateful JSON-RPC stdio session.
#[derive(Debug, Error)]
pub enum StatefulRpcError {
    #[error("failed to spawn stateful provider executable `{program}`")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to attach child process group")]
    ProcessGroup(#[from] ProcessGroupError),

    #[error("provider stdio channel closed unexpectedly")]
    ChannelClosed,

    #[error("failed to serialize or decode JSON-RPC payload")]
    Json(#[from] serde_json::Error),

    #[error("upstream rate limit reached ({code}): {message}")]
    RateLimited { code: i64, message: String },

    #[error("JSON-RPC error ({code}): {message}")]
    Rpc { code: i64, message: String },

    #[error("stateful provider turn timed out after {elapsed_ms} ms")]
    Timeout { elapsed_ms: u64 },
}

impl StatefulRpcError {
    /// Classifies whether an error can be retried idempotently (never retries 429 / capacity).
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. }
            | Self::Spawn { .. }
            | Self::ProcessGroup(_)
            | Self::Json(_)
            | Self::Rpc { .. }
            | Self::Timeout { .. } => false,
            Self::ChannelClosed => true,
        }
    }
}

/// Spawn parameters for a stateful stdio provider child process.
#[derive(Clone, Debug)]
pub struct StdioSpawnConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub max_line_bytes: usize,
}

#[derive(Debug, Deserialize)]
struct RawRpcFrame {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RawRpcError>,
}

#[derive(Debug, Deserialize)]
struct RawRpcError {
    code: i64,
    message: String,
}

/// Incoming server-initiated notification or request observed on stdout.
#[derive(Clone, Debug)]
pub struct RpcInboundMessage {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

type PendingMap = Mutex<HashMap<u64, oneshot::Sender<Result<Value, StatefulRpcError>>>>;

/// Active JSON-RPC 2.0 duplex session backed by either a child process or an async test stream.
pub struct StdioRpcPeer {
    next_id: AtomicU64,
    tx: mpsc::Sender<Bytes>,
    pending: Arc<PendingMap>,
    inbound_rx: tokio::sync::Mutex<mpsc::Receiver<RpcInboundMessage>>,
    _tasks: JoinSet<()>,
    _cancel_guard: DropGuard,
    _group_guard: Option<ProcessGroupGuard>,
}

impl StdioRpcPeer {
    /// Spawns a supervised child process in its own OS process group and starts framing tasks.
    pub fn spawn(cfg: &StdioSpawnConfig) -> Result<Self, StatefulRpcError> {
        let mut cmd = Command::new(&cfg.program);
        cmd.args(&cfg.args)
            .current_dir(&cfg.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        configure_command_group(&mut cmd);

        let mut child = cmd.spawn().map_err(|source| StatefulRpcError::Spawn {
            program: cfg.program.display().to_string(),
            source,
        })?;
        let group_guard = ProcessGroupGuard::attach(&child)?;
        let stdin = child.stdin.take().ok_or(StatefulRpcError::ChannelClosed)?;
        let stdout = child.stdout.take().ok_or(StatefulRpcError::ChannelClosed)?;

        let mut peer = Self::from_io(stdout, stdin, cfg.max_line_bytes, Some(group_guard));
        peer._tasks.spawn(async move {
            let _ = child.wait().await;
        });
        Ok(peer)
    }

    /// Constructs a JSON-RPC peer over arbitrary async reader and writer streams.
    #[must_use]
    pub fn from_io<R, W>(
        reader: R,
        mut writer: W,
        max_line_bytes: usize,
        group_guard: Option<ProcessGroupGuard>,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (write_tx, mut write_rx) = mpsc::channel::<Bytes>(128);
        let (inbound_tx, inbound_rx) = mpsc::channel::<RpcInboundMessage>(256);
        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        let cancel = CancellationToken::new();
        let cancel_writer = cancel.clone();
        let cancel_reader = cancel.clone();
        let cancel_guard = cancel.drop_guard();

        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = cancel_writer.cancelled() => break,
                    maybe_frame = write_rx.recv() => {
                        let Some(frame) = maybe_frame else { break; };
                        if writer.write_all(&frame).await.is_err() {
                            break;
                        }
                        if writer.flush().await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let pending_reader = Arc::clone(&pending);
        tasks.spawn(async move {
            let mut lines =
                FramedRead::new(reader, LinesCodec::new_with_max_length(max_line_bytes));
            loop {
                tokio::select! {
                    biased;
                    () = cancel_reader.cancelled() => break,
                    maybe_line = lines.next() => {
                        let Some(Ok(line)) = maybe_line else { break; };
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(frame) = serde_json::from_str::<RawRpcFrame>(trimmed) else {
                            continue;
                        };
                        if let Some(method) = frame.method {
                            let msg = RpcInboundMessage {
                                id: frame.id,
                                method,
                                params: frame.params.unwrap_or(Value::Null),
                            };
                            if inbound_tx.send(msg).await.is_err() {
                                break;
                            }
                        } else if let Some(id_val) = frame.id
                            && let Some(id_num) = id_val.as_u64()
                        {
                            let tx_opt = match pending_reader.lock() {
                                Ok(mut map) => map.remove(&id_num),
                                Err(poisoned) => poisoned.into_inner().remove(&id_num),
                            };
                            if let Some(waiter) = tx_opt {
                                let outcome = if let Some(err) = frame.error {
                                    let lower = err.message.to_ascii_lowercase();
                                    if err.code == 429
                                        || err.code == -32029
                                        || lower.contains("429")
                                        || lower.contains("rate limit")
                                        || lower.contains("quota")
                                    {
                                        Err(StatefulRpcError::RateLimited {
                                            code: err.code,
                                            message: err.message,
                                        })
                                    } else {
                                        Err(StatefulRpcError::Rpc {
                                            code: err.code,
                                            message: err.message,
                                        })
                                    }
                                } else {
                                    Ok(frame.result.unwrap_or(Value::Null))
                                };
                                let _ = waiter.send(outcome);
                            }
                        }
                    }
                }
            }
            let waiters: Vec<oneshot::Sender<Result<Value, StatefulRpcError>>> =
                match pending_reader.lock() {
                    Ok(mut map) => map.drain().map(|(_, w)| w).collect(),
                    Err(poisoned) => poisoned.into_inner().drain().map(|(_, w)| w).collect(),
                };
            for w in waiters {
                let _ = w.send(Err(StatefulRpcError::ChannelClosed));
            }
        });

        Self {
            next_id: AtomicU64::new(1),
            tx: write_tx,
            pending,
            inbound_rx: tokio::sync::Mutex::new(inbound_rx),
            _tasks: tasks,
            _cancel_guard: cancel_guard,
            _group_guard: group_guard,
        }
    }

    /// Sends a JSON-RPC 2.0 request and awaits the correlated response.
    pub async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, StatefulRpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        match self.pending.lock() {
            Ok(mut map) => {
                map.insert(id, tx);
            }
            Err(poisoned) => {
                poisoned.into_inner().insert(id, tx);
            }
        }

        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut encoded = serde_json::to_vec(&payload)?;
        encoded.push(b'\n');
        if self.tx.send(Bytes::from(encoded)).await.is_err() {
            return Err(StatefulRpcError::ChannelClosed);
        }

        let raw_val = rx.await.map_err(|_| StatefulRpcError::ChannelClosed)??;
        serde_json::from_value(raw_val).map_err(StatefulRpcError::Json)
    }

    /// Sends a JSON-RPC 2.0 notification (no id).
    pub async fn notify<P: Serialize>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<(), StatefulRpcError> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut encoded = serde_json::to_vec(&payload)?;
        encoded.push(b'\n');
        self.tx
            .send(Bytes::from(encoded))
            .await
            .map_err(|_| StatefulRpcError::ChannelClosed)
    }

    /// Sends a JSON-RPC 2.0 response to a server-initiated request (such as permission approval).
    pub async fn respond<R: Serialize>(
        &self,
        id: Value,
        result: &R,
    ) -> Result<(), StatefulRpcError> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        let mut encoded = serde_json::to_vec(&payload)?;
        encoded.push(b'\n');
        self.tx
            .send(Bytes::from(encoded))
            .await
            .map_err(|_| StatefulRpcError::ChannelClosed)
    }

    /// Receives the next inbound notification or server-initiated request.
    pub async fn next_inbound(&self) -> Option<RpcInboundMessage> {
        let mut guard = self.inbound_rx.lock().await;
        guard.recv().await
    }

    /// Non-blocking drain of any already-buffered inbound notifications before closing a turn.
    pub async fn try_next_inbound(&self) -> Option<RpcInboundMessage> {
        let mut guard = self.inbound_rx.lock().await;
        guard.try_recv().ok()
    }
}
