//! Discord CDN URLs and unauthenticated media downloads.
//!
//! Nothing here sends the account token: media hosts (the Discord CDN, the
//! media proxy, and third-party embed hosts) must never see it.

use std::sync::LazyLock;

use kc_core::Snowflake;

use crate::error::Result;

pub const CDN_BASE: &str = "https://cdn.discordapp.com";
pub const MEDIA_BASE: &str = "https://media.discordapp.net";

/// Largest single download the client will buffer (attachments previews, GIFs).
const MAX_DOWNLOAD_BYTES: usize = 32 * 1024 * 1024;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(kc_core::super_properties::USER_AGENT)
        .https_only(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("cdn client")
});

/// CDN sizes are powers of two between 16 and 4096.
fn cdn_size(pixels: u32) -> u32 {
    pixels.clamp(16, 4096).next_power_of_two()
}

/// An animated asset hash starts with `a_`.
pub fn is_animated_hash(hash: &str) -> bool {
    hash.starts_with("a_")
}

/// A user avatar, or the default avatar when the user has none.
pub fn user_avatar(user_id: Snowflake, avatar: Option<&str>, pixels: u32) -> String {
    match avatar {
        Some(hash) => format!(
            "{CDN_BASE}/avatars/{user_id}/{hash}.png?size={}",
            cdn_size(pixels)
        ),
        None => default_avatar(user_id),
    }
}

/// Default avatar for users on the new username system: `(id >> 22) % 6`.
pub fn default_avatar(user_id: Snowflake) -> String {
    format!("{CDN_BASE}/embed/avatars/{}.png", (user_id.get() >> 22) % 6)
}

/// A per-guild member avatar.
pub fn member_avatar(guild_id: Snowflake, user_id: Snowflake, hash: &str, pixels: u32) -> String {
    format!(
        "{CDN_BASE}/guilds/{guild_id}/users/{user_id}/avatars/{hash}.png?size={}",
        cdn_size(pixels)
    )
}

pub fn guild_icon(guild_id: Snowflake, hash: &str, pixels: u32) -> String {
    format!(
        "{CDN_BASE}/icons/{guild_id}/{hash}.png?size={}",
        cdn_size(pixels)
    )
}

/// An app's icon.
pub fn app_icon(app_id: Snowflake, hash: &str, pixels: u32) -> String {
    format!(
        "{CDN_BASE}/app-icons/{app_id}/{hash}.png?size={}",
        cdn_size(pixels)
    )
}

/// Group DM icon.
pub fn channel_icon(channel_id: Snowflake, hash: &str, pixels: u32) -> String {
    format!(
        "{CDN_BASE}/channel-icons/{channel_id}/{hash}.png?size={}",
        cdn_size(pixels)
    )
}

pub fn guild_banner(guild_id: Snowflake, hash: &str, pixels: u32) -> String {
    format!(
        "{CDN_BASE}/banners/{guild_id}/{hash}.png?size={}",
        cdn_size(pixels)
    )
}

pub fn user_banner(user_id: Snowflake, hash: &str, pixels: u32) -> String {
    format!(
        "{CDN_BASE}/banners/{user_id}/{hash}.png?size={}",
        cdn_size(pixels)
    )
}

/// A member's server-specific profile banner.
pub fn member_banner(guild_id: Snowflake, user_id: Snowflake, hash: &str, pixels: u32) -> String {
    format!(
        "{CDN_BASE}/guilds/{guild_id}/users/{user_id}/banners/{hash}.png?size={}",
        cdn_size(pixels)
    )
}

/// A custom emoji. `animated` selects the GIF; otherwise a static PNG.
pub fn emoji(emoji_id: Snowflake, animated: bool, pixels: u32) -> String {
    let extension = if animated { "gif" } else { "png" };
    format!(
        "{CDN_BASE}/emojis/{emoji_id}.{extension}?size={}",
        cdn_size(pixels)
    )
}

/// A sticker image. Format types: 1 PNG, 2 APNG, 3 Lottie (JSON, not an
/// image), 4 GIF. GIF stickers are only served from the media proxy.
pub fn sticker(sticker_id: Snowflake, format_type: u8) -> Option<String> {
    match format_type {
        1 | 2 => Some(format!("{MEDIA_BASE}/stickers/{sticker_id}.png?size=160")),
        4 => Some(format!("{MEDIA_BASE}/stickers/{sticker_id}.gif?size=160")),
        _ => None,
    }
}

/// Asks the media proxy for a downscaled copy of an attachment or embed image.
/// Only meaningful for `media.discordapp.net` URLs; others are returned as is.
pub fn resized(proxy_url: &str, width: u32, height: u32) -> String {
    if !proxy_url.starts_with(MEDIA_BASE) {
        return proxy_url.to_owned();
    }
    let separator = if proxy_url.contains('?') { '&' } else { '?' };
    format!("{proxy_url}{separator}width={width}&height={height}")
}

/// Downloads a media URL without credentials.
pub async fn fetch(url: &str) -> Result<Vec<u8>> {
    let response = CLIENT.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_DOWNLOAD_BYTES)
    {
        return Err(crate::Error::TooLarge);
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err(crate::Error::TooLarge);
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatar_sizes_round_up_to_a_power_of_two() {
        let id = Snowflake::new(80351110224678912);
        assert_eq!(
            user_avatar(id, Some("abc"), 40),
            "https://cdn.discordapp.com/avatars/80351110224678912/abc.png?size=64"
        );
    }

    #[test]
    fn missing_avatar_uses_the_default_set() {
        let id = Snowflake::new(80351110224678912);
        let url = user_avatar(id, None, 40);
        assert!(url.starts_with("https://cdn.discordapp.com/embed/avatars/"));
        assert!(url.ends_with(".png"));
    }

    #[test]
    fn animated_emoji_use_gif() {
        let id = Snowflake::new(1);
        assert!(emoji(id, true, 48).contains("/emojis/1.gif?size=64"));
        assert!(emoji(id, false, 48).contains("/emojis/1.png?size=64"));
    }

    #[test]
    fn resize_only_touches_the_media_proxy() {
        assert_eq!(
            resized(
                "https://media.discordapp.net/attachments/1/2/a.png",
                400,
                300
            ),
            "https://media.discordapp.net/attachments/1/2/a.png?width=400&height=300"
        );
        assert_eq!(
            resized("https://example.com/a.png", 400, 300),
            "https://example.com/a.png"
        );
    }
}
