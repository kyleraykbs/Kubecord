//! The lazily loaded guild member list (gateway op 14 and
//! `GUILD_MEMBER_LIST_UPDATE`).

use kc_core::Snowflake;
use serde_json::Value;

use crate::channel::Overwrite;
use crate::permissions::VIEW_CHANNEL;

/// Discord identifies a guild's member list by its *audience*: the default
/// `"everyone"`, or a hash of the channel's view-permission overwrites when the
/// channel is restricted. A guild therefore has one member list per distinct
/// audience, keyed by this id -- storing a single list per guild lets an update
/// for one audience wipe another's rows. This computes the id for a channel the
/// same way the official client does (MurmurHash3 of the sorted overwrite
/// strings), so the right list can be selected for display.
pub fn compute_list_id(overwrites: &[Overwrite], _guild_id: Snowflake) -> String {
    let mut parts: Vec<String> = Vec::new();
    for overwrite in overwrites {
        if overwrite.allow & VIEW_CHANNEL != 0 {
            parts.push(format!("allow:{}", overwrite.id));
        } else if overwrite.deny & VIEW_CHANNEL != 0 {
            parts.push(format!("deny:{}", overwrite.id));
        }
    }
    if parts.is_empty() {
        return "everyone".to_owned();
    }
    parts.sort();
    murmur3_32(parts.join(",").as_bytes(), 0).to_string()
}

/// MurmurHash3 x86 32-bit, the variant Discord uses to key member lists.
fn murmur3_32(data: &[u8], seed: u32) -> u32 {
    const C1: u32 = 0xcc9e_2d51;
    const C2: u32 = 0x1b87_3593;
    let mut hash = seed;
    let mut chunks = data.chunks_exact(4);
    for chunk in &mut chunks {
        let mut k = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        k = k.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        hash ^= k;
        hash = hash.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    let tail = chunks.remainder();
    if !tail.is_empty() {
        let mut k: u32 = 0;
        for (i, &byte) in tail.iter().enumerate() {
            k ^= u32::from(byte) << (8 * i);
        }
        k = k.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        hash ^= k;
    }
    hash ^= data.len() as u32;
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85eb_ca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2_ae35);
    hash ^= hash >> 16;
    hash
}

/// One row of the member list, in render order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberListItem {
    /// A group header: a hoisted role id, or `online` / `offline`.
    Group {
        id: String,
        count: u32,
    },
    Member {
        user_id: Snowflake,
    },
}

/// The member list of one guild as the server last described it. Rows that
/// were never synced (outside the subscribed ranges) are `None`.
#[derive(Clone, Debug, Default)]
pub struct MemberList {
    pub id: String,
    pub member_count: u32,
    pub online_count: u32,
    /// Member count per group id. Group rows inside `items` do not always
    /// carry their count; this table is refreshed with every update.
    pub group_counts: std::collections::HashMap<String, u32>,
    pub items: Vec<Option<MemberListItem>>,
}

impl MemberList {
    pub fn parse_item(item: &Value) -> Option<MemberListItem> {
        if let Some(group) = item.get("group") {
            return Some(MemberListItem::Group {
                id: group["id"].as_str()?.to_owned(),
                count: group["count"].as_u64().unwrap_or(0) as u32,
            });
        }
        let user_id = item
            .pointer("/member/user/id")
            .and_then(Value::as_str)?
            .parse()
            .ok()?;
        Some(MemberListItem::Member { user_id })
    }

    /// SYNC: replaces the rows starting at `start`, growing the list.
    pub fn sync(&mut self, start: usize, items: Vec<Option<MemberListItem>>) {
        let end = start + items.len();
        if self.items.len() < end {
            self.items.resize(end, None);
        }
        for (offset, item) in items.into_iter().enumerate() {
            self.items[start + offset] = item;
        }
    }

    pub fn insert(&mut self, index: usize, item: Option<MemberListItem>) {
        if index <= self.items.len() {
            self.items.insert(index, item);
        }
    }

    pub fn update(&mut self, index: usize, item: Option<MemberListItem>) {
        if let Some(slot) = self.items.get_mut(index) {
            *slot = item;
        }
    }

    pub fn delete(&mut self, index: usize) {
        if index < self.items.len() {
            self.items.remove(index);
        }
    }

    pub fn invalidate(&mut self, start: usize, end: usize) {
        for slot in self
            .items
            .iter_mut()
            .skip(start)
            .take(end.saturating_sub(start) + 1)
        {
            *slot = None;
        }
    }

    /// Known rows, in order.
    pub fn rows(&self) -> impl Iterator<Item = &MemberListItem> {
        self.items.iter().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u64) -> Option<MemberListItem> {
        Some(MemberListItem::Member {
            user_id: Snowflake(id),
        })
    }

    #[test]
    fn ops_shift_rows_like_the_server_expects() {
        let mut list = MemberList::default();
        list.sync(0, vec![member(1), member(2), member(3)]);
        list.insert(1, member(9));
        assert_eq!(list.items, vec![member(1), member(9), member(2), member(3)]);
        list.delete(0);
        list.update(0, member(8));
        assert_eq!(list.items, vec![member(8), member(2), member(3)]);
        list.invalidate(1, 2);
        assert_eq!(list.rows().count(), 1);
        // Out-of-range ops are ignored rather than panicking.
        list.delete(50);
        list.update(50, member(1));
        list.insert(50, member(1));
        assert_eq!(list.items.len(), 3);
    }

    #[test]
    fn murmur3_matches_known_vectors() {
        // The canonical MurmurHash3 x86-32 test vectors (seed 0).
        assert_eq!(murmur3_32(b"", 0), 0);
        assert_eq!(murmur3_32(b"hello", 0), 613_153_351);
        assert_eq!(murmur3_32(b"The quick brown fox jumps over the lazy dog", 0), 776_992_547);
    }

    #[test]
    fn list_id_is_everyone_without_view_overwrites() {
        // No overwrites, or overwrites that do not touch VIEW_CHANNEL, are the
        // default audience.
        assert_eq!(compute_list_id(&[], Snowflake(1)), "everyone");
        let other = Overwrite {
            id: Snowflake(2),
            kind: Overwrite::ROLE,
            allow: 1 << 11,
            deny: 0,
        };
        assert_eq!(compute_list_id(&[other], Snowflake(1)), "everyone");
    }

    #[test]
    fn list_id_hashes_view_overwrites() {
        // A deny of VIEW_CHANNEL for @everyone plus an allow for a role hashes
        // the sorted "deny:.../allow:..." strings -- a stable, non-"everyone" id.
        let overwrites = vec![
            Overwrite {
                id: Snowflake(1),
                kind: Overwrite::ROLE,
                allow: 0,
                deny: VIEW_CHANNEL,
            },
            Overwrite {
                id: Snowflake(42),
                kind: Overwrite::ROLE,
                allow: VIEW_CHANNEL,
                deny: 0,
            },
        ];
        let id = compute_list_id(&overwrites, Snowflake(1));
        assert_ne!(id, "everyone");
        // sorted(["deny:1", "allow:42"]) = ["allow:42", "deny:1"] -> "allow:42,deny:1"
        assert_eq!(id, murmur3_32(b"allow:42,deny:1", 0).to_string());
    }
}
