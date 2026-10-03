# sbxw

![Crates.io Version](https://img.shields.io/crates/v/sbxw)

> I'm lazy and hate orphaned branches, worktrees and sandboxes... so I just sorta merged the behavior of Docker Sandbox `sbx` and `git worktree`.

`sbxw` acts as a thin wrapper around Docker's [Host worktree workflow](https://docs.docker.com/ai/sandboxes/workflows/#host-worktree).

## Typical Workflow

Two ways to work, depending on how hands-on you want to be.

**Interactive** — you sit with the agent, then commit/push/clean up yourself:

```sh
sbxw launch feature-x        # create worktree + sandbox, drop into agent
# ... work with the agent inside the sandbox ...
# exit                       # back on the host
sbxw compare feature-x       # review the changes vs. the current branch
cd .sbxw/worktrees/feature-x && git commit -am "..." && git push
sbxw rm feature-x            # tear down sandbox + worktree
```

**Delegate a task** — prompt in, pull request out, nothing left on the host:

```sh
sbxw task "add a --dry-run flag to the deploy script"
# ... Claude's output streams here; when it's done, sbxw pushes the branch
# it committed to and opens a draft PR, then removes the sandbox.
```

Run `sbxw gc` periodically (or after merging PRs) to clean up anything either
workflow left behind.

## Requirements

- `git`
- [Docker Sandbox `sbx` CLI](https://docs.docker.com/ai/sandboxes/) (signed in via `sbx login`)
- Rust / Cargo

## Install

```sh
cargo install sbxw
```

## Layout & Naming Convention

`sbxw` does not have an explicit state file. Instead, it makes calls to `sbx` and `git` using an expected pattern to allow state to be derived by convention:

|                    | Value                           | Note                                                |
| ------------------ | ------------------------------- | --------------------------------------------------- |
| Worktree Directory | `<repo>/.sbxw/worktrees/<name>` | Adding `.sbxw` to your `.gitignore` is recommended. |
| Branch             | `<name>` (created off `HEAD`)   |                                                     |
| Docker Sandbox     | `sbxw-<name>`                   |                                                     |

The `sbxw-` sandbox prefix is how `sbxw` recognizes the sandboxes it manages (via `sbx ls --json`).

This convention means that you can still use all `git worktree` and `sbx` commands directly, **neat**.

## Commands

Run `sbxw --help` (or `sbxw <command> --help`) for exact usage and flags. At a glance:

| Command                | What it does                                                              |
| ---------------------- | ------------------------------------------------------------------------- |
| `sbxw launch <name>`   | Create (or attach to) the worktree + sandbox `<name>` and open its shell. |
| `sbxw ls`              | List every worktree/sandbox combination, orphans included.                |
| `sbxw compare <name>`  | Diff a base branch against worktree `<name>`'s working tree.              |
| `sbxw rm <name>`       | Tear down the sandbox and worktree for `<name>`.                          |
| `sbxw task "<prompt>"` | Run Claude on `<prompt>` in a throwaway clone-mode sandbox, push the branch it commits to, and open a PR. |
| `sbxw gc`              | Remove sandboxes/worktrees (from either workflow) whose branch or PR is already merged or closed. |

### Notes

- **`launch` is idempotent.** A missing worktree is created (new branch `<name>` off
  `HEAD`, or an existing `<name>` branch checked out); a missing sandbox is created;
  if both exist, `sbxw` just attaches. It drops you into a shell — to attach to the
  _agent_ instead, run `sbx run --name sbxw-<name>` directly.
- **`--kit` routing depends on lifecycle.** On a new sandbox, kits pass through to
  `sbx create … --kit`. On an existing one, each kit is applied with `sbx kit add`,
  which **recreates the sandbox container** (VM state, volumes, and agent history are
  preserved).
- **`compare` diffs the working tree**, not just branch tips, so uncommitted changes
  in the sandbox show up. The base branch defaults to the branch checked out wherever
  you invoke `sbxw`.
- **`rm` keeps the branch by default** so work isn't lost; pass `--delete-branch` to
  also run `git branch -D <name>`.
- **`task` creates its own `sbxw-<slug>` sandbox** — a short slug derived from the
  prompt plus a random suffix — in `sbx create --clone` mode, so the agent gets a
  private clone instead of your host working tree (no shared `node_modules`, no host
  worktree to clean up). It instructs the agent to commit to branch `<slug>` and never
  push or touch GitHub itself; `sbxw`, on the host, fetches that branch, pushes it to
  `origin`, and opens a draft PR (`--ready` for non-draft). A diff touching
  `.github/workflows/` pauses for confirmation before pushing, unless
  `--allow-workflow-changes` is passed. On failure, or if nothing was committed, the
  sandbox is left running with instructions to attach (`sbx run --name sbxw-<slug>`)
  or remove it (`sbx rm --force sbxw-<slug>`) — `--keep` does the same on success.
- **`task` warns if the host working tree is dirty** before creating the sandbox,
  since the clone only reflects committed state — uncommitted changes won't make it
  into the sandbox.
- **`gc` only removes finished work**: a `<name>` whose PR is merged/closed, or whose
  branch is already merged into the repo's default branch. It defaults to a dry run;
  pass `--force` to remove without the interactive confirmation. It never touches a
  worktree with uncommitted changes or a running sandbox unless you pass
  `--force-unsafe`, and it keeps branches by default, same as `rm` (`--delete-branch`
  to also delete them).

## Security Model

`task` is designed so the agent inside the sandbox can never write to GitHub itself —
only `sbxw`, running on your host with your host credentials, pushes branches and
opens PRs:

- The prompt sent to the agent explicitly tells it to commit locally and never push
  or open a PR; `sbxw` is the only thing that touches `origin` or runs `gh`.
- Before creating a sandbox, `task` checks `sbx secret ls` and warns if a `github`
  secret is visible to sandboxes — that would let the agent authenticate to GitHub
  directly, bypassing `sbxw`. SSH agent forwarding (`ssh.agentForwardingEnabled` in
  your `sbx` config) is another path to GitHub write access worth checking if you
  want this property to hold.
- This is advisory, not enforced: `sbxw` can't stop a sandbox from having GitHub
  credentials, it can only tell you if one does.
