use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const APP: &str = "kylecord";

/// `$XDG_CONFIG_HOME/kylecord`, falling back to `~/.config/kylecord`.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir).join(APP));
    }
    dirs::config_dir()
        .map(|dir| dir.join(APP))
        .ok_or(Error::NoConfigDir)
}

/// `$XDG_DATA_HOME/kylecord`, falling back to `~/.local/share/kylecord`.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir).join(APP));
    }
    dirs::data_dir()
        .map(|dir| dir.join(APP))
        .ok_or(Error::NoConfigDir)
}

pub fn accounts_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("accounts.json"))
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

/// Restricts a path to its owner. Best effort on non-Unix platforms.
pub fn restrict(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(
            |source| Error::Io {
                path: path.to_owned(),
                source,
            },
        )?;
    }
    Ok(())
}
