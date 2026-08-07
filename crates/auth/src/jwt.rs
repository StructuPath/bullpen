//! Just enough JWT reading to pull claims out of OpenAI access tokens.
//! No signature verification — these tokens are consumed, not trusted.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

use crate::AuthError;

pub fn claims(token: &str) -> Result<Value, AuthError> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| AuthError::Flow("access token is not a JWT".into()))?;
    let raw = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|e| AuthError::Flow(format!("decode token payload: {e}")))?;
    serde_json::from_slice(&raw).map_err(|e| AuthError::Flow(format!("parse token claims: {e}")))
}

/// The `exp` claim as unix seconds, when present.
pub fn expiry(token: &str) -> Option<u64> {
    claims(token).ok()?.get("exp")?.as_u64()
}

/// The ChatGPT account id required by the Codex subscription backend.
pub fn chatgpt_account_id(token: &str) -> Result<String, AuthError> {
    claims(token)?
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AuthError::Flow("token missing chatgpt_account_id claim".into()))
}

/// Build an unsigned JWT with the given payload (tests and fixtures only).
pub fn craft_unsigned(payload: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let body = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
    format!("{header}.{body}.sig")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_account_id_and_expiry() {
        let token = craft_unsigned(&json!({
            "exp": 1999999999u64,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct_42"}
        }));
        assert_eq!(chatgpt_account_id(&token).unwrap(), "acct_42");
        assert_eq!(expiry(&token), Some(1999999999));
    }

    #[test]
    fn rejects_non_jwt() {
        assert!(chatgpt_account_id("not-a-jwt").is_err());
        assert!(expiry("nope").is_none());
    }
}
