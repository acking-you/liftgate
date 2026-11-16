use std::io;
use std::path::Path;

use liftgate_types::config::ModelId;
use tokio::io::AsyncReadExt as _;

/// Read on each title request so a TUI selection applies without restarting active sessions.
pub async fn title_model(path: Option<&Path>) -> ModelId {
    let Some(path) = path else {
        return ModelId::fallback_fast();
    };
    match read_title_model(path).await {
        Ok(model) => model,
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(?error, path = %path.display(), "using default title model");
            }
            ModelId::fallback_fast()
        }
    }
}

async fn read_title_model(path: &Path) -> io::Result<ModelId> {
    const MAX_BYTES: u64 = 4096;
    let file = tokio::fs::File::open(path).await?;
    let mut id = String::new();
    if file.take(MAX_BYTES + 1).read_to_string(&mut id).await? as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "title model ID too long",
        ));
    }
    ModelId::try_new(id).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}
