#[derive(Debug, Clone, serde::Deserialize)]
pub struct ApiErrorBody {
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub retry_after: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("discord returned {status}: {message} (code {code:?})")]
    Api {
        status: reqwest::StatusCode,
        code: Option<i64>,
        message: String,
        retry_after: Option<f64>,
    },

    #[error("could not decode discord's response: {source}")]
    Decode {
        status: reqwest::StatusCode,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid url: {0}")]
    Url(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn status(&self) -> Option<reqwest::StatusCode> {
        match self {
            Self::Api { status, .. } | Self::Decode { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn is_unauthorized(&self) -> bool {
        self.status() == Some(reqwest::StatusCode::UNAUTHORIZED)
    }

    pub fn retry_after(&self) -> Option<f64> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

pub type ApiError = Error;