//! An MCP server that lets an agent take part in a Vibe Together room.
//!
//! It speaks speech on top of agentwire: every tool works on the participant's own link,
//! which `join` returns, so the server keeps no sessions of its own.
//! Its only state is what it took from the relay for a participant and has not handed on yet.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams, ContentBlock,
    CreateTaskResult, GetTaskParams, GetTaskResult, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, ServerCapabilities, ServerConfig, UpdateTaskParams,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::task_manager::{TaskExit, TaskManager, TaskOptions};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::Value;

const INSTRUCTIONS: &str = "\
You can take part in a Vibe Together room: people and agents talking in one session.
Join with the session's token or link and your name, such as luis/claude for luis's agent.
Each message names whom it is for: names, or * for everyone. Everyone in the room reads every message.
Whenever you are done with your own work, call receive: it returns once a message is for you, \
with what was said in between, so that you wake on the next message for you.
People read only the room: what you write outside it reaches no one there.";

/// How long one receive from the relay waits, so that a lost connection is noticed.
const RELAY_WAIT: Duration = Duration::from_secs(600);
/// How often a blocking receive tells the client that it is still waiting.
const PROGRESS_EVERY: Duration = Duration::from_secs(30);
/// How long a receive as a task waits at most; its task lives a minute longer.
const TASK_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// The server: one value shared by every MCP request.
#[derive(Clone)]
pub struct Room {
    /// The relay a session token alone refers to.
    pub relay: String,
    http: reqwest::Client,
    participants: Arc<Mutex<HashMap<String, Participant>>>,
    tasks: TaskManager,
    tool_router: ToolRouter<Room>,
}

/// What the server knows of a participant, by its own link.
#[derive(Default)]
struct Participant {
    name: Option<String>,
    /// The number of the last entry taken from the relay, after which the next receive
    /// from it starts: receiving after a number acknowledges only up to it.
    taken: Option<u64>,
    /// Entries taken from the relay and not yet returned by a receive, so that a receive
    /// cut off loses nothing.
    pending: Vec<Entry>,
    presence: Option<Value>,
}

#[derive(Clone, Deserialize)]
struct Entry {
    n: u64,
    from: String,
    #[serde(rename = "type")]
    kind: String,
    size: u64,
    filename: Option<String>,
    text: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct JoinArgs {
    /// The session's token, or its full link for a relay other than the default.
    session: String,
    /// Your name, lower case with hyphens, as `<person>/<agent>` such as `luis/claude`.
    name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SendArgs {
    /// Your own link, which join returned.
    link: String,
    /// Whom the message is for: names, or `*` for everyone.
    to: Vec<String>,
    /// The message, in markdown.
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct LinkArgs {
    /// Your own link, which join returned.
    link: String,
}

#[tool_router]
impl Room {
    pub fn new(relay: impl Into<String>) -> Self {
        Self {
            relay: relay.into().trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
            participants: Arc::default(),
            tasks: TaskManager::new(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Join a session. Returns your own link, a secret that the other tools take."
    )]
    async fn join(&self, Parameters(args): Parameters<JoinArgs>) -> CallToolResult {
        outcome(self.join_session(&args.session, &args.name).await)
    }

    #[tool(
        description = "Say something in the session, to the names you choose or `*` for everyone."
    )]
    async fn send(&self, Parameters(args): Parameters<SendArgs>) -> CallToolResult {
        outcome(self.say(&args.link, &args.to, &args.text).await)
    }

    #[tool(
        description = "Wait until a message is for you, then return it with everything said since your last receive. Call it whenever you are done with your own work."
    )]
    async fn receive(&self, Parameters(args): Parameters<LinkArgs>) -> CallToolResult {
        outcome(self.receive_for(&args.link).await)
    }

    #[tool(description = "Leave the session.")]
    async fn leave(&self, Parameters(args): Parameters<LinkArgs>) -> CallToolResult {
        outcome(self.leave_session(&args.link).await)
    }
}

impl Room {
    async fn join_session(&self, session: &str, name: &str) -> Result<String, String> {
        let link = if session.contains("://") {
            session.trim_end_matches('/').to_owned()
        } else {
            format!("{}/v2/s/{}", self.relay, session.trim())
        };
        #[derive(Deserialize)]
        struct Joined {
            name: String,
            read: u64,
            link: String,
        }
        let request = self
            .http
            .post(format!("{link}/join"))
            .query(&[("as", name)]);
        let joined: Joined = parse(&relay(request).await?)?;
        self.participant(&joined.link, |p| {
            p.name = Some(joined.name.clone());
            p.taken = Some(joined.read);
        });
        let request = self
            .http
            .get(&joined.link)
            .query(&[("after", joined.read)])
            .query(&[("wait", 0)]);
        let presence = relay(request)
            .await?
            .lines()
            .last()
            .map(parse::<Value>)
            .transpose()?;
        let presence = presence.map(|line| line["presence"].clone());
        Ok(format!(
            "Joined as {}. Your own link, a secret that send, receive and leave take: {}\n{}",
            joined.name,
            joined.link,
            describe_presence(presence.as_ref(), &joined.name),
        ))
    }

