use std::io;
use std::path::Path;

use liftgate_types::{config::ModelId, error::ValidationError};
use thiserror::Error;
use tokio::io::AsyncReadExt as _;

#[derive(Debug, Error)]
enum TitleModelError {
    #[error("failed to read title model file")]
    Io(#[from] io::Error),

    #[error("title model ID exceeds {max_bytes}-byte bound")]
    TooLong { max_bytes: u64 },

    #[error("invalid title model ID")]
    InvalidModelId(#[from] ValidationError),
}

/// Read on each title request so a TUI selection applies without restarting active sessions.
pub async fn title_model(path: Option<&Path>) -> ModelId {
    let Some(path) = path else {
        return ModelId::fallback_fast();
    };
    match read_title_model(path).await {
        Ok(model) => model,
        Err(TitleModelError::Io(err)) if err.kind() == io::ErrorKind::NotFound => {
            ModelId::fallback_fast()
        }
        Err(error) => {
            tracing::warn!(?error, path = %path.display(), "using default title model");
            ModelId::fallback_fast()
        }
    }
}

async fn read_title_model(path: &Path) -> Result<ModelId, TitleModelError> {
    const MAX_BYTES: u64 = 4096;
    let file = tokio::fs::File::open(path).await?;
    let mut id = String::new();
    if file.take(MAX_BYTES + 1).read_to_string(&mut id).await? as u64 > MAX_BYTES {
        return Err(TitleModelError::TooLong {
            max_bytes: MAX_BYTES,
        });
    }
    Ok(ModelId::try_new(id)?)
}
