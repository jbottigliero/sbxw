use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Output, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List worktree + Docker Sandbox combinations managed by sbxw
    Ls,
    /// Create (or attach to) a worktree and Docker Sandbox
    Launch {
        /// Name of the worktree/sandbox combination (also the branch name)
        name: String,
        /// Agent to provision the sandbox with (claude, codex, ...)
        #[arg(long, default_value = "claude")]
        agent: String,
        /// Kit reference
        #[arg(long = "kit", value_name = "REF")]
        kits: Vec<String>,
    },
    /// Remove a worktree and its Docker Sandbox
    Rm {
        /// Name of the worktree/sandbox combination to remove
        name: String,
        /// Also delete the git branch of the same name
        #[arg(long, default_value_t = false)]
        delete_branch: bool,
    },
    /// Show the git diff between a base branch and a worktree's working tree
    Compare {
        /// Name of the worktree to compare
        name: String,
        /// The base branch to compare against (defaults to the current branch)
        branch: Option<String>,
    },
    /// One-shot: run Claude on a prompt in a clone-mode sandbox and open a PR
    Task {
        /// The task for the agent to perform
        prompt: String,
        /// Base branch to branch off of and open the PR against (defaults to the current branch)
        #[arg(long)]
        base: Option<String>,
        /// Don't remove the sandbox after a successful PR
        #[arg(long, default_value_t = false)]
        keep: bool,
        /// Kit reference (passthrough to `sbx create --kit`, as in `launch`)
        #[arg(long = "kit", value_name = "REF")]
        kits: Vec<String>,
        /// Command to run in the sandbox before the agent starts (e.g. `npm ci`)
        #[arg(long)]
        setup: Option<String>,
        /// Open the PR ready for review instead of as a draft
        #[arg(long, default_value_t = false)]
        ready: bool,
        /// Pre-approve pushing changes that touch .github/workflows/
        #[arg(long, default_value_t = false)]
        allow_workflow_changes: bool,
    },
    /// Remove finished sbxw-managed sandboxes and worktrees
    Gc {
        /// Actually remove entries; without this, only list what would be removed
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Also delete the local branch for each removed entry, same as `rm`
        #[arg(long, default_value_t = false)]
        delete_branch: bool,
        /// Also remove entries with uncommitted changes or a running sandbox
        #[arg(long, default_value_t = false)]
        force_unsafe: bool,
    },
}

fn main() -> Result<()> {
    let args = Cli::parse();
    match args.command {
        Command::Ls => cmd_ls(),
        Command::Launch { name, agent, kits } => cmd_launch(&name, &agent, &kits),
        Command::Rm {
            name,
            delete_branch,
        } => cmd_rm(&name, delete_branch),
        Command::Compare { name, branch } => cmd_compare(&name, branch.as_deref()),
        Command::Task {
            prompt,
            base,
            keep,
            kits,
            setup,
            ready,
            allow_workflow_changes,
        } => cmd_task(
            &prompt,
            base.as_deref(),
            keep,
            &kits,
            setup.as_deref(),
            ready,
            allow_workflow_changes,
        ),
        Command::Gc {
            force,
            delete_branch,
            force_unsafe,
        } => cmd_gc(force, delete_branch, force_unsafe),
    }
}

// ---------------------------------------------------------------------------
// Layout / naming conventions
// ---------------------------------------------------------------------------

fn repo_root() -> Result<PathBuf> {
    let out = run("git", &["rev-parse", "--show-toplevel"])?;
    let path = String::from_utf8(out.stdout)
        .context("git output was not utf-8")?
        .trim()
        .to_string();
    if path.is_empty() {
        bail!("not inside a git repository");
    }
    Ok(PathBuf::from(path))
}

fn worktrees_dir(root: &Path) -> PathBuf {
    root.join(".sbxw").join("worktrees")
}

fn worktree_path(root: &Path, name: &str) -> PathBuf {
    worktrees_dir(root).join(name)
}

