//! `receive` against a relay of its own: as a task for a client that declares tasks,
//! blocking for one that doesn't. Another participant talks with plain HTTP, as with curl.

use std::time::Duration;

use agentwire_relay::{Options, Relay};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    GetTaskParams, Implementation, ProtocolVersion, TaskPayload,
};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient, object};
use tokio::net::TcpListener;
use vibetogether_mcp::{Room, router};

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
    let relay = serve(Relay::open(Options::default()).unwrap().router()).await;
    let mcp = serve(router(Room::new(&relay), ["127.0.0.1"], None)).await;
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

    /// Says `text` as luis, pinging `ping`, or everyone without it.
    async fn say(&self, ping: Option<&str>, text: &str) {
        let kind = match ping {
            Some(ping) => format!("text/plain; ping=\"{ping}\""),
            None => "text/plain".to_owned(),
        };
        self.post(&kind, text.as_bytes(), &self.luis).await;
    }

    async fn post(&self, kind: &str, body: &[u8], url: &str) {
        let response = self
            .http
            .post(url)
            .header("content-type", kind)
            .body(body.to_owned())
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
    }

    /// The read position of `name`, as the presence shows it to `luis`.
    async fn read_of(&self, name: &str) -> u64 {
        let url = format!("{}?wait=0", self.luis);
        let body = self
            .http
            .get(url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let presence: serde_json::Value =
            serde_json::from_str(body.lines().last().unwrap()).unwrap();
        presence["presence"][name]["read"].as_u64().unwrap()
    }
}

/// Joins as `luis/claude` through the client, and returns its own link.
async fn join(client: &RunningService<RoleClient, ClientConfig>, session: &str) -> String {
    let result = client
        .call_tool(
            CallToolRequestParams::new("join")
                .with_arguments(object!({ "room": session, "name": "luis/claude" })),
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

/// The text of a result, all its text blocks joined.
fn text(result: &CallToolResult) -> String {
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn receive(link: &str) -> CallToolRequestParams {
    CallToolRequestParams::new("receive").with_arguments(object!({ "link": link }))
}

#[tokio::test]
async fn receive_as_task_completes_only_for_a_message_pinging_the_agent() {
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

    world.say(Some("tristan"), "Not for you.").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!task().await.unwrap().task.status().is_terminal());
    assert_eq!(world.read_of("luis/claude").await, 0);

    world.say(Some("luis/claude"), "For you.").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    // The task is done, but its result is in nobody's context yet.
    assert_eq!(world.read_of("luis/claude").await, 0);
    let payload = task().await.unwrap().task.payload;
    assert_eq!(world.read_of("luis/claude").await, 4);
    let TaskPayload::Completed { result } = payload else {
        panic!("the task did not complete: {payload:?}");
    };
    let result: CallToolResult = serde_json::from_value(result.into()).unwrap();
    let text = text(&result);
    assert!(
        text.contains(r#""from":"luis","verified":false,"pings_you":false,"text":"Not for you.""#),
        "{text}"
    );
    assert!(
        text.contains(r#""from":"luis","verified":false,"pings_you":true,"text":"For you.""#),
        "{text}"
    );
}

#[tokio::test]
async fn receive_blocks_without_tasks_until_a_message_pings_the_agent() {
    let world = world().await;
    let client = world.client(ClientCapabilities::default()).await;
    let link = join(&client, &world.session).await;

    let receiving = tokio::spawn({
        let peer = client.peer().clone();
        let link = link.clone();
        async move { peer.call_tool(receive(&link)).await }
    });
    world.say(Some("tristan"), "Not for you.").await;
    world.say(Some(""), "For no one.").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!receiving.is_finished());
    assert_eq!(world.read_of("luis/claude").await, 0);

    world.say(None, "For everyone.").await;
    let result = tokio::time::timeout(Duration::from_secs(10), receiving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(world.read_of("luis/claude").await, 5);
    let text = text(&result);
    assert!(
        text.contains(r#""from":"luis","verified":false,"pings_you":false,"text":"Not for you.""#),
        "{text}"
    );
    assert!(
        text.contains(r#""pings_you":false,"text":"For no one.""#),
        "{text}"
    );
    assert!(
        text.contains(r#""pings_you":true,"text":"For everyone.""#),
        "{text}"
    );

    world.say(Some("tristan *"), "Again.").await;
    let text = self::text(&client.call_tool(receive(&link)).await.unwrap());
    assert!(
        text.starts_with(
            r#"{"n":6,"from":"luis","verified":false,"pings_you":true,"text":"Again."}"#
        ),
        "{text}"
    );
}

#[tokio::test]
async fn images_and_files_go_both_ways() {
    let world = world().await;
    let client = world.client(ClientCapabilities::default()).await;
    let link = join(&client, &world.session).await;

    let image = b"\x89PNG not really";
    let url = format!("{}?filename=plot.png", world.luis);
    world
        .post("image/png; ping=\"luis/claude\"", image, &url)
        .await;
    let result = client.call_tool(receive(&link)).await.unwrap();
    let shown = result
        .content
        .iter()
        .find_map(|block| block.as_image())
        .expect("no image in the result");
    assert_eq!(shown.mime_type, "image/png");
    assert_eq!(BASE64.decode(&shown.data).unwrap(), image);

    let file = b"some bytes";
    let sent = client
        .call_tool(CallToolRequestParams::new("send").with_arguments(object!({
            "link": link,
            "ping": ["luis"],
            "file": { "data": BASE64.encode(file), "type": "application/octet-stream", "name": "data.bin" },
        })))
        .await
        .unwrap();
    let n = text(&sent);
    let n = n.trim_start_matches("Sent as #").trim_end_matches('.');
    let got = world
        .http
        .get(format!("{}/{n}", world.luis))
        .send()
        .await
        .unwrap();
    let kind = got.headers()["content-type"].to_str().unwrap().to_owned();
    assert_eq!(kind, "application/octet-stream; ping=\"luis\"");
    assert_eq!(got.bytes().await.unwrap().as_ref(), file);
}
