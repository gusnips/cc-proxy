// Helpers the integration tests share: an environment lock and guard, isolated
// provider auth, and a request through the in-process app.

// Each test binary compiles its own copy of this module and uses a different
// part of it.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Method, Request};
use axum::response::Response;
use cc_proxy::{registry::Registry, server::app};
use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use tower::util::ServiceExt;

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Serialize all env-var-mutating tests so they never run concurrently.
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    // Recover from a poisoned mutex so a failing test doesn't cascade
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Write a valid auth.json for `provider` under `config_dir`.
pub fn write_auth(config_dir: &Path, provider: &str) {
    let dir = config_dir.join(provider);
    std::fs::create_dir_all(&dir).unwrap();
    let expires: i64 = 4102444800000;
    let auth = match provider {
        "codex" => {
            json!({"access":"test-access","refresh":"test-refresh","expires":expires,"account_id":"acct_test"})
        }
        "grok" => {
            json!({"access":"test-access","refresh":"test-refresh","expires_at_ms":expires,"issuer":"https://auth.x.ai","client_id":"test-client"})
        }
        _ => {
            json!({"access":"test-access","refresh":"test-refresh","expires":expires,"scope":"openid","userId":"user_test"})
        }
    };
    std::fs::write(dir.join("auth.json"), serde_json::to_vec(&auth).unwrap()).unwrap();
}

/// Sets or clears one environment variable and restores it on drop.
pub struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    pub fn unset(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// Send `POST /v1/messages` through the in-process app. The registry is built
/// here, after the caller set its environment: some providers read their
/// config when they are built.
pub async fn call_messages_body_with_headers(body: Value, headers: &[(&str, &str)]) -> Response {
    let _no_proxy_env = EnvGuard::set("NO_PROXY", "127.0.0.1,localhost");
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    app(Arc::new(Registry::with_default_alias()))
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}
