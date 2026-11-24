# `zed --agent-close` / `--agent-rename` (fork notes)

Companion to `--agent-new` / `--agent-list` (design: `designs/cpzed-3-agent-new.md`, sandbox root).

- `zed --agent-close <thread-id>`: closes that **terminal** thread (the sidebar's close: the terminal process
  and the metadata store entry go). Stale threads of closed workspaces (store only) are deleted too.
  Unknown id: exit 0, `{"closed": false, "message": "no such thread: ..."}`. Malformed id: exit 1.
  An agent-panel (non-terminal) thread id is refused: exit 7, nothing changed.
- `zed --agent-close --title "<exact title>"`: same by exact (custom) title. No match: exit 0. Several
  matches: exit 5, stderr lists the ids, nothing is closed (close one by id).
- `zed --agent-rename <id> "<title>"`: sets the custom title; replies with the `--agent-new` JSON
  (`id, kind, title, working_directory, folder_paths, window_id, workspace_id`). Unknown id: exit 3.
  Empty, multi-line or over-200-character title: exit 1.
- Exit codes (one table for all `--agent-*` commands): 0 ok (also: close of an unknown id/title);
  1 generic (bad args, invalid id/title, read-only, app too old to answer); 2 timeout (incl. metadata
  load); 3 no match / unknown id (rename); 4 AI disabled; 5 ambiguous title (close); 6 reserved for
  hierarchy refusals (cycle etc., H7); 7 not a terminal thread (agent-panel thread refused). Success reply for close:
  `{"id", "kind", "title", "closed": true, "was_open": bool}`.
- Wire: `CliRequest::CloseAgentThread { id, title }` and `RenameAgentThread { id, title }` appended last
  (bincode; CLI and app must come from the same tree). Handler: `crates/zed/src/zed/cli_agent.rs`.
- Wind-down recipe: `bin/cpzed-launch --close NAME` (sandbox root).
