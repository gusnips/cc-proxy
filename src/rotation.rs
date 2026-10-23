//! Retry a Messages request on another saved account, before output starts.
//! Only a 429 with a future retry time makes an account wait. No guessed windows.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};
use axum::response::Response;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::accounts::{Account, Accounts};
use crate::anthropic::{MessagesRequest, json_error};
use crate::provider::{Provider, ProviderError, ProviderErrorKind, RequestContext};
use crate::request_identity::ConversationIdentity;

/// Provider-confirmed limits can use a reset clock without adding client retry headers.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RotationHint {
    Stay,
    RetryAt { until_ms: u64, per_model: bool },
}

#[derive(Serialize, Deserialize)]
struct Cooldown {
    provider: String,
    account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    until_ms: u64,
}

pub(crate) struct Cooldowns {
    path: PathBuf,
}

impl Cooldowns {
    pub(crate) fn new() -> Self {
        Self {
            path: crate::paths::state_dir().join("rotation.json"),
        }
    }

    fn read(&self) -> Result<Vec<Cooldown>> {
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Can't read {}. Check its permissions and try again.",
                        self.path.display()
                    )
                });
            }
        };
        serde_json::from_slice(&raw).with_context(|| {
            format!(
                "Invalid cooldown file at {}. Fix or remove it, then try again.",
                self.path.display()
            )
        })
    }

    pub(crate) fn waits(&self, provider: &str, id: &str) -> Result<Vec<(Option<String>, u64)>> {
        Ok(self
            .read()?
            .into_iter()
            .filter(|c| c.provider == provider && c.account == id && c.until_ms > now_ms())
            .map(|c| (c.model, c.until_ms))
            .collect())
    }

    #[cfg(test)]
    fn until(&self, provider: &str, id: &str) -> Result<Option<u64>> {
        Ok(self
            .waits(provider, id)?
            .into_iter()
            .map(|(_, until)| until)
            .max())
    }

    fn mark(&self, provider: &str, id: &str, model: Option<&str>, until_ms: u64) -> Result<()> {
        fs::create_dir_all(
            self.path
                .parent()
                .context("The cooldown path has no parent folder")?,
        )?;
        let lock = File::create(self.path.with_extension("lock"))?;
        lock.lock()?;
        let mut entries = self.read()?;
        entries.retain(|c| c.until_ms > now_ms());
        match entries
            .iter_mut()
            .find(|c| c.provider == provider && c.account == id && c.model.as_deref() == model)
        {
            Some(entry) => entry.until_ms = entry.until_ms.max(until_ms),
            None => entries.push(Cooldown {
                provider: provider.into(),
                account: id.into(),
                model: model.map(str::to_string),
                until_ms,
            }),
        }
        crate::fsutil::write_atomic(&self.path, &serde_json::to_vec_pretty(&entries)?)?;
        Ok(())
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn retry_deadline(response: &Response, now: u64) -> Option<(u64, bool)> {
    if response.status() != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    if let Some(hint) = response.extensions().get::<RotationHint>() {
        return match *hint {
            RotationHint::Stay => None,
            RotationHint::RetryAt {
                until_ms,
                per_model,
            } => (until_ms > now).then_some((until_ms, per_model)),
        };
    }
    let wait = crate::retry::retry_after_ms(
        response
            .headers()
            .get(http::header::RETRY_AFTER)?
            .to_str()
            .ok()?,
    )?;
    now.checked_add(wait)
        .filter(|until| *until > now)
        .map(|until| (until, false))
}

/// Current account first, then the saved order. Every account gets at most one
/// provider call per request, even if a very short cooldown expires meanwhile.
fn eligible<'a>(
    list: &'a [Account],
    current: &str,
    tried: &HashSet<String>,
    cooldowns: &[Cooldown],
    provider: &str,
    model: &str,
    now: u64,
) -> Option<&'a Account> {
    let ready = |account: &&Account| {
        !tried.contains(&account.id)
            && !cooldowns.iter().any(|c| {
                c.provider == provider
                    && c.account == account.id
                    && c.model.as_deref().is_none_or(|m| m == model)
                    && c.until_ms > now
            })
    };
    list.iter()
        .filter(ready)
        .find(|a| a.id == current)
        .or_else(|| list.iter().find(ready))
}

