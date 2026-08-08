//! OpenRouter OAuth PKCE flow.
//!
//! This is OpenRouter's officially supported third-party app flow (verified
//! against their docs 2026-08-07): browser authorization at
//! `https://openrouter.ai/auth`, then a code-for-key exchange at
//! `POST /api/v1/auth/keys`. The exchange returns a plain API key — there are
//! no refresh tokens. Localhost callbacks work on any port; headless mode
//! omits the callback and shows the code on screen for pasting.

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::AuthError;

pub const PROVIDER: &str = "openrouter";
const AUTH_HOST: &str = "https://openrouter.ai";

/// The browser URL that starts authorization. Without a callback the code is
/// displayed on-screen for pasting (headless mode).
pub fn authorize_url(challenge: &str, callback_url: Option<&str>) -> String {
    match callback_url {
        Some(cb) => format!(
            "{AUTH_HOST}/auth?callback_url={}&code_challenge={challenge}&code_challenge_method=S256",
            urlencode(cb)
        ),
        None => format!(
            "{AUTH_HOST}/auth?key_label=bullpen&code_challenge={challenge}&code_challenge_method=S256"
        ),
    }
}

/// Exchange an authorization code for an API key. `base` overrides the host
/// for tests.
pub async fn exchange(
    http: &reqwest::Client,
    base: Option<&str>,
    code: &str,
    verifier: &str,
) -> Result<String, AuthError> {
    let url = format!("{}/api/v1/auth/keys", base.unwrap_or(AUTH_HOST));
    let resp = http
        .post(&url)
        .json(&json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256",
        }))
        .send()
        .await?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AuthError::Flow(format!(
            "openrouter key exchange failed ({status}): {body}"
        )));
    }
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("key").and_then(|k| k.as_str()).map(str::to_string))
        .ok_or_else(|| AuthError::Flow(format!("key exchange response missing `key`: {body}")))
}

/// Serve exactly one localhost callback and return the `code` query value.
/// The caller owns the listener so it can print the port before browsing.
pub async fn capture_code(listener: TcpListener) -> Result<String, AuthError> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8 * 1024];
        let n = stream.read(&mut buf).await?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let Some(target) = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
        else {
            continue;
        };
        // Browsers also ask for /favicon.ico; only the callback ends the loop.
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let mut code = None;
        let mut error = None;
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("code", v)) if !v.is_empty() => code = Some(urldecode(v)),
                Some(("error", v)) => error = Some(urldecode(v)),
                _ => {}
            }
        }

        if code.is_none() && error.is_none() {
            let _ = respond(&mut stream, "404 Not Found", "not the callback").await;
            continue;
        }
        let (status, page) = if code.is_some() {
            (
                "200 OK",
                "bullpen is connected to OpenRouter. You can close this tab.",
            )
        } else {
            (
                "200 OK",
                "Authorization was not completed. You can close this tab.",
            )
        };
        let _ = respond(&mut stream, status, page).await;

        return match (code, error) {
            (Some(code), _) => Ok(code),
            (None, Some(err)) => Err(AuthError::Flow(format!("authorization denied: {err}"))),
            (None, None) => unreachable!(),
        };
    }
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let page = format!("<html><body style=\"font-family: sans-serif\"><p>{body}</p></body></html>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Open a browser at `url`, best-effort. Returns whether a launcher spawned.
pub fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let launcher = "open";
    #[cfg(not(target_os = "macos"))]
    let launcher = "xdg-open";
    std::process::Command::new(launcher)
        .arg(url)
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn authorize_url_shapes() {
        let with_cb = authorize_url("CHAL", Some("http://localhost:8123/callback"));
        assert!(with_cb.contains("callback_url=http%3A%2F%2Flocalhost%3A8123%2Fcallback"));
        assert!(with_cb.contains("code_challenge=CHAL"));
        assert!(with_cb.contains("code_challenge_method=S256"));

        let headless = authorize_url("CHAL", None);
        assert!(headless.contains("key_label=bullpen"));
        assert!(!headless.contains("callback_url"));
    }

    #[tokio::test]
    async fn captures_code_and_ignores_favicon() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let capture = tokio::spawn(capture_code(listener));

        // A stray request first — must not terminate the wait.
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;

        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /callback?code=abc%2F123 HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;
        assert!(String::from_utf8_lossy(&buf).contains("200 OK"));

        assert_eq!(capture.await.unwrap().unwrap(), "abc/123");
    }

    #[tokio::test]
    async fn denial_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let capture = tokio::spawn(capture_code(listener));

        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /callback?error=access_denied HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;

        let err = capture.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("access_denied"));
    }

    #[tokio::test]
    async fn exchange_parses_key() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf).await.unwrap();
            let body = r#"{"key":"sk-or-v1-test"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
        });

        let key = exchange(
            &reqwest::Client::new(),
            Some(&format!("http://{addr}")),
            "code",
            "verifier",
        )
        .await
        .unwrap();
        assert_eq!(key, "sk-or-v1-test");
    }
}
