use thiserror::Error;

#[derive(Debug, Error)]
pub enum WalkerError {
    #[error("failed to read directory '{path}'")]
    DirectoryRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("file watcher error")]
    Watcher(#[source] notify::Error),
}
