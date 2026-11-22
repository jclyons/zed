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

## Not in this ticket

R5 (Glider archive check) and the R6 poll are T9 and land after this merges. `bundle`/`selfcheck` are T1 (PR #6);
`deploy` calls `script/cpzed/bundle` in the build worktree and refuses to deploy if `selfcheck` is missing.
