//! ChatGPT/Codex subscription auth.
//!
//! Device-code flow against `auth.openai.com` using the public Codex CLI
//! client id (flow verified against neo's implementation and the shape of a
//! real `~/.codex/auth.json`). Two token sources feed the Codex provider:
//!
//! - [`StoredCodex`]: bullpen's own login. Owns its refresh token, refreshes
//!   proactively, persists rotations to bullpen's auth store.
//! - [`CodexCliBorrow`]: read-only reuse of the Codex CLI's `auth.json`.
//!   Deliberately never refreshes — a rotation could invalidate the CLI's
//!   own session — so an expired borrowed token asks the user to run either
//!   tool's login instead.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use bullpen_llm::ProviderError;
use bullpen_llm::codex::{CodexToken, TokenSource};

use crate::{AuthError, Credential, jwt, now_unix};

pub const PROVIDER: &str = "codex";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE: &str = "https://auth.openai.com";
const DEVICE_POLL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Refresh when within this window of expiry.
const EXPIRY_SKEW_SECS: u64 = 60;

pub struct DeviceCodePrompt {
    pub verification_url: String,
    pub user_code: String,
}

/// The device-code login flow. `on_code` fires once with the URL and code the
/// user must enter in a browser; the call blocks until approval or timeout.
pub struct CodexAuth {
    http: reqwest::Client,
    base: String,
}

impl Default for CodexAuth {
    fn default() -> Self {
        Self::new(AUTH_BASE)
    }
}

