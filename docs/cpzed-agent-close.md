# `zed --agent-close` / `--agent-rename` (fork notes)

Companion to `--agent-new` / `--agent-list` (design: `designs/cpzed-3-agent-new.md`, sandbox root).

- `zed --agent-close <thread-id>`: closes that **terminal** thread (the sidebar's close: the terminal process
  and the metadata store entry go). Stale threads of closed workspaces (store only) are deleted too.
  Unknown id: exit 0, `{"closed": false, "message": "no such thread: ..."}`. Malformed id: exit 1.
  An agent-panel (non-terminal) thread id is refused: exit 6, nothing changed.
- `zed --agent-close --title "<exact title>"`: same by exact (custom) title. No match: exit 0. Several
  matches: exit 5, stderr lists the ids, nothing is closed (close one by id).
- `zed --agent-rename <id> "<title>"`: sets the custom title; replies with the `--agent-new` JSON
  (`id, kind, title, working_directory, folder_paths, window_id, workspace_id`). Unknown id: exit 3.
  Empty/multi-line title: exit 1.
- Exit codes shared with `--agent-new`: 1 internal, 4 AI disabled. Success reply for close:
  `{"id", "kind", "title", "closed": true, "was_open": bool}`.
- Wire: `CliRequest::CloseAgentThread { id, title }` and `RenameAgentThread { id, title }` appended last
  (bincode; CLI and app must come from the same tree). Handler: `crates/zed/src/zed/cli_agent.rs`.
- Wind-down recipe: `bin/cpzed-launch --close NAME` (sandbox root).
