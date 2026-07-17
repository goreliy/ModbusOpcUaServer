//! Authentication (phase 5): a custom `AuthManager` validating
//! username/password sessions against the config's user list.
//!
//! Two credential forms per user:
//! - `password_hash` — argon2id PHC string (preferred; generate with
//!   `opc-modbus-server hash-password`);
//! - `password` — plain text (commissioning; config validation warns).
//!   Compared in constant time.
//!
//! Anonymous sessions are allowed/refused by the same manager, driven by
//! `opcua.allow_anonymous`.

use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

use argon2::password_hash::PasswordHash;
use argon2::{Argon2, PasswordVerifier};
use parking_lot::Mutex;
use opcua::server::authenticator::{
    user_pass_security_policy_id, user_pass_security_policy_uri, AuthManager, Password, UserToken,
};
use opcua::server::ServerEndpoint;
use opcua::types::{Error, StatusCode, UAString, UserTokenPolicy, UserTokenType};

use gateway_config::schema::v1::OpcUaConfig;

/// Mirrors async-opcua's private `POLICY_ID_ANONYMOUS`.
const POLICY_ID_ANONYMOUS: &str = "anonymous";
const ANONYMOUS_TOKEN_ID: &str = "ANONYMOUS";

/// Cap on the recent-authentication history (roughly one GUI screen).
const MAX_RECENT_AUTHENTICATIONS: usize = 50;

/// One successful authentication (OPC UA session activation) as observed by
/// [`GatewayAuthenticator`].
///
/// This is a login EVENT, not a live session: a client that re-activates its
/// session (reconnect, token renewal) appears again, and there is no
/// corresponding "logout" record — async-opcua 0.18 never tells the
/// authenticator about session closes. For the LIVE picture, pair this with
/// the live session count (`OpcUaHandle::session_count`).
#[derive(Debug, Clone)]
pub struct AuthEvent {
    /// `"ANONYMOUS"` or the username.
    pub user: String,
    /// Path of the endpoint the client activated on (e.g. `"/"`).
    pub endpoint_path: String,
    /// Security policy of the endpoint (e.g. `"None"`, `"Basic256Sha256"`).
    pub security_policy: String,
    /// Message security mode of the endpoint (e.g. `"None"`, `"SignAndEncrypt"`).
    pub security_mode: String,
    /// When the activation happened.
    pub at: SystemTime,
}

enum Credential {
    Plain(String),
    Hash(String),
}

pub struct GatewayAuthenticator {
    allow_anonymous: bool,
    users: HashMap<String, Credential>,
    /// Recent successful logins, oldest first, capped at
    /// [`MAX_RECENT_AUTHENTICATIONS`].
    recent: Mutex<VecDeque<AuthEvent>>,
}

impl GatewayAuthenticator {
    pub fn from_config(cfg: &OpcUaConfig) -> Self {
        let users = cfg
            .users
            .iter()
            .filter_map(|u| {
                let cred = match (&u.password, &u.password_hash) {
                    (Some(p), None) => Credential::Plain(p.clone()),
                    (None, Some(h)) => Credential::Hash(h.clone()),
                    // Rejected by validation; ignore defensively.
                    _ => return None,
                };
                Some((u.username.clone(), cred))
            })
            .collect();
        Self {
            allow_anonymous: cfg.allow_anonymous,
            users,
            recent: Mutex::new(VecDeque::new()),
        }
    }

    /// Recent successful authentications, oldest first (see [`AuthEvent`] for
    /// what this is — and is not).
    pub fn recent_authentications(&self) -> Vec<AuthEvent> {
        self.recent.lock().iter().cloned().collect()
    }

    /// Log the session event (F3: file logs must show client connects) and
    /// remember it in the capped history.
    fn record(&self, user: &str, endpoint: &ServerEndpoint) {
        tracing::info!(
            user,
            endpoint = %endpoint.path,
            security_policy = %endpoint.security_policy,
            security_mode = %endpoint.security_mode,
            "OPC UA session authenticated"
        );
        let mut recent = self.recent.lock();
        if recent.len() >= MAX_RECENT_AUTHENTICATIONS {
            recent.pop_front();
        }
        recent.push_back(AuthEvent {
            user: user.to_string(),
            endpoint_path: endpoint.path.clone(),
            security_policy: endpoint.security_policy.clone(),
            security_mode: endpoint.security_mode.clone(),
            at: SystemTime::now(),
        });
    }

