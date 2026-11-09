//! The server acts on links of its own relay only: whatever link a tool is given,
//! no request, and no token, goes anywhere else.

use std::sync::{Arc, Mutex};

use agentwire_relay::{Options, Relay};
use axum::extract::{Request, State};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    ProtocolVersion,
};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient};
use tokio::net::TcpListener;
use vibetogether_mcp::{Room, router};

async fn serve(router: axum::Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

/// Every request that reaches it, as method and path.
async fn record(State(hits): State<Arc<Mutex<Vec<String>>>>, request: Request) -> &'static str {
    let hit = format!("{} {}", request.method(), request.uri());
    hits.lock().unwrap().push(hit);
    "{}"
}

async fn call(
    client: &RunningService<RoleClient, ClientConfig>,
    tool: &'static str,
    arguments: serde_json::Value,
) -> CallToolResult {
    let serde_json::Value::Object(arguments) = arguments else {
        unreachable!()
    };
    client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
        .await
        .unwrap()
}

#[tokio::test]
async fn no_link_leads_anywhere_but_the_relay() {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let elsewhere = serve(
        axum::Router::new()
            .fallback(record)
            .with_state(hits.clone()),
    )
    .await;
    let relay = serve(Relay::open(Options::default()).unwrap().router()).await;
    let mcp = serve(router(Room::new(&relay), ["127.0.0.1"], None)).await;
    let config =
        StreamableHttpClientTransportConfig::with_uri(format!("{mcp}/mcp")).auth_header("t-luis");
    let client = ClientConfig::new(
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
    .unwrap();

    let token = "AAAAAAAAAAAAAAAAAAAAAA";
    let sessions = [
        format!("{elsewhere}/v2/s/{token}"),
        format!("{relay}/v2/s/../../elsewhere"),
        format!("{relay}/v2/s/{token}/../x"),
        format!("{relay}@127.0.0.1/v2/s/{token}"),
        "../x".to_owned(),
    ];
    for session in &sessions {
        let joined = call(
            &client,
            "join",
            serde_json::json!({ "room": session, "name": "luis/claude" }),
        )
        .await;
        assert_eq!(joined.is_error, Some(true), "{session}: {joined:?}");
    }
    let links = [
        format!("{elsewhere}/v2/s/{token}/{token}"),
        format!("{relay}/v2/s/{token}/../../x"),
        format!("{relay}/v2/s/{token}"),
        format!("{relay}/v2/s/{token}/{token}/extra"),
    ];
    for link in &links {
        let sent = call(
            &client,
            "send",
            serde_json::json!({
                "link": link, "text": "hi",
                "file": null,
            }),
        )
        .await;
        assert_eq!(sent.is_error, Some(true), "{link}: {sent:?}");
        let file = call(
            &client,
            "send",
            serde_json::json!({
                "link": link,
                "file": {"data": "e30=", "type": "application/json"},
            }),
        )
        .await;
        assert_eq!(file.is_error, Some(true), "{link}: {file:?}");
        let left = call(&client, "leave", serde_json::json!({ "link": link })).await;
        assert_eq!(left.is_error, Some(true), "{link}: {left:?}");
        let received = call(&client, "receive", serde_json::json!({ "link": link })).await;
        assert_eq!(received.is_error, Some(true), "{link}: {received:?}");
    }
    assert!(
        hits.lock().unwrap().is_empty(),
        "{:?}",
        hits.lock().unwrap()
    );

    // A session on the relay still joins, by its token or its link.
    let http = reqwest::Client::new();
    let session = http.post(format!("{relay}/v2/s")).send().await.unwrap();
    let session = session.text().await.unwrap().trim().to_owned();
    let id = session.rsplit('/').next().unwrap().to_owned();
    for (session, name) in [(session.clone(), "luis/claude"), (id, "luis/codex")] {
        let joined = call(
            &client,
            "join",
            serde_json::json!({ "room": session, "name": name }),
        )
        .await;
        assert_ne!(joined.is_error, Some(true), "{joined:?}");
    }
}
