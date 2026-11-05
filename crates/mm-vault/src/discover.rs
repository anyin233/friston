//! Finds vaults registered with the Obsidian desktop app.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::VaultError;

/// A vault known to Obsidian.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vault {
    /// Obsidian's internal vault id.
    pub id: String,
    /// Absolute folder path.
    pub path: PathBuf,
    /// Folder name; this is what `obsidian://open?vault=` expects.
    pub name: String,
    /// Whether the vault is currently open in Obsidian.
    pub open: bool,
}

#[derive(Deserialize)]
struct ObsidianJson {
    #[serde(default)]
    vaults: BTreeMap<String, VaultEntry>,
}

#[derive(Deserialize)]
struct VaultEntry {
    path: String,
    #[serde(default)]
    open: bool,
}

/// Default location of Obsidian's registry on macOS.
///
/// Returns: `~/Library/Application Support/obsidian/obsidian.json`, if a home dir exists.
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("obsidian").join("obsidian.json"))
}

/// Lists vaults from an `obsidian.json` registry file.
///
/// Params: `config` – path to `obsidian.json`.
/// Returns: vaults sorted with open ones first, then by name.
pub fn discover_vaults(config: &Path) -> Result<Vec<Vault>, VaultError> {
    let raw = std::fs::read_to_string(config).map_err(|source| VaultError::Config {
        path: config.display().to_string(),
        source,
    })?;
    let parsed: ObsidianJson =
        serde_json::from_str(&raw).map_err(|source| VaultError::ConfigJson {
            path: config.display().to_string(),
            source,
        })?;
    let mut vaults: Vec<Vault> = parsed
        .vaults
        .into_iter()
        .map(|(id, v)| {
            let path = PathBuf::from(v.path);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            Vault {
                id,
                path,
                name,
                open: v.open,
            }
        })
        .collect();
    vaults.sort_by(|a, b| b.open.cmp(&a.open).then_with(|| a.name.cmp(&b.name)));
    Ok(vaults)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_and_puts_open_vault_first() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("obsidian.json");
        std::fs::write(
            &cfg,
            r#"{"vaults":{"a1":{"path":"/Users/me/Archive","ts":1},"b2":{"path":"/Users/me/Work Notes","ts":2,"open":true}}}"#,
        )
        .unwrap();
        let v = discover_vaults(&cfg).unwrap();
        assert_eq!(v[0].name, "Work Notes");
        assert!(v[0].open);
        assert_eq!(v[1].path, PathBuf::from("/Users/me/Archive"));
    }

    #[test]
    fn missing_config_reports_path() {
        let err = discover_vaults(Path::new("/nonexistent/obsidian.json")).unwrap_err();
        assert!(err.to_string().contains("/nonexistent/obsidian.json"));
    }
}