    fn verify(&self, username: &str, password: &str) -> bool {
        match self.users.get(username) {
            None => false,
            Some(Credential::Plain(expected)) => constant_time_eq(expected.as_bytes(), password.as_bytes()),
            Some(Credential::Hash(phc)) => match PasswordHash::new(phc) {
                Ok(parsed) => Argon2::default()
                    .verify_password(password.as_bytes(), &parsed)
                    .is_ok(),
                Err(e) => {
                    // Validation checks the prefix, but a malformed-yet-argon2
                    // string can still fail to parse — fail closed, loudly.
                    tracing::error!(user = username, error = %e, "unparseable password_hash");
                    false
                }
            },
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[async_trait::async_trait]
impl AuthManager for GatewayAuthenticator {
    async fn authenticate_anonymous_token(&self, endpoint: &ServerEndpoint) -> Result<(), Error> {
        if self.allow_anonymous {
            self.record(ANONYMOUS_TOKEN_ID, endpoint);
            Ok(())
        } else {
            Err(Error::new(
                StatusCode::BadIdentityTokenRejected,
                "anonymous sessions are disabled",
            ))
        }
    }

    async fn authenticate_username_identity_token(
        &self,
        endpoint: &ServerEndpoint,
        username: &str,
        password: &Password,
    ) -> Result<UserToken, Error> {
        if self.verify(username, password.get()) {
            self.record(username, endpoint);
            Ok(UserToken(username.to_string()))
        } else {
            tracing::warn!(user = username, "OPC UA session authentication failed");
            Err(Error::new(
                StatusCode::BadUserAccessDenied,
                "unknown user or wrong password",
            ))
        }
    }

    fn user_token_policies(&self, endpoint: &ServerEndpoint) -> Vec<UserTokenPolicy> {
        let mut policies = Vec::with_capacity(2);
        if endpoint.user_token_ids.contains(ANONYMOUS_TOKEN_ID) {
            policies.push(UserTokenPolicy {
                policy_id: UAString::from(POLICY_ID_ANONYMOUS),
                token_type: UserTokenType::Anonymous,
                issued_token_type: UAString::null(),
                issuer_endpoint_url: UAString::null(),
                security_policy_uri: UAString::null(),
            });
        }
        // Username/password policy when any of this endpoint's token ids is
        // one of our configured users.
        if endpoint
            .user_token_ids
            .iter()
            .any(|id| id != ANONYMOUS_TOKEN_ID && self.users.contains_key(id))
        {
            policies.push(UserTokenPolicy {
                policy_id: user_pass_security_policy_id(endpoint),
                token_type: UserTokenType::UserName,
                issued_token_type: UAString::null(),
                issuer_endpoint_url: UAString::null(),
                security_policy_uri: user_pass_security_policy_uri(endpoint),
            });
        }
        policies
    }
}

/// Produce an argon2id PHC string for `hash-password` (CLI helper).
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_config::schema::v1::OpcUaUser;

    fn auth(users: Vec<OpcUaUser>, allow_anonymous: bool) -> GatewayAuthenticator {
        GatewayAuthenticator::from_config(&OpcUaConfig {
            allow_anonymous,
            users,
            ..OpcUaConfig::default()
        })
    }

    fn plain(name: &str, pass: &str) -> OpcUaUser {
        OpcUaUser {
            username: name.into(),
            password: Some(pass.into()),
            password_hash: None,
        }
    }

    #[test]
    fn plaintext_verify_is_exact() {
        let a = auth(vec![plain("op", "secret1")], true);
        assert!(a.verify("op", "secret1"));
        assert!(!a.verify("op", "secret2"));
        assert!(!a.verify("op", "secret1 ")); // length differs
        assert!(!a.verify("nobody", "secret1"));
    }

    #[test]
    fn argon2_hash_round_trip() {
        let phc = hash_password("correct horse").expect("hash");
        assert!(phc.starts_with("$argon2id$"));
        let a = auth(
            vec![OpcUaUser {
                username: "op".into(),
                password: None,
                password_hash: Some(phc),
            }],
            true,
        );
        assert!(a.verify("op", "correct horse"));
        assert!(!a.verify("op", "wrong"));
    }

    #[test]
    fn malformed_hash_fails_closed() {
        let a = auth(
            vec![OpcUaUser {
                username: "op".into(),
                password: None,
                password_hash: Some("$argon2id$broken".into()),
            }],
            true,
        );
        assert!(!a.verify("op", "anything"));
    }

    #[tokio::test]
    async fn successful_authentications_are_recorded_and_capped() {
        use opcua::server::ServerEndpoint;
        let endpoint =
            ServerEndpoint::new_none("/", &["ANONYMOUS".to_string(), "op".to_string()]);
        let a = auth(vec![plain("op", "pw")], true);
        assert!(a.recent_authentications().is_empty());

        a.authenticate_anonymous_token(&endpoint).await.unwrap();
        a.authenticate_username_identity_token(&endpoint, "op", &Password::new("pw".into()))
            .await
            .unwrap();
        // Failed logins are NOT recorded.
        assert!(a
            .authenticate_username_identity_token(&endpoint, "op", &Password::new("bad".into()))
            .await
            .is_err());

        let events = a.recent_authentications();
        assert_eq!(
            events.iter().map(|e| e.user.as_str()).collect::<Vec<_>>(),
            vec!["ANONYMOUS", "op"],
            "oldest first, failures excluded"
        );
        assert!(events.iter().all(|e| e.endpoint_path == "/"
            && e.security_policy == "None"
            && e.security_mode == "None"));

        // Cap: the history never exceeds MAX_RECENT_AUTHENTICATIONS and
        // evicts the OLDEST entries.
        for _ in 0..MAX_RECENT_AUTHENTICATIONS {
            a.authenticate_anonymous_token(&endpoint).await.unwrap();
        }
        let events = a.recent_authentications();
        assert_eq!(events.len(), MAX_RECENT_AUTHENTICATIONS);
        // 50 anonymous logins on top of 2 existing entries: both originals
        // (the oldest) were evicted, only the new anonymous ones remain.
        assert!(
            events.iter().all(|e| e.user == "ANONYMOUS"),
            "oldest entries evicted first"
        );
    }

    #[tokio::test]
    async fn anonymous_gate_follows_config() {
        use opcua::server::ServerEndpoint;
        let endpoint = ServerEndpoint::new_none("/", &["ANONYMOUS".to_string()]);
        assert!(auth(vec![], true)
            .authenticate_anonymous_token(&endpoint)
            .await
            .is_ok());
        assert!(auth(vec![plain("u", "p")], false)
            .authenticate_anonymous_token(&endpoint)
            .await
            .is_err());
    }
}
