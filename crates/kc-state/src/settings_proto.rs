//! The parts of the `PreloadedUserSettings` protobuf the sidebar needs.
//!
//! With the USER_SETTINGS_PROTO capability (which the web client sends),
//! READY carries the account's settings as `user_settings_proto` (base64)
//! instead of the old JSON `user_settings`, and changes arrive as
//! USER_SETTINGS_PROTO_UPDATE. Field numbers are from the web client's
//! bundle (`discord_protos.discord_users.v1.PreloadedUserSettings`).

use base64::Engine;
use kc_core::Snowflake;

use crate::settings::{CustomStatus, GuildFolder};

/// `PreloadedUserSettings.status` (`StatusSettings`).
const STATUS: u64 = 11;
/// `PreloadedUserSettings.guild_folders` (`GuildFolders`).
const GUILD_FOLDERS: u64 = 14;

/// What one settings proto says. A field is `None` when the proto does not
/// carry it (in a partial update: it did not change).
#[derive(Debug, Default, PartialEq)]
pub struct ProtoSettings {
    pub status: Option<String>,
    /// `Some(None)`: the custom status was cleared (or has expired).
    pub custom_status: Option<Option<CustomStatus>>,
    pub guild_folders: Option<Vec<GuildFolder>>,
}

enum Field<'a> {
    Varint(u64),
    Fixed64(u64),
    Fixed32,
    Bytes(&'a [u8]),
}

fn varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The message's fields in order; stops at the first malformed one.
fn fields(bytes: &[u8]) -> Vec<(u64, Field<'_>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let Some(tag) = varint(bytes, &mut pos) else {
            break;
        };
        let field = match tag & 7 {
            0 => match varint(bytes, &mut pos) {
                Some(value) => Field::Varint(value),
                None => break,
            },
            1 => match bytes.get(pos..pos + 8) {
                Some(raw) => {
                    pos += 8;
                    Field::Fixed64(u64::from_le_bytes(raw.try_into().unwrap_or_default()))
                }
                None => break,
            },
            2 => {
                let Some(len) = varint(bytes, &mut pos) else {
                    break;
                };
                let Some(raw) = pos
                    .checked_add(len as usize)
                    .and_then(|end| bytes.get(pos..end))
                else {
                    break;
                };
                pos += raw.len();
                Field::Bytes(raw)
            }
            5 => {
                if pos + 4 > bytes.len() {
                    break;
                }
                pos += 4;
                Field::Fixed32
            }
            _ => break,
        };
        out.push((tag >> 3, field));
    }
    out
}

fn string(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// The value of a `google.protobuf.*Value` wrapper (its field 1).
fn wrapped<'a>(bytes: &'a [u8]) -> Option<Field<'a>> {
    fields(bytes)
        .into_iter()
        .find(|(number, _)| *number == 1)
        .map(|(_, field)| field)
}

fn wrapped_string(bytes: &[u8]) -> Option<String> {
    match wrapped(bytes) {
        Some(Field::Bytes(raw)) => string(raw),
        // An empty wrapper is an empty string.
        None => Some(String::new()),
        _ => None,
    }
}

fn wrapped_number(bytes: &[u8]) -> Option<u64> {
    match wrapped(bytes) {
        Some(Field::Varint(value)) => Some(value),
        None => Some(0),
        _ => None,
    }
}

fn custom_status(bytes: &[u8], now_ms: u64) -> Option<CustomStatus> {
    let mut status = CustomStatus {
        text: None,
        emoji_name: None,
        emoji_id: None,
    };
    let mut expires_at = 0;
    for (number, field) in fields(bytes) {
        match (number, field) {
            (1, Field::Bytes(raw)) => status.text = string(raw).filter(|text| !text.is_empty()),
            (2, Field::Fixed64(id)) if id != 0 => status.emoji_id = Some(Snowflake(id)),
            (3, Field::Bytes(raw)) => {
                status.emoji_name = string(raw).filter(|name| !name.is_empty());
            }
            (4, Field::Fixed64(at)) => expires_at = at,
            _ => {}
        }
    }
    let expired = expires_at != 0 && expires_at <= now_ms;
    let empty = status.text.is_none() && status.emoji_name.is_none() && status.emoji_id.is_none();
    (!expired && !empty).then_some(status)
}

fn guild_folder(bytes: &[u8]) -> GuildFolder {
    let mut folder = GuildFolder::default();
    for (number, field) in fields(bytes) {
        match (number, field) {
            // Packed (the usual encoding) or one id per field.
            (1, Field::Bytes(raw)) => folder.guild_ids.extend(
                raw.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|id| Snowflake(u64::from_le_bytes(*id))),
            ),
            (1, Field::Fixed64(id)) => folder.guild_ids.push(Snowflake(id)),
            (2, Field::Bytes(raw)) => folder.id = wrapped_number(raw).map(|id| id as i64),
            (3, Field::Bytes(raw)) => folder.name = wrapped_string(raw),
            (4, Field::Bytes(raw)) => folder.color = wrapped_number(raw).map(|color| color as u32),
            _ => {}
        }
    }
    folder
}

fn guild_folders(bytes: &[u8]) -> Vec<GuildFolder> {
    let mut folders = Vec::new();
    let mut positions = Vec::new();
    for (number, field) in fields(bytes) {
        match (number, field) {
            (1, Field::Bytes(raw)) => folders.push(guild_folder(raw)),
            (2, Field::Bytes(raw)) => positions.extend(
                raw.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|id| Snowflake(u64::from_le_bytes(*id))),
            ),
            (2, Field::Fixed64(id)) => positions.push(Snowflake(id)),
            _ => {}
        }
    }
    if folders.is_empty() {
        // Only positions: each guild on its own.
        folders = positions
            .into_iter()
            .map(|id| GuildFolder {
                guild_ids: vec![id],
                ..GuildFolder::default()
            })
            .collect();
    }
    folders
}

