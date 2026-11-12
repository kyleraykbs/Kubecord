#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("media error: {0}")]
    Message(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
