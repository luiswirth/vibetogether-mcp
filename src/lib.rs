//! An MCP server that lets an agent take part in a Vibe Together room.
//!
//! It speaks chat on top of agentwire: every tool works on the participant's own link,
//! which `join` returns, so the server keeps no sessions of its own.
//! Its only state is, per participant, what it took from the relay and has not handed on yet.
//!
//! A message counts as read exactly when it is in the agent's context:
//! the server waits for newer entries without moving the read position,
//! and acknowledges the entries it hands over as it hands them over.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
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
Everyone in the room gets every message, and a message pings, that is wakes, only whom it names. \
A message to the room pings everyone: when it doesn't concern all, ping only whom it concerns, \
and for an update nobody needs to act on, ping no one. A message that doesn't ping you is an fyi: \
read it, and don't answer it.
Whenever you are done with your own work, call receive: it returns once a message pings you, \
with what was said in between.
People read only the room: what you write outside it reaches no one there.";

/// How long one receive from the relay waits, so that a lost connection is noticed.
const RELAY_WAIT: Duration = Duration::from_secs(600);
/// How often a blocking receive tells the client that it is still waiting.
const PROGRESS_EVERY: Duration = Duration::from_secs(30);
/// How long a receive as a task waits at most; its task lives a minute longer.
const TASK_WAIT: Duration = Duration::from_secs(24 * 60 * 60);
/// The largest image handed to the agent as an image; a larger one is described instead.
const IMAGE_MAX: u64 = 5 * 1024 * 1024;
/// The largest MCP request: a payload of the relay's 10 MB, in base64, with room to spare.
const REQUEST_MAX: usize = 16 * 1024 * 1024;

/// The server: one value shared by every MCP request.
#[derive(Clone)]
pub struct Room {
    /// The relay a session token alone refers to.
    pub relay: String,
    http: reqwest::Client,
    participants: Arc<Mutex<HashMap<String, Participant>>>,
    tasks: TaskManager,
    /// What a finished receive task hands over, by task: acknowledged once the client
    /// has fetched its result.
    handovers: Arc<Mutex<HashMap<String, Handover>>>,
    tool_router: ToolRouter<Room>,
}

/// What the server knows of a participant, by its own link.
#[derive(Default)]
struct Participant {
    name: Option<String>,
    /// The number of the last entry taken from the relay, after which the next receive
    /// from it starts.
    taken: u64,
    /// The number of the last entry handed to the agent: its read position at the relay.
    read: u64,
    /// Entries taken from the relay and not yet handed to the agent.
    pending: Vec<Entry>,
    presence: Option<Value>,
}

