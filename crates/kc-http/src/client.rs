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

/// Everything the composer can put in one message.
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub content: String,
    pub reply_to: Option<Snowflake>,
    /// Whether a reply pings the author of the message replied to.
    pub mention_reply: bool,
    /// Attachment descriptors returned by [`HttpClient::upload_attachment`].
    pub attachments: Vec<Value>,
    pub sticker_ids: Vec<Snowflake>,
    /// Client-chosen id echoed back on the gateway, to match a pending row.
    pub nonce: Option<String>,
    /// `@silent`: deliver without a notification.
    pub silent: bool,
}

impl Default for Outgoing {
    fn default() -> Self {
        Self {
            content: String::new(),
            reply_to: None,
            mention_reply: true,
            attachments: Vec::new(),
            sticker_ids: Vec::new(),
            nonce: None,
            silent: false,
        }
    }
}

/// One page of message search results.
#[derive(Clone, Debug, Default)]
pub struct SearchPage {
    pub total: u64,
    pub messages: Vec<Message>,
    /// Discord is still indexing this guild; retry shortly.
    pub indexing: bool,
}

/// A GIF from Discord's picker endpoints.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct Gif {
    #[serde(default)]
    pub id: String,
    /// The provider page URL; this is what gets sent as the message.
    #[serde(default)]
    pub url: String,
    /// Media URL in the requested format.
    #[serde(default)]
    pub src: String,
    #[serde(default)]
    pub gif_src: String,
    /// Still preview image.
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct GifCategory {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub src: String,
}

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
        self.request_json_in(method, path, body, None).await
    }

    /// Like [`request_json`](Self::request_json), with the
    /// `X-Context-Properties` header the web client attaches to actions taken
    /// from a particular place in its interface (`{"location": "Friends"}`).
    async fn request_json_in(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        context: Option<&Value>,
    ) -> Result<Value> {
        let route = normalise_route(method.as_str(), path);

        for attempt in 0..MAX_ATTEMPTS {
            self.limiter.acquire(&route).await;

            let mut request = self.request(method.clone(), path);
            if let Some(body) = body {
                request = request.json(body);
            }
            if let Some(context) = context {
                request = request.header("x-context-properties", base64_json(context));
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

    /// A GET of an API path (`/channels/1/messages?limit=5`) returning the
    /// raw JSON, for diagnostics.
    pub async fn get_raw(&self, path: &str) -> Result<Value> {
        self.get(path).await
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

    /// Opens (or reopens) the DM with a user. Discord limits how many new
    /// DMs an account may start, so callers should prefer an existing channel.
    pub async fn create_dm(&self, recipient: Snowflake) -> Result<Channel> {
        let value = self
            .request_json_in(
                Method::POST,
                "/users/@me/channels",
                Some(&json!({ "recipients": [recipient.to_string()] })),
                Some(&json!({})),
            )
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    /// Closes a DM (it can be reopened) or leaves a group DM.
    pub async fn close_dm(&self, channel_id: Snowflake) -> Result<()> {
        self.delete(&format!("/channels/{channel_id}?silent=false"))
            .await?;
        Ok(())
    }

    // -- calls -----------------------------------------------------------

    /// Rings a DM or group DM so its other members get an incoming call.
    pub async fn ring_call(&self, channel_id: Snowflake) -> Result<()> {
        self.post(&format!("/channels/{channel_id}/call/ring"), json!({}))
            .await?;
        Ok(())
    }

    /// Stops ringing a DM call (hanging up before anyone answers).
    pub async fn stop_ringing(&self, channel_id: Snowflake) -> Result<()> {
        self.post(
            &format!("/channels/{channel_id}/call/stop-ringing"),
            json!({ "recipients": null }),
        )
        .await?;
        Ok(())
    }

    // -- members ---------------------------------------------------------

    /// Sets a guild nickname. `me` picks the `@me` route (always allowed);
    /// otherwise the member route, which needs Manage Nicknames.
    pub async fn set_nickname(
        &self,
        guild_id: Snowflake,
        user_id: Snowflake,
        me: bool,
        nick: &str,
    ) -> Result<()> {
        let path = if me {
            format!("/guilds/{guild_id}/members/@me")
        } else {
            format!("/guilds/{guild_id}/members/{user_id}")
        };
        self.patch(&path, json!({ "nick": nick })).await?;
        Ok(())
    }

    // -- guild management ------------------------------------------------

    /// Creates a channel, or a category when `kind` is `GuildCategory` (4).
    pub async fn create_channel(
        &self,
        guild_id: Snowflake,
        name: &str,
        kind: u8,
        parent_id: Option<Snowflake>,
    ) -> Result<Channel> {
        let mut body = json!({ "name": name, "type": kind });
        if let Some(parent) = parent_id {
            body["parent_id"] = json!(parent.to_string());
        }
        let value = self
            .request_json_in(
                Method::POST,
                &format!("/guilds/{guild_id}/channels"),
                Some(&body),
                Some(&json!({})),
            )
            .await?;
        Self::parse(value, StatusCode::CREATED)
    }

    /// Creates an invite to a channel and returns its code.
    pub async fn create_invite(&self, channel_id: Snowflake) -> Result<String> {
        let value = self
            .request_json_in(
                Method::POST,
                &format!("/channels/{channel_id}/invites"),
                Some(&json!({ "max_age": 86_400, "max_uses": 0, "unique": false })),
                Some(&json!({})),
            )
            .await?;
        Ok(value
            .get("code")
            .and_then(|code| code.as_str())
            .unwrap_or_default()
            .to_owned())
    }

    /// Leaves a guild.
    pub async fn leave_guild(&self, guild_id: Snowflake) -> Result<()> {
        self.delete(&format!("/users/@me/guilds/{guild_id}"))
            .await?;
        Ok(())
    }

    /// Edits a guild (e.g. rename). Needs Manage Server.
    pub async fn edit_guild(&self, guild_id: Snowflake, payload: Value) -> Result<Guild> {
        let value = self.patch(&format!("/guilds/{guild_id}"), payload).await?;
        Self::parse(value, StatusCode::OK)
    }

    /// Overrides the account's per-guild settings, e.g. `{"muted": true}`.
    pub async fn patch_guild_settings(&self, guild_id: Snowflake, payload: Value) -> Result<()> {
        self.patch(&format!("/users/@me/guild-settings/{guild_id}"), payload)
            .await?;
        Ok(())
    }

    // -- relationships ---------------------------------------------------

    /// Sends a friend request by unique username.
    pub async fn add_friend(&self, username: &str) -> Result<()> {
        self.request_json_in(
            Method::POST,
            "/users/@me/relationships",
            Some(&json!({ "username": username, "discriminator": null })),
            Some(&json!({ "location": "Add Friend" })),
        )
        .await?;
        Ok(())
    }

    /// Accepts an incoming request or sends one to a known user.
    pub async fn accept_friend(&self, user_id: Snowflake) -> Result<()> {
        self.request_json_in(
            Method::PUT,
            &format!("/users/@me/relationships/{user_id}"),
            Some(&json!({})),
            Some(&json!({ "location": "Friends" })),
        )
        .await?;
        Ok(())
    }

    pub async fn block_user(&self, user_id: Snowflake) -> Result<()> {
        self.request_json_in(
            Method::PUT,
            &format!("/users/@me/relationships/{user_id}"),
            Some(&json!({ "type": 2 })),
            Some(&json!({ "location": "ContextMenu" })),
        )
        .await?;
        Ok(())
    }

    /// Sets the private nickname shown for a friend; empty clears it.
    pub async fn set_friend_nickname(&self, user_id: Snowflake, nickname: &str) -> Result<()> {
        let nickname = (!nickname.is_empty()).then_some(nickname);
        self.patch(
            &format!("/users/@me/relationships/{user_id}"),
            json!({ "nickname": nickname }),
        )
        .await?;
        Ok(())
    }

    /// Removes a friend, declines or cancels a request, or unblocks.
    pub async fn remove_relationship(&self, user_id: Snowflake) -> Result<()> {
        self.request_json_in(
            Method::DELETE,
            &format!("/users/@me/relationships/{user_id}"),
            None,
            Some(&json!({ "location": "Friends" })),
        )
        .await?;
        Ok(())
    }

    // -- users -----------------------------------------------------------

    /// A user's profile: bio, pronouns, badges, mutual guilds and friends.
    pub async fn user_profile(
        &self,
        user_id: Snowflake,
        guild_id: Option<Snowflake>,
    ) -> Result<Value> {
        let mut path = format!(
            "/users/{user_id}/profile?type=popout&with_mutual_guilds=true&with_mutual_friends=true&with_mutual_friends_count=false"
        );
        if let Some(guild_id) = guild_id {
            path.push_str(&format!("&guild_id={guild_id}"));
        }
        self.get(&path).await
    }

    pub async fn set_note(&self, user_id: Snowflake, note: &str) -> Result<()> {
        self.request_json(
            Method::PUT,
            &format!("/users/@me/notes/{user_id}"),
            Some(&json!({ "note": note })),
        )
        .await?;
        Ok(())
    }

    /// Sets the account's display (global) name.
    pub async fn set_global_name(&self, name: &str) -> Result<()> {
        self.patch("/users/@me", json!({ "global_name": name }))
            .await?;
        Ok(())
    }

    /// Sets the account's bio and pronouns (the user profile).
    pub async fn set_profile(&self, bio: &str, pronouns: &str) -> Result<()> {
        self.patch(
            "/users/@me/profile",
            json!({ "bio": bio, "pronouns": pronouns }),
        )
        .await?;
        Ok(())
    }

    /// Persists the account's status (`online`, `idle`, `dnd`, `invisible`)
    /// so other sessions and the next login use it.
    pub async fn set_status(&self, status: &str) -> Result<()> {
        self.patch("/users/@me/settings", json!({ "status": status }))
            .await?;
        Ok(())
    }

    /// Sets or clears (`None`) the custom status. `emoji` is a unicode emoji.
    pub async fn set_custom_status(&self, text: Option<&str>, emoji: Option<&str>) -> Result<()> {
        let custom_status = match (text, emoji) {
            (None, None) => Value::Null,
            _ => json!({ "text": text, "emoji_name": emoji, "emoji_id": null, "expires_at": null }),
        };
        self.patch(
            "/users/@me/settings",
            json!({ "custom_status": custom_status }),
        )
        .await?;
        Ok(())
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

    /// Messages after an id, oldest first as Discord returns newest first.
    pub async fn messages_after(
        &self,
        channel_id: Snowflake,
        limit: u8,
        after: Snowflake,
    ) -> Result<Vec<Message>> {
        let limit = limit.clamp(1, 100);
        let value = self
            .get(&format!(
                "/channels/{channel_id}/messages?limit={limit}&after={after}"
            ))
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    /// A window of messages centred on an id (used to jump to a message).
    pub async fn messages_around(
        &self,
        channel_id: Snowflake,
        limit: u8,
        around: Snowflake,
    ) -> Result<Vec<Message>> {
        let limit = limit.clamp(1, 100);
        let value = self
            .get(&format!(
                "/channels/{channel_id}/messages?limit={limit}&around={around}"
            ))
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn send_message(
        &self,
        channel_id: Snowflake,
        content: &str,
        reply_to: Option<Snowflake>,
    ) -> Result<Message> {
        self.send(
            channel_id,
            &Outgoing {
                content: content.to_owned(),
                reply_to,
                ..Outgoing::default()
            },
        )
        .await
    }

    /// Sends a message with every option the composer supports.
    pub async fn send(&self, channel_id: Snowflake, message: &Outgoing) -> Result<Message> {
        let nonce = message.nonce.clone().unwrap_or_else(|| nonce().to_string());
        let mut body = json!({
            "content": message.content,
            "nonce": nonce,
            "tts": false,
            "flags": if message.silent { 1 << 12 } else { 0 },
        });
        if let Some(reply_to) = message.reply_to {
            body["message_reference"] = json!({
                "message_id": reply_to,
                "channel_id": channel_id,
            });
            body["allowed_mentions"] = json!({
                "parse": ["users", "roles", "everyone"],
                "replied_user": message.mention_reply,
            });
        }
        if !message.attachments.is_empty() {
            body["attachments"] = Value::Array(message.attachments.clone());
        }
        if !message.sticker_ids.is_empty() {
            body["sticker_ids"] = json!(message.sticker_ids);
        }
        let value = self
            .post(&format!("/channels/{channel_id}/messages"), body)
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    // -- pins and search -------------------------------------------------

    pub async fn pins(&self, channel_id: Snowflake) -> Result<Vec<Message>> {
        let value = self.get(&format!("/channels/{channel_id}/pins")).await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn pin_message(&self, channel_id: Snowflake, message_id: Snowflake) -> Result<()> {
        self.put_empty(&format!("/channels/{channel_id}/pins/{message_id}"))
            .await
    }

    pub async fn unpin_message(&self, channel_id: Snowflake, message_id: Snowflake) -> Result<()> {
        self.delete(&format!("/channels/{channel_id}/pins/{message_id}"))
            .await?;
        Ok(())
    }

    /// Full-text search in a guild (optionally one channel) or in a DM.
    /// Returns the total hit count and one page of 25 hits, newest first.
    /// Discord answers 202 while it is still indexing; that surfaces as an
    /// empty page with `indexing` set.
    pub async fn search(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        query: &str,
        offset: u32,
    ) -> Result<SearchPage> {
        let mut path = match guild_id {
            Some(guild_id) => {
                format!("/guilds/{guild_id}/messages/search?channel_id={channel_id}&")
            }
            None => format!("/channels/{channel_id}/messages/search?"),
        };
        path.push_str(&format!("content={}", percent_encode(query)));
        if offset > 0 {
            path.push_str(&format!("&offset={offset}"));
        }
        let value = self.get(&path).await?;
        let Some(groups) = value["messages"].as_array() else {
            return Ok(SearchPage {
                total: 0,
                messages: Vec::new(),
                indexing: value.get("retry_after").is_some() || value["code"] == 110000,
            });
        };
        let messages = groups
            .iter()
            .filter_map(|group| {
                group
                    .as_array()?
                    .iter()
                    .find(|hit| hit["hit"] == true)
                    .or(group.get(0))
            })
            .filter_map(|message| serde_json::from_value::<Message>(message.clone()).ok())
            .collect();
        Ok(SearchPage {
            total: value["total_results"].as_u64().unwrap_or(0),
            messages,
            indexing: false,
        })
    }

    // -- read states -----------------------------------------------------

    /// Marks several channels read at once (used for "mark server as read").
    pub async fn ack_bulk(&self, read_states: &[(Snowflake, Snowflake)]) -> Result<()> {
        let entries: Vec<Value> = read_states
            .iter()
            .map(|(channel_id, message_id)| {
                json!({ "channel_id": channel_id, "message_id": message_id, "read_state_type": 0 })
            })
            .collect();
        self.post("/read-states/ack-bulk", json!({ "read_states": entries }))
            .await?;
        Ok(())
    }

    // -- gifs ------------------------------------------------------------

    /// GIF categories shown when the picker opens.
    pub async fn gif_categories(&self) -> Result<Vec<GifCategory>> {
        let value = self
            .get("/gifs/trending?provider=tenor&locale=en-US&media_format=tinygif")
            .await?;
        Ok(serde_json::from_value(value["categories"].clone()).unwrap_or_default())
    }

    pub async fn gif_trending(&self) -> Result<Vec<Gif>> {
        let value = self
            .get("/gifs/trending-gifs?provider=tenor&locale=en-US&media_format=tinygif")
            .await?;
        Ok(serde_json::from_value(value).unwrap_or_default())
    }

    pub async fn gif_search(&self, query: &str) -> Result<Vec<Gif>> {
        let value = self
            .get(&format!(
                "/gifs/search?q={}&provider=tenor&locale=en-US&media_format=tinygif&limit=50",
                percent_encode(query)
            ))
            .await?;
        Ok(serde_json::from_value(value).unwrap_or_default())
    }

    /// Users who reacted with an emoji (first 25).
    pub async fn reactors(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &str,
    ) -> Result<Vec<User>> {
        let value = self
            .get(&format!(
                "/channels/{channel_id}/messages/{message_id}/reactions/{}?limit=25&type=0",
                encode_emoji(emoji)
            ))
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
        self.send(
            channel_id,
            &Outgoing {
                content: content.to_owned(),
                reply_to,
                attachments,
                ..Outgoing::default()
            },
        )
        .await
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
/// Percent-encodes a query-string value.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn base64_json(value: &Value) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(value.to_string())
}

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
        assert!(headers["user-agent"].to_str().unwrap().contains("Chrome/"));
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
