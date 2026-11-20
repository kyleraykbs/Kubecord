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
        browser_headers(
            self.http.request(method, format!("{}{}", self.base, path)),
            "https://discord.com/channels/@me",
            &self.locale,
            &self.timezone,
            &self.super_properties,
        )
        .header("authorization", self.token.expose())
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
        self.request_json_full(method, path, body, context, None)
            .await
    }

    /// A moderation request: `reason` (if any) goes to the audit log
    /// through the `X-Audit-Log-Reason` header.
    async fn request_json_reason(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        reason: Option<&str>,
    ) -> Result<Value> {
        self.send_with_retries(method, path, body, None, None, reason)
            .await
    }

    /// A request that may carry a solved captcha, for retrying one that
    /// Discord answered with [`Error::Captcha`].
    async fn request_json_full(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        context: Option<&Value>,
        captcha: Option<&CaptchaAnswer>,
    ) -> Result<Value> {
        self.send_with_retries(method, path, body, context, captcha, None)
            .await
    }

    async fn send_with_retries(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        context: Option<&Value>,
        captcha: Option<&CaptchaAnswer>,
        reason: Option<&str>,
    ) -> Result<Value> {
        let route = normalise_route(method.as_str(), path);
        let reason = reason.and_then(audit_log_reason);

        for attempt in 0..MAX_ATTEMPTS {
            self.limiter.acquire(&route).await;

            let mut request = self.request(method.clone(), path);
            if let Some(body) = body {
                request = request.json(body);
            }
            if let Some(context) = context {
                request = request.header("x-context-properties", base64_json(context));
            }
            if let Some(reason) = &reason {
                request = request.header("x-audit-log-reason", reason);
            }
            if let Some(captcha) = captcha {
                request = request.header("x-captcha-key", &captcha.key);
                if let Some(rqtoken) = &captcha.rqtoken {
                    request = request.header("x-captcha-rqtoken", rqtoken);
                }
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
                return Err(error_from_body(status, &bytes));
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

    /// Stops a DM call ringing: for `recipients` (declining is stopping it
    /// for yourself), or for everyone when `None`.
    pub async fn stop_ringing(
        &self,
        channel_id: Snowflake,
        recipients: Option<&[Snowflake]>,
    ) -> Result<()> {
        let recipients = recipients.map(|recipients| {
            recipients
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        });
        self.post(
            &format!("/channels/{channel_id}/call/stop-ringing"),
            json!({ "recipients": recipients }),
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

    /// Creates a one-day invite to a channel and returns its code.
    pub async fn create_invite(&self, channel_id: Snowflake) -> Result<String> {
        self.create_invite_with(channel_id, 86_400, 0).await
    }

    /// Creates an invite with a chosen lifetime (seconds, 0 = never) and
    /// use limit (0 = unlimited) and returns its code.
    pub async fn create_invite_with(
        &self,
        channel_id: Snowflake,
        max_age: u32,
        max_uses: u32,
    ) -> Result<String> {
        let value = self
            .request_json_in(
                Method::POST,
                &format!("/channels/{channel_id}/invites"),
                Some(&invite_body(max_age, max_uses)),
                Some(&json!({})),
            )
            .await?;
        Ok(value["code"].as_str().unwrap_or_default().to_owned())
    }

    /// Every invite of a guild (needs Manage Server).
    pub async fn guild_invites(&self, guild_id: Snowflake) -> Result<Vec<Invite>> {
        let value = self.get(&format!("/guilds/{guild_id}/invites")).await?;
        Self::parse(value, StatusCode::OK)
    }

    /// Revokes an invite.
    pub async fn delete_invite(&self, code: &str, reason: Option<&str>) -> Result<()> {
        self.request_json_reason(
            Method::DELETE,
            &format!("/invites/{}", percent_encode(code)),
            None,
            reason,
        )
        .await?;
        Ok(())
    }

    // -- moderation ------------------------------------------------------

    /// Removes a member from a guild (they may join again).
    pub async fn kick_member(
        &self,
        guild_id: Snowflake,
        user_id: Snowflake,
        reason: Option<&str>,
    ) -> Result<()> {
        self.request_json_reason(
            Method::DELETE,
            &format!("/guilds/{guild_id}/members/{user_id}"),
            None,
            reason,
        )
        .await?;
        Ok(())
    }

    /// Bans a user, deleting their messages of the last
    /// `delete_message_seconds` (0 to 604800).
    pub async fn ban_member(
        &self,
        guild_id: Snowflake,
        user_id: Snowflake,
        delete_message_seconds: u32,
        reason: Option<&str>,
    ) -> Result<()> {
        self.request_json_reason(
            Method::PUT,
            &format!("/guilds/{guild_id}/bans/{user_id}"),
            Some(&ban_body(delete_message_seconds)),
            reason,
        )
        .await?;
        Ok(())
    }

    pub async fn unban_member(
        &self,
        guild_id: Snowflake,
        user_id: Snowflake,
        reason: Option<&str>,
    ) -> Result<()> {
        self.request_json_reason(
            Method::DELETE,
            &format!("/guilds/{guild_id}/bans/{user_id}"),
            None,
            reason,
        )
        .await?;
        Ok(())
    }

    /// A guild's bans (needs Ban Members), up to 1000.
    pub async fn bans(&self, guild_id: Snowflake) -> Result<Vec<Ban>> {
        let value = self
            .get(&format!("/guilds/{guild_id}/bans?limit=1000"))
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    /// Changes one thing about a member: timeout, voice channel, server
    /// mute or deafen, nickname, or their whole role list.
    pub async fn edit_member(
        &self,
        guild_id: Snowflake,
        user_id: Snowflake,
        edit: &MemberEdit,
        reason: Option<&str>,
    ) -> Result<()> {
        self.request_json_reason(
            Method::PATCH,
            &format!("/guilds/{guild_id}/members/{user_id}"),
            Some(&edit.body()),
            reason,
        )
        .await?;
        Ok(())
    }

    /// Gives a member one role.
    pub async fn add_member_role(
        &self,
        guild_id: Snowflake,
        user_id: Snowflake,
        role_id: Snowflake,
        reason: Option<&str>,
    ) -> Result<()> {
        self.request_json_reason(
            Method::PUT,
            &format!("/guilds/{guild_id}/members/{user_id}/roles/{role_id}"),
            None,
            reason,
        )
        .await?;
        Ok(())
    }

    /// Takes one role off a member.
    pub async fn remove_member_role(
        &self,
        guild_id: Snowflake,
        user_id: Snowflake,
        role_id: Snowflake,
        reason: Option<&str>,
    ) -> Result<()> {
        self.request_json_reason(
            Method::DELETE,
            &format!("/guilds/{guild_id}/members/{user_id}/roles/{role_id}"),
            None,
            reason,
        )
        .await?;
        Ok(())
    }

    /// The most recent audit log entries (needs View Audit Log), with the
    /// users they refer to.
    pub async fn audit_log(&self, guild_id: Snowflake, limit: u8) -> Result<Value> {
        let limit = limit.clamp(1, 100);
        self.get(&format!("/guilds/{guild_id}/audit-logs?limit={limit}"))
            .await
    }

    // -- roles -----------------------------------------------------------

    pub async fn create_role(
        &self,
        guild_id: Snowflake,
        edit: &RoleEdit,
    ) -> Result<kc_state::Role> {
        let value = self
            .request_json_reason(
                Method::POST,
                &format!("/guilds/{guild_id}/roles"),
                Some(&edit.body()),
                None,
            )
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn edit_role(
        &self,
        guild_id: Snowflake,
        role_id: Snowflake,
        edit: &RoleEdit,
    ) -> Result<kc_state::Role> {
        let value = self
            .request_json_reason(
                Method::PATCH,
                &format!("/guilds/{guild_id}/roles/{role_id}"),
                Some(&edit.body()),
                None,
            )
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn delete_role(&self, guild_id: Snowflake, role_id: Snowflake) -> Result<()> {
        self.request_json_reason(
            Method::DELETE,
            &format!("/guilds/{guild_id}/roles/{role_id}"),
            None,
            None,
        )
        .await?;
        Ok(())
    }

    /// Sets role positions: `(role, position)` pairs.
    pub async fn reorder_roles(
        &self,
        guild_id: Snowflake,
        positions: &[(Snowflake, i32)],
    ) -> Result<()> {
        self.request_json_reason(
            Method::PATCH,
            &format!("/guilds/{guild_id}/roles"),
            Some(&positions_body(positions)),
            None,
        )
        .await?;
        Ok(())
    }

    // -- channels --------------------------------------------------------

    pub async fn edit_channel(&self, channel_id: Snowflake, edit: &ChannelEdit) -> Result<Channel> {
        let value = self
            .request_json_reason(
                Method::PATCH,
                &format!("/channels/{channel_id}"),
                Some(&edit.body()),
                None,
            )
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    pub async fn delete_channel(&self, channel_id: Snowflake) -> Result<()> {
        self.request_json_reason(
            Method::DELETE,
            &format!("/channels/{channel_id}"),
            None,
            None,
        )
        .await?;
        Ok(())
    }

    /// Sets a channel's permission overwrite for a role (`kind` 0) or a
    /// member (1).
    pub async fn set_overwrite(
        &self,
        channel_id: Snowflake,
        target: Snowflake,
        kind: u8,
        allow: u64,
        deny: u64,
    ) -> Result<()> {
        self.request_json_reason(
            Method::PUT,
            &format!("/channels/{channel_id}/permissions/{target}"),
            Some(&overwrite_body(kind, allow, deny)),
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn delete_overwrite(&self, channel_id: Snowflake, target: Snowflake) -> Result<()> {
        self.request_json_reason(
            Method::DELETE,
            &format!("/channels/{channel_id}/permissions/{target}"),
            None,
            None,
        )
        .await?;
        Ok(())
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

    /// Changes the account's notification settings for a guild (`None`:
    /// the DM settings), e.g. `{"muted": true}` or the payloads from
    /// [`guild_mute_payload`] / [`channel_notification_payload`]. Discord
    /// answers with the guild's whole settings object (and a
    /// `USER_GUILD_SETTINGS_UPDATE`). Endpoint as documented by
    /// discord-userdoccers and used by Abaddon.
    pub async fn patch_guild_settings(
        &self,
        guild_id: Option<Snowflake>,
        payload: Value,
    ) -> Result<Value> {
        self.patch(&user_guild_settings_path(guild_id), payload)
            .await
    }

    // -- relationships ---------------------------------------------------

    /// Sends a friend request by unique username.
    pub async fn add_friend(&self, username: &str, captcha: Option<&CaptchaAnswer>) -> Result<()> {
        self.request_json_full(
            Method::POST,
            "/users/@me/relationships",
            Some(&json!({ "username": username, "discriminator": null })),
            Some(&json!({ "location": "Add Friend" })),
            captcha,
        )
        .await?;
        Ok(())
    }

    /// Accepts an incoming request or sends one to a known user.
    pub async fn accept_friend(
        &self,
        user_id: Snowflake,
        captcha: Option<&CaptchaAnswer>,
    ) -> Result<()> {
        self.request_json_full(
            Method::PUT,
            &format!("/users/@me/relationships/{user_id}"),
            Some(&json!({})),
            Some(&json!({ "location": "Friends" })),
            captcha,
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

/// The headers the web client's browser sends with every API request (all
/// but `authorization`). `referer` is the page the request comes from.
pub(crate) fn browser_headers(
    request: reqwest::RequestBuilder,
    referer: &str,
    locale: &str,
    timezone: &str,
    super_properties: &str,
) -> reqwest::RequestBuilder {
    request
        .header("user-agent", USER_AGENT)
        .header("accept", "*/*")
        .header("accept-language", "en-US")
        .header("priority", "u=1, i")
        .header("referer", referer)
        .header(
            "sec-ch-ua",
            "\"Not:A-Brand\";v=\"24\", \"Chromium\";v=\"134\"",
        )
        .header("sec-ch-ua-mobile", "?0")
        .header("sec-ch-ua-platform", "\"Windows\"")
        .header("sec-fetch-dest", "empty")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-site", "same-origin")
        .header("x-discord-locale", locale)
        .header("x-discord-timezone", timezone)
        .header("x-super-properties", super_properties)
        .header("origin", "https://discord.com")
        .header("x-debug-options", "bugReporterEnabled")
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

/// One change to a guild member (`PATCH /guilds/{guild}/members/{user}`).
#[derive(Clone, Debug, PartialEq)]
pub enum MemberEdit {
    /// `None` clears the nickname.
    Nick(Option<String>),
    /// Replaces the member's whole role list.
    Roles(Vec<Snowflake>),
    /// Times out until this ISO 8601 moment; `None` removes the timeout.
    Timeout(Option<String>),
    /// Moves to a voice channel; `None` disconnects.
    Move(Option<Snowflake>),
    Mute(bool),
    Deafen(bool),
}

impl MemberEdit {
    pub fn body(&self) -> Value {
        match self {
            Self::Nick(nick) => json!({ "nick": nick.as_deref().unwrap_or("") }),
            Self::Roles(roles) => {
                json!({ "roles": roles.iter().map(ToString::to_string).collect::<Vec<_>>() })
            }
            Self::Timeout(until) => json!({ "communication_disabled_until": until }),
            Self::Move(channel) => json!({ "channel_id": channel.map(|id| id.to_string()) }),
            Self::Mute(mute) => json!({ "mute": mute }),
            Self::Deafen(deaf) => json!({ "deaf": deaf }),
        }
    }
}

/// Fields to set on a role; `None` leaves a field alone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoleEdit {
    pub name: Option<String>,
    /// RGB; 0 is no colour.
    pub color: Option<u32>,
    pub hoist: Option<bool>,
    pub mentionable: Option<bool>,
    pub permissions: Option<u64>,
}

impl RoleEdit {
    pub fn body(&self) -> Value {
        let mut body = serde_json::Map::new();
        if let Some(name) = &self.name {
            body.insert("name".into(), json!(name));
        }
        if let Some(color) = self.color {
            body.insert("color".into(), json!(color));
        }
        if let Some(hoist) = self.hoist {
            body.insert("hoist".into(), json!(hoist));
        }
        if let Some(mentionable) = self.mentionable {
            body.insert("mentionable".into(), json!(mentionable));
        }
        if let Some(permissions) = self.permissions {
            // A bit field wider than JSON numbers: Discord takes a string.
            body.insert("permissions".into(), json!(permissions.to_string()));
        }
        Value::Object(body)
    }
}

/// Fields to set on a guild channel; `None` leaves a field alone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChannelEdit {
    pub name: Option<String>,
    /// Empty clears the topic.
    pub topic: Option<String>,
    /// Slowmode in seconds (0 to 21600).
    pub rate_limit_per_user: Option<u32>,
    pub nsfw: Option<bool>,
    pub bitrate: Option<u32>,
    /// 0 is no limit.
    pub user_limit: Option<u32>,
}

impl ChannelEdit {
    pub fn body(&self) -> Value {
        let mut body = serde_json::Map::new();
        if let Some(name) = &self.name {
            body.insert("name".into(), json!(name));
        }
        if let Some(topic) = &self.topic {
            body.insert("topic".into(), json!(topic));
        }
        if let Some(seconds) = self.rate_limit_per_user {
            body.insert("rate_limit_per_user".into(), json!(seconds));
        }
        if let Some(nsfw) = self.nsfw {
            body.insert("nsfw".into(), json!(nsfw));
        }
        if let Some(bitrate) = self.bitrate {
            body.insert("bitrate".into(), json!(bitrate));
        }
        if let Some(limit) = self.user_limit {
            body.insert("user_limit".into(), json!(limit));
        }
        Value::Object(body)
    }
}

/// A ban: who, and the reason given.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Ban {
    pub user: User,
    #[serde(default)]
    pub reason: Option<String>,
}

/// An invite of a guild, as `GET /guilds/{guild}/invites` lists it.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct Invite {
    pub code: String,
    #[serde(default)]
    pub inviter: Option<User>,
    #[serde(default)]
    pub channel: Option<InviteChannel>,
    #[serde(default)]
    pub uses: u32,
    /// 0 is unlimited.
    #[serde(default)]
    pub max_uses: u32,
    /// Seconds from creation; 0 never expires.
    #[serde(default)]
    pub max_age: u32,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct InviteChannel {
    pub id: Snowflake,
    #[serde(default)]
    pub name: Option<String>,
}

fn invite_body(max_age: u32, max_uses: u32) -> Value {
    json!({ "max_age": max_age, "max_uses": max_uses, "temporary": false, "unique": true })
}

fn ban_body(delete_message_seconds: u32) -> Value {
    json!({ "delete_message_seconds": delete_message_seconds.min(604_800) })
}

fn positions_body(positions: &[(Snowflake, i32)]) -> Value {
    Value::Array(
        positions
            .iter()
            .map(|(id, position)| json!({ "id": id.to_string(), "position": position }))
            .collect(),
    )
}

fn overwrite_body(kind: u8, allow: u64, deny: u64) -> Value {
    json!({ "type": kind, "allow": allow.to_string(), "deny": deny.to_string() })
}

/// The `X-Audit-Log-Reason` value: percent-encoded UTF-8, at most 512
/// characters of reason. Blank reasons send no header.
fn audit_log_reason(reason: &str) -> Option<String> {
    let reason = reason.trim();
    if reason.is_empty() {
        return None;
    }
    let reason: String = reason.chars().take(512).collect();
    Some(percent_encode(&reason))
}

/// A `data:` URI for an image upload (guild icons).
pub fn image_data_uri(bytes: &[u8]) -> String {
    use base64::Engine;
    let mime = match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, ..] => "image/jpeg",
        [b'G', b'I', b'F', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        _ => "image/png",
    };
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// A solved captcha: the widget's response token, and the request token
/// from the challenge.
#[derive(Clone, Debug)]
pub struct CaptchaAnswer {
    pub key: String,
    pub rqtoken: Option<String>,
}

/// The error for a failed response. A captcha challenge has its own shape
/// (no `message` or `code`); other unexpected bodies are kept in the message
/// rather than dropped.
fn error_from_body(status: StatusCode, bytes: &[u8]) -> Error {
    let value = serde_json::from_slice::<Value>(bytes).ok();
    if let Some(value) = &value
        && value.get("captcha_key").is_some()
    {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        return Error::Captcha {
            service: text("captcha_service").unwrap_or_else(|| "hcaptcha".to_owned()),
            sitekey: text("captcha_sitekey").unwrap_or_default(),
            rqdata: text("captcha_rqdata"),
            rqtoken: text("captcha_rqtoken"),
        };
    }
    let detail = value
        .as_ref()
        .and_then(|value| value.get("errors"))
        .and_then(|errors| first_form_error(errors, ""));
    let parsed = value.and_then(|value| serde_json::from_value::<ApiErrorBody>(value).ok());
    let message = parsed
        .as_ref()
        .map(|body| match &detail {
            Some(detail) => format!("{} ({detail})", body.message),
            None => body.message.clone(),
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            let body = String::from_utf8_lossy(bytes);
            let body: String = body.trim().chars().take(200).collect();
            if body.is_empty() {
                status.to_string()
            } else {
                format!("{status}: {body}")
            }
        });
    Error::Api {
        status,
        code: parsed.as_ref().and_then(|body| body.code),
        message,
        retry_after: parsed.as_ref().and_then(|body| body.retry_after),
    }
}

/// The first field error of a 50035 "Invalid Form Body", as
/// `field.path: message`.
fn first_form_error(errors: &Value, path: &str) -> Option<String> {
    let object = errors.as_object()?;
    if let Some(message) = object
        .get("_errors")
        .and_then(|list| list.get(0))
        .and_then(|error| error["message"].as_str())
    {
        return Some(if path.is_empty() {
            message.to_owned()
        } else {
            format!("{path}: {message}")
        });
    }
    object.iter().find_map(|(key, value)| {
        let path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        first_form_error(value, &path)
    })
}

/// `/users/@me/guilds/{guild}/settings`, with `@me` for DMs.
pub fn user_guild_settings_path(guild_id: Option<Snowflake>) -> String {
    match guild_id {
        Some(guild_id) => format!("/users/@me/guilds/{guild_id}/settings"),
        None => "/users/@me/guilds/@me/settings".to_owned(),
    }
}

/// Mutes or unmutes a whole guild until it is turned back on.
pub fn guild_mute_payload(muted: bool) -> Value {
    if muted {
        json!({"muted": true, "mute_config": {"selected_time_window": -1, "end_time": null}})
    } else {
        json!({"muted": false})
    }
}

/// One channel's override: muted (until turned back on) or not, and its
/// notification level (0 all, 1 mentions, 2 nothing, 3 the server's).
pub fn channel_notification_payload(channel_id: Snowflake, muted: bool, level: u8) -> Value {
    let mut entry = json!({"muted": muted, "message_notifications": level});
    if muted {
        entry["mute_config"] = json!({"selected_time_window": -1, "end_time": null});
    }
    json!({"channel_overrides": {channel_id.to_string(): entry}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_settings_payloads() {
        assert_eq!(
            user_guild_settings_path(Some(Snowflake(5))),
            "/users/@me/guilds/5/settings"
        );
        assert_eq!(
            user_guild_settings_path(None),
            "/users/@me/guilds/@me/settings"
        );
        assert_eq!(
            guild_mute_payload(true),
            json!({"muted": true, "mute_config": {"selected_time_window": -1, "end_time": null}})
        );
        assert_eq!(guild_mute_payload(false), json!({"muted": false}));
        assert_eq!(
            channel_notification_payload(Snowflake(9), false, 1),
            json!({"channel_overrides": {"9": {"muted": false, "message_notifications": 1}}})
        );
        assert_eq!(
            channel_notification_payload(Snowflake(9), true, 3),
            json!({"channel_overrides": {"9": {"muted": true, "message_notifications": 3,
                "mute_config": {"selected_time_window": -1, "end_time": null}}}})
        );
    }

    #[test]
    fn a_captcha_challenge_is_its_own_error() {
        let body = br#"{"captcha_key":["captcha-required"],"captcha_sitekey":"site","captcha_service":"hcaptcha","captcha_rqdata":"data","captcha_rqtoken":"token"}"#;
        match error_from_body(StatusCode::BAD_REQUEST, body) {
            Error::Captcha {
                service,
                sitekey,
                rqdata,
                rqtoken,
            } => {
                assert_eq!(service, "hcaptcha");
                assert_eq!(sitekey, "site");
                assert_eq!(rqdata.as_deref(), Some("data"));
                assert_eq!(rqtoken.as_deref(), Some("token"));
            }
            other => panic!("expected a captcha error, got {other:?}"),
        }
    }

    #[test]
    fn api_errors_keep_code_and_message_and_odd_bodies_are_shown() {
        let body = br#"{"message":"Unknown User","code":10013}"#;
        let error = error_from_body(StatusCode::NOT_FOUND, body);
        assert!(
            matches!(error, Error::Api { code: Some(10013), ref message, .. } if message == "Unknown User")
        );
        let error = error_from_body(StatusCode::BAD_REQUEST, b"something else");
        assert!(error.to_string().contains("something else"), "{error}");
        assert!(!error.is_captcha());
    }

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

    #[test]
    fn audit_log_reasons_are_percent_encoded_and_bounded() {
        assert_eq!(audit_log_reason("  "), None);
        assert_eq!(
            audit_log_reason("spam & raids: 2/3 ✓").as_deref(),
            Some("spam%20%26%20raids%3A%202%2F3%20%E2%9C%93")
        );
        let long = "x".repeat(600);
        assert_eq!(audit_log_reason(&long).unwrap().len(), 512);
    }

    #[test]
    fn member_edits_build_discords_payloads() {
        assert_eq!(
            MemberEdit::Timeout(Some("2030-01-01T00:00:00+00:00".into())).body(),
            json!({"communication_disabled_until": "2030-01-01T00:00:00+00:00"})
        );
        assert_eq!(
            MemberEdit::Timeout(None).body(),
            json!({"communication_disabled_until": null})
        );
        assert_eq!(
            MemberEdit::Move(Some(Snowflake::new(5))).body(),
            json!({"channel_id": "5"})
        );
        assert_eq!(MemberEdit::Move(None).body(), json!({"channel_id": null}));
        assert_eq!(MemberEdit::Mute(true).body(), json!({"mute": true}));
        assert_eq!(MemberEdit::Deafen(false).body(), json!({"deaf": false}));
        assert_eq!(MemberEdit::Nick(None).body(), json!({"nick": ""}));
        assert_eq!(
            MemberEdit::Roles(vec![Snowflake::new(1), Snowflake::new(2)]).body(),
            json!({"roles": ["1", "2"]})
        );
    }

    #[test]
    fn role_channel_and_overwrite_payloads() {
        let edit = RoleEdit {
            name: Some("Mods".into()),
            permissions: Some(1 << 40 | 1 << 1),
            hoist: Some(true),
            ..RoleEdit::default()
        };
        assert_eq!(
            edit.body(),
            json!({"name": "Mods", "permissions": "1099511627778", "hoist": true})
        );
        assert_eq!(RoleEdit::default().body(), json!({}));
        let edit = ChannelEdit {
            topic: Some(String::new()),
            rate_limit_per_user: Some(30),
            user_limit: Some(0),
            ..ChannelEdit::default()
        };
        assert_eq!(
            edit.body(),
            json!({"topic": "", "rate_limit_per_user": 30, "user_limit": 0})
        );
        assert_eq!(
            overwrite_body(0, 1 << 10, 1 << 11),
            json!({"type": 0, "allow": "1024", "deny": "2048"})
        );
        assert_eq!(
            positions_body(&[(Snowflake::new(3), 2)]),
            json!([{"id": "3", "position": 2}])
        );
        assert_eq!(
            ban_body(9_999_999),
            json!({"delete_message_seconds": 604800})
        );
        assert_eq!(
            invite_body(0, 10),
            json!({"max_age": 0, "max_uses": 10, "temporary": false, "unique": true})
        );
    }

    #[test]
    fn image_uploads_are_data_uris_with_their_type() {
        assert!(image_data_uri(&[0x89, b'P', b'N', b'G', 1]).starts_with("data:image/png;base64,"));
        assert!(image_data_uri(&[0xFF, 0xD8, 0xFF]).starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn form_errors_name_the_field() {
        let body = br#"{"message":"Invalid Form Body","code":50035,"errors":{"name":{"_errors":[{"code":"BASE_TYPE_REQUIRED","message":"This field is required"}]}}}"#;
        let error = error_from_body(StatusCode::BAD_REQUEST, body);
        assert!(
            error.to_string().contains("name: This field is required"),
            "{error}"
        );
    }
}