fn sandbox_name(name: &str) -> String {
    format!("sbxw-{name}")
}

/// Reverse of `sandbox_name`: the user-facing name for a `sbxw-`-prefixed sandbox.
fn name_from_sandbox(sandbox: &str) -> Option<&str> {
    sandbox.strip_prefix("sbxw-")
}

/// Derive a short, branch-safe slug from a free-form prompt, with a random
/// suffix so similar prompts don't collide on the same name.
fn derive_slug(prompt: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = true; // suppresses a leading dash
    for ch in prompt.chars() {
        if slug.len() >= 40 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("task");
    }
    format!("{slug}-{}", random_suffix())
}

/// A short, non-cryptographic suffix good enough to avoid name collisions.
fn random_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mix = nanos ^ (std::process::id() as u128);
    format!("{:04x}", (mix & 0xffff) as u16)
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

/// Run a command capturing its output; error if it exits non-zero.
fn run(cmd: &str, args: &[&str]) -> Result<Output> {
    let output = ProcCommand::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn `{cmd}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`{cmd} {}` failed:\n{stderr}", args.join(" "));
    }
    Ok(output)
}

/// Run a command inheriting the terminal (stdin/stdout/stderr) — for interactive
/// shells and streamed diffs. Returns the child's exit code.
fn run_interactive(cmd: &str, args: &[&str]) -> Result<i32> {
    run_interactive_env(cmd, args, &[])
}

/// Like `run_interactive`, but with extra environment variables set on the child.
fn run_interactive_env(cmd: &str, args: &[&str], env: &[(&str, &str)]) -> Result<i32> {
    let mut command = ProcCommand::new(cmd);
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (k, v) in env {
        command.env(k, v);
    }
    let status = command
        .status()
        .with_context(|| format!("failed to spawn `{cmd}`"))?;
    Ok(status.code().unwrap_or(1))
}

/// Like `run`, but with extra environment variables set on the child.
fn run_env(cmd: &str, args: &[&str], env: &[(&str, &str)]) -> Result<Output> {
    let mut command = ProcCommand::new(cmd);
    command.args(args);
    for (k, v) in env {
        command.env(k, v);
    }
    let output = command
        .output()
        .with_context(|| format!("failed to spawn `{cmd}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`{cmd} {}` failed:\n{stderr}", args.join(" "));
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// State discovery (convention-based, no state file)
// ---------------------------------------------------------------------------

struct Worktree {
    path: PathBuf,
    branch: Option<String>,
}

/// Parse `git worktree list --porcelain` into records.
fn list_worktrees() -> Result<Vec<Worktree>> {
    let out = run("git", &["worktree", "list", "--porcelain"])?;
    let text = String::from_utf8(out.stdout).context("git output was not utf-8")?;
    let mut worktrees = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line.is_empty() {
            if let Some(p) = path.take() {
                worktrees.push(Worktree {
                    path: p,
                    branch: branch.take(),
                });
            }
            branch = None;
        }
    }
    if let Some(p) = path.take() {
        worktrees.push(Worktree { path: p, branch });
    }
    Ok(worktrees)
}

/// The worktrees we manage, keyed by their `<name>` (directory basename under
/// `.sbxw/worktrees/`).
fn managed_worktrees(root: &Path) -> Result<Vec<(String, Worktree)>> {
    let dir = worktrees_dir(root);
    let mut managed = Vec::new();
    for wt in list_worktrees()? {
        if wt.path.parent() == Some(dir.as_path())
            && let Some(name) = wt.path.file_name().and_then(|n| n.to_str())
        {
            managed.push((name.to_string(), wt));
        }
    }
    Ok(managed)
}

#[derive(Debug, Deserialize)]
struct SbxEntry {
    name: String,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Deserialize)]
struct SbxList {
    #[serde(default)]
    sandboxes: Vec<SbxEntry>,
}