impl CodexAuth {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            base: base.into(),
        }
    }

    pub async fn login(
        &self,
        on_code: impl FnOnce(&DeviceCodePrompt),
    ) -> Result<Credential, AuthError> {
        let device = self.request_device_code().await?;
        on_code(&DeviceCodePrompt {
            verification_url: format!("{}/codex/device", self.base),
            user_code: device.user_code.clone(),
        });
        let approved = self.poll_device_code(&device).await?;
        self.exchange_code(&approved.authorization_code, &approved.code_verifier)
            .await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<Credential, AuthError> {
        let mut credential = self
            .post_token(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", CLIENT_ID),
            ])
            .await?;
        // A refresh response may omit a new refresh token; keep the old one.
        if let Credential::Oauth { refresh_token: rt, .. } = &mut credential
            && rt.is_empty()
        {
            *rt = refresh_token.to_string();
        }
        Ok(credential)
    }

    async fn request_device_code(&self) -> Result<DeviceCode, AuthError> {
        let url = format!("{}/api/accounts/deviceauth/usercode", self.base);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({"client_id": CLIENT_ID}))
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AuthError::Flow(format!(
                "device-code endpoint {status}: {body}"
            )));
        }
        let v: Value = serde_json::from_str(&body)
            .map_err(|e| AuthError::Flow(format!("decode device-code response: {e} ({body})")))?;

        let device_auth_id = v
            .get("device_auth_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // The endpoint has been seen using both `user_code` and `usercode`.
        let user_code = v
            .get("user_code")
            .or_else(|| v.get("usercode"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if device_auth_id.is_empty() || user_code.is_empty() {
            return Err(AuthError::Flow(format!(
                "device-code response missing device_auth_id or user_code: {body}"
            )));
        }
        // `interval` arrives as an integer or a string.
        let interval = match v.get("interval") {
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
            _ => 0,
        };
        Ok(DeviceCode {
            device_auth_id,
            user_code,
            interval: Duration::from_secs(if interval == 0 { 5 } else { interval }),
        })
    }

    async fn poll_device_code(&self, device: &DeviceCode) -> Result<Approved, AuthError> {
        let url = format!("{}/api/accounts/deviceauth/token", self.base);
        let deadline = tokio::time::Instant::now() + DEVICE_POLL_TIMEOUT;
        loop {
            let resp = self
                .http
                .post(&url)
                .json(&serde_json::json!({
                    "device_auth_id": device.device_auth_id,
                    "user_code": device.user_code,
                }))
                .send()
                .await?;
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();

            // 403/404 mean "user hasn't approved yet".
            if status != 403 && status != 404 {
                if status >= 400 {
                    return Err(AuthError::Flow(format!(
                        "device-code token endpoint {status}: {body}"
                    )));
                }
                let v: Value = serde_json::from_str(&body).map_err(|e| {
                    AuthError::Flow(format!("decode device-code token response: {e} ({body})"))
                })?;
                let authorization_code = v
                    .get("authorization_code")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let code_verifier = v
                    .get("code_verifier")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if authorization_code.is_empty() || code_verifier.is_empty() {
                    return Err(AuthError::Flow(format!(
                        "device-code token response missing fields: {body}"
                    )));
                }
                return Ok(Approved {
                    authorization_code,
                    code_verifier,
                });
            }

            if tokio::time::Instant::now() + device.interval > deadline {
                return Err(AuthError::Flow(format!(
                    "device-code login timed out after {}s",
                    DEVICE_POLL_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(device.interval).await;
        }
    }

    async fn exchange_code(&self, code: &str, verifier: &str) -> Result<Credential, AuthError> {
        let redirect = format!("{}/deviceauth/callback", self.base);
        self.post_token(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", &redirect),
        ])
        .await
    }

    async fn post_token(&self, form: &[(&str, &str)]) -> Result<Credential, AuthError> {
        let url = format!("{}/oauth/token", self.base);
        let resp = self.http.post(&url).form(form).send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AuthError::Flow(format!("token endpoint {status}: {body}")));
        }
        let v: Value = serde_json::from_str(&body)
            .map_err(|e| AuthError::Flow(format!("decode token response: {e} ({body})")))?;

        let access_token = v
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if access_token.is_empty() {
            return Err(AuthError::Flow(format!(
                "token response missing access_token: {body}"
            )));
        }
        let refresh_token = v
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let expires_in = v.get("expires_in").and_then(Value::as_u64).unwrap_or(0);
        let account_id = jwt::chatgpt_account_id(&access_token)?;

        Ok(Credential::Oauth {
            access_token,
            refresh_token,
            expires_at: now_unix() + expires_in,
            account_id,
        })
    }
}

struct DeviceCode {
    device_auth_id: String,
    user_code: String,
    interval: Duration,
}

struct Approved {
    authorization_code: String,
    code_verifier: String,
}

// ── Token sources ───────────────────────────────────────────────────────────

/// Bullpen-owned credentials from the auth store; refreshes and persists.
pub struct StoredCodex {
    auth_path: PathBuf,
    auth: CodexAuth,
}

impl StoredCodex {
    pub fn new(auth_path: PathBuf) -> Self {
        Self {
            auth_path,
            auth: CodexAuth::default(),
        }
    }
}

#[async_trait::async_trait]
impl TokenSource for StoredCodex {
    async fn token(&self) -> Result<CodexToken, ProviderError> {
        let fail = |e: AuthError| ProviderError::Failure(format!("codex auth: {e}"));

        let mut file = crate::AuthFile::load(&self.auth_path).map_err(fail)?;
        let Some(Credential::Oauth {
            access_token,
            refresh_token,
            expires_at,
            account_id,
        }) = file.get(PROVIDER).cloned()
        else {
            return Err(ProviderError::Failure(
                "no codex credentials — run `bullpen login codex`".into(),
            ));
        };

        if expires_at > now_unix() + EXPIRY_SKEW_SECS {
            return Ok(CodexToken {
                access_token,
                account_id,
            });
        }

        let refreshed = self.auth.refresh(&refresh_token).await.map_err(fail)?;
        file.set(PROVIDER, refreshed.clone()).map_err(fail)?;
        let Credential::Oauth {
            access_token,
            account_id,
            ..
        } = refreshed
        else {
            unreachable!("refresh returns oauth credentials");
        };
        Ok(CodexToken {
            access_token,
            account_id,
        })
    }
}

/// Read-only reuse of the Codex CLI's own `auth.json`. Never refreshes.
pub struct CodexCliBorrow {
    path: PathBuf,
}

impl CodexCliBorrow {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> PathBuf {
        std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".codex")
            .join("auth.json")
    }

    pub fn available(&self) -> bool {
        self.path.exists()
    }
}

#[async_trait::async_trait]
impl TokenSource for CodexCliBorrow {
    async fn token(&self) -> Result<CodexToken, ProviderError> {
        let raw = std::fs::read(&self.path).map_err(|e| {
            ProviderError::Failure(format!("cannot read {}: {e}", self.path.display()))
        })?;
        let v: Value = serde_json::from_slice(&raw).map_err(|e| {
            ProviderError::Failure(format!("cannot parse {}: {e}", self.path.display()))
        })?;

        let tokens = v.get("tokens").unwrap_or(&Value::Null);
        let access_token = tokens
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let account_id = tokens
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| jwt::chatgpt_account_id(&access_token).ok())
            .unwrap_or_default();
        if access_token.is_empty() || account_id.is_empty() {
            return Err(ProviderError::Failure(format!(
                "{} has no usable subscription tokens — run `codex` to log in, or `bullpen login codex`",
                self.path.display()
            )));
        }

        if let Some(exp) = jwt::expiry(&access_token)
            && exp <= now_unix() + EXPIRY_SKEW_SECS
        {
            return Err(ProviderError::Failure(
                "borrowed Codex CLI token is expired — run `codex` once to refresh it, \
                 or `bullpen login codex` for an independent session"
                    .into(),
            ));
        }

        Ok(CodexToken {
            access_token,
            account_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A tiny scripted HTTP server: routes requests by path substring.
    async fn serve(routes: Vec<(&'static str, Value)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 16 * 1024];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = request
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let body = routes
                    .iter()
                    .find(|(p, _)| path.contains(p))
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_else(|| "{}".into());
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    fn access_jwt(account: &str, exp: u64) -> String {
        jwt::craft_unsigned(&json!({
            "exp": exp,
            "https://api.openai.com/auth": {"chatgpt_account_id": account}
        }))
    }

    #[tokio::test]
    async fn full_device_flow() {
        let token = access_jwt("acct_9", now_unix() + 3600);
        let base = serve(vec![
            (
                "usercode",
                json!({"device_auth_id": "dev_1", "user_code": "ABCD-EFGH", "interval": "0"}),
            ),
            (
                "deviceauth/token",
                json!({"authorization_code": "authz", "code_verifier": "ver"}),
            ),
            (
                "oauth/token",
                json!({"access_token": token, "refresh_token": "rt_1", "expires_in": 3600}),
            ),
        ])
        .await;

        let mut prompted = None;
        let credential = CodexAuth::new(&base)
            .login(|p| prompted = Some((p.verification_url.clone(), p.user_code.clone())))
            .await
            .unwrap();

        let (url, code) = prompted.unwrap();
        assert!(url.ends_with("/codex/device"));
        assert_eq!(code, "ABCD-EFGH");
        let Credential::Oauth {
            refresh_token,
            account_id,
            expires_at,
            ..
        } = credential
        else {
            panic!("expected oauth credential");
        };
        assert_eq!(refresh_token, "rt_1");
        assert_eq!(account_id, "acct_9");
        assert!(expires_at > now_unix());
    }

    #[tokio::test]
    async fn refresh_keeps_old_token_when_omitted() {
        let token = access_jwt("acct_1", now_unix() + 3600);
        let base = serve(vec![(
            "oauth/token",
            json!({"access_token": token, "expires_in": 100}),
        )])
        .await;
        let credential = CodexAuth::new(&base).refresh("rt_old").await.unwrap();
        let Credential::Oauth { refresh_token, .. } = credential else {
            panic!("expected oauth");
        };
        assert_eq!(refresh_token, "rt_old");
    }

    #[tokio::test]
    async fn borrow_reads_fresh_cli_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "x",
                    "access_token": access_jwt("acct_cli", now_unix() + 3600),
                    "refresh_token": "rt",
                    "account_id": "acct_cli"
                },
                "last_refresh": "2026-08-07T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();

        let token = CodexCliBorrow::new(path).token().await.unwrap();
        assert_eq!(token.account_id, "acct_cli");
    }

    #[tokio::test]
    async fn borrow_rejects_expired_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            json!({
                "tokens": {
                    "access_token": access_jwt("acct_cli", now_unix() - 10),
                    "account_id": "acct_cli"
                }
            })
            .to_string(),
        )
        .unwrap();

        let err = CodexCliBorrow::new(path).token().await.unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
    }

    #[tokio::test]
    async fn stored_source_refreshes_and_persists() {
        let fresh = access_jwt("acct_s", now_unix() + 3600);
        let base = serve(vec![(
            "oauth/token",
            json!({"access_token": fresh, "refresh_token": "rt_new", "expires_in": 3600}),
        )])
        .await;

        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        let mut file = crate::AuthFile::load(&auth_path).unwrap();
        file.set(
            PROVIDER,
            Credential::Oauth {
                access_token: "stale".into(),
                refresh_token: "rt_old".into(),
                expires_at: now_unix() - 5,
                account_id: "acct_s".into(),
            },
        )
        .unwrap();

        let source = StoredCodex {
            auth_path: auth_path.clone(),
            auth: CodexAuth::new(&base),
        };
        let token = source.token().await.unwrap();
        assert_ne!(token.access_token, "stale");

        // Rotation persisted.
        let reloaded = crate::AuthFile::load(&auth_path).unwrap();
        let Some(Credential::Oauth { refresh_token, .. }) = reloaded.get(PROVIDER) else {
            panic!("expected stored oauth");
        };
        assert_eq!(refresh_token, "rt_new");
    }
}
