//! Credential acquisition and storage.
//!
//! Two credential shapes cover every supported provider: plain API keys
//! (OpenRouter, and later GLM/Kimi/Anthropic) and OAuth token sets with
//! refresh (ChatGPT/Codex). Everything lives in `~/.bullpen/auth.json` (or
//! `$BULLPEN_HOME/auth.json` when that is set to a non-empty path), written
//! atomically with mode 0600.
//!
//! This crate also implements `bullpen_llm::codex::TokenSource` twice:
//! [`codex::StoredCodex`] (bullpen's own login, refreshes and persists) and
//! [`codex::CodexCliBorrow`] (read-only reuse of `~/.codex/auth.json` — never
//! refreshed, so bullpen can't invalidate the Codex CLI's session).

pub mod codex;
pub mod jwt;
pub mod openrouter;
pub mod pkce;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("{0}")]
    Flow(String),
    #[error("credential store is corrupt: {0}")]
    Corrupt(#[from] serde_json::Error),
    #[error("no stored credential for `{0}`")]
    Missing(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        key: String,
    },
    Oauth {
        access_token: String,
        refresh_token: String,
        /// Unix seconds.
        expires_at: u64,
        account_id: String,
    },
}

/// The directory holding bullpen's own state: `$BULLPEN_HOME` when set,
/// otherwise `~/.bullpen`.
pub fn home_dir() -> PathBuf {
    resolve_home(std::env::var_os("BULLPEN_HOME"), std::env::home_dir())
}

/// `bullpen_home` is `$BULLPEN_HOME` and `home` is `$HOME` (the caller reads
/// the environment). An empty `BULLPEN_HOME` counts as unset: taken
/// literally it would put credentials in whatever directory the process
/// happened to start in.
fn resolve_home(bullpen_home: Option<OsString>, home: Option<PathBuf>) -> PathBuf {
    match bullpen_home {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home.unwrap_or_else(|| PathBuf::from(".")).join(".bullpen"),
    }
}

/// The on-disk credential store.
#[derive(Debug)]
pub struct AuthFile {
    path: PathBuf,
    providers: BTreeMap<String, Credential>,
}

impl AuthFile {
    pub fn default_path() -> PathBuf {
        home_dir().join("auth.json")
    }

    /// Load the store; a missing file is an empty store.
    pub fn load(path: &Path) -> Result<Self, AuthError> {
        let providers = match std::fs::read(path) {
            Ok(raw) => serde_json::from_slice::<Stored>(&raw)?.providers,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path: path.to_path_buf(),
            providers,
        })
    }

    pub fn get(&self, provider: &str) -> Option<&Credential> {
        self.providers.get(provider)
    }

    /// Insert or replace a credential and persist atomically (0600).
    pub fn set(&mut self, provider: &str, credential: Credential) -> Result<(), AuthError> {
        self.providers.insert(provider.to_string(), credential);
        self.save()
    }

    fn save(&self) -> Result<(), AuthError> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let body = serde_json::to_vec_pretty(&Stored {
            providers: self.providers.clone(),
        })?;

        let tmp = self.path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    providers: BTreeMap<String, Credential>,
}

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_bullpen_home_still_resolves_under_dot_bullpen() {
        assert_eq!(
            resolve_home(None, Some(PathBuf::from("/h"))).join("auth.json"),
            PathBuf::from("/h/.bullpen/auth.json")
        );
        // No $HOME either — the long-standing relative fallback.
        assert_eq!(
            resolve_home(None, None).join("auth.json"),
            PathBuf::from("./.bullpen/auth.json")
        );
    }

    #[test]
    fn bullpen_home_overrides_the_default_directory() {
        assert_eq!(
            resolve_home(Some("/tmp/pen".into()), Some(PathBuf::from("/h"))).join("auth.json"),
            PathBuf::from("/tmp/pen/auth.json")
        );
        assert_eq!(
            resolve_home(Some("".into()), Some(PathBuf::from("/h"))).join("auth.json"),
            PathBuf::from("/h/.bullpen/auth.json")
        );
    }

    #[test]
    fn roundtrip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        let mut file = AuthFile::load(&path).unwrap();
        assert!(file.get("openrouter").is_none());

        file.set(
            "openrouter",
            Credential::ApiKey {
                key: "sk-or-x".into(),
            },
        )
        .unwrap();
        file.set(
            "codex",
            Credential::Oauth {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_at: 123,
                account_id: "acct".into(),
            },
        )
        .unwrap();

        let reloaded = AuthFile::load(&path).unwrap();
        assert_eq!(
            reloaded.get("openrouter"),
            Some(&Credential::ApiKey {
                key: "sk-or-x".into()
            })
        );
        assert!(matches!(
            reloaded.get("codex"),
            Some(Credential::Oauth { account_id, .. }) if account_id == "acct"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut file = AuthFile::load(&path).unwrap();
        file.set("x", Credential::ApiKey { key: "k".into() })
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
