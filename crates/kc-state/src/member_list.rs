//! The lazily loaded guild member list (gateway op 14 and
//! `GUILD_MEMBER_LIST_UPDATE`).

use kc_core::Snowflake;
use serde_json::Value;

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
}
