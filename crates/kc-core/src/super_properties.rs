//! Client identity sent to Discord.
//!
//! This mirrors Abaddon's web-client identity rather than the desktop client.
//! The web shape is what Discord expects for a non-official client and is the
//! one Abaddon has kept working: `guild_hashes` (not `guild_versions`) in
//! `client_state`, capabilities `4605`, and a Chrome user agent.

use base64::Engine;
use serde::Serialize;

pub const CLIENT_BUILD_NUMBER: u64 = 455_964;
/// Abaddon sends 4605; bit 12 is needed for CALL_CREATE.
pub const CAPABILITIES: u64 = 4_605;
pub const BROWSER_VERSION: &str = "134.0.6998.205";
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.6998.205 Safari/537.36";

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
            client_build_number: CLIENT_BUILD_NUMBER,
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
        assert_eq!(value["client_build_number"], CLIENT_BUILD_NUMBER);
        assert!(
            value["browser_user_agent"]
                .as_str()
                .unwrap()
                .contains("Chrome/")
        );
        assert!(value.get("client_version").is_none());
    }
}
