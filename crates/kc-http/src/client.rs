use kc_core::{Snowflake, Token};
use kc_state::{Channel, Guild, Message, User};
use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tracing::debug;

use crate::API_BASE;
use crate::error::{ApiErrorBody, Error, Result};
use crate::rate_limit::{RateLimiter, normalise_route};

const USER_AGENT: &str = kc_core::super_properties::USER_AGENT;
const MAX_ATTEMPTS: usize = 5;

/// A REST client authenticated as one user account.
#[derive(Clone)]
pub struct HttpClient {
    http: reqwest::Client,
    token: Token,
    base: String,
    super_properties: String,
    locale: String,
    timezone: String,
    limiter: RateLimiter,
}

impl HttpClient {
    pub fn new(token: Token) -> Self {
        Self::with_base(token, API_BASE)
    }

    pub fn with_base(token: Token, base: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .https_only(true)
            .build()
            .expect("reqwest client");

        Self {
            http,
            token,
            base: base.into().trim_end_matches('/').to_owned(),
            super_properties: kc_core::super_properties::SuperProperties::new().as_header(),
            locale: "en-US".to_owned(),
            timezone: std::env::var("TZ").unwrap_or_else(|_| "UTC".to_owned()),
            limiter: RateLimiter::default(),
        }
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base, path))
            .header("authorization", self.token.expose())
            .header("user-agent", USER_AGENT)
            .header("accept", "*/*")
            .header("accept-language", "en-US")
            .header("priority", "u=1, i")
            .header("referer", "https://discord.com/channels/@me")
            .header(
                "sec-ch-ua",
                "\"Not:A-Brand\";v=\"24\", \"Chromium\";v=\"134\"",
            )
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Windows\"")
            .header("sec-fetch-dest", "empty")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-site", "same-origin")
            .header("x-discord-locale", &self.locale)
            .header("x-discord-timezone", &self.timezone)
            .header("x-super-properties", &self.super_properties)
            .header("origin", "https://discord.com")
            .header("x-debug-options", "bugReporterEnabled")
    }

    async fn request_json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let route = normalise_route(method.as_str(), path);

        for attempt in 0..MAX_ATTEMPTS {
            self.limiter.acquire(&route).await;

            let mut request = self.request(method.clone(), path);
            if let Some(body) = body {
                request = request.json(body);
            }

            let response = request.send().await?;
            let status = response.status();
            let headers = response.headers().clone();
            self.limiter.update(&route, &headers);

            if status == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = parse_retry_after(&headers, response).await;
                if headers
                    .get("x-ratelimit-global")
                    .and_then(|value| value.to_str().ok())
                    == Some("true")
                {
                    self.limiter.set_global(retry_after);
                }
                debug!(route, retry_after, attempt, "rate limited");
                if attempt + 1 == MAX_ATTEMPTS {
                    return Err(Error::Api {
                        status,
                        code: None,
                        message: "rate limited".to_owned(),
                        retry_after: Some(retry_after),
                    });
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(
                    (retry_after + 0.05).min(60.0),
                ))
                .await;
                continue;
            }

            if !status.is_success() {
                let bytes = response.bytes().await.unwrap_or_default();
                let parsed = serde_json::from_slice::<ApiErrorBody>(&bytes).ok();
                return Err(Error::Api {
                    status,
                    code: parsed.as_ref().and_then(|body| body.code),
                    message: parsed
                        .as_ref()
                        .map(|body| body.message.clone())
                        .filter(|message| !message.is_empty())
                        .unwrap_or_else(|| status.to_string()),
                    retry_after: parsed.as_ref().and_then(|body| body.retry_after),
                });
            }

            if status == StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }

            let bytes = response.bytes().await?;
            if bytes.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_slice(&bytes)
                .map_err(|source| Error::Decode { status, source });
        }

        unreachable!("rate-limit loop returns")
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.request_json(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.request_json(Method::POST, path, Some(&body)).await
    }

    async fn patch(&self, path: &str, body: Value) -> Result<Value> {
        self.request_json(Method::PATCH, path, Some(&body)).await
    }

    async fn delete(&self, path: &str) -> Result<Value> {
        self.request_json(Method::DELETE, path, None).await
    }

    fn parse<T: serde::de::DeserializeOwned>(value: Value, status: StatusCode) -> Result<T> {
        serde_json::from_value(value).map_err(|source| Error::Decode { status, source })
    }

    // -- identity --------------------------------------------------------

    pub async fn current_user(&self) -> Result<User> {
        let value = self.get("/users/@me").await?;
        Self::parse(value, StatusCode::OK)
    }

    /// Partial guilds (id, name, icon, owner, permissions) in sidebar order.
    pub async fn current_user_guilds(&self) -> Result<Vec<Guild>> {
        let value = self.get("/users/@me/guilds").await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn gateway_url(&self) -> Result<String> {
        let value = self.get("/gateway").await?;
        value["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::Api {
                status: StatusCode::OK,
                code: None,
                message: "gateway response has no url".to_owned(),
                retry_after: None,
            })
    }

    // -- guilds and channels --------------------------------------------

    pub async fn guild(&self, guild_id: Snowflake) -> Result<Guild> {
        let value = self
            .get(&format!("/guilds/{guild_id}?with_counts=true"))
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn guild_channels(&self, guild_id: Snowflake) -> Result<Vec<Channel>> {
        let value = self.get(&format!("/guilds/{guild_id}/channels")).await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn channel(&self, channel_id: Snowflake) -> Result<Channel> {
        let value = self.get(&format!("/channels/{channel_id}")).await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn create_dm(&self, recipient: Snowflake) -> Result<Channel> {
        let value = self
            .post(
                "/users/@me/channels",
                json!({ "recipients": [recipient.to_string()] }),
            )
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    // -- messages --------------------------------------------------------

    pub async fn messages(
        &self,
        channel_id: Snowflake,
        limit: u8,
        before: Option<Snowflake>,
    ) -> Result<Vec<Message>> {
        let limit = limit.clamp(1, 100);
        let mut path = format!("/channels/{channel_id}/messages?limit={limit}");
        if let Some(before) = before {
            path.push_str(&format!("&before={before}"));
        }
        let value = self.get(&path).await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn send_message(
        &self,
        channel_id: Snowflake,
        content: &str,
        reply_to: Option<Snowflake>,
    ) -> Result<Message> {
        let mut body = json!({ "content": content, "nonce": Snowflake::new(nonce()) });
        if let Some(reply_to) = reply_to {
            body["message_reference"] = json!({
                "message_id": reply_to,
                "channel_id": channel_id,
            });
        }
        let value = self
            .post(&format!("/channels/{channel_id}/messages"), body)
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn edit_message(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        content: &str,
    ) -> Result<Message> {
        let value = self
            .patch(
                &format!("/channels/{channel_id}/messages/{message_id}"),
                json!({ "content": content }),
            )
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn delete_message(&self, channel_id: Snowflake, message_id: Snowflake) -> Result<()> {
        self.delete(&format!("/channels/{channel_id}/messages/{message_id}"))
            .await?;
        Ok(())
    }

    pub async fn send_typing(&self, channel_id: Snowflake) -> Result<()> {
        self.post(&format!("/channels/{channel_id}/typing"), json!({}))
            .await?;
        Ok(())
    }

    pub async fn ack_message(&self, channel_id: Snowflake, message_id: Snowflake) -> Result<()> {
        self.post(
            &format!("/channels/{channel_id}/messages/{message_id}/ack"),
            json!({ "token": null }),
        )
        .await?;
        Ok(())
    }

    // -- reactions -------------------------------------------------------

    pub async fn add_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &str,
    ) -> Result<()> {
        self.put_empty(&format!(
            "/channels/{channel_id}/messages/{message_id}/reactions/{}/@me",
            encode_emoji(emoji)
        ))
        .await
    }

    pub async fn remove_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &str,
    ) -> Result<()> {
        self.delete(&format!(
            "/channels/{channel_id}/messages/{message_id}/reactions/{}/@me",
            encode_emoji(emoji)
        ))
        .await?;
        Ok(())
    }

    async fn put_empty(&self, path: &str) -> Result<()> {
        let route = normalise_route("PUT", path);
        self.limiter.acquire(&route).await;
        let response = self.request(Method::PUT, path).send().await?;
        self.limiter.update(&route, response.headers());
        let status = response.status();
        if response.status().is_success() {
            Ok(())
        } else {
            let bytes = response.bytes().await.unwrap_or_default();
            let parsed = serde_json::from_slice::<ApiErrorBody>(&bytes).ok();
            Err(Error::Api {
                status,
                code: parsed.as_ref().and_then(|body| body.code),
                message: parsed
                    .as_ref()
                    .map(|body| body.message.clone())
                    .unwrap_or_else(|| status.to_string()),
                retry_after: parsed.as_ref().and_then(|body| body.retry_after),
            })
        }
    }

    // -- attachments -----------------------------------------------------

    /// Uploads one file and returns the attachment id/handle the message send
    /// expects. Discord uses a three-step flow: reserve, upload, send.
    pub async fn upload_attachment(
        &self,
        channel_id: Snowflake,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<Value> {
        let size = bytes.len();
        let reserved = self
            .post(
                &format!("/channels/{channel_id}/attachments"),
                json!({
                    "files": [{
                        "id": "0",
                        "filename": filename,
                        "file_size": size,
                    }]
                }),
            )
            .await?;

        let upload = reserved["attachments"]
            .get(0)
            .cloned()
            .ok_or_else(|| Error::Api {
                status: StatusCode::OK,
                code: None,
                message: "attachment reservation returned no attachments".to_owned(),
                retry_after: None,
            })?;

        let upload_url = upload["upload_url"].as_str().ok_or_else(|| Error::Api {
            status: StatusCode::OK,
            code: None,
            message: "attachment reservation has no upload_url".to_owned(),
            retry_after: None,
        })?;

        let response = self
            .http
            .put(upload_url)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::Api {
                status: response.status(),
                code: None,
                message: "attachment upload failed".to_owned(),
                retry_after: None,
            });
        }

        Ok(upload)
    }

    pub async fn send_message_with_attachments(
        &self,
        channel_id: Snowflake,
        content: &str,
        attachments: Vec<Value>,
        reply_to: Option<Snowflake>,
    ) -> Result<Message> {
        let mut body = json!({
            "content": content,
            "nonce": Snowflake::new(nonce()),
            "attachments": attachments,
        });
        if let Some(reply_to) = reply_to {
            body["message_reference"] = json!({
                "message_id": reply_to,
                "channel_id": channel_id,
            });
        }
        let value = self
            .post(&format!("/channels/{channel_id}/messages"), body)
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    pub fn token(&self) -> &Token {
        &self.token
    }

    #[cfg(test)]
    fn build_request(&self, method: Method, path: &str) -> reqwest::Request {
        self.request(method, path).build().expect("request builds")
    }
}

async fn parse_retry_after(headers: &HeaderMap, response: reqwest::Response) -> f64 {
    let from_header = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok());
    if let Some(value) = from_header {
        return value;
    }
    response
        .json::<ApiErrorBody>()
        .await
        .ok()
        .and_then(|body| body.retry_after)
        .unwrap_or(1.0)
}

/// Discord's snowflake epoch is 1420070400000 ms; a locally unique nonce only
/// needs to be recent and unique, so reuse it with a counter.
fn nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    (millis << 12) | (COUNTER.fetch_add(1, Ordering::Relaxed) & 0xfff)
}

/// `name:id` for custom emoji, percent-encoded UTF-8 for unicode emoji.
fn encode_emoji(emoji: &str) -> String {
    let is_custom = emoji
        .rsplit_once(':')
        .map(|(_, id)| id.chars().all(|ch| ch.is_ascii_digit()))
        .unwrap_or(false);
    if is_custom {
        return emoji.to_owned();
    }
    let mut out = String::with_capacity(emoji.len() * 3);
    for byte in emoji.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_carries_user_headers() {
        let client = HttpClient::new(Token::new("test-token"));
        let request = client.build_request(Method::GET, "/users/@me");
        let headers = request.headers();
        assert_eq!(headers["authorization"], "test-token");
        assert!(headers.contains_key("x-super-properties"));
        assert_eq!(headers["x-discord-locale"], "en-US");
        assert_eq!(headers["origin"], "https://discord.com");
        assert!(headers["user-agent"].to_str().unwrap().contains("discord/"));
    }

    #[test]
    fn emoji_encoding() {
        assert_eq!(encode_emoji("👍"), "%F0%9F%91%8D");
        assert_eq!(
            encode_emoji("name:123456789012345678"),
            "name:123456789012345678"
        );
    }

    #[test]
    fn attachment_flow_builds() {
        let client = HttpClient::new(Token::new("test-token"));
        let request = client.build_request(Method::POST, "/channels/10/attachments");
        assert_eq!(request.method(), Method::POST);
    }
}
