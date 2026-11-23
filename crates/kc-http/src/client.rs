use std::collections::HashMap;

use kc_core::{Snowflake, Token};
use kc_state::{Channel, Guild, Message, User};
use wreq::header::HeaderMap;
use wreq::{Method, StatusCode};
use serde_json::{Value, json};
use tracing::debug;

use crate::API_BASE;
use crate::error::{ApiErrorBody, Error, Result};
use crate::rate_limit::{RateLimiter, normalise_route};

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
    /// A poll to send (see [`NewPoll::body`]).
    pub poll: Option<Value>,
    /// Forwards this message (its guild, channel and id); Discord wants no
    /// content with it, so a note goes in a message of its own.
    pub forward: Option<(Option<Snowflake>, Snowflake, Snowflake)>,
    /// A voice message: the one attachment is its recording.
    pub voice: bool,
}

/// A poll to create: a question, two to ten answers, how long it runs and
/// whether several answers may be chosen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewPoll {
    pub question: String,
    /// Each answer's text and, optionally, a Unicode emoji.
    pub answers: Vec<(String, Option<String>)>,
    /// How long voting runs, in hours (Discord allows 1 to 768).
    pub hours: u32,
    pub allow_multiselect: bool,
}

impl NewPoll {
    /// The message's `poll` field, as the web client sends it.
    pub fn body(&self) -> Value {
        let answers: Vec<Value> = self
            .answers
            .iter()
            .filter(|(text, _)| !text.trim().is_empty())
            .map(|(text, emoji)| {
                let mut media = json!({ "text": text.trim() });
                if let Some(emoji) = emoji.as_deref().filter(|emoji| !emoji.is_empty()) {
                    media["emoji"] = json!({ "name": emoji });
                }
                json!({ "poll_media": media })
            })
            .collect();
        json!({
            "question": { "text": self.question.trim() },
            "answers": answers,
            "allow_multiselect": self.allow_multiselect,
            "duration": self.hours.clamp(1, 768),
            "layout_type": 1,
        })
    }
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
            poll: None,
            forward: None,
            voice: false,
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

/// Discord's `FavoriteGif.format` for a real `image/gif` (animated from its
/// own bytes).
pub const FAVORITE_GIF_FORMAT_IMAGE: u32 = 2;
/// Discord's `FavoriteGif.format` for a video (an MP4 `gifv`).
pub const FAVORITE_GIF_FORMAT_VIDEO: u32 = 3;

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
    /// Set for a favourite GIF whose saved format is a video (an MP4 gifv),
    /// so the picker plays it like a `gifv` rather than animating it as a GIF.
    /// Trending/search results leave this `false`.
    #[serde(default)]
    pub video: bool,
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
    http: wreq::Client,
    token: Token,
    base: String,
    locale: String,
    timezone: String,
    limiter: RateLimiter,
    profiles: ProfileCache,
    /// The token the last ack's answer carried; the next ack sends it back,
    /// as the web client does (`null` until there is one).
    ack_token: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

/// `path` with its snowflakes replaced by `{id}`, for logs.
fn redact_ids(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut digits = String::new();
    for ch in path.chars().chain(std::iter::once('/')) {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        if digits.len() >= 15 {
            out.push_str("{id}");
        } else {
            out.push_str(&digits);
        }
        digits.clear();
        out.push(ch);
    }
    out.pop();
    out
}

/// Days since Discord's epoch (2015-01-01), rounded up: the `last_viewed`
/// the web client sends with an ack.
fn days_since_discord_epoch(now_ms: u64) -> u64 {
    const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
    const DAY_MS: u64 = 86_400_000;
    now_ms.saturating_sub(DISCORD_EPOCH_MS).div_ceil(DAY_MS)
}

/// Profiles fetched lately, by user and server, with when they were
/// fetched. The web client keeps profiles for a while too: fetching the same
/// one again and again is what scrapers do.
type ProfileCache =
    std::sync::Arc<std::sync::Mutex<HashMap<(Snowflake, Option<Snowflake>), (std::time::Instant, Value)>>>;

/// How long a fetched profile is shown again without asking Discord.
pub const PROFILE_KEEP: std::time::Duration = std::time::Duration::from_secs(5 * 60);

impl HttpClient {
    pub fn new(token: Token) -> Self {
        Self::with_base(token, API_BASE)
    }

    pub fn with_base(token: Token, base: impl Into<String>) -> Self {
        let http = crate::browser::client_builder()
            .https_only(true)
            // A browser keeps the cookies discord.com sets (`__dcfduid`,
            // `__sdcfduid`, `__cfruid`) and sends them back.
            .cookie_store(true)
            .build()
            .expect("wreq client");

        Self {
            http,
            token,
            base: base.into().trim_end_matches('/').to_owned(),
            locale: "en-US".to_owned(),
            timezone: std::env::var("TZ").unwrap_or_else(|_| "UTC".to_owned()),
            limiter: RateLimiter::default(),
            profiles: ProfileCache::default(),
            ack_token: Default::default(),
        }
    }