/// All `sbxw-`-prefixed sandboxes reported by `sbx ls --json`.
fn list_sandboxes() -> Result<Vec<SbxEntry>> {
    let out = run("sbx", &["ls", "--json"])?;
    let text = String::from_utf8(out.stdout).context("sbx output was not utf-8")?;
    // `sbx` may emit daemon-startup lines before the JSON; start at the first '{'.
    let json = match text.find('{') {
        Some(i) => &text[i..],
        None => return Ok(Vec::new()),
    };
    let list: SbxList = serde_json::from_str(json).context("failed to parse `sbx ls --json`")?;
    Ok(list
        .sandboxes
        .into_iter()
        .filter(|s| s.name.starts_with("sbxw-"))
        .collect())
}

fn worktree_exists(root: &Path, name: &str) -> bool {
    worktree_path(root, name).is_dir()
}

/// Union of worktree names and sandbox-derived names — every name sbxw might
/// know about for a given `<name>`, orphans on either side included.
fn union_names(worktrees: &[(String, Worktree)], sandboxes: &[SbxEntry]) -> Vec<String> {
    let mut names: Vec<String> = worktrees.iter().map(|(n, _)| n.clone()).collect();
    for s in sandboxes {
        if let Some(n) = name_from_sandbox(&s.name)
            && !names.iter().any(|x| x == n)
        {
            names.push(n.to_string());
        }
    }
    names.sort();
    names
}

