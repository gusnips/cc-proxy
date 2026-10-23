use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::constants::{CLIENT_ID, REFRESH_MARGIN_MS, oauth_host};
use super::headers::common_headers;
use super::jwt::extract_user_id;
use super::login::TokenResponse;
use super::token_store::{KimiTokenStore, StoredAuth};
use crate::auth::AuthStorage;

const MAX_REFRESH_ATTEMPTS: u32 = 3;
const RETRYABLE_STATUSES: &[u16] = &[429, 500, 502, 503, 504];

enum RefreshError {
    Unauthorized(String),
    Other(anyhow::Error),
}

pub fn initial_auth(tokens: &TokenResponse, device_id: String) -> anyhow::Result<StoredAuth> {
    if tokens.access_token.trim().is_empty()
        || tokens
            .refresh_token
            .as_deref()
            .is_none_or(|token| token.trim().is_empty())
        || tokens.expires_in == Some(0)
    {
        anyhow::bail!(
            "Kimi did not return a usable sign-in. Run `cc-proxy auth login kimi` again."
        );
    }
    Ok(StoredAuth {
        access: tokens.access_token.clone(),
        refresh: tokens.refresh_token.clone().unwrap_or_default(),
        expires: now_ms().saturating_add(tokens.expires_in.unwrap_or(900).saturating_mul(1000)),
        scope: tokens.scope.clone(),
        user_id: extract_user_id(&tokens.access_token),
        device_id,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub struct KimiAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: KimiTokenStore<S>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
    oauth_host: String,
}

impl<S: AuthStorage<StoredAuth>> KimiAuthManager<S> {
    pub fn new(store: KimiTokenStore<S>) -> Self {
        Self::with_oauth_host(store, oauth_host())
    }

    fn with_oauth_host(store: KimiTokenStore<S>, oauth_host: String) -> Self {
        let refresh_lock = crate::auth::refresh_lock(&store.auth_path());
        Self {
            store,
            refresh_lock,
            oauth_host,
        }
    }

    pub fn get_auth(&self) -> anyhow::Result<StoredAuth> {
        let stored = self.load_auth()?;
        if stored.expires > now_ms().saturating_add(REFRESH_MARGIN_MS) {
            return Ok(stored);
        }
        self.refresh(false, None)
    }

    pub fn force_refresh(&self, rejected_access: &str) -> anyhow::Result<StoredAuth> {
        self.refresh(true, Some(rejected_access))
    }

    fn load_auth(&self) -> anyhow::Result<StoredAuth> {
        self.store
            .load_auth()?
            .ok_or_else(|| anyhow::anyhow!("Not authenticated. Run: cc-proxy auth login kimi"))
    }

    fn refresh(&self, force: bool, rejected_access: Option<&str>) -> anyhow::Result<StoredAuth> {
        let _guard = self.refresh_lock.blocking_lock();
        // A manager for this account may have refreshed while this one waited.
        let current = self.load_auth()?;
        if (!force && current.expires > now_ms().saturating_add(REFRESH_MARGIN_MS))
            || rejected_access.is_some_and(|access| current.access != access)
        {
            return Ok(current);
        }
        self.refresh_now(&current)
    }

    fn refresh_now(&self, current: &StoredAuth) -> anyhow::Result<StoredAuth> {
        match self.refresh_with(current) {
            Ok(next) => Ok(next),
            Err(RefreshError::Other(err)) => Err(err),
            Err(RefreshError::Unauthorized(msg)) => {
                // A peer process can rotate the refresh token before this one.
                let reloaded = self.store.load_auth()?;
                match reloaded {
                    Some(peer) if peer != *current => {
                        if peer.expires > now_ms().saturating_add(REFRESH_MARGIN_MS) {
                            return Ok(peer);
                        }
                        match self.refresh_with(&peer) {
                            Ok(next) => Ok(next),
                            Err(RefreshError::Unauthorized(peer_msg)) => {
                                if let Some(latest) = self.store.load_auth()?
                                    && latest != peer
                                {
                                    return Ok(latest);
                                }
                                self.store.clear_auth()?;
                                anyhow::bail!(peer_msg)
                            }
                            Err(RefreshError::Other(err)) => Err(err),
                        }
                    }
                    _ => {
                        self.store.clear_auth()?;
                        anyhow::bail!(msg)
                    }
                }
            }
        }
    }

    fn refresh_with(&self, current: &StoredAuth) -> Result<StoredAuth, RefreshError> {
        if current.refresh.is_empty() {
            return Err(RefreshError::Other(anyhow::anyhow!(
                "No refresh token stored; re-authenticate"
            )));
        }

        let headers = common_headers(&current.device_id).map_err(RefreshError::Other)?;
        let client = reqwest::blocking::Client::new();

        for attempt in 0..MAX_REFRESH_ATTEMPTS {
            let form = [
                ("client_id", CLIENT_ID.to_string()),
                ("grant_type", "refresh_token".to_string()),
                ("refresh_token", current.refresh.clone()),
            ];

            let resp = match client
                .post(format!("{}/api/oauth/token", self.oauth_host))
                .headers(headers.clone())
                .form(&form)
                .send()
            {
                Ok(r) => r,
                Err(err) => {
                    if attempt < MAX_REFRESH_ATTEMPTS - 1 {
                        let ms = 2u64.pow(attempt) * 1000;
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        continue;
                    }
                    return Err(RefreshError::Other(anyhow::anyhow!(
                        "refresh network error: {err}"
                    )));
                }
            };

            if resp.status().as_u16() == 200 {
                let tokens: TokenResponse =
                    resp.json().map_err(|e| RefreshError::Other(e.into()))?;
                if tokens.access_token.trim().is_empty() || tokens.expires_in == Some(0) {
                    return Err(RefreshError::Other(anyhow::anyhow!(
                        "Kimi returned an invalid token refresh response"
                    )));
                }
                let next = StoredAuth {
                    access: tokens.access_token.clone(),
                    refresh: tokens
                        .refresh_token
                        .filter(|token| !token.is_empty())
                        .unwrap_or_else(|| current.refresh.clone()),
                    expires: now_ms()
                        .saturating_add(tokens.expires_in.unwrap_or(900).saturating_mul(1000)),
                    scope: tokens.scope.or_else(|| current.scope.clone()),
                    user_id: extract_user_id(&tokens.access_token)
                        .or_else(|| current.user_id.clone()),
                    device_id: current.device_id.clone(),
                };
                self.store
                    .save_auth(next.clone())
                    .map_err(RefreshError::Other)?;
                return Ok(next);
            }

            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                let err_msg = resp
                    .text()
                    .unwrap_or_else(|_| "Token refresh unauthorized".to_string());
                return Err(RefreshError::Unauthorized(err_msg));
            }

            if !RETRYABLE_STATUSES.contains(&status) {
                return Err(RefreshError::Other(anyhow::anyhow!(
                    "Token refresh failed: {status}"
                )));
            }

            if attempt < MAX_REFRESH_ATTEMPTS - 1 {
                let ms = 2u64.pow(attempt) * 1000;
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        }

        Err(RefreshError::Other(anyhow::anyhow!(
            "Token refresh failed after {MAX_REFRESH_ATTEMPTS} attempts"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Accounts;
    use crate::auth::InMemoryAuthStore;
    use crate::providers::codex::auth::test_http;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_store() -> KimiTokenStore<InMemoryAuthStore<StoredAuth>> {
        KimiTokenStore::new(InMemoryAuthStore::new())
    }

    fn stale_auth() -> StoredAuth {
        StoredAuth {
            access: "stale_access".into(),
            refresh: "stale_refresh".into(),
            expires: 1,
            scope: None,
            user_id: None,
            device_id: "test-device".into(),
        }
    }

    #[test]
    fn get_auth_returns_stored() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "test_access".into(),
                refresh: "test_refresh".into(),
                expires: 9999999999999,
                scope: Some("openid".into()),
                user_id: Some("user1".into()),
                device_id: "test-device".into(),
            })
            .unwrap();
        let manager = KimiAuthManager::new(store);
        let result = manager.get_auth().unwrap();
        assert_eq!(result.access, "test_access");
        assert_eq!(result.user_id.as_deref(), Some("user1"));
        assert_eq!(result.device_id, "test-device");
    }

    #[test]
    fn get_auth_fails_when_no_auth() {
        let manager = KimiAuthManager::new(test_store());
        assert!(manager.get_auth().is_err());
    }

    #[test]
    fn refresh_adopts_peer_rotated_credential_on_unauthorized() {
        let server = test_http::spawn_mock_server("kimi oauth mock ready", |request| {
            if request.contains("refresh_token=stale_refresh") {
                test_http::json_response(401, r#"{"error":"invalid_grant"}"#)
            } else if request.contains("refresh_token=peer_refresh") {
                test_http::json_response(
                    200,
                    r#"{"access_token":"peer_access","refresh_token":"rotated_refresh","expires_in":900}"#,
                )
            } else {
                test_http::json_response(400, r#"{"error":"unexpected_request"}"#)
            }
        });
        let store = test_store();
        let stale = stale_auth();
        store.save_auth(stale.clone()).unwrap();
        let manager = KimiAuthManager::with_oauth_host(store, server.url.clone());
        manager
            .store
            .save_auth(StoredAuth {
                access: "peer_stale_access".into(),
                refresh: "peer_refresh".into(),
                ..stale.clone()
            })
            .unwrap();
        let result = manager.refresh_now(&stale).unwrap();
        assert_eq!(result.access, "peer_access");
        assert_eq!(result.refresh, "rotated_refresh");
        assert_eq!(
            manager.store.load_auth().unwrap().unwrap().refresh,
            "rotated_refresh"
        );
    }

    #[test]
    fn refresh_clears_auth_only_when_disk_has_no_newer_credential() {
        let server = test_http::spawn_mock_server("kimi oauth mock ready", |_| {
            test_http::json_response(401, r#"{"error":"invalid_grant"}"#)
        });
        let store = test_store();
        let stale = stale_auth();
        store.save_auth(stale.clone()).unwrap();
        let manager = KimiAuthManager::with_oauth_host(store, server.url.clone());
        let err = manager.refresh_now(&stale).unwrap_err();
        assert!(err.to_string().contains("invalid_grant"));
        assert!(manager.store.load_auth().unwrap().is_none());
    }

    #[test]
    fn stale_401_and_logout_read_the_durable_login() {
        let manager = KimiAuthManager::new(test_store());
        let rotated = StoredAuth {
            expires: u64::MAX,
            ..stale_auth()
        };
        manager.store.save_auth(rotated.clone()).unwrap();
        assert_eq!(manager.force_refresh("rejected_access").unwrap(), rotated);
        manager.store.clear_auth().unwrap();
        assert!(manager.get_auth().is_err());
    }

    #[test]
    fn account_refreshes_keep_their_device_id_and_share_only_their_own_lock() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::in_dir("kimi", dir.path().join("kimi"));
        let mut ids = Vec::new();
        for name in ["first", "second"] {
            let auth = StoredAuth {
                device_id: format!("device-{name}"),
                refresh: format!("refresh-{name}"),
                ..stale_auth()
            };
            accounts
                .install(name, None, &serde_json::to_value(auth).unwrap(), None)
                .unwrap();
            ids.push(accounts.require(name).unwrap().id);
        }
        let refreshes = Arc::new(AtomicUsize::new(0));
        let calls = refreshes.clone();
        let server = test_http::spawn_mock_server("kimi oauth mock ready", move |request| {
            calls.fetch_add(1, Ordering::SeqCst);
            let name = if request.contains("refresh_token=refresh-first") {
                "first"
            } else {
                "second"
            };
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains(&format!("x-msh-device-id: device-{name}"))
            );
            test_http::json_response(
                200,
                &format!(
                    r#"{{"access_token":"access-{name}","refresh_token":"rotated-{name}","expires_in":900}}"#
                ),
            )
        });
        let first = KimiAuthManager::with_oauth_host(
            KimiTokenStore::new(accounts.store(&ids[0])),
            server.url.clone(),
        );
        let peer = KimiAuthManager::with_oauth_host(
            KimiTokenStore::new(accounts.store(&ids[0])),
            server.url.clone(),
        );
        let second = KimiAuthManager::with_oauth_host(
            KimiTokenStore::new(accounts.store(&ids[1])),
            server.url.clone(),
        );
        assert!(Arc::ptr_eq(&first.refresh_lock, &peer.refresh_lock));
        assert!(!Arc::ptr_eq(&first.refresh_lock, &second.refresh_lock));
        std::thread::scope(|scope| {
            let a = scope.spawn(|| first.get_auth().unwrap());
            let b = scope.spawn(|| peer.get_auth().unwrap());
            assert_eq!(a.join().unwrap().access, "access-first");
            assert_eq!(b.join().unwrap().device_id, "device-first");
        });
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(
            second.store.load_auth().unwrap().unwrap().refresh,
            "refresh-second"
        );
        assert_eq!(second.get_auth().unwrap().device_id, "device-second");
        assert_eq!(
            first.store.load_auth().unwrap().unwrap().refresh,
            "rotated-first"
        );
        assert_eq!(refreshes.load(Ordering::SeqCst), 2);
    }
}
