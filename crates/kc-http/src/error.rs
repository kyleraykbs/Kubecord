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

    /// Discord wants a captcha solved before it does this (friend requests
    /// often ask). The request can be retried with the solution in the
    /// `X-Captcha-Key` header (and `X-Captcha-Rqtoken`).
    #[error("discord wants a captcha solved first ({service})")]
    Captcha {
        service: String,
        sitekey: String,
        rqdata: Option<String>,
        rqtoken: Option<String>,
    },

    #[error("could not decode discord's response: {source}")]
    Decode {
        status: reqwest::StatusCode,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid url: {0}")]
    Url(String),

    #[error("download is larger than the client will buffer")]
    TooLarge,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn status(&self) -> Option<reqwest::StatusCode> {
        match self {
            Self::Api { status, .. } | Self::Decode { status, .. } => Some(*status),
            Self::Captcha { .. } => Some(reqwest::StatusCode::BAD_REQUEST),
            _ => None,
        }
    }

    pub fn is_captcha(&self) -> bool {
        matches!(self, Self::Captcha { .. })
    }

    /// Discord wants the account verified (by email, phone, ...) before
    /// it allows this.
    pub fn needs_verification(&self) -> bool {
        matches!(
            self,
            Self::Api {
                code: Some(40002),
                ..
            }
        )
    }

    pub fn is_unauthorized(&self) -> bool {
        self.status() == Some(reqwest::StatusCode::UNAUTHORIZED)
    }

    /// A short explanation for people, naming the usual causes of the
    /// codes moderation runs into; anything else falls back to Discord's
    /// own message.
    pub fn describe(&self) -> String {
        let Self::Api {
            status,
            code,
            message,
            ..
        } = self
        else {
            return self.to_string();
        };
        let known = match code {
            Some(50013) => Some(
                "You don't have permission to do that. Discord checks both the permission and that \
                 your highest role is above theirs",
            ),
            Some(50001) => Some("You don't have access to that"),
            Some(10007) => Some("They are no longer a member of this server"),
            Some(10011) => Some("That role no longer exists"),
            Some(10026) => Some("That user is not banned"),
            Some(10006) => Some("That invite no longer exists"),
            Some(10003) => Some("That channel no longer exists"),
            Some(40032) => Some("They are not in a voice channel"),
            Some(30005) => Some("This server has reached the maximum number of roles (250)"),
            Some(30013) => Some("This server has reached the maximum number of channels (500)"),
            Some(50028) => Some("That role is managed by an integration"),
            _ => None,
        };
        match known {
            Some(text) => format!("{text}."),
            None if *status == reqwest::StatusCode::FORBIDDEN => {
                format!("Discord refused: {message}")
            }
            None => message.clone(),
        }
    }

    pub fn retry_after(&self) -> Option<f64> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

pub type ApiError = Error;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_permissions_reads_as_a_sentence() {
        let error = Error::Api {
            status: reqwest::StatusCode::FORBIDDEN,
            code: Some(50013),
            message: "Missing Permissions".into(),
            retry_after: None,
        };
        assert!(error.describe().starts_with("You don't have permission"));
        let other = Error::Api {
            status: reqwest::StatusCode::FORBIDDEN,
            code: Some(1),
            message: "Nope".into(),
            retry_after: None,
        };
        assert_eq!(other.describe(), "Discord refused: Nope");
    }
}
