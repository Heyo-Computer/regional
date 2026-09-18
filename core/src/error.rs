use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("region config: {0}")]
    Region(String),

    #[error("meilisearch: {0}")]
    Meili(#[from] Box<meilisearch_sdk::errors::Error>),

    #[error("meilisearch task {task_uid} failed: {message}")]
    Task { task_uid: u32, message: String },

    #[error("timed out waiting for meilisearch at {url} after {seconds}s")]
    MeiliUnreachable { url: String, seconds: u64 },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl From<meilisearch_sdk::errors::Error> for Error {
    fn from(e: meilisearch_sdk::errors::Error) -> Self {
        Error::Meili(Box::new(e))
    }
}

impl Error {
    pub fn other(msg: impl fmt::Display) -> Self {
        Error::Other(msg.to_string())
    }
}
