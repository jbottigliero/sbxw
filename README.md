# sbxw

![Crates.io Version](https://img.shields.io/crates/v/sbxw)

> I'm lazy and hate orphaned branches, worktrees and sandboxes... so I just sorta merged the behavior of Docker Sandbox `sbx` and `git worktree`.

`sbxw` acts as a thin wrapper around Docker's [Host worktree workflow](https://docs.docker.com/ai/sandboxes/workflows/#host-worktree).

## Typical Workflow

```sh
sbxw launch feature-x        # create worktree + sandbox, drop into agent
# ... work with the agent inside the sandbox ...
# exit                       # back on the host
sbxw compare feature-x       # review the changes vs. the current branch
cd .sbxw/worktrees/feature-x && git commit -am "..." && git push
sbxw rm feature-x            # tear down sandbox + worktree
```

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

| Command               | What it does                                                              |
| --------------------- | ------------------------------------------------------------------------- |
| `sbxw launch <name>`  | Create (or attach to) the worktree + sandbox `<name>` and open its shell. |
| `sbxw ls`             | List every worktree/sandbox combination, orphans included.                |
| `sbxw compare <name>` | Diff a base branch against worktree `<name>`'s working tree.              |
| `sbxw rm <name>`      | Tear down the sandbox and worktree for `<name>`.                          |

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
