#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("voice transport error: {0}")]
    Message(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