    async fn say(&self, link: &str, to: &[String], text: &str) -> Result<String, String> {
        if to.is_empty()
            || to
                .iter()
                .any(|name| name.is_empty() || name.contains([' ', '"']))
        {
            return Err(
                "to needs at least one name, or *, each without spaces or quotes".to_owned(),
            );
        }
        let kind = format!("text/markdown; to=\"{}\"", to.join(" "));
        let request = self
            .http
            .post(link)
            .header("content-type", kind)
            .body(text.to_owned());
        let sent: Value = parse(&relay(request).await?)?;
        Ok(format!("Sent as #{}.", sent["n"]))
    }

    async fn leave_session(&self, link: &str) -> Result<String, String> {
        relay(self.http.post(format!("{link}/leave"))).await?;
        self.participants.lock().unwrap().remove(link);
        Ok("Left the session.".to_owned())
    }

    /// Takes entries from the relay until one is for this participant,
    /// then returns all of them.
    async fn receive_for(&self, link: &str) -> Result<String, String> {
        let name = match self.participant(link, |p| p.name.clone()) {
            Some(name) => name,
            None => {
                // A receive that returns at once fails for a link that is not in,
                // before joining again tells whose the link is.
                self.pull(link, Duration::ZERO).await?;
                let joined: Value = parse(&relay(self.http.post(format!("{link}/join"))).await?)?;
                let name = joined["name"].as_str().unwrap_or_default().to_owned();
                self.participant(link, |p| p.name = Some(name.clone()));
                name
            }
        };
        loop {
            let delivered = self.participant(link, |p| {
                p.pending
                    .iter()
                    .any(|entry| addresses(entry, &name))
                    .then(|| {
                        let entries = std::mem::take(&mut p.pending);
                        describe(&entries, &name, link, p.presence.as_ref())
                    })
            });
            if let Some(text) = delivered {
                return Ok(text);
            }
            self.pull(link, RELAY_WAIT).await?;
        }
    }

    /// One receive from the relay, after what was taken, into what is pending.
    async fn pull(&self, link: &str, wait: Duration) -> Result<(), String> {
        let after = self.participant(link, |p| p.taken);
        let mut request = self.http.get(link).query(&[("wait", wait.as_secs())]);
        if let Some(after) = after {
            request = request.query(&[("after", after)]);
        }
        let body = relay(request).await?;
        let mut lines: Vec<&str> = body.lines().collect();
        let presence = lines.pop().map(parse::<Value>).transpose()?;
        let entries = lines
            .into_iter()
            .map(parse::<Entry>)
            .collect::<Result<Vec<_>, _>>()?;
        self.participant(link, |p| {
            p.taken = entries.last().map(|entry| entry.n).or(p.taken);
            p.pending.extend(entries);
            p.presence = presence.map(|presence| presence["presence"].clone());
        });
        Ok(())
    }

    fn participant<T>(&self, link: &str, f: impl FnOnce(&mut Participant) -> T) -> T {
        f(self
            .participants
            .lock()
            .unwrap()
            .entry(link.to_owned())
            .or_default())
    }

    /// The blocking receive, which tells the client every so often that it still waits,
    /// if the client asked for progress.
    async fn receive_blocking(
        &self,
        link: &str,
        context: &RequestContext<RoleServer>,
    ) -> CallToolResult {
        let token = context.meta.get_progress_token();
        let receive = self.receive_for(link);
        tokio::pin!(receive);
        let mut ticks =
            tokio::time::interval_at(tokio::time::Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY);
        let mut progress = 0.0;
        loop {
            tokio::select! {
                result = &mut receive => return outcome(result),
                _ = context.ct.cancelled() => return outcome(Err("cancelled".to_owned())),
                _ = ticks.tick() => if let Some(token) = &token {
                    progress += 1.0;
                    let message = "Waiting for a message for you.";
                    let param = ProgressNotificationParam::new(token.clone(), progress).with_message(message);
                    let _ = context.peer.notify_progress(param).await;
                },
            }
        }
    }
}