fn all_limited(
    list: &[Account],
    cooldowns: &[Cooldown],
    provider: &str,
    model: &str,
    now: u64,
) -> Option<Response> {
    let deadlines: Option<Vec<_>> = list
        .iter()
        .map(|a| {
            cooldowns
                .iter()
                .filter(|c| {
                    c.provider == provider
                        && c.account == a.id
                        && c.model.as_deref().is_none_or(|m| m == model)
                        && c.until_ms > now
                })
                .map(|c| c.until_ms)
                .max()
        })
        .collect();
    let first = deadlines?.into_iter().min()?;
    let seconds = (first - now).div_ceil(1000);
    let who = if list.len() == 1 {
        format!(
            "The saved {} account is",
            crate::ui::provider_name(provider)
        )
    } else {
        format!(
            "All {} saved {} accounts are",
            list.len(),
            crate::ui::provider_name(provider)
        )
    };
    let mut error = ProviderError::new(
        StatusCode::TOO_MANY_REQUESTS,
        ProviderErrorKind::RateLimit,
        format!(
            "{who} at the limit. Try again in {}, or add an account with `cc-proxy auth add {provider}`.",
            crate::ui::duration(std::time::Duration::from_secs(seconds))
        ),
    );
    error.retry_after = Some(seconds.to_string());
    error.should_retry = Some(false);
    Some(error.response())
}

pub(crate) async fn execute(
    provider: &dyn Provider,
    body: MessagesRequest,
    context: RequestContext,
    identity: Option<ConversationIdentity>,
) -> Response {
    let Some(support) = provider.cli().accounts() else {
        return provider
            .handle_messages_with_conversation_identity(body, context, identity)
            .await;
    };
    if !crate::config::auth_auto_rotate()
        || support.pinned_credentials().is_some()
        || (provider.name() == "codex" && !crate::providers::codex::can_rotate(&body))
    {
        return provider
            .handle_messages_with_conversation_identity(body, context, identity)
            .await;
    }
    let accounts = match Accounts::open(provider.name(), support) {
        Ok(accounts) => accounts,
        Err(error) => return storage_error(error),
    };
    execute_with(
        provider,
        body,
        context,
        identity,
        &accounts,
        &Cooldowns::new(),
    )
    .await
}

fn storage_error(error: anyhow::Error) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "api_error",
        format!("Can't choose a saved account: {error:#}"),
    )
}

