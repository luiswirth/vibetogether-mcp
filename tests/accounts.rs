//! Signing in, lazily, against a stub of the account service that is also the relay's
//! access service: everything works without an account but joining a room, which is
//! challenged, and a token is passed on, so that its name shows as verified.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use agentwire::Client;
use agentwire_relay::{Options, Relay};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    ProtocolVersion,
};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient, object};
use serde::Deserialize;
use tokio::net::TcpListener;
use vibetogether_mcp::{Accounts, Room, router};

const RESOURCE: &str = "https://vibetogether.example/mcp";
const METADATA: &str = "https://vibetogether.example/.well-known/oauth-protected-resource/mcp";

/// The rooms, by session token; the one account is `luis`, whose token is `t-luis`.
#[derive(Default)]
struct Stub {
    rooms: Mutex<HashSet<String>>,
}

async fn me(headers: HeaderMap) -> (StatusCode, String) {
    match headers.get("authorization").and_then(|v| v.to_str().ok()) {
        Some("Bearer t-luis") => (StatusCode::OK, r#"{"name":"luis"}"#.into()),
        _ => (StatusCode::UNAUTHORIZED, String::new()),
    }
}

async fn rooms(headers: HeaderMap) -> (StatusCode, String) {
    match headers.get("authorization").and_then(|v| v.to_str().ok()) {
        Some("Bearer t-luis") => (
            StatusCode::OK,
            r#"[{"id":"r_1","name":"agents","owner":"luis","members":[],"link":"L"}]"#.into(),
        ),
        _ => (StatusCode::UNAUTHORIZED, String::new()),
    }
}

#[derive(Deserialize)]
struct Join {
    session: String,
    name: String,
    token: Option<String>,
    guarded: bool,
}

async fn join(State(stub): State<Arc<Stub>>, Json(join): Json<Join>) -> (StatusCode, String) {
    let room = stub.rooms.lock().unwrap().contains(&join.session);
    match join.token.as_deref() {
        Some("t-luis") if join.name.split('/').next() == Some("luis") => {
            (StatusCode::OK, r#"{"account":"luis"}"#.into())
        }
        Some(_) => (StatusCode::FORBIDDEN, "not your name\n".into()),
        None if join.guarded || room => (StatusCode::FORBIDDEN, "sign in first\n".into()),
        None => (StatusCode::OK, r#"{"account":null}"#.into()),
    }
}

async fn session(State(stub): State<Arc<Stub>>, Path(id): Path<String>) -> Json<serde_json::Value> {
    let lasting = stub.rooms.lock().unwrap().contains(&id);
    Json(serde_json::json!({ "lasting": lasting }))
}

async fn serve(router: axum::Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

/// The server, signed in to the stub, with a room and a session by secret link.
struct World {
    mcp: String,
    room: String,
    open: String,
}

async fn world() -> World {
    let stub = Arc::new(Stub::default());
    let service = serve(
        axum::Router::new()
            .route("/api/me", get(me))
            .route("/api/rooms", get(rooms))
            .route("/agentwire/join", post(join))
            .route("/agentwire/sessions/{id}", get(session))
            .with_state(stub.clone()),
    )
    .await;
    let relay = Relay::open(Options {
        access: Some(format!("{service}/agentwire")),
        ..Options::default()
    })
    .unwrap();
    let relay = serve(relay.router()).await;
    let accounts = Accounts::new(&service, RESOURCE).unwrap();
    let mcp = serve(router(Room::new(&relay), ["127.0.0.1"], Some(accounts))).await;
    let client = Client::new();
    let room = client.create_guarded(&relay).await.unwrap();
    let id = room.rsplit('/').next().unwrap().to_owned();
    stub.rooms.lock().unwrap().insert(id);
    let open = client.create(&relay).await.unwrap();
    World {
        mcp: format!("{mcp}/mcp"),
        room,
        open,
    }
}

/// A client of the server, with `token` as its bearer.
async fn client(mcp: &str, token: Option<&str>) -> RunningService<RoleClient, ClientConfig> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(mcp.to_owned());
    if let Some(token) = token {
        config = config.auth_header(token);
    }
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("test", "0"),
    )
    .serve_with_lifecycle(
        StreamableHttpClientTransport::from_config(config),
        ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        },
    )
    .await
    .unwrap()
}

async fn join_as(
    client: &RunningService<RoleClient, ClientConfig>,
    session: &str,
    name: &str,
) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new("join")
                .with_arguments(object!({ "session": session, "name": name })),
        )
        .await
        .unwrap()
}

