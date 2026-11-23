//! Client identity sent to Discord.
//!
//! This mirrors Abaddon's web-client identity rather than the desktop client.
//! The web shape is what Discord expects for a non-official client and is the
//! one Abaddon has kept working: `guild_hashes` (not `guild_versions`) in
//! `client_state`, capabilities `4605`, and a Chrome user agent.

use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use serde::Serialize;

/// The web client's build number when this was written. The current one is
/// read from discord.com at startup ([`set_build_number`]), and the last one
/// read is kept for the next start.
pub const CLIENT_BUILD_NUMBER: u64 = 679_055;
/// The gateway capabilities the web client identifies with (from its
/// bundle). They change READY's shape: settings come as a protobuf, guild
/// fields under `properties`, reactions in batches, and passive updates
/// for servers the client is not subscribed to.
pub const CAPABILITIES: u64 = 1_767_421;
/// The Chrome release the identity claims to be. Keep near Chrome's current
/// stable: an old browser stands out. The user agent, `browser_version` and
/// `sec-ch-ua` all follow from it.
pub const CHROME_MAJOR: u32 = 159;
/// Chrome reports only its major version since it reduced its user agent.
pub const BROWSER_VERSION: &str = "159.0.0.0";
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/159.0.0.0 Safari/537.36";

static BUILD_NUMBER: AtomicU64 = AtomicU64::new(CLIENT_BUILD_NUMBER);

/// The web client build number to send.
pub fn build_number() -> u64 {
    BUILD_NUMBER.load(Ordering::Relaxed)
}

/// Uses `number` from now on (connections made after this send it). An
/// implausible number is ignored.
pub fn set_build_number(number: u64) {
    if number >= 100_000 {
        BUILD_NUMBER.store(number, Ordering::Relaxed);
    }
}

/// The build number in discord.com's app page (`"BUILD_NUMBER":"679055"`
/// in its `GLOBAL_ENV`).
pub fn parse_build_number(html: &str) -> Option<u64> {
    let start = html.find("\"BUILD_NUMBER\"")? + "\"BUILD_NUMBER\"".len();
    let digits: String = html[start..]
        .trim_start_matches([':', ' ', '"'])
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok().filter(|number| *number >= 100_000)
}

/// Where the last build number read is kept.
fn build_number_cache() -> Option<std::path::PathBuf> {
    crate::paths::cache_dir().ok().map(|dir| dir.join("build-number"))
}

/// Uses the build number read on an earlier start, if there is one.
pub fn load_cached_build_number() {
    if let Some(number) = build_number_cache()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| text.trim().parse().ok())
    {
        set_build_number(number);
    }
}

/// Keeps `number` for the next start.
pub fn save_build_number(number: u64) {
    if let Some(path) = build_number_cache() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, number.to_string());
    }
}

/// The `sec-ch-ua` brand list Chrome sends for [`CHROME_MAJOR`]: Chromium,
/// Google Chrome and a "GREASE" brand, whose name, version and the order of
/// all three Chrome derives from its major version (Chromium's
/// `GenerateBrandVersionList`).
pub fn sec_ch_ua() -> String {
    sec_ch_ua_for(CHROME_MAJOR)
}

fn sec_ch_ua_for(major: u32) -> String {
    const ESCAPED: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
    const GREASED_VERSIONS: [&str; 3] = ["8", "99", "24"];
    const ORDERS: [[usize; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
    let seed = major as usize;
    let grease = format!(
        "\"Not{}A{}Brand\";v=\"{}\"",
        ESCAPED[seed % ESCAPED.len()],
        ESCAPED[(seed + 1) % ESCAPED.len()],
        GREASED_VERSIONS[seed % GREASED_VERSIONS.len()]
    );
    let chromium = format!("\"Chromium\";v=\"{major}\"");
    let chrome = format!("\"Google Chrome\";v=\"{major}\"");
    let order = ORDERS[seed % ORDERS.len()];
    let mut brands = [String::new(), String::new(), String::new()];
    brands[order[0]] = grease;
    brands[order[1]] = chromium;
    brands[order[2]] = chrome;
    brands.join(", ")
}

/// The identify/config properties object. Field names are Discord's, not ours.
#[derive(Clone, Debug, Serialize)]
pub struct SuperProperties {
    pub os: &'static str,
    pub browser: &'static str,
    pub device: &'static str,
    pub system_locale: &'static str,
    pub has_client_mods: bool,
    pub browser_user_agent: &'static str,
    pub browser_version: &'static str,
    pub os_version: &'static str,
    pub referrer: &'static str,
    pub referring_domain: &'static str,
    pub referrer_current: &'static str,
    pub referring_domain_current: &'static str,
    pub release_channel: &'static str,
    pub client_build_number: u64,
    pub client_event_source: Option<()>,
}

impl SuperProperties {
    pub fn new() -> Self {
        Self {
            os: "Windows",
            browser: "Chrome",
            device: "",
            system_locale: "en-US",
            has_client_mods: false,
            browser_user_agent: USER_AGENT,
            browser_version: BROWSER_VERSION,
            os_version: "10",
            referrer: "",
            referring_domain: "",
            referrer_current: "",
            referring_domain_current: "",
            release_channel: "stable",
            client_build_number: build_number(),
            client_event_source: None,
        }
    }

    /// Base64 of the ASCII JSON object, as sent in `X-Super-Properties`.
    pub fn as_header(&self) -> String {
        let json = serde_json::to_string(self).expect("super properties serialise");
        base64::engine::general_purpose::STANDARD.encode(json)
    }
}

impl Default for SuperProperties {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_base64_json_with_web_identity() {
        let props = SuperProperties::new();
        let header = props.as_header();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(header)
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["os"], "Windows");
        assert_eq!(value["browser"], "Chrome");
        assert_eq!(value["client_build_number"], build_number());
        assert!(
            value["browser_user_agent"]
                .as_str()
                .unwrap()
                .contains("Chrome/")
        );
        assert!(value.get("client_version").is_none());
    }

    #[test]
    fn identity_agrees_with_the_chrome_version() {
        assert!(USER_AGENT.contains(&format!("Chrome/{BROWSER_VERSION} ")));
        assert_eq!(BROWSER_VERSION, format!("{CHROME_MAJOR}.0.0.0"));
    }

    #[test]
    fn brand_list_matches_what_chrome_sends() {
        // Real Chrome 134's header.
        assert_eq!(
            sec_ch_ua_for(134),
            "\"Chromium\";v=\"134\", \"Not:A-Brand\";v=\"24\", \"Google Chrome\";v=\"134\""
        );
        assert!(sec_ch_ua().contains(&format!("\"Chromium\";v=\"{CHROME_MAJOR}\"")));
    }

    #[test]
    fn build_number_is_read_from_the_app_page() {
        let html = r#"<script>window.GLOBAL_ENV = {"NODE_ENV":"production","BUILD_NUMBER":"679055","PROJECT_ENV":"production"}</script>"#;
        assert_eq!(parse_build_number(html), Some(679_055));
        assert_eq!(parse_build_number("<html>no number</html>"), None);
        assert_eq!(parse_build_number(r#""BUILD_NUMBER":"12""#), None);
    }
}
