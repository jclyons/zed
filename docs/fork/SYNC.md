# Upstream sync (CPZed fork)

How the fork (`jclyons/zed`) keeps up with upstream (`zed-industries/zed`). Design: `designs/cpzed-fork-update-install.md` (Decision 2, ticket T3). User decisions: `designs/DECISIONS.md` (items 2 and 3).

## Rules

- **Merge commits, never squash.** A sync PR is merged with a real merge commit. This is a user-approved exception to the fork's squash-only rule. A squash or rebase would make the next sync re-conflict on every commit already merged.
- **Daily, plus on demand.** `cpzed-sre` runs `script/cpzed/sync` daily. Anyone can run it by hand.
- **Never push `main`, never force-push, never touch upstream.** The script only fetches from `origin` (upstream, read-only) and pushes one branch, `cpzed-sync-<upstream10>`, to `fork`. It refuses to run if the `fork` remote points at `zed-industries/zed`. No PR, comment, issue or push ever goes to `zed-industries/zed` (Zed's policy bans agent-authored contributions).
- **No fork migration on an upstream DB domain.** The fork must never append a migration to a database domain that upstream owns (`sqlez` migrations: workspace, KVP, terminal-thread metadata and so on). Upstream may later add a step at the same index, and `sqlez` bails on a changed step at an index it has already stored (`sqlez/src/migrations.rs`). Fork-owned state goes in a fork-owned domain or a new fork-owned table. A sync PR must be checked for this (see the checklist).

## Running it

```bash
cd ~/code/cpzed-sandbox/worktree-branch-sync   
script/cpzed/sync --dry-run                    # fetch and report only
script/cpzed/sync                              # merge + push branch + open PR
script/cpzed/sync --no-pr                      # merge into a LOCAL branch, no push, no PR
```

Outcomes (exit code):

| Situation | Result |
|---|---|
| `fork/main` already contains `origin/main` | Prints "up to date", does nothing. Exit 0. |
| A `cpzed-sync-*` PR is already open on the fork (for an older upstream head) | Skips and names the open PR, so daily runs never pile up overlapping sync PRs. Exit 0. |
| Branch `cpzed-sync-<upstream10>` already on `fork` and a PR exists for it (open or merged) | Reports the PR, does nothing. Exit 0. |
| That branch is on `fork` but **no PR exists** (an earlier `gh pr create` failed) | Opens the PR, after checking the tip is a 2-parent merge containing the upstream head. Exit 0. |
| Clean merge | `cpzed-sync-<upstream10>` = `fork/main` + a `--no-ff` merge of `origin/main`, pushed to `fork` by explicit refspec; PR opened on `jclyons/zed` titled `CPZED sync: upstream <base10>..<head10> (<n> commits)`. Exit 0. |
| Conflict | Prints the conflicting files, aborts, removes the temp worktree. Exit **2**. |
| Merge failed but not a conflict (for example unrelated histories) | Prints git's message. Exit **3**. |
| Any error, including every `gh` or `git ls-remote` failure (auth, network), a bad remote or a bad `--repo` | Message on stderr. Exit **1**. The script never guesses "no PR" or "no branch" from a failed query. |

The script never switches or modifies the worktree it is run from (it only fetches remote-tracking refs). It does the merge in a throwaway `git worktree add --detach` under a temp dir, removed on every exit path, so a dirty or busy worktree is fine. `--repo` must be `jclyons/zed` (case-insensitive) and the `fork` remote's push URL must be `github.com/jclyons/zed`; anything else, including any `zed-industries` URL, is refused.

## Conflicts: handing one to a coder

1. The script exits 2 and lists the files. `cpzed-sre` sends `cpzed-manager` one line: the upstream range, the conflicting files, and the script output.
2. `cpzed-manager` assigns a coder and a Jira ticket under CPZED-17. The coder works in `worktree-branch-sync` (the only editor there), runs `git switch -c cpzed-sync-<upstream10> fork/main` and `git merge --no-ff origin/main`, resolves the conflicts, and commits the merge commit.
3. Resolve toward upstream in upstream-hot files and keep the fork change as small as possible (the fork keeps changes behind a narrow seam for exactly this reason). Note each resolution in the PR body.
4. Push the branch to `fork` only and open the PR on `jclyons/zed` with the same title format. The same merge-commit rule applies.
5. Never resolve by dropping upstream changes or by force-pushing `main`.

## Reviewing a sync PR (reviewer)

The diff against `main` is the whole upstream range, so it is not reviewable as-is. Review the **merge itself**:

```bash
git fetch fork
git show --remerge-diff <merge-sha>        # git >= 2.36; what the merge changed relative to an automatic re-merge
git diff <merge-sha>^1 <merge-sha> --stat  # size of the upstream range, sanity check only
```

- A clean automatic merge has an **empty** `--remerge-diff`. Anything in it is a manual resolution or an evil merge: read every hunk.
- Confirm the commit has exactly two parents: first parent = fork `main`, second = the upstream head named in the PR body.
- Confirm the PR body's upstream head equals `git rev-parse origin/main` at the time of review (or is an ancestor of it).

## Drift checklist (reviewer, and tester where noted)

Each item is a check on what upstream changed in the range (`git diff <fork-main>..<upstream-head> -- <path>`).

1. **Updater gate**: `crates/auto_update/src/auto_update.rs` still reads `ZED_UPDATE_EXPLANATION` the same way (compile-time and runtime). Run `script/cpzed/selfcheck` once it exists (T1).
2. **Release channel**: `crates/release_channel/src/lib.rs` and `script/bundle-mac` (channel selection, bundle sections, Info.plist keys, `LSEnvironment`, icons, signing, version stamping). T1's `bundle` depends on their shape.
3. **Single-instance port**: `crates/zed/src/zed/mac_only_instance.rs` (the port formula the deploy gate and health check use).
4. **CLI / agent seam**: `crates/cli/src/cli.rs`, `crates/zed/src/zed/open_listener.rs` (`CliRequest`, `zed://agent`), and `crates/agent_ui/src/agent_panel.rs` and `terminal_thread_metadata_store.rs`, where the fork's thread API and terminal restore live. Rename or signature changes here need a fork follow-up.
5. **Sidebar and workspace persistence**: `crates/sidebar/`, `crates/workspace/src/multi_workspace.rs`, `persistence.rs` (the sidebar revamp).
6. **DB migrations (hard rule)**: list upstream migration changes in the range, `git diff <fork-main>..<upstream-head> -- '*.rs' | grep -n 'MIGRATIONS\|sql!'` or read the touched `db`/`persistence` files. Then confirm **no fork migration was added to an upstream domain**, and that no upstream step was reordered under a fork step. Any fork-owned migration must be in a fork-owned domain.
7. **Workflows**: `.github/workflows/` changes in the range. Upstream workflows run on the fork's Actions, so the reviewer flags any change there to `cpzed-manager` (new triggers, secrets, deploy or release jobs, runner labels) before approving.
8. **Fork-owned dirs untouched**: `script/cpzed/`, `docs/fork/`, `briefs/` have no upstream changes (a path collision would be a conflict anyway).
9. **Build and test (tester)**: targeted `cargo check` / tests for the crates the fork touches (agent_ui, sidebar, workspace, cli, zed) on the exact head SHA, niced and capped per `briefs/COMMON.md`. The full suite is not required.
10. **Dependencies and licences**: `Cargo.lock`, `script/check-licenses` outcomes if upstream changed dependencies; the fork adds none.

## Merging (sre)

```bash
gh pr merge <n> --repo jclyons/zed --merge --match-head-commit <full-head-sha>
```

- **Merge commit only.** Never `--squash`, never `--rebase`, never `--admin`, never `--auto`. Squashing would drop the second parent and the next sync would re-merge the same upstream commits and conflict again.
- Only after `reviewer` and `tester` are clean on that exact head SHA (a new head, for example after a conflict fix-up push, needs fresh approval).
- **Re-cut rule (avoid churn).** If `main` moved after the sync branch was cut, do not re-cut by default: the merge commit stays valid while GitHub reports the PR mergeable. Re-cut (close the PR, delete the branch, re-run the script) only if **both** hold: the merge base moved (a fork PR landed on `main` after the cut) **and** that changed the conflicts, meaning the PR is no longer mergeable or `git merge-tree --write-tree fork/main <sync-head>` differs from the reviewed merge. Never rebase a sync branch.
- Only one sync PR is open at a time. The script skips while one is open; a newer upstream head is picked up by the first run after it merges.
- A merged sync PR is an eligible commit for `script/cpzed/deploy` like any other.

## What the script does not do

- It does not run builds or tests; the PR gates do.
- It does not resolve conflicts.
- It does not clean up merged sync branches on `fork` (delete them after merge).
