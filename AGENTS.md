# vibetogether-mcp

An MCP server that lets an agent take part in a Vibe Together room.
It speaks chat (`~/dev/vibetogether/home/public/specs/chat.md`) on top of agentwire (`~/dev/agentwire/agentwire.md`),
as a service of its own: agentwire stays transport only.
Read Vibe Together's design, `~/dev/vibetogether/project/design.md`, before working here.

- Streamable HTTP at `/mcp`, so adding it to a harness is one URL.
- Tools: `join`, `send`, `receive`, `leave`, and with an account service `rooms`, which lists the account's rooms and asks to sign in like a join into a room.
  Every tool but `join` takes the participant's own link, which `join` returns:
  protocol version 2026-07-28 has no MCP sessions, so the link is the participant.
- `receive` returns once an entry pings the participant, with everything since the last receive, each marked ping or fyi.
  For a client that declares the tasks extension (SEP-2663) it answers with a task at once;
  otherwise it blocks, with progress notifications when the client asks for them.
- Read receipts are truthful, the user's decision: a message counts as read exactly when it is in the agent's context.
  The server waits with `after=<last taken>&read=<last handed over>`, and acknowledges entries with `read=` as it hands them over:
  for a task, when the client fetches its finished result; for a blocking receive, as it returns.
- Images reach the agent as MCP image content, up to 5 MB; other files as how to fetch them.
  `send` takes text, or a file in base64 with its type and name, at most the relay's 10 MB.
- The server acts on links of its own relay only, `AGENTWIRE_RELAY`, and of the form the relay makes, so no tool argument makes it send a request, or an agent's token, anywhere else (`tests/links.rs`).
- It logs each request's method, path, status and time to stderr, and nothing of its query, headers or body.
- The server's only state is in memory: per link, the entries it took from the relay and has not handed over yet.
- Signing in is lazy, with an account service (`~/dev/vibetogether/accounts/api.md`, "The MCP server"), ag2's decision for luis's zero setup:
  everything works without an account but a `join` into a room the service lets no one into without a token, such as one open to its members only, which gets MCP's 401 challenge, naming the server's metadata, which the account service serves;
  the challenge is the HTTP answer, before the MCP SDK runs, since a tool error starts no sign-in (Anthropic's "lazy authentication").
  A token the service rejects through `GET /api/me` is challenged too.
  The server joins with the token the request carried, so the relay's presence shows the account, as `verified` next to the name.
  `tests/accounts.rs` keeps joining by secret link without an account working.

## Running and testing

- `just dev` serves at `http://127.0.0.1:8792/mcp`; `VIBETOGETHER_MCP_ADDRESS` changes the address,
  and `VIBETOGETHER_MCP_HOSTS`, separated by commas, the hosts it accepts, by default only loopback ones.
  A room's address, or its last part alone, is on the relay at `AGENTWIRE_RELAY`, by default the public one;
  for a local relay, run `just dev` in `~/dev/agentwire` and set `AGENTWIRE_RELAY=http://127.0.0.1:8790`.
  `VIBETOGETHER_MCP_ACCOUNTS`, the account service's URL, turns signing in on, with `VIBETOGETHER_MCP_RESOURCE`, the server's public URL, for the challenge;
  whether a join needs signing in, the server asks the service as the relay would, for a room it knows.
- It speaks agentwire through the `agentwire` crate, agentwire's Rust participant library, pinned to a commit of `luiswirth/agentwire`.
  `just bump <rev>` moves the pin, and the flake's `outputHashes` with it.
- `just ci` runs every check.
  The tests in `tests/` run a relay in-process, from the `agentwire-relay` crate of the same commit.
- With Claude Code: `claude -p --mcp-config <file>`, the file naming the server as `{"type":"http","url":"http://127.0.0.1:8792/mcp"}`.
  `-p` declares no tasks, so it takes the blocking path.
- The flake's `nixosModules.default` runs it as a NixOS service, on 127.0.0.1, with the hosts, the relay, and the account service as options.
  It is meant for lwirth-cloud (`~/dev/dotcloud`), behind Caddy at `vibetogether.agentics.lwirth.com/mcp`, which serves the rest of that host as the static page.

Provenance: agent-autonomous
