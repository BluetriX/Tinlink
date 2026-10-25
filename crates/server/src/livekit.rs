//! Minimal LiveKit access token issuer.
//!
//! LiveKit access tokens are HS256 JWTs signed with the API secret. They are
//! built by hand (HMAC-SHA256 + base64url) to avoid pulling in a full JWT
//! library with RSA support we do not need. The
//! server decides what a member may do and encodes it into the grant, so
//! LiveKit itself never needs to know about groups or roles.
//! See <https://docs.livekit.io/home/get-started/authentication/>.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tinlink_protocol::Permissions;

use crate::config::LiveKitConfig;

const TOKEN_TTL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VideoGrant {
    room: String,
    room_join: bool,
    can_subscribe: bool,
    can_publish: bool,
    can_publish_data: bool,
    can_publish_sources: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct Claims {
    iss: String,
    sub: String,
    name: String,
    nbf: u64,
    exp: u64,
    video: VideoGrant,
}

/// Creates a token that lets `identity` join `room` with the media sources
/// allowed by `perms`.
pub fn issue_token(
    cfg: &LiveKitConfig,
    identity: &str,
    display_name: &str,
    room: &str,
    perms: Permissions,
) -> Result<String, serde_json::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut sources = Vec::new();
    if perms.contains(Permissions::SPEAK) {
        sources.push("microphone");
    }
    if perms.contains(Permissions::SHARE_SCREEN) {
        sources.extend(["screen_share", "screen_share_audio"]);
    }

    let claims = Claims {
        iss: cfg.api_key.clone(),
        sub: identity.to_owned(),
        name: display_name.to_owned(),
        nbf: now,
        exp: now + TOKEN_TTL.as_secs(),
        video: VideoGrant {
            room: room.to_owned(),
            room_join: perms.contains(Permissions::CONNECT),
            can_subscribe: true,
            can_publish: !sources.is_empty(),
            can_publish_data: true,
            can_publish_sources: sources,
        },
    };
    sign_hs256(&claims, cfg.api_secret.as_bytes())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WebhookError {
    #[error("malformed token")]
    Malformed,
    #[error("invalid signature")]
    Signature,
    #[error("token expired or not yet valid")]
    Expired,
    #[error("unknown api key")]
    ApiKey,
    #[error("body hash mismatch")]
    BodyHash,
}

#[derive(Debug, serde::Deserialize)]
struct WebhookClaims {
    iss: String,
    #[serde(default)]
    nbf: Option<u64>,
    #[serde(default)]
    exp: Option<u64>,
    sha256: String,
}

/// Verifies a LiveKit webhook: the `Authorization` header carries an HS256
/// JWT signed with the API secret whose `sha256` claim is the base64 SHA-256
/// of the request body.
pub fn verify_webhook(
    cfg: &LiveKitConfig,
    authorization: &str,
    body: &[u8],
) -> Result<(), WebhookError> {
    use base64::engine::general_purpose::STANDARD;

    let token = authorization
        .strip_prefix("Bearer ")
        .unwrap_or(authorization);
    let (signing_input, signature) = token.rsplit_once('.').ok_or(WebhookError::Malformed)?;
    let signature = B64.decode(signature).map_err(|_| WebhookError::Malformed)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(cfg.api_secret.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(signing_input.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| WebhookError::Signature)?;

    let payload = signing_input
        .split('.')
        .nth(1)
        .ok_or(WebhookError::Malformed)?;
    let payload = B64.decode(payload).map_err(|_| WebhookError::Malformed)?;
    let claims: WebhookClaims =
        serde_json::from_slice(&payload).map_err(|_| WebhookError::Malformed)?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Allow a little clock skew between LiveKit and this server.
    const LEEWAY: u64 = 60;
    if claims.exp.is_some_and(|exp| exp + LEEWAY < now)
        || claims.nbf.is_some_and(|nbf| nbf > now + LEEWAY)
    {
        return Err(WebhookError::Expired);
    }
    if claims.iss != cfg.api_key {
        return Err(WebhookError::ApiKey);
    }
    let expected = STANDARD.encode(Sha256::digest(body));
    let actual = claims.sha256.trim_end_matches('=');
    if expected.trim_end_matches('=') != actual {
        return Err(WebhookError::BodyHash);
    }
    Ok(())
}

fn sign_hs256(claims: &impl Serialize, secret: &[u8]) -> Result<String, serde_json::Error> {
    let header = B64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = B64.encode(serde_json::to_vec(claims)?);
    let signing_input = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(signing_input.as_bytes());
    let signature = B64.encode(mac.finalize().into_bytes());
    Ok(format!("{signing_input}.{signature}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the signature and returns the decoded payload.
    fn verify(token: &str, secret: &[u8]) -> serde_json::Value {
        let (signing_input, signature) = token.rsplit_once('.').unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(signing_input.as_bytes());
        mac.verify_slice(&B64.decode(signature).unwrap())
            .expect("valid signature");
        let payload = signing_input.split('.').nth(1).unwrap();
        serde_json::from_slice(&B64.decode(payload).unwrap()).unwrap()
    }

    fn test_config() -> LiveKitConfig {
        LiveKitConfig {
            public_url: "ws://localhost:7880".into(),
            api_key: "devkey".into(),
            api_secret: "a-secret-that-is-at-least-32-bytes-long".into(),
        }
    }

    /// Builds a webhook authorization token the way LiveKit does.
    pub(crate) fn webhook_token(cfg: &LiveKitConfig, body: &[u8]) -> String {
        use base64::engine::general_purpose::STANDARD;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = serde_json::json!({
            "iss": cfg.api_key,
            "nbf": now,
            "exp": now + 300,
            "sha256": STANDARD.encode(Sha256::digest(body)),
        });
        sign_hs256(&claims, cfg.api_secret.as_bytes()).unwrap()
    }

    #[test]
    fn webhook_verification() {
        let cfg = test_config();
        let body = br#"{"event":"room_started"}"#;
        let token = webhook_token(&cfg, body);
        assert_eq!(verify_webhook(&cfg, &token, body), Ok(()));
        assert_eq!(
            verify_webhook(&cfg, &token, b"{}"),
            Err(WebhookError::BodyHash)
        );
        assert_eq!(
            verify_webhook(&cfg, "garbage", body),
            Err(WebhookError::Malformed)
        );

        let other = LiveKitConfig {
            api_secret: "another-secret-another-secret-xx".into(),
            ..test_config()
        };
        assert_eq!(
            verify_webhook(&other, &token, body),
            Err(WebhookError::Signature)
        );
        let other_key = LiveKitConfig {
            api_key: "otherkey".into(),
            ..test_config()
        };
        assert_eq!(
            verify_webhook(&other_key, &token, body),
            Err(WebhookError::ApiKey)
        );
    }

    #[test]
    fn grant_reflects_permissions() {
        let cfg = test_config();
        let perms = Permissions::CONNECT | Permissions::SPEAK;
        let token = issue_token(&cfg, "erik@localhost", "Erik", "group1/lobby", perms).unwrap();

        let claims = verify(&token, cfg.api_secret.as_bytes());
        let video = &claims["video"];
        assert_eq!(claims["iss"], "devkey");
        assert_eq!(claims["sub"], "erik@localhost");
        assert_eq!(video["room"], "group1/lobby");
        assert_eq!(video["roomJoin"], true);
        assert_eq!(
            video["canPublishSources"],
            serde_json::json!(["microphone"])
        );
    }
}
