# CPZed deploy pipeline (`script/cpzed/deploy`)

Ticket CPZED-23 (T2 of CPZED-17). Design: `designs/cpzed-fork-update-install.md` section 5 and
`designs/DECISIONS.md` items 11, 12, 15, 16. This is the code that quits the user's Zed and swaps
`/Applications/CPZed.app` unattended, so every guard has a test that fails when the guard is removed.

## Files

| File | Role |
|---|---|
| `deploy` | Orchestrator. `start` (detached), `run [--first-swap]`, `eligible`, `quiescent`, `status`. |
| `detach` | `nohup` + perl `setsid`: own session, no TTY, stdio to a log, optional pidfile. |
| `gate` | Quiescence gate (`check`, `wait`). Never forces. |
| `health` | Post-relaunch health check. Ignores keychain and privacy noise. |
| `which` | Read-only summary of what is installed and running. |
| `backup` | Data backup, rollback ring (3), stock archive. |
| `deploy-util` | Python helpers: state file, atomic `renamex_np`, guarded `rm`, process walk, eligibility. |
| `deploy-lib.sh` | Shared paths, test-prefix confinement, notifications, hold. |
| `test-deploy` | Temp-prefix tests with a dummy app, plus one mutation test per guard. |

## Phases (`state.json` `phase`)

`building > staged > gating > snapshot > quitting > swapping > launching > health > done`. A failed health check
goes to `rollback`: the previous build first (re-stamped `n+1`, re-signed), then stock from its zip.
A deploy that died in `swapping`, `launching`, `health` or `rollback` is never resumed: the next run sets
`hold` and notifies. A death before the swap restarts from the staged bundle.

## Exit codes of `deploy run`

0 deployed. 10 nothing to do, deferred (interval, coalesce) or the commit failed before. 20 rolled back to the
previous build. 21 rolled back to stock (hold set). 30 aborted (hold set, or the old app was relaunched).
40 no rollback worked (hold set). 75 another deploy is running. 78 hold file present.

## Operating it

- The sre calls `deploy quiescent` about every 60 s while `ListAgents` shows every local session idle. The gate
  accepts only a `quiescent_at` under 120 s old. Then `deploy start` (detached) per eligible commit; it is
  safe to call repeatedly: a lock refuses a second run.
- Notifications go to `~/Library/Application Support/CPZed/notifications.jsonl` (kinds: `ready`, `restarting`,
  `running`, `rollback`, `waiting`, `abort`, `urgent`, `first-swap`) and to a macOS notification. The sre forwards
  them to cpzed-manager. The `running` text carries the resume list and the MBP/Glider notice.
- Kill switch: create `~/Library/Application Support/CPZed/hold`. Remove it to resume.
- `deploy run --first-swap` is the removal-window install: it quits stock, prints
  `sudo rm -rf /Applications/Zed.app`, and waits (CPZED_FIRST_SWAP_WAIT_S, 30 min) for the user to
  `touch ~/Library/Application\ Support/CPZed/first-swap-confirm`. It never runs `sudo`. On a timeout, or if
  stock is still present after the marker, it relaunches stock, sets `hold` and notifies. After a good health check
  the `running` note carries the one `sudo ln -sfn ... /usr/local/bin/zed` line.

## Eligibility

The newest first-parent commit on `fork/main` whose subject carries `(#N)` (squash) or `Merge pull request #N`,
where `merge-ledger.json` has PR N with `reviewer` and `tester` `clean`, `hold` false, `user_approved` true, and
`gh pr view N` shows MERGED, `mergeCommit.oid` equal to the commit and `headRefOid` equal to the ledger `sha`.
Since the last deployed commit, every first-parent commit must be eligible: the first ineligible one blocks the
deploy ("blocked by <sha>"). `fork/main` must be quiet for 30 min (coalesce), at least 4 h since the last deploy,
at most 2 per day; rollbacks are exempt.

## Safety properties added in review round 1

- **Fail closed on overrides.** Outside a confined test run (`CPZED_TEST_PREFIX` unset) every script refuses to
  start (exit 2) if ANY `CPZED_*` variable is present, even empty; only `CPZED_IDENTITY` is allowed. A test run must
  point every path at a real directory inside the prefix and override quit/open/notify.
