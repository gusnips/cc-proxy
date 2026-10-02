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

#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    /// Sign in using browser-based authentication
    Login,
    /// Sign in using a device code
    Device,
    /// Show the current authentication status
    Status,
    /// Delete stored authentication credentials
    Logout,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn supported_models(&self) -> Vec<String>;
    fn cli(&self) -> &'static dyn CliHandlers;
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
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub req_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub provider: String,
    pub traffic: Option<Arc<TrafficCapture>>,
    pub monitor: Option<MonitorHandle>,
}