fn text(result: &CallToolResult) -> String {
    result.content[0].as_text().unwrap().text.clone()
}

/// What the server answers a plain `join` call, with `token` as the bearer.
async fn post_join(mcp: &str, session: &str, token: Option<&str>) -> reqwest::Response {
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "join", "arguments": {"session": session, "name": "luis/claude"}},
    });
    let mut request = reqwest::Client::new()
        .post(mcp)
        .header("accept", "application/json, text/event-stream")
        .json(&call);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn without_an_account_a_session_by_secret_link_works() {
    let world = world().await;
    let client = client(&world.mcp, None).await;
    let joined = join_as(&client, &world.open, "luis/claude").await;
    assert_ne!(joined.is_error, Some(true), "{joined:?}");
    let text = text(&joined);
    assert!(
        text.contains(r#"{"name":"luis/claude","you":true,"verified":false,"listening":false}"#),
        "{text}"
    );
}

#[tokio::test]
async fn joining_a_room_without_a_token_is_challenged() {
    let world = world().await;
    let answer = post_join(&world.mcp, &world.room, None).await;
    assert_eq!(answer.status(), 401);
    let challenge = answer.headers()["www-authenticate"].to_str().unwrap();
    assert_eq!(
        challenge,
        format!(r#"Bearer resource_metadata="{METADATA}""#)
    );
    // A room named by its token alone, too.
    let id = world.room.rsplit('/').next().unwrap();
    assert_eq!(post_join(&world.mcp, id, None).await.status(), 401);
    // The session by secret link is no room.
    assert_ne!(post_join(&world.mcp, &world.open, None).await.status(), 401);
}

#[tokio::test]
async fn a_token_the_service_rejects_is_challenged() {
    let world = world().await;
    let answer = post_join(&world.mcp, &world.open, Some("t-ben")).await;
    assert_eq!(answer.status(), 401);
    let challenge = answer.headers()["www-authenticate"].to_str().unwrap();
    assert_eq!(
        challenge,
        format!(r#"Bearer resource_metadata="{METADATA}", error="invalid_token""#)
    );
}

#[tokio::test]
async fn an_agent_joins_with_its_token_and_shows_as_verified() {
    let world = world().await;
    let client = client(&world.mcp, Some("t-luis")).await;
    for session in [&world.room, &world.open] {
        let joined = join_as(&client, session, "luis/claude").await;
        assert_ne!(joined.is_error, Some(true), "{joined:?}");
        let text = text(&joined);
        assert!(
            text.contains(r#""name":"luis/claude","you":true,"verified":true"#),
            "{text}"
        );
    }
    let refused = join_as(&client, &world.open, "ben/claude").await;
    assert_eq!(refused.is_error, Some(true));
    assert!(text(&refused).contains("not your name"), "{refused:?}");
}

#[tokio::test]
async fn listing_the_rooms_asks_to_sign_in_first() {
    let world = world().await;
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "rooms", "arguments": {}},
    });
    let answer = reqwest::Client::new()
        .post(&world.mcp)
        .header("accept", "application/json, text/event-stream")
        .json(&call)
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 401);

    let client = client(&world.mcp, Some("t-luis")).await;
    let listed = client
        .call_tool(CallToolRequestParams::new("rooms"))
        .await
        .unwrap();
    assert_eq!(
        text(&listed),
        r#"{"rooms":[{"name":"agents","owner":"luis","link":"L"}]}"#
    );
}