impl ServerHandler for Room {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tasks()
                .build(),
        )
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tool_router.list_all()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name != "receive" {
            return self
                .tool_router
                .call(ToolCallContext::new(self, request, context))
                .await;
        }
        let LinkArgs { link } =
            serde_json::from_value(Value::Object(request.arguments.unwrap_or_default()))
                .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        if !context
            .client_capabilities()
            .is_some_and(|caps| caps.supports_tasks())
        {
            return Ok(CallToolResponse::Complete(
                self.receive_blocking(&link, &context).await,
            ));
        }
        let room = self.clone();
        let ttl = TASK_WAIT + Duration::from_secs(60);
        let options = TaskOptions::new().with_ttl_ms(ttl.as_millis() as u64);
        let task = self.tasks.spawn(options, move |task| {
            Box::pin(async move {
                tokio::select! {
                    _ = task.cancelled() => Err(TaskExit::Cancelled),
                    result = tokio::time::timeout(TASK_WAIT, room.receive_for(&link)) => Ok(outcome(
                        result.unwrap_or_else(|_| Ok("Nothing for you in a day. Call receive again to keep listening.".to_owned())),
                    )),
                }
            })
        });
        Ok(CallToolResponse::Task(CreateTaskResult::new(task)))
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        Ok(GetTaskResult::new(self.tasks.get_task(&request.task_id)?))
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.tasks
            .update_task(&request.task_id, request.input_responses)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.tasks.cancel_task(&request.task_id)
    }
}

/// The service to mount at a path of an HTTP server, such as `/mcp`.
pub fn service(room: Room) -> StreamableHttpService<Room, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(room.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    )
}

/// Speech's addressing: a `to` parameter of the type names the recipients, or `*` for everyone.
/// A message without one is for everyone, but no entry of a vendor type, such as a join.
fn addresses(entry: &Entry, name: &str) -> bool {
    let essence = entry.kind.split(';').next().unwrap_or_default().trim();
    match recipients(entry) {
        Some(to) => to
            .split(' ')
            .any(|recipient| recipient == "*" || recipient == name),
        None => !essence.starts_with("application/vnd."),
    }
}

fn recipients(entry: &Entry) -> Option<&str> {
    entry.kind.split(';').skip(1).find_map(|parameter| {
        let (key, value) = parameter.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("to")
            .then(|| value.trim().trim_matches('"'))
    })
}

fn describe(entries: &[Entry], name: &str, link: &str, presence: Option<&Value>) -> String {
    let mut out = String::new();
    for entry in entries {
        let (n, from) = (entry.n, &entry.from);
        let line = match entry.kind.as_str() {
            "application/vnd.agentwire.join" => format!("#{n} {from} joined."),
            "application/vnd.agentwire.leave" => format!("#{n} {from} left."),
            kind => {
                let to = recipients(entry).unwrap_or("*");
                let marker = if addresses(entry, name) {
                    "for you"
                } else {
                    "context"
                };
                let essence = kind.split(';').next().unwrap_or_default();
                match (&entry.text, &entry.filename) {
                    (Some(text), None) if essence.starts_with("text/") => {
                        format!("#{n} from {from} to {to} ({marker}):\n{text}")
                    }
                    _ => format!(
                        "#{n} from {from} to {to} ({marker}): {essence}{}, {} bytes, fetched with curl -s -o <file> '{link}/{n}'",
                        entry
                            .filename
                            .as_ref()
                            .map(|f| format!(" {f}"))
                            .unwrap_or_default(),
                        entry.size,
                    ),
                }
            }
        };
        out.push_str(&line);
        out.push_str("\n\n");
    }
    out.push_str(&describe_presence(presence, name));
    out
}

fn describe_presence(presence: Option<&Value>, me: &str) -> String {
    let names: Vec<String> = presence
        .and_then(Value::as_object)
        .map(|people| {
            people
                .iter()
                .map(|(name, state)| {
                    if name == me {
                        format!("{name} (you)")
                    } else if state["receiving"].as_bool() == Some(true) {
                        format!("{name} (listening)")
                    } else {
                        name.clone()
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    format!("In the room: {}.", names.join(", "))
}

/// Sends a request to the relay and returns its body, or the relay's one line of why not.
async fn relay(request: reqwest::RequestBuilder) -> Result<String, String> {
    let response = request
        .send()
        .await
        .map_err(|error| format!("relay unreachable: {error}"))?;
    let status = response.status();
    let body = response.text().await.map_err(|error| error.to_string())?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(format!(
            "relay answered {}: {}",
            status.as_u16(),
            body.trim()
        ))
    }
}

fn parse<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|error| format!("unexpected answer from the relay: {error}"))
}

fn outcome(result: Result<String, String>) -> CallToolResult {
    match result {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
    }
}
