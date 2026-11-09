//! Signing in, lazily, with an account service such as Vibe Together's.
//!
//! Everything works without an account, as zero setup needs, except joining a room:
//! a `join` into one without a token gets MCP's challenge, a 401 that points at the
//! server's metadata (RFC 9728), from which an MCP client gets a token through the
//! service's OAuth and retries. The challenge has to be the HTTP answer itself, before
//! the MCP SDK runs, since a tool's error, inside a 200, starts no sign-in.
//! A token is passed on to the relay on joining, so that the presence shows its account;
//! one the service rejects is challenged too, so that the client signs in again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;

/// How long the service's yes to a token is kept: the relay asks again on each of the
/// participant's requests anyway, so a revoked token is cut off there.
const KEEP: Duration = Duration::from_secs(5);
/// How long the service may take to answer.
const ANSWER: Duration = Duration::from_secs(10);

/// The account service, and how the server names itself in its challenge.
pub struct Accounts {
    /// The service's own URL, under which `/api/me` and `/agentwire/sessions/<id>` answer.
    service: String,
    /// What the challenge's `WWW-Authenticate` names: the server's metadata.
    metadata: String,
    /// The relay a session token alone refers to, where the rooms are.
    relay: String,
    http: reqwest::Client,
    /// The tokens the service vouched for, until when.
    vouched: Mutex<HashMap<String, Instant>>,
}

#[derive(Deserialize)]
struct Lasting {
    lasting: bool,
}

impl Accounts {
    /// The service at `service`, such as `http://127.0.0.1:8793`, for the server at its
    /// public URL `resource`, such as `https://vibetogether.agentics.lwirth.com/mcp`,
    /// whose metadata the service serves where RFC 9728 puts it.
    pub fn new(service: &str, resource: &str) -> Result<Self, String> {
        let resource = reqwest::Url::parse(resource)
            .map_err(|error| format!("the resource {resource:?} is no URL: {error}"))?;
        let metadata = format!(
            "{}/.well-known/oauth-protected-resource{}",
            resource.origin().ascii_serialization(),
            resource.path().trim_end_matches('/'),
        );
        Ok(Self {
            service: service.trim_end_matches('/').to_owned(),
            metadata,
            relay: String::new(),
            http: reqwest::Client::new(),
            vouched: Mutex::default(),
        })
    }

    /// The same, with the relay whose sessions may be rooms.
    pub(crate) fn on(mut self, relay: &str) -> Self {
        self.relay = relay.trim_end_matches('/').to_owned();
        self
    }

    /// Whether the service vouches for `token`, or `None` where it gave no answer.
    async fn vouches(&self, token: &str) -> Option<bool> {
        let now = Instant::now();
        {
            let mut vouched = self.vouched.lock().unwrap();
            vouched.retain(|_, until| *until > now);
            if vouched.contains_key(token) {
                return Some(true);
            }
        }
        let answer = self
            .http
            .get(format!("{}/api/me", self.service))
            .bearer_auth(token)
            .timeout(ANSWER)
            .send()
            .await
            .ok()?;
        match answer.status() {
            StatusCode::OK => {
                let mut vouched = self.vouched.lock().unwrap();
                vouched.insert(token.to_owned(), now + KEEP);
                Some(true)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Some(false),
            _ => None,
        }
    }

    /// Whether `session`, a token or a link as `join` takes it, is a room;
    /// no where the service doesn't say.
    async fn is_room(&self, session: &str) -> bool {
        let session = session.trim().trim_end_matches('/');
        let id = match session.strip_prefix(&format!("{}/v2/s/", self.relay)) {
            Some(id) => id,
            None if !session.contains('/') => session,
            // A session on another relay is no room of this service's.
            None => return false,
        };
        if !crate::is_token(id) {
            return false;
        }
        let answer = self
            .http
            .get(format!("{}/agentwire/sessions/{id}", self.service))
            .timeout(ANSWER)
            .send()
            .await;
        match answer {
            Ok(answer) if answer.status().is_success() => answer
                .json::<Lasting>()
                .await
                .is_ok_and(|lasting| lasting.lasting),
            _ => false,
        }
    }

    fn challenge(&self, invalid: bool) -> Response {
        let (error, why) = if invalid {
            (
                r#", error="invalid_token""#,
                "sign in again: the account service doesn't know this token\n",
            )
        } else {
            (
                "",
                "sign in: joining a room takes a Vibe Together account\n",
            )
        };
        let value = format!(r#"Bearer resource_metadata="{}"{error}"#, self.metadata);
        (StatusCode::UNAUTHORIZED, [(WWW_AUTHENTICATE, value)], why).into_response()
    }
}

/// Challenges a request with a token the service rejects, and a `join` into a room
/// without a token; lets everything else through.
pub async fn check(
    State(accounts): State<Arc<Accounts>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(token) = bearer(request.headers()) {
        // Where the service doesn't answer, the relay decides.
        if accounts.vouches(token).await == Some(false) {
            return accounts.challenge(true);
        }
        return next.run(request).await;
    }
    if request.method() != Method::POST {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, crate::REQUEST_MAX).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "the request is too large\n").into_response();
    };
    for session in joins(&bytes) {
        if accounts.is_room(&session).await {
            return accounts.challenge(false);
        }
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// The sessions that the `join` calls in a JSON-RPC message, or a batch of them, name.
fn joins(body: &[u8]) -> Vec<String> {
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let messages = match message {
        Value::Array(messages) => messages,
        message => vec![message],
    };
    messages
        .iter()
        .filter(|message| message["method"] == "tools/call" && message["params"]["name"] == "join")
        .filter_map(|message| message["params"]["arguments"]["session"].as_str())
        .map(str::to_owned)
        .collect()
}

/// The request's bearer token, if any.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}
