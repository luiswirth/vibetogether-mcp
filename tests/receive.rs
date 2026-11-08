//! `receive` against a relay of its own: as a task for a client that declares tasks,
//! blocking for one that doesn't. Another participant talks with plain HTTP, as with curl.

use std::time::Duration;

use agentwire_relay::Relay;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    GetTaskParams, Implementation, ProtocolVersion, TaskPayload,
};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient, object};
use tokio::net::TcpListener;
use vibe_together_mcp::{Room, service};

struct World {
    mcp: String,
    session: String,
    luis: String,
    http: reqwest::Client,
}

async fn serve(router: axum::Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

/// A relay, the server, and a session that `luis` joined.
async fn world() -> World {
    let relay = serve(Relay::open(None).unwrap().router()).await;
    let mcp = serve(axum::Router::new().nest_service("/mcp", service(Room::new(&relay)))).await;
    let http = reqwest::Client::new();
    let session = http
        .post(format!("{relay}/v2/s"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let session = session.trim().to_owned();
    let joined: serde_json::Value = http
        .post(format!("{session}/join?as=luis"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let luis = joined["link"].as_str().unwrap().to_owned();
    World {
        mcp: format!("{mcp}/mcp"),
        session,
        luis,
        http,
    }
}

impl World {
    async fn client(
        &self,
        capabilities: ClientCapabilities,
    ) -> RunningService<RoleClient, ClientConfig> {
        ClientConfig::new(capabilities, Implementation::new("test", "0"))
            .serve_with_lifecycle(
                StreamableHttpClientTransport::from_uri(self.mcp.clone()),
                ClientLifecycleMode::Auto {
                    preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                    legacy_version: Some(ProtocolVersion::V_2025_11_25),
                },
            )
            .await
            .unwrap()
    }

    async fn say(&self, to: &str, text: &str) {
        let response = self
            .http
            .post(&self.luis)
            .header("content-type", format!("text/plain; to=\"{to}\""))
            .body(text.to_owned())
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
    }
}

/// Joins as `luis/claude` through the client, and returns its own link.
async fn join(client: &RunningService<RoleClient, ClientConfig>, session: &str) -> String {
    let result = client
        .call_tool(
            CallToolRequestParams::new("join")
                .with_arguments(object!({ "session": session, "name": "luis/claude" })),
        )
        .await
        .unwrap();
    let text = text(&result);
    assert!(text.contains("luis/claude"), "{text}");
    text.split("take: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned()
}

fn text(result: &CallToolResult) -> String {
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.content[0].as_text().unwrap().text.clone()
}

fn receive(link: &str) -> CallToolRequestParams {
    CallToolRequestParams::new("receive").with_arguments(object!({ "link": link }))
}

#[tokio::test]
async fn receive_as_task_completes_only_for_a_message_to_the_agent() {
    let world = world().await;
    let client = world
        .client(ClientCapabilities::builder().enable_tasks().build())
        .await;
    let link = join(&client, &world.session).await;

    let CallToolResponse::Task(created) = client.call_tool_once(receive(&link)).await.unwrap()
    else {
        panic!("receive answered without a task");
    };
    let task = || {
        client
            .peer()
            .get_task(GetTaskParams::new(created.task.task_id.clone()))
    };

    world.say("tristan", "Not for you.").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!task().await.unwrap().task.status().is_terminal());

    world.say("luis/claude", "For you.").await;
    let payload = loop {
        let got = task().await.unwrap().task;
        if got.status().is_terminal() {
            break got.payload;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let TaskPayload::Completed { result } = payload else {
        panic!("the task did not complete: {payload:?}");
    };
    let result: CallToolResult = serde_json::from_value(result.into()).unwrap();
    let text = text(&result);
    assert!(
        text.contains("to tristan (context):\nNot for you."),
        "{text}"
    );
    assert!(
        text.contains("to luis/claude (for you):\nFor you."),
        "{text}"
    );
}

#[tokio::test]
async fn receive_blocks_without_tasks_until_a_message_is_for_the_agent() {
    let world = world().await;
    let client = world.client(ClientCapabilities::default()).await;
    let link = join(&client, &world.session).await;

    let receiving = tokio::spawn({
        let peer = client.peer().clone();
        let link = link.clone();
        async move { peer.call_tool(receive(&link)).await }
    });
    world.say("tristan", "Not for you.").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!receiving.is_finished());

    world.say("*", "For everyone.").await;
    let result = tokio::time::timeout(Duration::from_secs(10), receiving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = text(&result);
    assert!(
        text.contains("to tristan (context):\nNot for you."),
        "{text}"
    );
    assert!(text.contains("to * (for you):\nFor everyone."), "{text}");

    world.say("luis/claude", "Again.").await;
    let text = self::text(&client.call_tool(receive(&link)).await.unwrap());
    assert!(
        text.starts_with("#5 from luis to luis/claude (for you):\nAgain."),
        "{text}"
    );
}
