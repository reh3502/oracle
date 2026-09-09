//! Private local-control protocol and bounded newline-delimited socket transport.
use oracle_core::{Error, ErrorCode, GuildId, ModuleId, Result};
use oracle_operations::ingress::OperationRequest;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ModuleRequest {
    Install {
        source: PathBuf,
        trust_native: bool,
    },
    List {},
    Load {
        digest: String,
    },
    Upgrade {
        module: ModuleId,
        digest: String,
        grace_ms: u64,
    },
    Activate {
        module: ModuleId,
        guild: GuildId,
        grants: Vec<String>,
        bindings: BTreeMap<String, ModuleId>,
    },
    Invoke {
        module: ModuleId,
        guild: GuildId,
        operation: String,
        input: serde_json::Value,
    },
    Deactivate {
        module: ModuleId,
        guild: GuildId,
        grace_ms: u64,
    },
    Unload {
        module: ModuleId,
        grace_ms: u64,
    },
    Health {},
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    PublishCommands,
    Operation {
        guild: GuildId,
        request: OperationRequest,
    },
    Module {
        request: ModuleRequest,
    },
    Status {
        guild: Option<GuildId>,
    },
    Control {
        guild: GuildId,
        paused: bool,
        expected_revision: Option<u64>,
    },
    Recovery {
        guild: GuildId,
        limit: u32,
    },
    Backup {
        output: PathBuf,
    },
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Response {
    // Explicit JSON null is a valid invocation result; only a missing field is absent.
    #[serde(default, deserialize_with = "present_result")]
    pub(crate) result: Option<serde_json::Value>,
    pub(crate) error: Option<ErrorCode>,
}
fn present_result<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}
pub(crate) const MAX_FRAME: u64 = 1024 * 1024;
pub(crate) async fn read_frame<R: tokio::io::AsyncRead + Unpin>(stream: R) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    BufReader::new(stream.take(MAX_FRAME + 1))
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
    if bytes.len() > MAX_FRAME as usize || bytes.last() != Some(&b'\n') {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(bytes)
}
pub(crate) async fn remote(mut stream: UnixStream, request: Request) -> Result<serde_json::Value> {
    let mut bytes =
        serde_json::to_vec(&request).map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME as usize {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| Error::with_source(ErrorCode::Io, e))?;
    let bytes = tokio::time::timeout(Duration::from_secs(300), read_frame(stream))
        .await
        .map_err(|_| Error::new(ErrorCode::Cancelled))??;
    let response: Response = serde_json::from_slice(&bytes)
        .map_err(|e| Error::with_source(ErrorCode::InvalidInput, e))?;
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (_, Some(code)) => Err(Error::new(code)),
        _ => Err(Error::new(ErrorCode::Integrity)),
    }
}

/// Commands that require a running host never fall back to opening a deployment.
pub(crate) async fn send(socket: &std::path::Path, request: Request) -> Result<serde_json::Value> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| Error::with_source(ErrorCode::ModuleUnavailable, e))?;
    remote(stream, request).await
}

/// Process one bounded request. The caller owns concurrency limits and cancellation.
pub(crate) async fn serve_connection(
    host: &crate::host::Host,
    mut stream: UnixStream,
) -> std::result::Result<(), oracle_core::tasks::TaskError> {
    use oracle_core::tasks::TaskError;
    let result = match tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream)).await {
        Ok(Ok(bytes)) => match serde_json::from_slice(&bytes) {
            Ok(request) => host.handle(request).await,
            Err(_) => Err(Error::new(ErrorCode::InvalidInput)),
        },
        _ => Err(Error::new(ErrorCode::InvalidInput)),
    };
    let response = match result {
        Ok(result) => Response {
            result: Some(result),
            error: None,
        },
        Err(error) => Response {
            result: None,
            error: Some(error.code),
        },
    };
    let mut bytes = serde_json::to_vec(&response).map_err(|_| TaskError)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME as usize {
        bytes = serde_json::to_vec(&Response {
            result: None,
            error: Some(ErrorCode::QuotaExceeded),
        })
        .map_err(|_| TaskError)?;
        bytes.push(b'\n');
    }
    stream.write_all(&bytes).await.map_err(|_| TaskError)
}
