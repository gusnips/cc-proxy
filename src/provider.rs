use crate::anthropic::schema::MessagesRequest;
use crate::monitor::MonitorHandle;
use crate::request_identity::ConversationIdentity;
use crate::traffic::TrafficCapture;
use anyhow::Result;
use async_trait::async_trait;
use axum::{body::Body, http::StatusCode, response::Response};
use bytes::Bytes;
use clap::Subcommand;
use std::sync::Arc;

#[derive(Debug, Clone, clap::Args)]
pub struct LoginArgs {
    /// Provider to sign in to. Leave it out to pick from a list
    pub provider: Option<String>,
    /// Sign in using a device code instead of opening a browser
    #[arg(long)]
    pub device: bool,
    /// Sign in again to an account you already saved, instead of asking
    #[arg(long)]
    pub replace: bool,
    /// With --replace: the account to sign in to again. Default: the one in use
    #[arg(long, requires = "replace")]
    pub account: Option<String>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct LogoutArgs {
    /// Provider to sign out of. Leave it out to pick from a list
    pub provider: Option<String>,
    /// Account to sign out of. Leave it out when the provider has one
    pub account: Option<String>,
    /// Sign out of every account the provider has
    #[arg(long, conflicts_with = "account")]
    pub all: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    /// Sign in to a provider
    Login(LoginArgs),
    /// Save one more account for a provider
    Add {
        /// Provider to add an account to. Leave it out to pick from a list
        provider: Option<String>,
        /// A short name for the account, such as `work`
        #[arg(long)]
        name: Option<String>,
        /// Sign in using a device code instead of opening a browser
        #[arg(long)]
        device: bool,
    },
    /// Choose which saved account a provider uses
    Use {
        /// Provider to switch. Leave it out to pick from a list
        provider: Option<String>,
        /// Account to use. Leave it out to pick from a list
        account: Option<String>,
    },
    /// Sign in using a device code
    Device {
        /// Provider to sign in to. Leave it out to pick from a list
        provider: Option<String>,
    },
    /// Show the current authentication status
    Status {
        /// Provider to check. Leave it out to see all of them
        provider: Option<String>,
    },
    /// Delete stored authentication credentials
    Logout(LogoutArgs),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn supported_models(&self) -> Vec<String>;

    /// The models Claude's names can map to. Everything the provider supports
    /// by default. A provider leaves out the ones that would only confuse the
    /// match, such as Codex's `-fast` copies of every model.
    fn catalog(&self) -> Vec<String> {
        self.supported_models()
    }

    /// True when the catalog is a mix of other vendors' models, so no model
    /// is the obvious match for a Claude name and the user chooses each one.
    fn aggregator(&self) -> bool {
        false
    }

    fn cli(&self) -> &dyn CliHandlers;
    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response;

    async fn handle_messages_with_conversation_identity(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        let _ = conversation_identity;
        self.handle_messages(body, ctx).await
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response;

    async fn generate_anthropic_stream(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        Err(ProviderError::new(
            StatusCode::NOT_IMPLEMENTED,
            ProviderErrorKind::InvalidRequest,
            format!(
                "provider '{}' does not support OpenAI-compatible generation",
                self.name()
            ),
        ))
    }
}

pub enum GenerationBody {
    BufferedSse(Bytes),
    LiveSse(Body),
}

pub struct Generation {
    pub body: GenerationBody,
    pub resolved_model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    Permission,
    RateLimit,
    InvalidRequest,
    Overloaded,
    Api,
}

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub status: StatusCode,
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retry_after: Option<String>,
    pub param: Option<String>,
    pub code: Option<String>,
    /// Sent as `x-should-retry`. `Some(false)` stops a client's retry loop for
    /// a failure no backoff can fix, such as a spent balance.
    pub should_retry: Option<bool>,
    pub(crate) rotation_hint: Option<crate::rotation::RotationHint>,
}

impl ProviderError {
    pub fn new(status: StatusCode, kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after: None,
            param: None,
            code: None,
            should_retry: None,
            rotation_hint: None,
        }
    }

