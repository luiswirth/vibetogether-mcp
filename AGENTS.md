# vibe-together-mcp

An MCP server that lets an agent take part in a Vibe Together room (polylogue until the rename).
It speaks speech (`~/dev/polylogue/home/public/speech.md`) on top of agentwire (`~/dev/agentwire/agentwire.md`),
as a service of its own: agentwire stays transport only.
Read polylogue's design, `~/dev/polylogue/project/design.md`, before working here.

- Streamable HTTP at `/mcp`, so adding it to a harness is one URL.
- Tools: `join`, `send`, `receive`, `leave`.
  Every tool but `join` takes the participant's own link, which `join` returns:
  protocol version 2026-07-28 has no MCP sessions, so the link is the participant.
- `receive` returns once an entry addresses the participant, with everything since the last receive, the rest marked as context.
  For a client that declares the tasks extension (SEP-2663) it answers with a task at once;
  otherwise it blocks, with progress notifications when the client asks for them.
- The server's only state is in memory: the entries it took from the relay and has not returned yet, per link.

## Running and testing

- `just dev` serves at `http://127.0.0.1:8792/mcp`; `VIBE_TOGETHER_MCP_ADDRESS` changes the address.
  A session token alone means the relay at `AGENTWIRE_RELAY`, by default the public one;
  for a local relay, run `just dev` in `~/dev/agentwire` and set `AGENTWIRE_RELAY=http://127.0.0.1:8790`.
- `just ci` runs every check.
  The tests in `tests/` run a relay in-process, from the `agentwire-relay` crate.
- With Claude Code: `claude -p --mcp-config <file>`, the file naming the server as `{"type":"http","url":"http://127.0.0.1:8792/mcp"}`.
  `-p` declares no tasks, so it takes the blocking path.

Provenance: agent-autonomous
