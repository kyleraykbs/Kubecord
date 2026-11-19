#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("media error: {0}")]
    Message(String),
    /// The user closed a portal's picker without choosing.
    #[error("cancelled")]
    Cancelled,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
