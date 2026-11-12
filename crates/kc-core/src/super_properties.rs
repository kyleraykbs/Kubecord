//! Client identity sent to Discord.
//!
//! Discord's user gateway and REST API look at `properties` /
//! `X-Super-Properties`; the shape below mirrors the official desktop client.
//! Keep the version constants in one place so they can be bumped together.

use base64::Engine;
use serde::Serialize;
use uuid::Uuid;

pub const CLIENT_VERSION: &str = "1.0.9210";
pub const CLIENT_BUILD_NUMBER: u64 = 455_964;
pub const NATIVE_BUILD_NUMBER: u64 = 69_976;
pub const BROWSER_VERSION: &str = "35.3.0";
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) discord/1.0.9210 Chrome/134.0.6998.205 Electron/35.3.0 Safari/537.36";

/// The identify/config properties object. Field names are Discord's, not ours.
#[derive(Clone, Debug, Serialize)]
pub struct SuperProperties {
    pub os: &'static str,
    pub browser: &'static str,
    pub device: &'static str,
    pub system_locale: &'static str,
    pub client_version: &'static str,
    pub release_channel: &'static str,
    pub os_version: &'static str,
    pub os_arch: &'static str,
    pub app_arch: &'static str,
    pub client_build_number: u64,
    pub native_build_number: u64,
    pub client_event_source: Option<()>,
    pub browser_user_agent: &'static str,
    pub browser_version: &'static str,
    pub os_sdk_version: &'static str,
    pub has_client_mods: bool,
    pub client_launch_id: Uuid,
    pub launch_signature: Uuid,
    pub client_heartbeat_session_id: Uuid,
    pub client_app_state: &'static str,
}

impl SuperProperties {
    pub fn new() -> Self {
        Self {
            os: "Windows",
            browser: "Discord Client",
            device: "",
            system_locale: "en-US",
            client_version: CLIENT_VERSION,
            release_channel: "stable",
            os_version: "10.0.19044",
            os_arch: "x64",
            app_arch: "x64",
            client_build_number: CLIENT_BUILD_NUMBER,
            native_build_number: NATIVE_BUILD_NUMBER,
            client_event_source: None,
            browser_user_agent: USER_AGENT,
            browser_version: BROWSER_VERSION,
            os_sdk_version: "19044",
            has_client_mods: false,
            client_launch_id: Uuid::new_v4(),
            launch_signature: Uuid::new_v4(),
            client_heartbeat_session_id: Uuid::new_v4(),
            client_app_state: "focused",
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
    fn header_is_base64_json_with_discord_keys() {
        let props = SuperProperties::new();
        let header = props.as_header();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(header)
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["os"], "Windows");
        assert_eq!(value["browser"], "Discord Client");
        assert_eq!(value["client_version"], CLIENT_VERSION);
        assert!(value["client_launch_id"].is_string());
    }
}