fn sandbox_exists(name: &str) -> Result<bool> {
    let target = sandbox_name(name);
    Ok(list_sandboxes()?.iter().any(|s| s.name == target))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_launch(name: &str, agent: &str, kits: &[String]) -> Result<()> {
    let root = repo_root()?;
    let wt_path = worktree_path(&root, name);
    let wt_path_str = wt_path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid utf-8"))?;
    let sbx = sandbox_name(name);

    std::fs::create_dir_all(worktrees_dir(&root)).context("failed to create .sbxw/worktrees")?;

    let had_worktree = worktree_exists(&root, name);
    let had_sandbox = sandbox_exists(name)?;

    // 1. Ensure the worktree exists.
    if !had_worktree {
        if branch_exists(name)? {
            println!("Adding worktree for existing branch `{name}`...");
            run("git", &["worktree", "add", wt_path_str, name])?;
        } else {
            println!("Creating worktree and branch `{name}`...");
            run("git", &["worktree", "add", "-b", name, wt_path_str])?;
        }
    }

    // 2. Ensure the sandbox exists. Kits can only be applied at creation, so a
    //    fresh sandbox gets them via `sbx create --kit`; an existing one via
    //    `sbx kit add` (below).
    if !had_sandbox {
        println!("Creating sandbox `{sbx}` ({agent})...");
        let mut args = vec!["create", agent, wt_path_str, "--name", &sbx];
        for kit in kits {
            args.push("--kit");
            args.push(kit);
        }
        run("sbx", &args)?;
    } else {
        // Existing sandbox: append each requested kit. `sbx kit add` recreates
        // the sandbox container (preserving VM state, volumes, agent history).
        for kit in kits {
            println!("Adding kit `{kit}` to `{sbx}` (recreates the sandbox)...");
            run("sbx", &["kit", "add", &sbx, kit])?;
        }
    }

    if had_worktree && had_sandbox {
        println!("Attaching to existing `{name}`...");
    }

    // 3. Open an interactive shell in the sandbox.
    println!("Opening shell in `{sbx}` (exit to return)...");
    let code = run_interactive("sbx", &["run", "--name", &sbx])?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn cmd_ls() -> Result<()> {
    let root = repo_root()?;
    let worktrees = managed_worktrees(&root)?;
    let sandboxes = list_sandboxes()?;

    let names = union_names(&worktrees, &sandboxes);

    if names.is_empty() {
        println!("No sbxw worktree/sandbox combinations found.");
        return Ok(());
    }

    println!("{:<20} {:<20} {:<12} WORKTREE", "NAME", "BRANCH", "SANDBOX");
    for name in names {
        let wt = worktrees.iter().find(|(n, _)| n == &name).map(|(_, w)| w);
        let branch = wt
            .and_then(|w| w.branch.clone())
            .unwrap_or_else(|| "-".to_string());
        let worktree = match wt {
            Some(w) => w.path.display().to_string(),
            None => "(missing)".to_string(),
        };
        let status = sandboxes
            .iter()
            .find(|s| name_from_sandbox(&s.name) == Some(name.as_str()))
            .map(|s| s.status.clone())
            .unwrap_or_else(|| "(missing)".to_string());
        println!("{name:<20} {branch:<20} {status:<12} {worktree}");
    }
    Ok(())
}

fn cmd_rm(name: &str, delete_branch: bool) -> Result<()> {
    let root = repo_root()?;
    let wt_path = worktree_path(&root, name);
    let sbx = sandbox_name(name);

    if sandbox_exists(name)? {
        println!("Removing sandbox `{sbx}`...");
        run("sbx", &["rm", "-f", &sbx])?;
    } else {
        println!("No sandbox `{sbx}`.");
    }

    if worktree_exists(&root, name) {
        let wt_path_str = wt_path
            .to_str()
            .ok_or_else(|| anyhow!("worktree path is not valid utf-8"))?;
        println!("Removing worktree `{wt_path_str}`...");
        run("git", &["worktree", "remove", "--force", wt_path_str])?;
    } else {
        println!("No worktree for `{name}`.");
    }

    if delete_branch {
        if branch_exists(name)? {
            println!("Deleting branch `{name}`...");
            run("git", &["branch", "-D", name])?;
        } else {
            println!("No branch `{name}`.");
        }
    }
    Ok(())
}

fn cmd_compare(name: &str, branch: Option<&str>) -> Result<()> {
    let root = repo_root()?;
    if !worktree_exists(&root, name) {
        bail!("no worktree named `{name}` under .sbxw/worktrees");
    }
    let wt_path = worktree_path(&root, name);
    let wt_path_str = wt_path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid utf-8"))?;

    // Base branch to compare against; default to the branch checked out here.
    let branch = match branch {
        Some(b) => b.to_string(),
        None => current_branch()?,
    };

    // `git diff <branch>` only reports paths git already knows about, so untracked
    // files in the worktree would be invisible. Work against a throwaway copy of the
    // worktree's index and mark untracked files intent-to-add there — they then show
    // up in the diff as new files, without touching the real index.
    let out = run(
        "git",
        &[
            "-C",
            wt_path_str,
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index",
        ],
    )?;
    let real_index = String::from_utf8(out.stdout)
        .context("git output was not utf-8")?
        .trim()
        .to_string();

    // Keep the temp index inside .git (next to the real one) so it never lands under
    // the worktree tree and gets picked up by `git add`.
    let tmp_index = format!("{real_index}.sbxw-compare");
    if std::path::Path::new(&real_index).exists() {
        std::fs::copy(&real_index, &tmp_index).context("failed to copy git index")?;
    } else {
        // No index yet (fresh worktree); make sure a stale temp index isn't reused.
        let _ = std::fs::remove_file(&tmp_index);
    }
    let index_env = [("GIT_INDEX_FILE", tmp_index.as_str())];

    // Intent-to-add untracked (non-ignored) files into the throwaway index.
    run_env(
        "git",
        &["-C", wt_path_str, "add", "-N", "--", "."],
        &index_env,
    )?;

    // Diff the base branch against the worktree's working tree (so uncommitted
    // changes show up too), running git from within the worktree.
    let code = run_interactive_env("git", &["-C", wt_path_str, "diff", &branch], &index_env)?;

    let _ = std::fs::remove_file(&tmp_index);

    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn cmd_task(
    prompt: &str,
    base: Option<&str>,
    keep: bool,
    kits: &[String],
    setup: Option<&str>,
    ready: bool,
    allow_workflow_changes: bool,
) -> Result<()> {
    // `task` doesn't need the repo root for anything but this existence check.
    repo_root()?;

    let dirty = run("git", &["status", "--porcelain"])?;
    if !dirty.stdout.is_empty() {
        eprintln!(
            "Warning: the working tree has uncommitted changes. The sandbox clone \
             only reflects committed state, so they won't be included.\n"
        );
    }

    warn_if_github_secret_visible();

    let base = match base {
        Some(b) => b.to_string(),
        None => current_branch()?,
    };
    let slug = derive_slug(prompt);
    let sbx = sandbox_name(&slug);
    let remote = format!("sandbox-{sbx}");

    // Once the sandbox exists, failures leave it behind (with instructions)
    // rather than removing evidence of what went wrong.
    let fail = |step: &str, detail: String| -> anyhow::Error {
        anyhow!(
            "{step}: {detail}\n\n\
             The sandbox `{sbx}` was left running for inspection:\n  \
             attach: sbx run --name {sbx}\n  \
             remove: sbx rm --force {sbx}"
        )
    };

    println!("Creating sandbox `{sbx}` (clone mode)...");
    let mut create_args = vec!["create", "--clone", "--name", sbx.as_str(), "claude", "."];
    for kit in kits {
        create_args.push("--kit");
        create_args.push(kit);
    }
    run("sbx", &create_args)?;

    println!("Checking out `{slug}` from `{base}` in the sandbox...");
    run(
        "sbx",
        &[
            "exec",
            &sbx,
            "git",
            "checkout",
            "-b",
            &slug,
            &format!("origin/{base}"),
        ],
    )
    .map_err(|e| fail("checkout failed", e.to_string()))?;

    if let Some(cmd) = setup {
        println!("Running setup command in the sandbox...");
        let code = run_interactive("sbx", &["exec", &sbx, "bash", "-c", cmd])?;
        if code != 0 {
            return Err(fail("setup command failed", format!("exit code {code}")));
        }
    }

    let agent_prompt = format!(
        "You are already checked out on git branch `{slug}` (branched from `{base}`). \
         Complete the following task, then stage and commit all your changes to this \
         branch. Do not push, open a pull request, or otherwise write to GitHub — a \
         separate tool handles that outside this sandbox.\n\nTask: {prompt}"
    );

    println!("Running Claude in `{sbx}`...");
    let code = run_interactive(
        "sbx",
        &[
            "exec",
            &sbx,
            "claude",
            "-p",
            &agent_prompt,
            "--dangerously-skip-permissions",
        ],
    )?;
    if code != 0 {
        return Err(fail("agent run failed", format!("exit code {code}")));
    }

    let commit_count_out = run(
        "sbx",
        &[
            "exec",
            &sbx,
            "git",
            "rev-list",
            "--count",
            &format!("origin/{base}..{slug}"),
        ],
    )
    .map_err(|e| fail("could not check for commits", e.to_string()))?;
    let commit_count: u64 = String::from_utf8_lossy(&commit_count_out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    if commit_count == 0 {
        return Err(fail(
            "no commits",
            format!("the agent did not commit anything to `{slug}`"),
        ));
    }
    println!("{commit_count} commit(s) on `{slug}`.");

    println!("Fetching `{slug}` from `{remote}`...");
    run("git", &["fetch", &remote, "--no-tags", &slug])
        .map_err(|e| fail("fetch failed", e.to_string()))?;

    let changed = run("git", &["diff", "--name-only", &base, "FETCH_HEAD"])
        .map_err(|e| fail("diff failed", e.to_string()))?;
    let touches_workflows = String::from_utf8_lossy(&changed.stdout)
        .lines()
        .any(|l| l.starts_with(".github/workflows/"));
    if touches_workflows && !allow_workflow_changes {
        eprintln!(
            "This change touches .github/workflows/ — pushing it can run CI with repo \
             secrets. Pass --allow-workflow-changes to skip this prompt next time."
        );
        if !confirm("Push anyway?")? {
            return Err(fail(
                "push declined",
                "workflow changes were not approved".to_string(),
            ));
        }
    }

    println!("Pushing `{slug}` to origin...");
    let refspec = format!("FETCH_HEAD:refs/heads/{slug}");
    run("git", &["push", "origin", &refspec]).map_err(|e| fail("push failed", e.to_string()))?;

    println!("Opening pull request...");
    let title = pr_title(prompt);
    let body = format!("## Task\n\n{prompt}\n\n---\nOpened by `sbxw task`.");
    let mut pr_args = vec![
        "pr", "create", "--title", &title, "--body", &body, "--base", &base, "--head", &slug,
    ];
    if !ready {
        pr_args.push("--draft");
    }
    let pr_out = run("gh", &pr_args).map_err(|e| fail("gh pr create failed", e.to_string()))?;
    println!("{}", String::from_utf8_lossy(&pr_out.stdout).trim());

    if keep {
        println!("Leaving sandbox `{sbx}` (--keep).");
    } else {
        println!("Removing sandbox `{sbx}`...");
        run("sbx", &["rm", "--force", &sbx])?;
    }
    Ok(())
}

/// First line of the prompt, trimmed to a reasonable PR title length.
fn pr_title(prompt: &str) -> String {
    let first_line = prompt.lines().next().unwrap_or(prompt).trim();
    if first_line.chars().count() > 72 {
        let truncated: String = first_line.chars().take(69).collect();
        format!("{truncated}...")
    } else {
        first_line.to_string()
    }
}

fn cmd_gc(force: bool, delete_branch: bool, force_unsafe: bool) -> Result<()> {
    let root = repo_root()?;
    let worktrees = managed_worktrees(&root)?;
    let sandboxes = list_sandboxes()?;
    let default = default_branch()?;

    let names = union_names(&worktrees, &sandboxes);

    struct Candidate {
        name: String,
        reason: String,
        wt_dirty: bool,
        sbx_running: bool,
    }

    let mut candidates = Vec::new();
    for name in &names {
        let reason = match pr_finished(name)? {
            Some(r) => Some(r),
            None if branch_exists(name)? && is_merged_into(name, &default) => {
                Some("branch merged into default".to_string())
            }
            None => None,
        };
        let Some(reason) = reason else { continue };

        let wt = worktrees.iter().find(|(n, _)| n == name).map(|(_, w)| w);
        let wt_dirty = match wt {
            Some(w) => {
                let wt_path_str = w
                    .path
                    .to_str()
                    .ok_or_else(|| anyhow!("worktree path is not valid utf-8"))?;
                let out = run("git", &["-C", wt_path_str, "status", "--porcelain"])?;
                !out.stdout.is_empty()
            }
            None => false,
        };
        let sbx_running = sandboxes
            .iter()
            .find(|s| name_from_sandbox(&s.name) == Some(name.as_str()))
            .map(|s| s.status == "running")
            .unwrap_or(false);

        candidates.push(Candidate {
            name: name.clone(),
            reason,
            wt_dirty,
            sbx_running,
        });
    }

    if candidates.is_empty() {
        println!("Nothing finished to collect.");
        return Ok(());
    }

    println!("{:<20} {:<28} NOTE", "NAME", "REASON");
    for c in &candidates {
        let note = if c.wt_dirty && !force_unsafe {
            "skip: uncommitted changes"
        } else if c.sbx_running && !force_unsafe {
            "skip: sandbox running"
        } else {
            "will remove"
        };
        println!("{:<20} {:<28} {}", c.name, c.reason, note);
    }

    let proceed = if force {
        true
    } else {
        confirm("Remove the above entries?")?
    };
    if !proceed {
        println!("Dry run only; nothing removed. Pass --force to remove without confirming.");
        return Ok(());
    }

    for c in &candidates {
        if (c.wt_dirty || c.sbx_running) && !force_unsafe {
            continue;
        }
        let sbx = sandbox_name(&c.name);
        if sandboxes.iter().any(|s| s.name == sbx) {
            println!("Removing sandbox `{sbx}`...");
            run("sbx", &["rm", "--force", &sbx])?;
        }
        if worktree_exists(&root, &c.name) {
            let wt_path = worktree_path(&root, &c.name);
            let wt_path_str = wt_path
                .to_str()
                .ok_or_else(|| anyhow!("worktree path is not valid utf-8"))?;
            println!("Removing worktree `{wt_path_str}`...");
            run("git", &["worktree", "remove", "--force", wt_path_str])?;
        }
        if delete_branch && branch_exists(&c.name)? {
            println!("Deleting branch `{}`...", c.name);
            run("git", &["branch", "-D", &c.name])?;
        }
    }
    Ok(())
}

/// Whether `name`'s PR (if any) is finished, and why, per `gh pr list`.
fn pr_finished(name: &str) -> Result<Option<String>> {
    let out = run(
        "gh",
        &[
            "pr",
            "list",
            "--head",
            name,
            "--state",
            "all",
            "--json",
            "state",
            "--jq",
            ".[0].state",
        ],
    );
    let out = match out {
        Ok(out) => out,
        Err(_) => return Ok(None), // no `gh`, no auth, or no PR — not fatal for gc
    };
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match state.as_str() {
        "MERGED" => Ok(Some("PR merged".to_string())),
        "CLOSED" => Ok(Some("PR closed".to_string())),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Small git helpers
// ---------------------------------------------------------------------------

/// The branch currently checked out in the invocation's working directory.
fn current_branch() -> Result<String> {
    let out = run("git", &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = String::from_utf8(out.stdout)
        .context("git output was not utf-8")?
        .trim()
        .to_string();
    if branch.is_empty() || branch == "HEAD" {
        bail!("could not determine the current branch (detached HEAD?)");
    }
    Ok(branch)
}

fn branch_exists(name: &str) -> Result<bool> {
    let status = ProcCommand::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ])
        .status()
        .context("failed to spawn `git show-ref`")?;
    Ok(status.success())
}

/// Whether `base` has already been merged into `target` (e.g. a feature branch
/// fully merged without going through a PR).
fn is_merged_into(base: &str, target: &str) -> bool {
    ProcCommand::new("git")
        .args(["merge-base", "--is-ancestor", base, target])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The repository's default branch, per GitHub, falling back to the local
/// `origin/HEAD` symbolic ref if `gh` isn't available or not authenticated.
fn default_branch() -> Result<String> {
    if let Ok(out) = run(
        "gh",
        &[
            "repo",
            "view",
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ],
    ) {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !name.is_empty() && name != "null" {
            return Ok(name);
        }
    }
    let out = run(
        "git",
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .context("could not determine the default branch via `gh` or `origin/HEAD`")?;
    let name = String::from_utf8(out.stdout)
        .context("git output was not utf-8")?
        .trim()
        .strip_prefix("origin/")
        .unwrap_or_default()
        .to_string();
    if name.is_empty() {
        bail!("could not determine the default branch");
    }
    Ok(name)
}

/// Ask a yes/no question on the terminal; anything but y/yes is "no".
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    print!("{prompt} [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("failed to read confirmation from stdin")?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Best-effort warning: if a `github` secret is visible to sandboxes, the agent
/// could push or open PRs directly, bypassing `sbxw`'s host-side push. Never
/// fails `task` on its own — this is advisory only.
fn warn_if_github_secret_visible() {
    let out = match run("sbx", &["secret", "ls"]) {
        Ok(out) => out,
        Err(_) => return,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let visible = text
        .lines()
        .any(|line| line.split_whitespace().nth(2) == Some("github"));
    if visible {
        eprintln!(
            "Warning: a `github` secret is visible to sandboxes (see `sbx secret ls`). \
             The agent inside the sandbox could use it to push or open PRs on its own, \
             bypassing sbxw. Consider scoping it away from this sandbox. SSH agent \
             forwarding (`ssh.agentForwardingEnabled`) is another path to GitHub write \
             access — worth checking too.\n"
        );
    }
}