async fn execute_with(
    provider: &dyn Provider,
    body: MessagesRequest,
    context: RequestContext,
    identity: Option<ConversationIdentity>,
    accounts: &Accounts,
    cooldowns: &Cooldowns,
) -> Response {
    let mut tried = HashSet::new();
    let mut last_response = None;
    let model = body.model.as_deref().unwrap_or_default();
    loop {
        let selection = (|| -> Result<_> {
            let list = accounts.list()?;
            let Some(current) = accounts.current()? else {
                return Ok(None);
            };
            let snapshot = cooldowns.read()?;
            let now = now_ms();
            let next = eligible(
                &list,
                &current.id,
                &tried,
                &snapshot,
                provider.name(),
                model,
                now,
            )
            .cloned();
            Ok(Some((list, current, snapshot, now, next)))
        })();
        let (list, current, snapshot, now, next) = match selection {
            Ok(Some(selected)) => selected,
            Ok(None) => {
                return match last_response {
                    Some(response) => response,
                    None => {
                        provider
                            .handle_messages_with_conversation_identity(body, context, identity)
                            .await
                    }
                };
            }
            Err(error) => return storage_error(error),
        };
        let Some(next) = next else {
            return all_limited(&list, &snapshot, provider.name(), model, now)
                .or(last_response)
                .unwrap_or_else(|| {
                    json_error(
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limit_error",
                        "Every saved account was tried. Try the request again later.",
                    )
                });
        };
        if next.id != current.id {
            if let Err(error) = accounts.switch_if_current(&current.id, &next.id) {
                return storage_error(error);
            }
            crate::logging::create_logger("auth").info(
                "account_switched",
                Some(serde_json::Map::from_iter([
                    ("provider".into(), serde_json::json!(provider.name())),
                    ("account".into(), serde_json::json!(&next.name)),
                    ("reqId".into(), serde_json::json!(&context.req_id)),
                ])),
            );
        }
        tried.insert(next.id.clone());
        let mut attempt = context.clone();
        attempt.account = Some(next.id.clone());
        let response = provider
            .handle_messages_with_conversation_identity(body.clone(), attempt, identity.clone())
            .await;
        let Some((until, per_model)) = retry_deadline(&response, now_ms()) else {
            return response;
        };
        if let Err(error) =
            cooldowns.mark(provider.name(), &next.id, per_model.then_some(model), until)
        {
            return storage_error(error);
        }
        last_response = Some(response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AuthState, CliHandlers};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    struct Fake {
        calls: Mutex<Vec<String>>,
        limited: Vec<String>,
        retry: Option<&'static str>,
        status: StatusCode,
    }
    impl CliHandlers for Fake {
        fn auth_state(&self) -> AuthState {
            AuthState::Missing
        }
        fn login(&self) -> Result<()> {
            unreachable!()
        }
        fn device(&self) -> Result<()> {
            unreachable!()
        }
        fn status(&self) -> Result<()> {
            unreachable!()
        }
        fn logout(&self) -> Result<()> {
            unreachable!()
        }
    }
    #[async_trait]
    impl Provider for Fake {
        fn name(&self) -> &'static str {
            "codex"
        }
        fn supported_models(&self) -> Vec<String> {
            vec![]
        }
        fn cli(&self) -> &dyn CliHandlers {
            self
        }
        async fn handle_count_tokens(&self, _: MessagesRequest, _: RequestContext) -> Response {
            unreachable!()
        }
        async fn handle_messages(&self, _: MessagesRequest, ctx: RequestContext) -> Response {
            let id = ctx.account.unwrap();
            self.calls.lock().unwrap().push(id.clone());
            let mut response = Response::new(axum::body::Body::from("reply"));
            if self.limited.contains(&id) {
                *response.status_mut() = self.status;
                if let Some(retry) = self.retry {
                    response
                        .headers_mut()
                        .insert(http::header::RETRY_AFTER, retry.parse().unwrap());
                }
            }
            response
        }
    }
    fn fixture() -> (tempfile::TempDir, Accounts, Cooldowns, Vec<Account>) {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::in_dir("codex", dir.path().join("codex"));
        for name in ["a", "b"] {
            accounts
                .install(name, None, &json!({"access":name}), None)
                .unwrap();
        }
        let list = accounts.list().unwrap();
        let cooldowns = Cooldowns {
            path: dir.path().join("rotation.json"),
        };
        (dir, accounts, cooldowns, list)
    }
    async fn call(fake: &Fake, accounts: &Accounts, cooldowns: &Cooldowns) -> Response {
        let ctx = RequestContext {
            req_id: "rotation-test".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            account: None,
            traffic: None,
            monitor: None,
        };
        execute_with(
            fake,
            serde_json::from_value(json!({"messages":[],"model":"test"})).unwrap(),
            ctx,
            None,
            accounts,
            cooldowns,
        )
        .await
    }
    fn fake(limited: Vec<String>, retry: Option<&'static str>, status: StatusCode) -> Fake {
        Fake {
            calls: Mutex::new(vec![]),
            limited,
            retry,
            status,
        }
    }

    #[tokio::test]
    async fn a_limit_moves_to_b_and_the_next_request_stays_there() {
        let (_dir, accounts, cooldowns, list) = fixture();
        let fake = fake(
            vec![list[0].id.clone()],
            Some("3600"),
            StatusCode::TOO_MANY_REQUESTS,
        );
        assert_eq!(
            call(&fake, &accounts, &cooldowns).await.status(),
            StatusCode::OK
        );
        assert_eq!(accounts.current().unwrap().unwrap().name, "b");
        assert!(cooldowns.until("codex", &list[0].id).unwrap().is_some());
        assert_eq!(
            call(&fake, &accounts, &cooldowns).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            *fake.calls.lock().unwrap(),
            [list[0].id.clone(), list[1].id.clone(), list[1].id.clone()]
        );
    }

    #[tokio::test]
    async fn all_limited_returns_the_first_deadline_without_calling_again() {
        let (_dir, accounts, cooldowns, list) = fixture();
        let fake = fake(
            list.iter().map(|a| a.id.clone()).collect(),
            Some("3600"),
            StatusCode::TOO_MANY_REQUESTS,
        );
        let response = call(&fake, &accounts, &cooldowns).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            response.headers()["retry-after"]
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                <= 3600
        );
        call(&fake, &accounts, &cooldowns).await;
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn only_a_429_with_a_future_retry_time_rotates() {
        for (retry, status) in [
            (None, 429),
            (Some("bad"), 429),
            (Some("-1"), 429),
            (Some("0"), 429),
            (Some("Wed, 01 Jan 2020 00:00:00 GMT"), 429),
            (Some("3600"), 503),
            (Some("3600"), 200),
        ] {
            let (_dir, accounts, cooldowns, list) = fixture();
            let fake = fake(
                vec![list[0].id.clone()],
                retry,
                StatusCode::from_u16(status).unwrap(),
            );
            assert_eq!(
                call(&fake, &accounts, &cooldowns).await.status().as_u16(),
                status
            );
            assert_eq!(fake.calls.lock().unwrap().len(), 1);
            assert_eq!(accounts.current().unwrap().unwrap().name, "a");
        }
    }

    #[test]
    fn expiry_is_checked_lazily_and_attempts_are_never_repeated() {
        let (_dir, _accounts, _store, list) = fixture();
        let entries = vec![Cooldown {
            provider: "codex".into(),
            account: list[0].id.clone(),
            model: None,
            until_ms: 200,
        }];
        assert_eq!(
            eligible(
                &list,
                &list[0].id,
                &HashSet::new(),
                &entries,
                "codex",
                "test",
                100
            )
            .unwrap()
            .name,
            "b"
        );
        assert_eq!(
            eligible(
                &list,
                &list[0].id,
                &HashSet::new(),
                &entries,
                "codex",
                "test",
                200
            )
            .unwrap()
            .name,
            "a"
        );
        let tried = list.iter().map(|a| a.id.clone()).collect();
        assert!(eligible(&list, &list[0].id, &tried, &[], "codex", "test", 300).is_none());
    }

    #[test]
    fn provider_hints_do_not_change_the_client_retry_headers() {
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        response.extensions_mut().insert(RotationHint::RetryAt {
            until_ms: 20_000,
            per_model: false,
        });
        assert_eq!(retry_deadline(&response, 10_000), Some((20_000, false)));
        assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
        response
            .headers_mut()
            .insert(http::header::RETRY_AFTER, "10".parse().unwrap());
        response.extensions_mut().insert(RotationHint::Stay);
        assert_eq!(retry_deadline(&response, 10_000), None);
        response.extensions_mut().insert(RotationHint::RetryAt {
            until_ms: 10_000,
            per_model: false,
        });
        assert_eq!(retry_deadline(&response, 10_000), None);
        *response.status_mut() = StatusCode::OK;
        response.extensions_mut().insert(RotationHint::RetryAt {
            until_ms: 20_000,
            per_model: false,
        });
        assert_eq!(retry_deadline(&response, 10_000), None);
    }

    #[test]
    fn model_limits_never_block_other_models_or_providers() {
        let (_dir, _accounts, cooldowns, list) = fixture();
        let now = now_ms();
        cooldowns
            .mark("codex", &list[0].id, Some("model-a"), now + 60_000)
            .unwrap();
        cooldowns
            .mark("codex", &list[1].id, Some("model-a"), now + 90_000)
            .unwrap();
        let snapshot = cooldowns.read().unwrap();
        assert!(
            eligible(
                &list,
                &list[0].id,
                &HashSet::new(),
                &snapshot,
                "codex",
                "model-a",
                now
            )
            .is_none()
        );
        assert_eq!(
            eligible(
                &list,
                &list[0].id,
                &HashSet::new(),
                &snapshot,
                "codex",
                "model-b",
                now
            )
            .unwrap()
            .id,
            list[0].id
        );
        assert!(
            eligible(
                &list,
                &list[0].id,
                &HashSet::new(),
                &snapshot,
                "other",
                "model-a",
                now
            )
            .is_some()
        );
        let response = all_limited(&list, &snapshot, "codex", "model-a", now).unwrap();
        assert_eq!(response.headers()["retry-after"], "60");
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert!(all_limited(&list, &snapshot, "codex", "model-b", now).is_none());
    }

    #[test]
    fn concurrent_marks_keep_every_account_and_the_longest_deadline() {
        let (dir, _accounts, cooldowns, list) = fixture();
        let until = now_ms() + 60_000;
        std::thread::scope(|scope| {
            for account in &list {
                let path = cooldowns.path.clone();
                scope.spawn(move || {
                    Cooldowns { path }
                        .mark("codex", &account.id, None, until)
                        .unwrap()
                });
            }
        });
        cooldowns
            .mark("codex", &list[0].id, None, until - 1000)
            .unwrap();
        let reopened = Cooldowns {
            path: dir.path().join("rotation.json"),
        };
        assert_eq!(reopened.until("codex", &list[0].id).unwrap(), Some(until));
        assert_eq!(reopened.until("codex", &list[1].id).unwrap(), Some(until));
    }
}
