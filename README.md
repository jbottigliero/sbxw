# sbxw

A thin wrapper around
Docker's [Host worktree workflow](https://docs.docker.com/ai/sandboxes/workflows/#host-worktree).

## Typical Workflow

```sh
sbxw launch feature-x        # create worktree + sandbox, drop into agent
# ... work with the agent inside the sandbox ...
# exit                       # back on the host
sbxw compare main feature-x  # review the changes
cd .sbxw/worktrees/feature-x && git commit -am "..." && git push
sbxw rm feature-x            # tear down sandbox + worktree
```

## Requirements

- `git`
- The [`sbx` CLI](https://docs.docker.com/ai/sandboxes/) (signed in via `sbx login`)
- Rust / Cargo (to build)

## Install

```sh
cargo build --release
# binary at target/release/sbxw
```

## Layout & Naming Convention

`sbxw` keeps no state file — everything is derived by convention:

|                    | Value                           |
| ------------------ | ------------------------------- |
| Worktree Directory | `<repo>/.sbxw/worktrees/<name>` |
| Branch             | `<name>` (created off `HEAD`)   |
| Docker Sandbox     | `sbxw-<name>`                   |

The `sbxw-` sandbox prefix is how `sbxw` recognizes the sandboxes it manages (via `sbx ls --json`).

## Commands

### `sbxw launch <name> [--agent claude]`

Create (or attach to) a worktree + sandbox pair named `<name>`

- If the worktree is missing, it's created: a new branch `<name>` off `HEAD`, or the
  existing branch `<name>` is checked out into a fresh worktree if it already exists.
- If the sandbox is missing, it's created with `sbx create <agent> <worktree> --name sbxw-<name>`.
- If both already exist, `sbxw` just attaches.
- On exit from the shell, `sbxw` returns.

`--agent` selects the agent the sandbox is provisioned with (`claude`, `codex`,
`shell`, …); default `claude`. To attach to the _agent_ instead of a shell, run
`sbx run --name sbxw-<name>` directly.

### `sbxw ls`

List every worktree/sandbox combination, including orphans (a worktree with no
sandbox, or vice-versa, shows `(missing)`).

```
NAME                 BRANCH               SANDBOX      WORKTREE
feature-x            feature-x            running      /repo/.sbxw/worktrees/feature-x
```

### `sbxw compare <branch> <name>`

Print the full `git diff` between `<branch>` and the branch checked out in worktree
`<name>`.

```sh
sbxw compare main feature-x
```

### `sbxw rm <name> [--delete-branch]`

Remove the sandbox (`sbx rm -f`) and the worktree (`git worktree remove --force`).
The branch is **kept by default** so work isn't lost; pass `--delete-branch` to also
run `git branch -D <name>`.