/// Entries handed to the agent, up to `read`, to acknowledge at the relay.
struct Handover {
    link: String,
    read: u64,
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
    /// Whom the message pings, that is wakes: names, or `*` for everyone.
    /// Left out, it pings everyone; empty, it pings no one. Everyone gets it either way.
    ping: Option<Vec<String>>,
    /// The message, in markdown. Give either this or a file.
    text: Option<String>,
    /// An image or any other file, at most 10 MB. Give either this or a text.
    file: Option<File>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct File {
    /// The file's content, in base64.
    data: String,
    /// Its MIME type, such as `image/png`; an image is shown inline.
    #[serde(rename = "type")]
    kind: String,
    /// Its file name, such as `plot.png`.
    name: Option<String>,
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
            handovers: Arc::default(),
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
        description = "Say something in the session, or share an image or a file. Everyone gets it; it pings everyone, or only whom `ping` names."
    )]
    async fn send(&self, Parameters(args): Parameters<SendArgs>) -> CallToolResult {
        outcome(self.say(args).await)
    }

    #[tool(
        description = "Wait until a message pings you, then return it with everything said since your last receive, each marked ping or fyi. Call it whenever you are done with your own work."
    )]
    async fn receive(&self, Parameters(args): Parameters<LinkArgs>) -> CallToolResult {
        self.receive_handing_over(&args.link).await
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
        let request = self
            .http
            .post(format!("{link}/join"))
            .query(&[("as", name)]);
        let joined: Joined = parse(&relay(request).await?)?;
        self.know(&joined);
        let request = self.http.get(&joined.link).query(&[
            ("after", joined.read),
            ("read", joined.read),
            ("wait", 0),
        ]);
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

    fn know(&self, joined: &Joined) {
        self.participant(&joined.link, |p| {
            p.name = Some(joined.name.clone());
            p.taken = joined.read;
            p.read = joined.read;
        });
    }

    async fn say(&self, args: SendArgs) -> Result<String, String> {
        let ping = match &args.ping {
            None => String::new(),
            Some(names) => {
                if names
                    .iter()
                    .any(|name| name.is_empty() || name.contains([' ', '"']))
                {
                    return Err("ping takes names, or *, each without spaces or quotes".to_owned());
                }
                format!("; ping=\"{}\"", names.join(" "))
            }
        };
        let request = match (args.text, args.file) {
            (Some(text), None) => self
                .http
                .post(&args.link)
                .header("content-type", format!("text/markdown{ping}"))
                .body(text),
            (None, Some(file)) => {
                let data = BASE64
                    .decode(file.data.trim())
                    .map_err(|error| format!("the file's data is no base64: {error}"))?;
                let mut request = self
                    .http
                    .post(&args.link)
                    .header("content-type", format!("{}{ping}", file.kind))
                    .body(data);
                if let Some(name) = file.name {
                    request = request.query(&[("filename", name)]);
                }
                request
            }
            _ => return Err("give either a text or a file".to_owned()),
        };
        let sent: Value = parse(&relay(request).await?)?;
        Ok(format!("Sent as #{}.", sent["n"]))
    }

    async fn leave_session(&self, link: &str) -> Result<String, String> {
        relay(self.http.post(format!("{link}/leave"))).await?;
        self.participants.lock().unwrap().remove(link);
        Ok("Left the session.".to_owned())
    }

    /// Takes entries from the relay until one pings this participant, then returns all of
    /// them, without acknowledging them.
    async fn receive_for(&self, link: &str) -> Result<(Vec<ContentBlock>, Handover), String> {
        let name = match self.participant(link, |p| p.name.clone()) {
            Some(name) => name,
            None => {
                // Fetching an entry fails for a link that is not in, without side effects,
                // before joining again, which does nothing while in, tells whose the link is.
                relay(self.http.get(format!("{link}/1"))).await?;
                let joined: Joined = parse(&relay(self.http.post(format!("{link}/join"))).await?)?;
                self.know(&joined);
                joined.name
            }
        };
        loop {
            let ready = self.participant(link, |p| {
                p.pending
                    .iter()
                    .any(|entry| pings(entry, &name))
                    .then(|| (std::mem::take(&mut p.pending), p.presence.clone()))
            });
            if let Some((entries, presence)) = ready {
                let read = entries.last().map_or(0, |entry| entry.n);
                let content = self
                    .describe(&entries, &name, link, presence.as_ref())
                    .await;
                let handover = Handover {
                    link: link.to_owned(),
                    read,
                };
                return Ok((content, handover));
            }
            self.pull(link).await?;
        }
    }

    /// Receives and acknowledges at once, for a client that gets the result right away.
    async fn receive_handing_over(&self, link: &str) -> CallToolResult {
        match self.receive_for(link).await {
            Ok((content, handover)) => {
                self.hand_over(handover).await;
                CallToolResult::success(content)
            }
            Err(error) => outcome(Err(error)),
        }
    }

    /// Acknowledges at the relay what was handed to the agent.
    async fn hand_over(&self, handover: Handover) {
        let Handover { link, read } = handover;
        let taken = self.participant(&link, |p| {
            p.read = p.read.max(read);
            p.taken
        });
        let request = self
            .http
            .get(&link)
            .query(&[("after", taken), ("read", read), ("wait", 0)]);
        // The next receive acknowledges it anyway, through its read position.
        let _ = relay(request).await;
    }

    /// One receive from the relay, after what was taken, into what is pending,
    /// leaving the read position at what was handed over.
    async fn pull(&self, link: &str) -> Result<(), String> {
        let (taken, read) = self.participant(link, |p| (p.taken, p.read));
        let request = self.http.get(link).query(&[
            ("after", taken),
            ("read", read),
            ("wait", RELAY_WAIT.as_secs()),
        ]);
        let body = relay(request).await?;
        let mut lines: Vec<&str> = body.lines().collect();
        let presence = lines.pop().map(parse::<Value>).transpose()?;
        let entries = lines
            .into_iter()
            .map(parse::<Entry>)
            .collect::<Result<Vec<_>, _>>()?;
        self.participant(link, |p| {
            p.taken = entries.last().map_or(p.taken, |entry| entry.n);
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
        let receive = self.receive_handing_over(link);
        tokio::pin!(receive);
        let mut ticks =
            tokio::time::interval_at(tokio::time::Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY);
        let mut progress = 0.0;
        loop {
            tokio::select! {
                result = &mut receive => return result,
                _ = context.ct.cancelled() => return outcome(Err("cancelled".to_owned())),
                _ = ticks.tick() => if let Some(token) = &token {
                    progress += 1.0;
                    let message = "Waiting for a message that pings you.";
                    let param = ProgressNotificationParam::new(token.clone(), progress).with_message(message);
                    let _ = context.peer.notify_progress(param).await;
                },
            }
        }
    }

    /// The entries as the agent reads them: text, with images inline.
    async fn describe(
        &self,
        entries: &[Entry],
        name: &str,
        link: &str,
        presence: Option<&Value>,
    ) -> Vec<ContentBlock> {
        let mut content = Vec::new();
        let mut text = String::new();
        for entry in entries {
            let (n, from) = (entry.n, &entry.from);
            let essence = entry.kind.split(';').next().unwrap_or_default().trim();
            let marker = if pings(entry, name) { "ping" } else { "fyi" };
            let heading = format!("#{n} from {from} ({marker})");
            let file = format!(
                "{essence}{}, {} bytes",
                entry
                    .filename
                    .as_ref()
                    .map(|f| format!(" {f}"))
                    .unwrap_or_default(),
                entry.size
            );
            match (essence, &entry.text) {
                ("application/vnd.agentwire.join", _) => {
                    text.push_str(&format!("#{n} {from} joined."))
                }
                ("application/vnd.agentwire.leave", _) => {
                    text.push_str(&format!("#{n} {from} left."))
                }
                (_, Some(body)) if essence.starts_with("text/") && entry.filename.is_none() => {
                    text.push_str(&format!("{heading}:\n{body}"))
                }
                (_, _) if essence.starts_with("image/") && entry.size <= IMAGE_MAX => {
                    match self.fetch(link, n).await {
                        Ok(data) => {
                            text.push_str(&format!("{heading}: {file}, shown next."));
                            content.push(ContentBlock::text(std::mem::take(&mut text)));
                            content.push(ContentBlock::image(BASE64.encode(data), essence));
                        }
                        Err(error) => {
                            text.push_str(&format!("{heading}: {file}, not fetched: {error}"))
                        }
                    }
                }
                _ => text.push_str(&format!(
                    "{heading}: {file}, fetched with curl -s -o <file> '{link}/{n}'"
                )),
            }
            text.push_str("\n\n");
        }
        text.push_str(&describe_presence(presence, name));
        content.push(ContentBlock::text(text));
        content
    }

    async fn fetch(&self, link: &str, n: u64) -> Result<Vec<u8>, String> {
        let response = self
            .http
            .get(format!("{link}/{n}"))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("relay answered {}", response.status().as_u16()));
        }
        let bytes = response.bytes().await.map_err(|error| error.to_string())?;
        Ok(bytes.to_vec())
    }
}