/// Decodes a base64 `PreloadedUserSettings`. `partial` is whether it is a
/// partial update, where a status without a custom status leaves the custom
/// status alone.
pub fn decode(base64: &str, partial: bool, now_ms: u64) -> Option<ProtoSettings> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64.trim())
        .ok()?;
    let mut settings = ProtoSettings::default();
    for (number, field) in fields(&bytes) {
        let Field::Bytes(raw) = field else {
            continue;
        };
        match number {
            STATUS => {
                let mut custom = None;
                for (number, field) in fields(raw) {
                    match (number, field) {
                        (1, Field::Bytes(status)) => settings.status = wrapped_string(status),
                        (2, Field::Bytes(status)) => custom = Some(custom_status(status, now_ms)),
                        _ => {}
                    }
                }
                settings.custom_status = match custom {
                    Some(custom) => Some(custom),
                    None if partial => None,
                    None => Some(None),
                };
            }
            GUILD_FOLDERS => settings.guild_folders = Some(guild_folders(raw)),
            _ => {}
        }
    }
    if !partial && settings.custom_status.is_none() {
        settings.custom_status = Some(None);
    }
    Some(settings)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn tag(out: &mut Vec<u8>, number: u64, wire: u64) {
        varint_into(out, (number << 3) | wire);
    }

    fn varint_into(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    pub(crate) fn message(out: &mut Vec<u8>, number: u64, body: &[u8]) {
        tag(out, number, 2);
        varint_into(out, body.len() as u64);
        out.extend_from_slice(body);
    }

    fn wrapper_string(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        message(&mut out, 1, text.as_bytes());
        out
    }

    fn wrapper_number(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        tag(&mut out, 1, 0);
        varint_into(&mut out, value);
        out
    }

    /// A settings proto like Discord's: status `dnd` with a custom status,
    /// one named folder of two guilds and one guild on its own.
    pub(crate) fn sample() -> String {
        let mut custom = Vec::new();
        message(&mut custom, 1, b"coding");
        message(&mut custom, 3, "\u{1f4bb}".as_bytes());
        let mut status = Vec::new();
        message(&mut status, 1, &wrapper_string("dnd"));
        message(&mut status, 2, &custom);

        let ids = |ids: &[u64]| ids.iter().flat_map(|id| id.to_le_bytes()).collect::<Vec<_>>();
        let mut folder = Vec::new();
        message(&mut folder, 1, &ids(&[10, 11]));
        message(&mut folder, 2, &wrapper_number(77));
        message(&mut folder, 3, &wrapper_string("Games"));
        message(&mut folder, 4, &wrapper_number(0x5865f2));
        let mut single = Vec::new();
        message(&mut single, 1, &ids(&[12]));
        let mut folders = Vec::new();
        message(&mut folders, 1, &folder);
        message(&mut folders, 1, &single);
        message(&mut folders, 2, &ids(&[10, 11, 12]));

        let mut settings = Vec::new();
        message(&mut settings, 6, b"\x08\x01"); // text_and_images: skipped
        message(&mut settings, STATUS, &status);
        message(&mut settings, GUILD_FOLDERS, &folders);
        base64::engine::general_purpose::STANDARD.encode(settings)
    }

    #[test]
    fn reads_status_custom_status_and_folders() {
        let settings = decode(&sample(), false, 0).unwrap();
        assert_eq!(settings.status.as_deref(), Some("dnd"));
        let custom = settings.custom_status.unwrap().unwrap();
        assert_eq!(custom.text.as_deref(), Some("coding"));
        assert_eq!(custom.emoji_name.as_deref(), Some("\u{1f4bb}"));
        let folders = settings.guild_folders.unwrap();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].id, Some(77));
        assert_eq!(folders[0].name.as_deref(), Some("Games"));
        assert_eq!(folders[0].color, Some(0x5865f2));
        assert_eq!(folders[0].guild_ids, vec![Snowflake(10), Snowflake(11)]);
        assert!(folders[0].is_folder() && !folders[1].is_folder());
    }

    #[test]
    fn partial_updates_leave_out_what_did_not_change() {
        let mut status = Vec::new();
        message(&mut status, 1, &wrapper_string("idle"));
        let mut settings = Vec::new();
        message(&mut settings, STATUS, &status);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&settings);
        let partial = decode(&encoded, true, 0).unwrap();
        assert_eq!(partial.status.as_deref(), Some("idle"));
        assert_eq!(partial.custom_status, None);
        assert_eq!(partial.guild_folders, None);
        // A full proto without one means there is none.
        assert_eq!(decode(&encoded, false, 0).unwrap().custom_status, Some(None));
        assert!(decode("not base64!", false, 0).is_none());
    }

    #[test]
    fn an_expired_custom_status_is_none() {
        let mut custom = Vec::new();
        message(&mut custom, 1, b"brb");
        tag(&mut custom, 4, 1);
        custom.extend(1_000u64.to_le_bytes());
        let mut status = Vec::new();
        message(&mut status, 2, &custom);
        let mut settings = Vec::new();
        message(&mut settings, STATUS, &status);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&settings);
        assert!(decode(&encoded, false, 999).unwrap().custom_status.unwrap().is_some());
        assert_eq!(decode(&encoded, false, 1_000).unwrap().custom_status, Some(None));
    }
}
