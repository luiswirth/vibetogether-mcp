//! An MCP server that lets an agent take part in a Vibe Together room.
//!
//! It speaks chat on top of agentwire: every tool works on the participant's own link,
//! which `join` returns, so the server keeps no sessions of its own.
//! Its only state is, per participant, what it took from the relay and has not handed on yet.
//!
//! A message counts as read exactly when it is in the agent's context:
//! the server waits for newer entries without moving the read position,
//! and acknowledges the entries it hands over as it hands them over.

mod accounts;

pub use accounts::Accounts;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentwire::chat::{self, Ping};
use agentwire::{Client, Entry, Joined, Presence, Receive};

use axum::http::request::Parts;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::{Extension, ToolCallContext};
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
Join with the session's token or link and your name, such as luiswirth/claude for luiswirth's agent.
A name marked verified is that account's or one of its agents'; any other name says only who joined with it.
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
pub(crate) const REQUEST_MAX: usize = 16 * 1024 * 1024;

/// The server: one value shared by every MCP request.
#[derive(Clone)]
pub struct Room {
    /// The relay a session token alone refers to.
    pub relay: String,
    client: Client,
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
    presence: Option<BTreeMap<String, Presence>>,
}

/// Entries handed to the agent, up to `read`, to acknowledge at the relay.
struct Handover {
    link: String,
    read: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct JoinArgs {
    /// The session's token, or its full link for a relay other than the default.
    session: String,
    /// Your name, lower case with hyphens, as `<person>/<agent>` such as `luiswirth/claude`.
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
            client: Client::new(),
            participants: Arc::default(),
            tasks: TaskManager::new(),
            handovers: Arc::default(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Join a session. Returns your own link, a secret that the other tools take."
    )]
    async fn join(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<JoinArgs>,
    ) -> CallToolResult {
        let token = accounts::bearer(&parts.headers);
        outcome(self.join_session(&args.session, &args.name, token).await)
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
    async fn receive(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<LinkArgs>,
    ) -> CallToolResult {
        let token = accounts::bearer(&parts.headers);
        self.receive_handing_over(&args.link, token).await
    }

    #[tool(description = "Leave the session.")]
    async fn leave(&self, Parameters(args): Parameters<LinkArgs>) -> CallToolResult {
        outcome(self.leave_session(&args.link).await)
    }
}

impl Room {
    /// Joins with the token the agent's MCP client presented, where it presented one.
    async fn join_session(
        &self,
        session: &str,
        name: &str,
        token: Option<&str>,
    ) -> Result<String, String> {
        let link = if session.contains("://") {
            session.trim_end_matches('/').to_owned()
        } else {
            format!("{}/v2/s/{}", self.relay, session.trim())
        };
        let joined = self.client.join(&link, name, token).await.map_err(fail)?;
        self.know(&joined);
        let now = Receive {
            after: Some(joined.read),
            read: Some(joined.read),
            wait: Some(Duration::ZERO),
            ..Receive::default()
        };
        let received = self
            .client
            .receive(&joined.link, &now)
            .await
            .map_err(fail)?;
        Ok(format!(
            "Joined as {}. Your own link, a secret that send, receive and leave take: {}\n{}",
            joined.name,
            joined.link,
            describe_presence(Some(&received.presence), &joined.name),
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
        let ping = match args.ping {
            None => None,
            Some(names) => {
                if !names.iter().all(|name| name == "*" || Ping::valid(name)) {
                    return Err("ping takes names, or *, each without spaces or quotes".to_owned());
                }
                Some(if names.iter().any(|name| name == "*") {
                    Ping::Everyone
                } else {
                    Ping::Only(names)
                })
            }
        };
        let typed = |kind: &str| match &ping {
            None => kind.to_owned(),
            Some(ping) => ping.on(kind),
        };
        let (kind, filename, body) = match (args.text, args.file) {
            (Some(text), None) => (typed("text/markdown"), None, text.into_bytes()),
            (None, Some(file)) => {
                let data = BASE64
                    .decode(file.data.trim())
                    .map_err(|error| format!("the file's data is no base64: {error}"))?;
                (typed(&file.kind), file.name, data)
            }
            _ => return Err("give either a text or a file".to_owned()),
        };
        let n = self
            .client
            .send(&args.link, &kind, filename.as_deref(), body)
            .await
            .map_err(fail)?;
        Ok(format!("Sent as #{n}."))
    }

    async fn leave_session(&self, link: &str) -> Result<String, String> {
        self.client.leave(link).await.map_err(fail)?;
        self.participants.lock().unwrap().remove(link);
        Ok("Left the session.".to_owned())
    }

    /// Takes entries from the relay until one pings this participant, then returns all of
    /// them, without acknowledging them.
    async fn receive_for(
        &self,
        link: &str,
        token: Option<&str>,
    ) -> Result<(Vec<ContentBlock>, Handover), String> {
        let name = match self.participant(link, |p| p.name.clone()) {
            Some(name) => name,
            None => {
                // Fetching an entry fails for a link that is not in, without side effects,
                // before joining again, which does nothing while in, tells whose the link is.
                self.client.fetch(link, 1).await.map_err(fail)?;
                let joined = self.client.rejoin(link, token).await.map_err(fail)?;
                self.know(&joined);
                joined.name
            }
        };
        loop {
            let ready = self.participant(link, |p| {
                p.pending
                    .iter()
                    .any(|entry| chat::pings(&entry.kind, &name))
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
    async fn receive_handing_over(&self, link: &str, token: Option<&str>) -> CallToolResult {
        match self.receive_for(link, token).await {
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
        let now = Receive {
            after: Some(taken),
            read: Some(read),
            wait: Some(Duration::ZERO),
            ..Receive::default()
        };
        // The next receive acknowledges it anyway, through its read position.
        let _ = self.client.receive(&link, &now).await;
    }

    /// One receive from the relay, after what was taken, into what is pending,
    /// leaving the read position at what was handed over.
    async fn pull(&self, link: &str) -> Result<(), String> {
        let (taken, read) = self.participant(link, |p| (p.taken, p.read));
        let receive = Receive {
            after: Some(taken),
            read: Some(read),
            wait: Some(RELAY_WAIT),
            ..Receive::default()
        };
        let received = self.client.receive(link, &receive).await.map_err(fail)?;
        self.participant(link, |p| {
            p.taken = received.entries.last().map_or(p.taken, |entry| entry.n);
            p.pending.extend(received.entries);
            p.presence = Some(received.presence);
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
        bearer: Option<&str>,
        context: &RequestContext<RoleServer>,
    ) -> CallToolResult {
        let token = context.meta.get_progress_token();
        let receive = self.receive_handing_over(link, bearer);
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
        presence: Option<&BTreeMap<String, Presence>>,
    ) -> Vec<ContentBlock> {
        let mut content = Vec::new();
        let mut text = String::new();
        for entry in entries {
            let (n, from) = (entry.n, &entry.from);
            let essence = entry.essence();
            let marker = if chat::pings(&entry.kind, name) {
                "ping"
            } else {
                "fyi"
            };
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
                (agentwire::JOIN, _) => text.push_str(&format!("#{n} {from} joined.")),
                (agentwire::LEAVE, _) => text.push_str(&format!("#{n} {from} left.")),
                (_, Some(body)) if essence.starts_with("text/") && entry.filename.is_none() => {
                    text.push_str(&format!("{heading}:\n{body}"))
                }
                (_, _) if essence.starts_with("image/") && entry.size <= IMAGE_MAX => {
                    match self.client.fetch(link, n).await {
                        Ok(payload) => {
                            text.push_str(&format!("{heading}: {file}, shown next."));
                            content.push(ContentBlock::text(std::mem::take(&mut text)));
                            content
                                .push(ContentBlock::image(BASE64.encode(payload.bytes), essence));
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
        let token = context
            .extensions
            .get::<Parts>()
            .and_then(|parts| accounts::bearer(&parts.headers))
            .map(str::to_owned);
        if !context
            .client_capabilities()
            .is_some_and(|caps| caps.supports_tasks())
        {
            return Ok(CallToolResponse::Complete(
                self.receive_blocking(&link, token.as_deref(), &context)
                    .await,
            ));
        }
        let room = self.clone();
        let ttl = TASK_WAIT + Duration::from_secs(60);
        let options = TaskOptions::new().with_ttl_ms(ttl.as_millis() as u64);
        let task = self.tasks.spawn(options, move |task| {
            Box::pin(async move {
                let received = tokio::select! {
                    _ = task.cancelled() => return Err(TaskExit::Cancelled),
                    received = tokio::time::timeout(TASK_WAIT, room.receive_for(&link, token.as_deref())) => received,
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

/// The server at `/mcp`, accepting requests for the hosts given, such as `localhost` or
/// a public domain, and with an account service, asking to sign in to it where joining
/// a room needs it.
pub fn router(
    room: Room,
    hosts: impl IntoIterator<Item = impl Into<String>>,
    accounts: Option<Accounts>,
) -> axum::Router {
    let relay = room.relay.clone();
    let router = axum::Router::new().nest_service("/mcp", service(room, hosts));
    match accounts {
        Some(accounts) => router.layer(axum::middleware::from_fn_with_state(
            Arc::new(accounts.on(&relay)),
            accounts::check,
        )),
        None => router,
    }
}

fn service(
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

fn describe_presence(presence: Option<&BTreeMap<String, Presence>>, me: &str) -> String {
    let names: Vec<String> = presence
        .map(|people| {
            people
                .iter()
                .map(|(name, state)| {
                    let marks: Vec<String> = [
                        (name == me).then(|| "you".to_owned()),
                        state
                            .account
                            .as_deref()
                            .map(|account| verified(name, account)),
                        (name != me && state.receiving).then(|| "listening".to_owned()),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    if marks.is_empty() {
                        name.clone()
                    } else {
                        format!("{name} ({})", marks.join(", "))
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    format!("In the room: {}.", names.join(", "))
}

/// What a name's account says of it: that it is verified, where the name is the
/// account's or one of its agents', as Vibe Together's accounts make it.
fn verified(name: &str, account: &str) -> String {
    if name.split('/').next() == Some(account) {
        "verified".to_owned()
    } else {
        format!("account {account}")
    }
}

/// What the agent reads of a failed request to the relay.
fn fail(error: agentwire::Error) -> String {
    error.to_string()
}

fn outcome(result: Result<String, String>) -> CallToolResult {
    match result {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
    }
}