#[derive(Deserialize)]
struct Joined {
    name: String,
    read: u64,
    link: String,
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
                let received = tokio::select! {
                    _ = task.cancelled() => return Err(TaskExit::Cancelled),
                    received = tokio::time::timeout(TASK_WAIT, room.receive_for(&link)) => received,
                };
                Ok(match received {
                    Ok(Ok((content, handover))) => {
                        // Handed over once the client fetches the result.
                        room.handovers
                            .lock()
                            .unwrap()
                            .insert(task.task_id().to_owned(), handover);
                        CallToolResult::success(content)
                    }
                    Ok(Err(error)) => outcome(Err(error)),
                    Err(_) => outcome(Ok(
                        "Nothing pinged you in a day. Call receive again to keep listening."
                            .to_owned(),
                    )),
                })
            })
        });
        Ok(CallToolResponse::Task(CreateTaskResult::new(task)))
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        let task = self.tasks.get_task(&request.task_id)?;
        if task.status().is_terminal() {
            let handover = self.handovers.lock().unwrap().remove(&request.task_id);
            if let Some(handover) = handover {
                self.hand_over(handover).await;
            }
        }
        Ok(GetTaskResult::new(task))
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

/// The service to mount at a path of an HTTP server, such as `/mcp`,
/// accepting requests for the hosts given, such as `localhost` or a public domain.
pub fn service(
    room: Room,
    hosts: impl IntoIterator<Item = impl Into<String>>,
) -> StreamableHttpService<Room, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(room.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(hosts)
            .with_max_request_body_bytes(REQUEST_MAX),
    )
}

/// Chat's pings: a `ping` parameter of the type names whom a message wakes, or `*` for
/// everyone, and empty, no one. A message without one pings everyone,
/// but no entry of a vendor type, such as a join.
fn pings(entry: &Entry, name: &str) -> bool {
    let essence = entry.kind.split(';').next().unwrap_or_default().trim();
    match ping(entry) {
        Some(names) => names
            .split_whitespace()
            .any(|pinged| pinged == "*" || pinged == name),
        None => !essence.starts_with("application/vnd."),
    }
}

fn ping(entry: &Entry) -> Option<&str> {
    entry.kind.split(';').skip(1).find_map(|parameter| {
        let (key, value) = parameter.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("ping")
            .then(|| value.trim().trim_matches('"'))
    })
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