    fn request(&self, method: Method, path: &str) -> wreq::RequestBuilder {
        tracing::debug!(target: "kc_http::requests", %method, path = %redact_ids(path), "request");
        // A change to the account's own profile (name, bio, avatar, a
        // server profile) makes the kept copy of it stale.
        if method != Method::GET
            && (path == "/users/@me"
                || path.starts_with("/users/@me/profile")
                || path.contains("/members/@me")
                || path.contains("/profile/@me"))
            && let Ok(mut cache) = self.profiles.lock()
        {
            cache.clear();
        }
        browser_headers(
            self.http.request(method, format!("{}{}", self.base, path)),
            "https://discord.com/channels/@me",
            &self.locale,
            &self.timezone,
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
                let (retry_after, body) = parse_retry_after(&headers, response).await;
                // Slowmode (or a channel write limit) is not a pacing problem
                // to wait out and retry: the person has to wait. Hand it back
                // at once with its wait, rather than sending the message late.
                if let Some(body) = &body
                    && matches!(body.code, Some(20016 | 20028))
                {
                    debug!(route, retry_after, code = body.code, "slowmode");
                    return Err(Error::Api {
                        status,
                        code: body.code,
                        message: body.message.clone(),
                        retry_after: Some(retry_after),
                    });
                }
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

    /// Creates a new guild owned by the account, from Discord's default
    /// "Create My Own" template (a general text and voice channel). Returns the
    /// new guild's id. `icon` is an optional base64 data URI.
    pub async fn create_guild(&self, name: &str, icon: Option<&str>) -> Result<Snowflake> {
        let value = self
            .post(
                "/guilds",
                json!({
                    "name": name,
                    "icon": icon,
                    "channels": [],
                    "system_channel_id": null,
                    // The stable code for the official "Create My Own" template.
                    "guild_template_code": "2TffvPucqHkN",
                }),
            )
            .await?;
        value["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .ok_or_else(|| Error::Other("Discord did not return the new server".to_owned()))
    }

    /// Looks up an invite without accepting it, with member/online counts, so
    /// the client can render an invite card.
    pub async fn resolve_invite(&self, code: &str) -> Result<Invite> {
        let value = self
            .get(&format!(
                "/invites/{}?with_counts=true&with_expiration=true",
                percent_encode(code)
            ))
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    /// Joins the guild an invite points to by accepting it, as the website
    /// does: with the gateway session's ID, and where it was accepted from
    /// (the invite, looked up first). Returns the guild's id when the
    /// response carries one (so the client can navigate to it).
    pub async fn accept_invite(
        &self,
        code: &str,
        session_id: Option<&str>,
        invite: Option<&Invite>,
        captcha: Option<&CaptchaAnswer>,
    ) -> Result<Option<Snowflake>> {
        // The web client's body: its session, and the invite's instance id
        // when the resolved invite has one (JSON drops it otherwise).
        let mut body = match session_id {
            Some(session_id) => json!({ "session_id": session_id }),
            None => json!({}),
        };
        if let Some(instance) = invite.and_then(|invite| invite.invite_instance_id.as_deref()) {
            body["invite_instance_id"] = json!(instance);
        }
        let mut context = json!({ "location": "Join Guild" });
        if let Some(invite) = invite {
            if let Some(guild) = &invite.guild {
                context["location_guild_id"] = json!(guild.id.to_string());
            }
            if let Some(channel) = &invite.channel {
                context["location_channel_id"] = json!(channel.id.to_string());
                context["location_channel_type"] = json!(channel.kind);
            }
        }
        let value = self
            .request_json_full(
                Method::POST,
                &format!("/invites/{}", percent_encode(code)),
                Some(&body),
                Some(&context),
                captcha,
            )
            .await?;
        Ok(value["guild"]["id"].as_str().and_then(|id| id.parse().ok()))
    }

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
                Some(&recipients_body(&[recipient])),
                Some(&json!({})),
            )
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    /// Starts a group DM with `recipients` (the account is added itself).
    /// Discord allows ten people in a group, the account included.
    pub async fn create_group_dm(&self, recipients: &[Snowflake]) -> Result<Channel> {
        let value = self
            .request_json_in(
                Method::POST,
                "/users/@me/channels",
                Some(&recipients_body(recipients)),
                Some(&json!({})),
            )
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    /// Adds a friend to a group DM.
    pub async fn add_group_recipient(
        &self,
        channel_id: Snowflake,
        user_id: Snowflake,
    ) -> Result<()> {
        self.put_empty(&format!("/channels/{channel_id}/recipients/{user_id}"))
            .await
    }

    /// Removes someone from a group DM; only its owner may.
    pub async fn remove_group_recipient(
        &self,
        channel_id: Snowflake,
        user_id: Snowflake,
    ) -> Result<()> {
        self.delete(&format!("/channels/{channel_id}/recipients/{user_id}"))
            .await?;
        Ok(())
    }

    /// Renames a group DM; an empty name goes back to the members' names.
    pub async fn rename_group_dm(&self, channel_id: Snowflake, name: &str) -> Result<Channel> {
        let value = self
            .patch(&format!("/channels/{channel_id}"), group_name_body(name))
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

    /// Accepts an incoming request or sends one to a known user. `location`
    /// is where in the app it was done, as the web client reports it
    /// ("Friends", "ContextMenu", "User Profile").
    pub async fn accept_friend(
        &self,
        user_id: Snowflake,
        location: &str,
        captcha: Option<&CaptchaAnswer>,
    ) -> Result<()> {
        self.request_json_full(
            Method::PUT,
            &format!("/users/@me/relationships/{user_id}"),
            Some(&json!({})),
            Some(&json!({ "location": location })),
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

    /// A user's profile: bio, pronouns, badges, mutual guilds and friends,
    /// as the web client's full profile (`type=modal`) asks for it.
    pub async fn user_profile(
        &self,
        user_id: Snowflake,
        guild_id: Option<Snowflake>,
    ) -> Result<Value> {
        let mut path = format!(
            "/users/{user_id}/profile?type=modal&with_mutual_guilds=true&with_mutual_friends=true&with_mutual_friends_count=false"
        );
        if let Some(guild_id) = guild_id {
            path.push_str(&format!("&guild_id={guild_id}"));
        }
        let profile = self.get(&path).await?;
        if let Ok(mut cache) = self.profiles.lock() {
            cache.retain(|_, (fetched, _)| fetched.elapsed() < PROFILE_KEEP);
            cache.insert((user_id, guild_id), (std::time::Instant::now(), profile.clone()));
        }
        Ok(profile)
    }

    /// The profile fetched for `user_id` (in `guild_id`) in the last few
    /// minutes, if any.
    pub fn cached_profile(&self, user_id: Snowflake, guild_id: Option<Snowflake>) -> Option<Value> {
        let cache = self.profiles.lock().ok()?;
        let (fetched, profile) = cache.get(&(user_id, guild_id))?;
        (fetched.elapsed() < PROFILE_KEEP).then(|| profile.clone())
    }

    /// Forgets the kept profiles of `user_id` (after it changed).
    pub fn forget_profile(&self, user_id: Snowflake) {
        if let Ok(mut cache) = self.profiles.lock() {
            cache.retain(|(user, _), _| *user != user_id);
        }
    }

    /// The note on `user_id`, if there is one (Discord answers 404 when
    /// there is none). READY no longer carries notes (the LAZY_USER_NOTES
    /// capability), so the web client fetches the note with the profile.
    pub async fn note(&self, user_id: Snowflake) -> Result<Option<String>> {
        match self.get(&format!("/users/@me/notes/{user_id}")).await {
            Ok(value) => Ok(value["note"].as_str().map(str::to_owned)),
            Err(Error::Api { status, .. }) if status == StatusCode::NOT_FOUND => Ok(None),
            Err(error) => Err(error),
        }
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

    /// Sets (`Some`) or removes (`None`) the account's avatar. The bytes are
    /// sent as a `data:` URI; `None` sends a null to clear it.
    pub async fn set_avatar(&self, image: Option<&[u8]>) -> Result<()> {
        self.patch("/users/@me", avatar_body(image)).await?;
        Ok(())
    }

    /// Edits the per-server profile: nickname, bio, pronouns and avatar.
    /// Each `None` field is left untouched; `Some(None)`-style clears
    /// (empty text, null avatar) are encoded by the callers through
    /// [`ServerProfileEdit`]. The nickname rides the member route so it is
    /// shown to everyone, matching [`Self::set_nickname`].
    pub async fn set_server_profile(
        &self,
        guild_id: Snowflake,
        edit: &ServerProfileEdit,
    ) -> Result<()> {
        self.patch(&format!("/guilds/{guild_id}/profile/@me"), edit.body())
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

    /// Sets or clears (all `None`) the custom status. The emoji is a Unicode
    /// emoji (`emoji_name` alone) or a server emoji (its id, and its name).
    pub async fn set_custom_status(
        &self,
        text: Option<&str>,
        emoji_name: Option<&str>,
        emoji_id: Option<Snowflake>,
    ) -> Result<()> {
        let custom_status = match (text, emoji_name, emoji_id) {
            (None, None, None) => Value::Null,
            _ => json!({
                "text": text,
                "emoji_name": emoji_name,
                "emoji_id": emoji_id.map(|id| id.to_string()),
                "expires_at": null,
            }),
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

    /// The commands apps offer in `scope`.
    pub async fn command_index(
        &self,
        scope: crate::commands::CommandScope,
    ) -> Result<crate::commands::CommandIndex> {
        let value = self.request_json(Method::GET, &scope.path(), None).await?;
        Ok(crate::commands::CommandIndex::from_value(&value))
    }

    /// Sends an interaction built by [`crate::InteractionPlace`]. Discord
    /// answers with no content; the outcome arrives on the gateway.
    pub async fn interact(&self, body: &Value) -> Result<()> {
        self.request_json(Method::POST, "/interactions", Some(body))
            .await
            .map(|_| ())
    }

    /// A fresh nonce, to match an interaction with its answer.
    pub fn new_nonce() -> String {
        nonce().to_string()
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
        // The web client's body, in its order.
        let mut body = json!({
            "mobile_network_type": "unknown",
            "content": message.content,
            "nonce": nonce,
            "tts": false,
            "flags": if message.silent { 1 << 12 } else { 0 } | if message.voice { 1 << 13 } else { 0 },
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
        if let Some((guild_id, source_channel, message_id)) = message.forward {
            let mut reference = json!({});
            if let Some(guild_id) = guild_id {
                reference["guild_id"] = json!(guild_id);
            }
            reference["channel_id"] = json!(source_channel);
            reference["message_id"] = json!(message_id);
            reference["type"] = json!(1);
            body["message_reference"] = reference;
        }
        if !message.attachments.is_empty() {
            body["attachments"] = Value::Array(message.attachments.clone());
        }
        if !message.sticker_ids.is_empty() {
            body["sticker_ids"] = json!(message.sticker_ids);
        }
        if let Some(poll) = &message.poll {
            body["poll"] = poll.clone();
        }
        let value = self
            .request_json_in(
                Method::POST,
                &format!("/channels/{channel_id}/messages"),
                Some(&body),
                Some(&json!({ "location": "chat_input" })),
            )
            .await?;
        Self::parse(value, StatusCode::OK)
    }

    // -- soundboard ------------------------------------------------------

    /// Discord's own soundboard sounds.
    pub async fn default_soundboard_sounds(&self) -> Result<Vec<kc_state::SoundboardSound>> {
        let body = self.get("/soundboard-default-sounds").await?;
        Ok(serde_json::from_value(body).unwrap_or_default())
    }

    /// Plays a sound to the voice channel we are in (its emoji shows as
    /// the effect), as the web client sends it.
    pub async fn send_soundboard_sound(&self, channel_id: Snowflake, sound: &kc_state::SoundboardSound) -> Result<()> {
        let mut body = json!({ "sound_id": sound.sound_id, "emoji_id": sound.emoji_id, "emoji_name": sound.emoji_name });
        if let Some(guild_id) = sound.guild_id.filter(|guild| guild.0 != 0) {
            body["source_guild_id"] = json!(guild_id);
        }
        self.post(&format!("/channels/{channel_id}/send-soundboard-sound"), body).await?;
        Ok(())
    }

    /// Adds a sound to a server's soundboard: an MP3 or OGG up to 512 KiB
    /// and 5.2 seconds.
    pub async fn create_soundboard_sound(
        &self,
        guild_id: Snowflake,
        name: &str,
        bytes: &[u8],
        filename: &str,
        volume: f64,
        emoji_name: Option<&str>,
    ) -> Result<()> {
        use base64::Engine as _;
        let sound = format!(
            "data:{};base64,{}",
            content_type(filename),
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        self.post(
            &format!("/guilds/{guild_id}/soundboard-sounds"),
            json!({ "name": name, "sound": sound, "volume": volume, "emoji_id": null, "emoji_name": emoji_name }),
        )
        .await?;
        Ok(())
    }

    pub async fn edit_soundboard_sound(&self, guild_id: Snowflake, sound_id: Snowflake, name: &str, volume: f64, emoji_name: Option<&str>) -> Result<()> {
        self.request_json(
            Method::PATCH,
            &format!("/guilds/{guild_id}/soundboard-sounds/{sound_id}"),
            Some(&json!({ "name": name, "volume": volume, "emoji_name": emoji_name })),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_soundboard_sound(&self, guild_id: Snowflake, sound_id: Snowflake) -> Result<()> {
        self.request_json(Method::DELETE, &format!("/guilds/{guild_id}/soundboard-sounds/{sound_id}"), None)
            .await?;
        Ok(())
    }

    // -- stages ----------------------------------------------------------

    /// Starts a stage in a stage channel, with its topic (server members
    /// only; `notify` tells the server's members it started).
    pub async fn start_stage(&self, channel_id: Snowflake, topic: &str, notify: bool) -> Result<()> {
        self.post(
            "/stage-instances",
            json!({ "channel_id": channel_id, "topic": topic, "privacy_level": 2, "send_start_notification": notify }),
        )
        .await?;
        Ok(())
    }

    pub async fn edit_stage(&self, channel_id: Snowflake, topic: &str) -> Result<()> {
        self.request_json(Method::PATCH, &format!("/stage-instances/{channel_id}"), Some(&json!({ "topic": topic })))
            .await?;
        Ok(())
    }

    pub async fn end_stage(&self, channel_id: Snowflake) -> Result<()> {
        self.request_json(Method::DELETE, &format!("/stage-instances/{channel_id}"), None)
            .await?;
        Ok(())
    }

    /// This account on a stage: become a speaker (`suppress: false`) or go
    /// back to the audience, and raise (`Some(true)`) or lower the hand.
    pub async fn set_stage_self(
        &self,
        guild_id: Snowflake,
        channel_id: Snowflake,
        suppress: Option<bool>,
        hand: Option<bool>,
    ) -> Result<()> {
        let mut body = json!({ "channel_id": channel_id });
        if let Some(suppress) = suppress {
            body["suppress"] = json!(suppress);
        }
        if let Some(hand) = hand {
            body["request_to_speak_timestamp"] =
                if hand { json!(kc_state::time::now_iso() + "Z") } else { Value::Null };
        }
        self.request_json(Method::PATCH, &format!("/guilds/{guild_id}/voice-states/@me"), Some(&body))
            .await?;
        Ok(())
    }

    /// A stage moderator inviting someone up to speak (`suppress: false`)
    /// or moving them to the audience.
    pub async fn set_stage_speaker(&self, guild_id: Snowflake, user_id: Snowflake, channel_id: Snowflake, suppress: bool) -> Result<()> {
        self.request_json(
            Method::PATCH,
            &format!("/guilds/{guild_id}/voice-states/{user_id}"),
            Some(&json!({ "channel_id": channel_id, "suppress": suppress })),
        )
        .await?;
        Ok(())
    }

    // -- message requests ------------------------------------------------

    /// Accepts a message request: it becomes an ordinary DM.
    pub async fn accept_message_request(&self, channel_id: Snowflake) -> Result<()> {
        self.request_json(
            Method::PUT,
            &format!("/channels/{channel_id}/recipients/@me"),
            Some(&json!({ "consent_status": 2 })),
        )
        .await?;
        Ok(())
    }

    /// Ignores (rejects and removes) a message request.
    pub async fn reject_message_request(&self, channel_id: Snowflake) -> Result<()> {
        self.request_json(Method::DELETE, &format!("/channels/{channel_id}/recipients/@me"), None)
            .await?;
        Ok(())
    }

    /// The message that started each request (up to 25).
    pub async fn message_request_previews(&self, channel_ids: &[Snowflake]) -> Result<Vec<(Snowflake, Message)>> {
        if channel_ids.is_empty() {
            return Ok(Vec::new());
        }
        let query: Vec<String> = channel_ids.iter().take(25).map(|id| format!("channel_ids={id}")).collect();
        let body = self
            .get(&format!("/users/@me/message-requests/supplemental-data?{}", query.join("&")))
            .await?;
        Ok(body
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let channel = entry["channel_id"].as_str()?.parse::<u64>().ok()?;
                        let message = serde_json::from_value::<Message>(entry["message_preview"].clone()).ok()?;
                        Some((Snowflake(channel), message))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    // -- recent mentions -------------------------------------------------

    /// Messages that mention this account, newest first: 25 before
    /// `before`, in one server or all, with or without role and
    /// @everyone/@here mentions.
    pub async fn mentions(
        &self,
        before: Option<Snowflake>,
        guild_id: Option<Snowflake>,
        roles: bool,
        everyone: bool,
    ) -> Result<Vec<Message>> {
        let mut path = String::from("/users/@me/mentions?");
        if let Some(before) = before {
            path.push_str(&format!("before={before}&"));
        }
        path.push_str("limit=25");
        if let Some(guild_id) = guild_id {
            path.push_str(&format!("&guild_id={guild_id}"));
        }
        path.push_str(&format!("&roles={roles}&everyone={everyone}"));
        let body = self.get(&path).await?;
        Ok(body
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|message| serde_json::from_value::<Message>(message.clone()).ok())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Takes a mention out of the inbox.
    pub async fn delete_mention(&self, message_id: Snowflake) -> Result<()> {
        self.request_json(Method::DELETE, &format!("/users/@me/mentions/{message_id}"), None)
            .await?;
        Ok(())
    }

    // -- stickers and server emoji ---------------------------------------

    /// Discord's own sticker packs (the free ones everyone may send).
    pub async fn sticker_packs(&self) -> Result<Vec<kc_state::StickerPack>> {
        let body = self.get("/sticker-packs?locale=en-US").await?;
        Ok(serde_json::from_value(body["sticker_packs"].clone()).unwrap_or_default())
    }

    /// Greets someone with a sticker (the "Wave to say hi" on a join
    /// message), replying to `reply_to` (its channel, id and server).
    pub async fn greet(
        &self,
        channel_id: Snowflake,
        sticker_id: Snowflake,
        reply_to: Option<(Option<Snowflake>, Snowflake)>,
    ) -> Result<Message> {
        let mut body = json!({ "sticker_ids": [sticker_id] });
        if let Some((guild_id, message_id)) = reply_to {
            let mut reference = json!({ "channel_id": channel_id, "message_id": message_id });
            if let Some(guild_id) = guild_id {
                reference["guild_id"] = json!(guild_id);
            }
            body["message_reference"] = reference;
        }
        let value = self
            .request_json_in(
                Method::POST,
                &format!("/channels/{channel_id}/greet"),
                Some(&body),
                Some(&json!({ "location": "Greet" })),
            )
            .await?;
        serde_json::from_value(value).map_err(|source| Error::Decode { status: StatusCode::OK, source })
    }

    /// Adds a server emoji from an image (PNG, JPEG, GIF or WebP; Discord
    /// takes up to 256 KiB).
    pub async fn create_emoji(&self, guild_id: Snowflake, name: &str, bytes: &[u8], filename: &str) -> Result<Value> {
        use base64::Engine as _;
        let image = format!(
            "data:{};base64,{}",
            content_type(filename),
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        self.post(&format!("/guilds/{guild_id}/emojis"), json!({ "image": image, "name": name, "roles": [] }))
            .await
    }

    pub async fn rename_emoji(&self, guild_id: Snowflake, emoji_id: Snowflake, name: &str) -> Result<()> {
        self.request_json(Method::PATCH, &format!("/guilds/{guild_id}/emojis/{emoji_id}"), Some(&json!({ "name": name })))
            .await?;
        Ok(())
    }

    pub async fn delete_emoji(&self, guild_id: Snowflake, emoji_id: Snowflake) -> Result<()> {
        self.request_json(Method::DELETE, &format!("/guilds/{guild_id}/emojis/{emoji_id}"), None)
            .await?;
        Ok(())
    }

    /// Adds a server sticker (PNG, APNG, GIF or Lottie JSON, up to 512 KiB),
    /// sent as a form as the web client does. `tags` is the emoji it stands
    /// for.
    pub async fn create_sticker(
        &self,
        guild_id: Snowflake,
        name: &str,
        description: &str,
        tags: &str,
        bytes: Vec<u8>,
        filename: &str,
    ) -> Result<Value> {
        let path = format!("/guilds/{guild_id}/stickers");
        let route = normalise_route("POST", &path);
        self.limiter.acquire(&route).await;
        let file = wreq::multipart::Part::bytes(bytes)
            .file_name(filename.to_owned())
            .mime_str(content_type(filename))
            .map_err(|error| Error::Api {
                status: StatusCode::BAD_REQUEST,
                code: None,
                message: error.to_string(),
                retry_after: None,
            })?;
        let form = wreq::multipart::Form::new()
            .text("name", name.to_owned())
            .text("tags", tags.to_owned())
            .text("description", description.to_owned())
            .part("file", file);
        let response = self.request(Method::POST, &path).multipart(form).send().await?;
        let status = response.status();
        self.limiter.update(&route, response.headers());
        let bytes = response.bytes().await.unwrap_or_default();
        if !status.is_success() {
            return Err(error_from_body(status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|source| Error::Decode { status, source })
    }

    pub async fn edit_sticker(&self, guild_id: Snowflake, sticker_id: Snowflake, name: &str, description: &str, tags: &str) -> Result<()> {
        self.request_json(
            Method::PATCH,
            &format!("/guilds/{guild_id}/stickers/{sticker_id}"),
            Some(&json!({ "name": name, "description": description, "tags": tags })),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_sticker(&self, guild_id: Snowflake, sticker_id: Snowflake) -> Result<()> {
        self.request_json(Method::DELETE, &format!("/guilds/{guild_id}/stickers/{sticker_id}"), None)
            .await?;
        Ok(())
    }

    // -- polls -----------------------------------------------------------

    /// Votes in a poll: these answers replace any earlier vote, and none
    /// takes the vote back.
    pub async fn vote_poll(&self, channel_id: Snowflake, message_id: Snowflake, answers: &[u32]) -> Result<()> {
        let ids: Vec<String> = answers.iter().map(u32::to_string).collect();
        self.request_json(
            Method::PUT,
            &format!("/channels/{channel_id}/polls/{message_id}/answers/@me"),
            Some(&json!({ "answer_ids": ids })),
        )
        .await?;
        Ok(())
    }

    /// Ends a poll now (its author only).
    pub async fn end_poll(&self, channel_id: Snowflake, message_id: Snowflake) -> Result<()> {
        self.post(&format!("/channels/{channel_id}/polls/{message_id}/expire"), json!({}))
            .await?;
        Ok(())
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

    /// Searches messages in a server or a DM (see [`crate::search`]).
    /// Returns the total hit count and one page of hits. Discord answers
    /// 202 while it is still indexing; that surfaces as an empty page with
    /// `indexing` set.
    pub async fn search(
        &self,
        scope: crate::search::Scope,
        params: &crate::search::SearchParams,
        tab: crate::search::Tab,
        sort: crate::search::Sort,
        offset: u32,
    ) -> Result<SearchPage> {
        // NSFW channels only for someone who has agreed to see them in that
        // server, which Kubecord does not track: left out, as the web client
        // does until then.
        let value = self.get(&params.path(scope, tab, sort, offset, false)).await?;
        let Some(groups) = value["messages"].as_array() else {
            return Ok(SearchPage {
                total: 0,
                messages: Vec::new(),
                indexing: value.get("retry_after").is_some() || value["code"] == 110000,
            });
        };
        // Each hit comes with the messages around it; the hit is marked.
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

    /// The GIFs the user has favourited (on any client). Read-only: fetches
    /// the `FrecencyUserSettings` protobuf from `GET
    /// /users/@me/settings-proto/2`, base64-decodes it and reads the
    /// `favorite_gifs` field. Ordered by the saved order. Favourites whose
    /// format is a video are flagged so the picker plays them as a gifv.
    ///
    /// Fails soft: a settings blob that cannot be fetched, decoded or parsed
    /// yields an empty list (logged), never an error the picker shows.
    pub async fn favorite_gifs(&self) -> Result<Vec<Gif>> {
        let value = match self.get("/users/@me/settings-proto/2").await {
            Ok(value) => value,
            Err(error) => {
                tracing::debug!(%error, "could not fetch favourite gifs");
                return Ok(Vec::new());
            }
        };
        let Some(encoded) = value["settings"].as_str() else {
            return Ok(Vec::new());
        };
        use base64::Engine;
        let bytes = match base64::engine::general_purpose::STANDARD.decode(encoded) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, "favourite gifs settings were not valid base64");
                return Ok(Vec::new());
            }
        };
        let favorites = crate::favorites::parse_settings_proto(&bytes);
        Ok(favorites
            .into_iter()
            .filter(|gif| !gif.src.is_empty())
            .map(|gif| Gif {
                id: String::new(),
                // Sent as the message when picked: the favourite's key, a GIF
                // page Discord previews properly (as its own client sends),
                // or the media itself when that is all the key is.
                url: if gif.url.is_empty() { gif.src.clone() } else { gif.url.clone() },
                src: gif.src.clone(),
                gif_src: gif.src.clone(),
                preview: gif.src.clone(),
                width: gif.width,
                height: gif.height,
                // Discord's FavoriteGif format: 3 is a video (gifv/MP4).
                video: gif.format == FAVORITE_GIF_FORMAT_VIDEO,
            })
            .collect())
    }

    /// Favourites (`favorite = true`) or unfavourites (`false`) the GIF at
    /// `url`, syncing with the official client via the `FrecencyUserSettings`
    /// proto.
    ///
    /// This GETs the current settings proto, applies the change to only the
    /// `favorite_gifs` map (every other setting -- favourite emoji/stickers,
    /// all frecency data, unknown fields -- is preserved byte-for-byte), then
    /// `PATCH`es the whole proto back to `/users/@me/settings-proto/2`.
    ///
    /// `url` is the media URL used as the map key (and, for a favourite, as the
    /// `src` if `src` is empty). `format` is Discord's `FavoriteGif.format`
    /// (2 = image/real GIF, 3 = video/gifv); it is ignored when unfavouriting.
    ///
    /// Errors (rather than silently dropping settings) if the proto cannot be
    /// fetched, decoded or safely rewritten.
    pub async fn set_favorite_gif(
        &self,
        url: &str,
        src: &str,
        format: u32,
        width: u32,
        height: u32,
        favorite: bool,
    ) -> Result<()> {
        use base64::Engine;
        let value = self.get("/users/@me/settings-proto/2").await?;
        // A missing/empty settings proto decodes to empty bytes: favouriting
        // then creates a fresh favorite_gifs; unfavouriting is a no-op.
        let encoded = value["settings"].as_str().unwrap_or_default();
        let bytes = if encoded.is_empty() {
            Vec::new()
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| {
                    Error::Other(format!("settings proto was not valid base64: {error}"))
                })?
        };

        let src = if src.is_empty() { url } else { src };
        let updated = if favorite {
            crate::favorites::add_favorite(&bytes, url, src, format, width, height)
        } else {
            crate::favorites::remove_favorite(&bytes, url)
        }
        .ok_or_else(|| {
            Error::Other(
                "could not rewrite the favourites proto safely; not changing settings".to_owned(),
            )
        })?;

        let settings = base64::engine::general_purpose::STANDARD.encode(updated);
        self.patch(
            "/users/@me/settings-proto/2",
            json!({ "settings": settings }),
        )
        .await?;
        Ok(())
    }

    /// Convenience for favouriting a GIF (see [`Self::set_favorite_gif`]).
    pub async fn favorite_gif(
        &self,
        url: &str,
        src: &str,
        format: u32,
        width: u32,
        height: u32,
    ) -> Result<()> {
        self.set_favorite_gif(url, src, format, width, height, true)
            .await
    }

    /// Convenience for unfavouriting a GIF (see [`Self::set_favorite_gif`]).
    pub async fn unfavorite_gif(&self, url: &str) -> Result<()> {
        self.set_favorite_gif(url, "", 0, 0, 0, false).await
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

    /// Marks a channel read up to `message_id`, with the body the web client
    /// sends: the previous ack's token and the day the channel was viewed.
    pub async fn ack_message(&self, channel_id: Snowflake, message_id: Snowflake) -> Result<()> {
        let token = self.ack_token.lock().ok().and_then(|token| token.clone());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        let answer = self
            .post(
                &format!("/channels/{channel_id}/messages/{message_id}/ack"),
                json!({ "token": token, "last_viewed": days_since_discord_epoch(now_ms) }),
            )
            .await?;
        if let Ok(mut kept) = self.ack_token.lock() {
            *kept = answer["token"].as_str().map(str::to_owned);
        }
        Ok(())
    }

    // -- reactions -------------------------------------------------------

    /// Adds a reaction. `location` is where it was picked, as the web client
    /// reports it: "Message" (an existing reaction clicked) or "Message
    /// Reaction Picker".
    pub async fn add_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &str,
        location: &str,
    ) -> Result<()> {
        self.put_empty(&format!(
            "/channels/{channel_id}/messages/{message_id}/reactions/{}/@me?location={}&type=0",
            encode_emoji(emoji),
            location.replace(' ', "%20"),
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
        // The web client numbers the files it has picked this session and
        // names their type (empty when the browser does not know it).
        static PICKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let picked = PICKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reserved = self
            .post(
                &format!("/channels/{channel_id}/attachments"),
                json!({
                    "files": [{
                        "filename": filename,
                        "file_size": size,
                        "id": picked.to_string(),
                        "is_clip": false,
                        "original_content_type": content_type(filename),
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
            .header("origin", "https://discord.com")
            .header("referer", "https://discord.com/")
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

    /// Sends a voice message: uploads the recording, then posts it with its
    /// length and waveform, as the web client does.
    pub async fn send_voice_message(
        &self,
        channel_id: Snowflake,
        ogg: Vec<u8>,
        duration_secs: f64,
        waveform: &[u8],
        reply_to: Option<Snowflake>,
    ) -> Result<Message> {
        use base64::Engine as _;
        let filename = "voice-message.ogg";
        let upload = self.upload_attachment(channel_id, filename, ogg).await?;
        let attachment = json!({
            "id": "0",
            "filename": filename,
            "uploaded_filename": upload["upload_filename"],
            "duration_secs": (duration_secs * 100.0).round() / 100.0,
            "waveform": base64::engine::general_purpose::STANDARD.encode(waveform),
        });
        self.send(
            channel_id,
            &Outgoing {
                attachments: vec![attachment],
                reply_to,
                voice: true,
                ..Outgoing::default()
            },
        )
        .await
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
    fn build_request(&self, method: Method, path: &str) -> wreq::Request {
        self.request(method, path).build().expect("request builds")
    }
}

/// The headers the web client's browser sends with every API request (all
/// but `authorization`). `referer` is the page the request comes from.
pub(crate) fn browser_headers(
    request: wreq::RequestBuilder,
    referer: &str,
    locale: &str,
    timezone: &str,
) -> wreq::RequestBuilder {
    use kc_core::super_properties as identity;
    // Built for each request: the build number and the window's focus change.
    let super_properties = identity::SuperProperties::new().as_header();
    let request = request
        .header("user-agent", kc_core::super_properties::user_agent())
        .header("accept", "*/*")
        .header("accept-language", "en-US")
        .header("priority", "u=1, i")
        .header("referer", referer)
        .header("sec-ch-ua", kc_core::super_properties::sec_ch_ua())
        .header("sec-ch-ua-mobile", "?0")
        .header("sec-ch-ua-platform", "\"Windows\"")
        .header("sec-fetch-dest", "empty")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-site", "same-origin")
        .header("x-discord-locale", locale)
        .header("x-discord-timezone", timezone)
        .header("x-super-properties", super_properties)
        .header("origin", "https://discord.com")
        .header("x-debug-options", "bugReporterEnabled");
    match identity::installation_id() {
        Some(id) => request.header("x-installation-id", id),
        None => request,
    }
}

impl HttpClient {
    /// Asks for the account's experiment assignments, as the web client does
    /// on load, and keeps the installation id Discord hands out with them.
    pub async fn fetch_installation_id(&self) {
        match self.get("/apex/experiments?surface=2").await {
            Ok(body) => {
                if let Some(id) = body["installation"].as_str()
                    && kc_core::super_properties::set_installation_id(id)
                {
                    tracing::info!("Discord gave this install its installation id");
                }
            }
            Err(error) => tracing::debug!(%error, "could not read the experiment assignments"),
        }
    }
}

/// The wait a 429 asks for (the body's precise `retry_after`, else the
/// header's, else a second) and its body, which names the cause (slowmode's
/// code 20016, for instance).
async fn parse_retry_after(
    headers: &HeaderMap,
    response: wreq::Response,
) -> (f64, Option<ApiErrorBody>) {
    let from_header = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok());
    let body = response.json::<ApiErrorBody>().await.ok();
    let wait = body
        .as_ref()
        .and_then(|body| body.retry_after)
        .or(from_header)
        .unwrap_or(1.0);
    (wait, body)
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

/// A message nonce as the web client makes it: the snowflake for now
/// (milliseconds since Discord's epoch, shifted 22 bits). Two in the same
/// millisecond would clash, so a later one moves on a millisecond.
fn nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
        .saturating_sub(DISCORD_EPOCH_MS);
    let millis = LAST
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |last| Some(now.max(last + 1)))
        .map_or(now, |last| now.max(last + 1));
    millis << 22
}

/// The MIME type a browser gives a file, from its extension; empty when it
/// does not know it (as the web client then sends).
pub fn content_type(filename: &str) -> &'static str {
    let extension = filename.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        _ => "",
    }
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

/// One change to an avatar field (`/users/@me` or a server profile).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum AvatarEdit {
    /// Leave the avatar as it is (field omitted).
    #[default]
    Keep,
    /// Remove the avatar (field sent as `null`).
    Remove,
    /// Replace the avatar with these image bytes (sent as a `data:` URI).
    Set(Vec<u8>),
}

impl AvatarEdit {
    /// The field value, if the edit changes it: `Some(Value::Null)` to
    /// clear, `Some(data uri)` to set, `None` to leave it out.
    fn value(&self) -> Option<Value> {
        match self {
            Self::Keep => None,
            Self::Remove => Some(Value::Null),
            Self::Set(bytes) => Some(json!(image_data_uri(bytes))),
        }
    }
}

/// Fields to set on the per-server profile
/// (`PATCH /guilds/{guild}/profile/@me`). `None`/`Keep` leaves a field
/// untouched; empty `nick`/`bio`/`pronouns` strings clear those fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerProfileEdit {
    pub nick: Option<String>,
    pub bio: Option<String>,
    pub pronouns: Option<String>,
    pub avatar: AvatarEdit,
}

impl ServerProfileEdit {
    pub fn body(&self) -> Value {
        let mut body = serde_json::Map::new();
        if let Some(nick) = &self.nick {
            body.insert("nick".into(), json!(nick));
        }
        if let Some(bio) = &self.bio {
            body.insert("bio".into(), json!(bio));
        }
        if let Some(pronouns) = &self.pronouns {
            body.insert("pronouns".into(), json!(pronouns));
        }
        if let Some(avatar) = self.avatar.value() {
            body.insert("avatar".into(), avatar);
        }
        Value::Object(body)
    }

    /// Whether anything would actually be sent.
    pub fn is_empty(&self) -> bool {
        self.nick.is_none()
            && self.bio.is_none()
            && self.pronouns.is_none()
            && self.avatar == AvatarEdit::Keep
    }
}

/// The `PATCH /users/@me` body for an avatar change: the image as a `data:`
/// URI, or `null` to remove it.
fn avatar_body(image: Option<&[u8]>) -> Value {
    match image {
        Some(bytes) => json!({ "avatar": image_data_uri(bytes) }),
        None => json!({ "avatar": null }),
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
    /// Sent back when accepting, as the web client does.
    #[serde(default)]
    pub invite_instance_id: Option<String>,
    #[serde(default)]
    pub inviter: Option<User>,
    /// The server the invite leads to (present when resolving an invite).
    #[serde(default)]
    pub guild: Option<InviteGuild>,
    /// Approximate total members / online members, with `with_counts=true`.
    #[serde(default)]
    pub approximate_member_count: u32,
    #[serde(default)]
    pub approximate_presence_count: u32,
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
    /// The channel's type (0 for a text channel).
    #[serde(rename = "type", default)]
    pub kind: u8,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct InviteGuild {
    pub id: Snowflake,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
}

/// The body that opens a DM (one recipient) or a group DM (several).
fn recipients_body(recipients: &[Snowflake]) -> Value {
    let ids: Vec<String> = recipients.iter().map(ToString::to_string).collect();
    json!({ "recipients": ids })
}

fn group_name_body(name: &str) -> Value {
    let name = name.trim();
    json!({ "name": (!name.is_empty()).then_some(name) })
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
    fn nonces_are_snowflakes_for_now_and_never_repeat() {
        let first = nonce();
        let second = nonce();
        assert!(second > first);
        // A snowflake's low 22 bits are zero, as the web client's are.
        assert_eq!(first & ((1 << 22) - 1), 0);
        let millis = (first >> 22) + 1_420_070_400_000;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(now.abs_diff(millis) < 5_000);
        assert_eq!(first.to_string().len(), 19);
    }

    #[test]
    fn content_types_are_what_a_browser_names() {
        assert_eq!(content_type("cat.PNG"), "image/png");
        assert_eq!(content_type("clip.mp4"), "video/mp4");
        assert_eq!(content_type("notes"), "");
        assert_eq!(content_type("weird.xyz"), "");
    }

    #[test]
    fn last_viewed_counts_days_since_discords_epoch() {
        // 2026-11-23 06:00 UTC is in day 4345 since 2015-01-01 (rounded up).
        assert_eq!(days_since_discord_epoch(1_795_413_600_000), 4345);
        assert_eq!(days_since_discord_epoch(1_420_070_400_000), 0);
        assert_eq!(days_since_discord_epoch(1_420_070_400_001), 1);
    }

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
    fn private_channel_payloads() {
        assert_eq!(
            recipients_body(&[Snowflake::new(5)]),
            json!({"recipients": ["5"]})
        );
        assert_eq!(
            recipients_body(&[Snowflake::new(5), Snowflake::new(80000000000000001)]),
            json!({"recipients": ["5", "80000000000000001"]})
        );
        assert_eq!(
            group_name_body(" Night Crew "),
            json!({"name": "Night Crew"})
        );
        assert_eq!(group_name_body("  "), json!({"name": null}));
    }

    #[test]
    fn image_uploads_are_data_uris_with_their_type() {
        assert!(image_data_uri(&[0x89, b'P', b'N', b'G', 1]).starts_with("data:image/png;base64,"));
        assert!(image_data_uri(&[0xFF, 0xD8, 0xFF]).starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn avatar_bodies_set_or_null_the_field() {
        // Removing sends an explicit null, not an omitted field.
        assert_eq!(avatar_body(None), json!({ "avatar": null }));
        // Setting sends a data URI of the image's detected type.
        let body = avatar_body(Some(&[0x89, b'P', b'N', b'G', 1]));
        let uri = body["avatar"].as_str().unwrap();
        assert!(uri.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn server_profile_edits_build_discords_payload() {
        // Keep everything: nothing is sent.
        let empty = ServerProfileEdit::default();
        assert!(empty.is_empty());
        assert_eq!(empty.body(), json!({}));

        // A nickname change only.
        let nick_only = ServerProfileEdit {
            nick: Some("Night Owl".into()),
            ..ServerProfileEdit::default()
        };
        assert!(!nick_only.is_empty());
        assert_eq!(nick_only.body(), json!({ "nick": "Night Owl" }));

        // Clearing text fields sends empty strings; removing the avatar
        // sends null.
        let clear = ServerProfileEdit {
            nick: Some(String::new()),
            bio: Some(String::new()),
            pronouns: Some(String::new()),
            avatar: AvatarEdit::Remove,
        };
        assert_eq!(
            clear.body(),
            json!({ "nick": "", "bio": "", "pronouns": "", "avatar": null })
        );

        // Setting an avatar yields a data URI; untouched text fields are
        // omitted.
        let set = ServerProfileEdit {
            bio: Some("per-server bio".into()),
            avatar: AvatarEdit::Set(vec![0x89, b'P', b'N', b'G', 1]),
            ..ServerProfileEdit::default()
        };
        let body = set.body();
        assert_eq!(body["bio"], json!("per-server bio"));
        assert!(body.get("nick").is_none());
        assert!(body.get("pronouns").is_none());
        assert!(
            body["avatar"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
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

/// Reads the web client's current build number from discord.com's app page
/// (as a browser opening Discord loads it) and sends it from now on, keeping
/// it for the next start. A stale build number marks a client as not the
/// website. On failure the last one known stays.
pub async fn refresh_build_number() {
    use kc_core::super_properties as identity;
    let client = match crate::browser::client_builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "no client to read the build number");
            return;
        }
    };
    let response = client
        .get("https://discord.com/app")
        .header("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("accept-language", "en-US,en;q=0.9")
        .header("sec-ch-ua", identity::sec_ch_ua())
        .header("sec-ch-ua-mobile", "?0")
        .header("sec-ch-ua-platform", "\"Windows\"")
        .header("sec-fetch-dest", "document")
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-site", "none")
        .header("sec-fetch-user", "?1")
        .header("upgrade-insecure-requests", "1")
        .send()
        .await;
    let html = match response {
        Ok(response) => {
            tracing::info!(version = ?response.version(), status = %response.status(), "read the app page");
            response.text().await.unwrap_or_default()
        }
        Err(error) => {
            tracing::warn!(%error, "could not read the build number");
            return;
        }
    };
    match identity::parse_build_number(&html) {
        Some(number) => {
            tracing::info!(number, "web client build number");
            identity::set_build_number(number);
            identity::save_build_number(number);
        }
        None => tracing::warn!("no build number in the app page"),
    }
}

/// Reads Chrome's current stable version from Google's version history and
/// keeps it for the next start, so the user agent follows Chrome's releases.
pub async fn refresh_chrome_version() {
    use kc_core::super_properties as identity;
    let client = match crate::browser::client_builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "no client to read Chrome's version");
            return;
        }
    };
    let response = client
        .get("https://versionhistory.googleapis.com/v1/chrome/platforms/win/channels/stable/versions?pageSize=5")
        .send()
        .await;
    let body = match response {
        Ok(response) => response.text().await.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%error, "could not read Chrome's version");
            return;
        }
    };
    match identity::parse_chrome_major(&body) {
        Some(major) => {
            tracing::info!(major, using = identity::chrome_major(), "Chrome's stable version");
            if major > identity::chrome_major() {
                identity::save_chrome_major(major);
            }
        }
        None => tracing::warn!("no version in Chrome's version history"),
    }
}
