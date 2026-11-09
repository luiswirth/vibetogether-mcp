//! Signing in, lazily, with an account service such as Vibe Together's.
//!
//! Everything works without an account, as zero setup needs, except joining a room and
//! listing the account's rooms: such a call without a token gets MCP's challenge, a 401 that points at the
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

/// A room, as the service lists it.
#[derive(Deserialize)]
struct Listed {
    name: String,
    owner: String,
    address: String,
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

    /// Whether joining `room`, its address or the address's last part as `join` takes
    /// it, as `name` without a token needs signing in: a room an account owns that the
    /// service lets no one in without a token, such as one open to its members only.
    /// No where the service doesn't say.
    async fn needs_sign_in(&self, room: &str, name: &str) -> bool {
        let session = room;
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
        if !answer.is_ok_and(|answer| answer.status().is_success()) {
            return false;
        }
        let join = serde_json::json!({"session": id, "name": name, "token": null, "owned": true});
        let answer = self
            .http
            .post(format!("{}/agentwire/join", self.service))
            .json(&join)
            .timeout(ANSWER)
            .send()
            .await;
        answer.is_ok_and(|answer| answer.status() == StatusCode::FORBIDDEN)
    }

    /// The rooms of the account whose token is `token`, as the agent reads them: JSON,
    /// since a room's name is the owner's to choose.
    pub(crate) async fn rooms(&self, token: &str) -> Result<String, String> {
        let unreachable = |_| "the account service doesn't answer; try again later".to_owned();
        let answer = self
            .http
            .get(format!("{}/api/rooms", self.service))
            .bearer_auth(token)
            .timeout(ANSWER)
            .send()
            .await
            .map_err(unreachable)?;
        if !answer.status().is_success() {
            return Err(format!("the account service refused: {}", answer.status()));
        }
        let rooms: Vec<Listed> = answer.json().await.map_err(unreachable)?;
        let rooms: Vec<Value> = rooms
            .into_iter()
            .map(|room| serde_json::json!({"name": room.name, "owner": room.owner, "address": room.address}))
            .collect();
        Ok(serde_json::json!({ "rooms": rooms }).to_string())
    }

    fn challenge(&self, invalid: bool) -> Response {
        let (error, why) = if invalid {
            (
                r#", error="invalid_token""#,
                "sign in again: the account service doesn't know this token\n",
            )
        } else {
            ("", "sign in: this takes a Vibe Together account\n")
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
    let calls = calls(&bytes);
    if calls.iter().any(|(tool, _, _)| tool == "rooms") {
        return accounts.challenge(false);
    }
    for (tool, room, name) in &calls {
        if tool == "join"
            && let (Some(room), Some(name)) = (room, name)
            && accounts.needs_sign_in(room, name).await
        {
            return accounts.challenge(false);
        }
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// The tools a JSON-RPC message, or a batch of them, calls, each with the room and the
/// name it gives, if any.
fn calls(body: &[u8]) -> Vec<(String, Option<String>, Option<String>)> {
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let messages = match message {
        Value::Array(messages) => messages,
        message => vec![message],
    };
    messages
        .iter()
        .filter(|message| message["method"] == "tools/call")
        .filter_map(|message| {
            let params = &message["params"];
            let tool = params["name"].as_str()?.to_owned();
            let arguments = &params["arguments"];
            let room = arguments["room"].as_str().map(str::to_owned);
            let name = arguments["name"].as_str().map(str::to_owned);
            Some((tool, room, name))
        })
        .collect()
}

/// The request's bearer token, if any.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}