- **The gate cannot be skipped.** `deploy` proceeds only when `gate wait` exits exactly 0 (a crash aborts and sets
  `hold`). After the Glider archiver wait the whole `gate check` runs again, then the staged bundle is re-verified
  (signature, sha, n) before the quit and again right before the swap.
- **`--first-swap` needs a verified stock archive** (sha256 sidecar plus a temp-extract `codesign --verify
  --deep --strict`) before anything is quit; it creates one with `backup stock-add` when missing.
- **Eligibility is a strict chain** from the deployed commit (`git rev-list --first-parent deployed..base`, no
  window); a deployed commit that is not an ancestor of `fork/main` blocks. Only the very first deploy (no
  `deployed_sha`) accepts the newest eligible commit in the last 50, because all earlier history predates the ledger.
- **One deploy at a time** by a kernel `flock` held on fd 200 for the whole run (no stale-lock takeover).
- **Quit refusal = abort + hold + notify**, never a retry loop and never a kill (a deliberate deviation from design
  section 5 step 5, confirmed by cpzed-manager).
- The data backup is taken after the quit, when the SQLite files are quiescent.

## Safety properties added in review round 2

- **A lost or corrupt `state.json` fails closed.** Only a MISSING file is an empty state. An unreadable, truncated,
  garbage, wrong-typed or non-object file makes `deploy run` set `hold`, notify urgently and exit 30; it is never
  rewritten. With CPZed.app installed, `deployed_sha` comes from the installed app's `cpzed-build.json`, and `n`
  is at least that build's n plus 1; a state that disagrees with the installed app, or an installed app without a
  valid `cpzed-build.json`, also holds. The first-deploy leniency applies only to the attended `--first-swap`.
- **Test prefix** must be a directory under a temp root that equals or contains none of the real HOME, `~/Library`,
  Application Support, `~/.config`, `/Applications`; `CPZED_PORT` is required under a prefix.
- **`gate` fails closed** when `deploy-util busy` or `claudes` fails.
- **`verify_app`** pins `Authority=<identity>` as a whole line and the hardened-runtime flag, besides
  `codesign --verify --deep --strict` (stock is verified for signature validity only).
- **The app is identified by its executable path, not by the port** (N4): the quit is never aimed at a process on the
  port that is not `$APP/Contents/MacOS/zed`; a running-but-not-listening instance is still quit; and right after
  the quit and again right before the rename no process may run from either bundle's executable (a quit race holds).
  Paths are compared resolved and case-insensitively. The executable and argv come from the kernel
  (`sysctl KERN_PROCARGS2`), so a bundle path with spaces matches; an interpreted script is identified by its script
  path. Zed's own `--crash-handler` helper (same executable) is not counted as a running instance. Known limit: a
  process started from a copy of the bundle at another path is not seen. A hold names the pid and argv still running.
- **Mid-run state corruption** (the file turns unreadable after the start check): the next state write is refused,
  and `deploy` sets `hold`, notifies and exits 30 instead of carrying on. Reads of a corrupt file return empty, but
  the pipeline writes its phase at every step, so it stops at the next transition (before the quit or the swap).

## Safety properties added in review round 3

- **A signal never leaves the editor down silently.** `deploy` traps TERM/INT/HUP: if the app was quit and the swap
  is not done it relaunches the old app, sets `hold` and notifies urgently; after the swap or during a rollback it
  sets `hold` and notifies (nothing is guessed).
- **The first deploy is strict too.** It ships every fork-only first-parent commit up to the target
  (`origin/main..target`), so every one of them must have an approved ledger entry; the `ready` and first-swap
  notices list the included commits.
- A session-snapshot failure aborts before the quit; `quit_and_wait` waits for the process (not just the port) to exit.

## Not in this ticket

R5 (Glider archive check) and the R6 poll are T9 and land after this merges. `bundle`/`selfcheck` are T1 (PR #6);
`deploy` calls `script/cpzed/bundle` in the build worktree and refuses to deploy if `selfcheck` is missing.