    pub fn error_type(&self) -> &'static str {
        match self.kind {
            ProviderErrorKind::Authentication => "authentication_error",
            ProviderErrorKind::Permission => "permission_error",
            ProviderErrorKind::RateLimit => "rate_limit_error",
            ProviderErrorKind::InvalidRequest => "invalid_request_error",
            ProviderErrorKind::Overloaded => "overloaded_error",
            ProviderErrorKind::Api => "api_error",
        }
    }

    /// The Anthropic error response, with the retry headers the client reads.
    pub fn response(self) -> Response {
        let mut response =
            crate::anthropic::error::json_error(self.status, self.error_type(), self.message);
        // A provider that confirmed a reset is spent until then, not
        // throttled. Say so with the headers Claude Code reads, so it names
        // the limit, counts down, and resumes the turn by itself.
        if let Some(crate::rotation::RotationHint::RetryAt { until_ms, .. }) = self.rotation_hint {
            response
                .headers_mut()
                .extend(crate::anthropic::error::usage_limit_headers(
                    Some(until_ms),
                    None,
                ));
            response
                .extensions_mut()
                .insert(crate::rotation::ExhaustedAt {
                    until_ms,
                    spent: false,
                });
        }
        let headers = response.headers_mut();
        if let Some(retry_after) = self
            .retry_after
            .and_then(|value| http::HeaderValue::from_str(&value).ok())
        {
            headers.insert(http::header::RETRY_AFTER, retry_after);
        }
        if self.should_retry == Some(false) {
            headers.insert("x-should-retry", http::HeaderValue::from_static("false"));
        }
        if let Some(hint) = self.rotation_hint {
            response.extensions_mut().insert(hint);
        }
        response
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Lets a classified failure ride inside an `anyhow::Error` through the stream
/// translators and be recovered with `downcast_ref` where the response is built.
impl std::error::Error for ProviderError {}

/// What cc-proxy holds for a provider, without calling the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// A sign-in is saved. `expires_ms` is when its access token runs out;
    /// the proxy renews it on use.
    SignedIn {
        account: Option<String>,
        expires_ms: Option<u64>,
    },
    /// An API key is set, in the config file or the environment.
    KeySaved,
    Missing,
}

impl AuthState {
    pub fn describe(&self) -> String {
        match self {
            Self::SignedIn {
                account,
                expires_ms,
            } => {
                let who = account.as_ref().map_or("signed in".to_string(), |account| {
                    format!("signed in as {account}")
                });
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_millis() as u64);
                match expires_ms {
                    Some(expires) if *expires > now_ms => format!(
                        "{who}, token good for {}",
                        crate::ui::duration(std::time::Duration::from_millis(expires - now_ms))
                    ),
                    Some(_) => format!("{who}, renews on next use"),
                    None => who,
                }
            }
            Self::KeySaved => "API key set".to_string(),
            Self::Missing => "not set up".to_string(),
        }
    }
}

/// What a provider said about an API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCheck {
    Accepted,
    /// The provider refused the key: it is wrong, revoked or expired.
    Rejected(String),
    /// The check didn't settle it: the provider was offline or answered
    /// with something else.
    Unverified(String),
}

pub trait CliHandlers: Send + Sync {
    fn auth_state(&self) -> AuthState;
    fn login(&self) -> Result<()>;
    fn device(&self) -> Result<()>;
    fn status(&self) -> Result<()>;
    fn logout(&self) -> Result<()>;

    /// Set for a provider that can keep more than one login. The others keep one.
    fn accounts(&self) -> Option<&dyn crate::accounts::AccountSupport> {
        None
    }
}

pub fn intern_provider_name(name: &str) -> &'static str {
    static INTERNED: once_cell::sync::Lazy<
        std::sync::Mutex<std::collections::HashSet<&'static str>>,
    > = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut guard = INTERNED.lock().unwrap();
    if let Some(&existing) = guard.get(name) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub req_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub provider: String,
    /// Saved account ID, fixed for this attempt. None uses the current account.
    pub account: Option<String>,
    pub traffic: Option<Arc<TrafficCapture>>,
    pub monitor: Option<MonitorHandle>,
}

impl RequestContext {
    pub(crate) fn select_account(&mut self, account: &crate::accounts::Account) {
        self.account = Some(account.id.clone());
        if let Some(monitor) = &self.monitor {
            monitor.account_selected(&self.req_id, &account.name);
        }
        crate::logging::create_logger("auth").info(
            "account_selected",
            Some(serde_json::Map::from_iter([
                ("reqId".into(), serde_json::json!(&self.req_id)),
                ("provider".into(), serde_json::json!(&self.provider)),
                ("account".into(), serde_json::json!(&account.name)),
            ])),
        );
    }

    pub(crate) fn pin_account(
        &mut self,
        support: &dyn crate::accounts::AccountSupport,
    ) -> Result<(), ProviderError> {
        if support.pinned_credentials().is_some() {
            return Ok(());
        }
        let select = || -> anyhow::Result<_> {
            let accounts = crate::accounts::Accounts::open(&self.provider, support)?;
            match &self.account {
                Some(id) => accounts.list()?.into_iter().find(|account| &account.id == id)
                    .map(Some).ok_or_else(|| anyhow::anyhow!("The saved login was removed or replaced. Retry to use the current account.")),
                None => accounts.current(),
            }
        };
        let account = select().map_err(|error| {
            ProviderError::new(
                StatusCode::UNAUTHORIZED,
                ProviderErrorKind::Authentication,
                format!("Can't select a saved {} login: {error:#}", self.provider),
            )
        })?;
        if let Some(account) = account {
            self.select_account(&account);
        }
        Ok(())
    }
}
