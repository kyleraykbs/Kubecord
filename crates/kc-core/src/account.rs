use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::id::Snowflake;
use crate::paths;
use crate::token::Token;

/// One logged-in Discord account.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    /// Known after the first successful gateway READY.
    #[serde(default)]
    pub user_id: Option<Snowflake>,
    pub token: Token,
    #[serde(default)]
    pub label: Option<String>,
}

impl Account {
    pub fn display_name(&self) -> &str {
        self.label
            .as_deref()
            .unwrap_or_else(|| self.user_id.map(|_| "account").unwrap_or("pending login"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AccountsFile {
    version: u32,
    #[serde(default)]
    accounts: Vec<Account>,
}

impl Default for AccountsFile {
    fn default() -> Self {
        Self {
            version: 1,
            accounts: Vec::new(),
        }
    }
}

/// The on-disk account list.
///
/// Lives at `$XDG_CONFIG_HOME/kubecord/accounts.json`, mode 0600. Tokens are
/// plaintext on disk on purpose: this is a local, single-user client and the
/// alternative (a keyring dependency) adds a daemon requirement to a GTK app.
#[derive(Clone, Debug, Default)]
pub struct AccountStore {
    path: PathBuf,
    accounts: Vec<Account>,
}

impl AccountStore {
    pub fn load() -> Result<Self> {
        Self::load_from(paths::accounts_path()?)
    }

    pub fn load_from(path: PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                path,
                accounts: Vec::new(),
            });
        }

        let bytes = std::fs::read(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let file: AccountsFile = serde_json::from_slice(&bytes).map_err(|source| Error::Json {
            path: path.clone(),
            source,
        })?;
        Ok(Self {
            path,
            accounts: file.accounts,
        })
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            paths::ensure_dir(parent)?;
            paths::restrict(parent)?;
        }

        let file = AccountsFile {
            version: 1,
            accounts: self.accounts.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).expect("accounts serialise");

        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|source| Error::Io {
            path: tmp.clone(),
            source,
        })?;
        paths::restrict(&tmp)?;
        std::fs::rename(&tmp, &self.path).map_err(|source| Error::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Adds an account or replaces the token of an account with the same
    /// `user_id` (or, when the id is not known yet, the same token).
    pub fn upsert(&mut self, account: Account) {
        match self
            .accounts
            .iter()
            .position(|existing| match (existing.user_id, account.user_id) {
                (Some(a), Some(b)) => a == b,
                _ => existing.token == account.token,
            }) {
            Some(index) => self.accounts[index] = account,
            None => self.accounts.push(account),
        }
    }

    pub fn remove(&mut self, user_id: Snowflake) -> Result<Account> {
        let index = self
            .accounts
            .iter()
            .position(|account| account.user_id == Some(user_id))
            .ok_or(Error::UnknownAccount(user_id.get()))?;
        Ok(self.accounts.remove(index))
    }

    /// Removes the account with this exact token, whether or not its
    /// `user_id` is known yet. Returns `true` if one was removed.
    pub fn remove_by_token(&mut self, token: &Token) -> bool {
        let before = self.accounts.len();
        self.accounts.retain(|account| &account.token != token);
        self.accounts.len() != before
    }

    pub fn get(&self, user_id: Snowflake) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|account| account.user_id == Some(user_id))
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kubecord-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("accounts.json")
    }

    #[test]
    fn round_trips_accounts() {
        let path = temp_path("roundtrip");
        let mut store = AccountStore::load_from(path.clone()).unwrap();
        assert!(store.is_empty());

        store.upsert(Account {
            user_id: Some(Snowflake(7)),
            token: Token::new("abc"),
            label: Some("main".into()),
        });
        store.save().unwrap();

        let loaded = AccountStore::load_from(path.clone()).unwrap();
        assert_eq!(loaded.accounts().len(), 1);
        assert_eq!(loaded.get(Snowflake(7)).unwrap().display_name(), "main");
        assert_eq!(loaded.accounts()[0].token.expose(), "abc");

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn remove_by_token_drops_only_the_matching_account() {
        let mut store = AccountStore::load_from(temp_path("remove-token")).unwrap();
        store.upsert(Account {
            user_id: Some(Snowflake(7)),
            token: Token::new("keep"),
            label: None,
        });
        // A second account whose id is not known yet (never reached READY).
        store.upsert(Account {
            user_id: None,
            token: Token::new("pending"),
            label: Some("pending".into()),
        });
        assert_eq!(store.accounts().len(), 2);
        // Removing a token that is not present changes nothing.
        assert!(!store.remove_by_token(&Token::new("absent")));
        assert_eq!(store.accounts().len(), 2);
        // The pending account (no user_id) is removable by token.
        assert!(store.remove_by_token(&Token::new("pending")));
        assert_eq!(store.accounts().len(), 1);
        assert_eq!(store.accounts()[0].token.expose(), "keep");
    }

    #[test]
    fn upsert_replaces_by_user_id() {
        let mut store = AccountStore::load_from(temp_path("upsert")).unwrap();
        store.upsert(Account {
            user_id: Some(Snowflake(7)),
            token: Token::new("old"),
            label: None,
        });
        store.upsert(Account {
            user_id: Some(Snowflake(7)),
            token: Token::new("new"),
            label: Some("renamed".into()),
        });
        assert_eq!(store.accounts().len(), 1);
        assert_eq!(store.accounts()[0].token.expose(), "new");
    }
}
