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
    /// Show the full git diff between a branch and a worktree's branch
    Compare {
        /// The base branch to compare against
        branch: String,
        /// Name of the worktree to compare
        name: String,
    },
}

fn main() -> Result<()> {
    let args = Cli::parse();
    match args.command {
        Command::Ls => cmd_ls(),
        Command::Launch { name, agent, kits } => cmd_launch(&name, &agent, &kits),
        Command::Rm { name, delete_branch } => cmd_rm(&name, delete_branch),
        Command::Compare { branch, name } => cmd_compare(&branch, &name),
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
    let status = ProcCommand::new(cmd)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to spawn `{cmd}`"))?;
    Ok(status.code().unwrap_or(1))
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
        if wt.path.parent() == Some(dir.as_path()) {
            if let Some(name) = wt.path.file_name().and_then(|n| n.to_str()) {
                managed.push((name.to_string(), wt));
            }
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

    // Union of names from both sides so orphans are visible.
    let mut names: Vec<String> = worktrees.iter().map(|(n, _)| n.clone()).collect();
    for s in &sandboxes {
        if let Some(n) = name_from_sandbox(&s.name) {
            if !names.iter().any(|x| x == n) {
                names.push(n.to_string());
            }
        }
    }
    names.sort();

    if names.is_empty() {
        println!("No sbxw worktree/sandbox combinations found.");
        return Ok(());
    }

    println!("{:<20} {:<20} {:<12} {}", "NAME", "BRANCH", "SANDBOX", "WORKTREE");
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

fn cmd_compare(branch: &str, name: &str) -> Result<()> {
    let root = repo_root()?;
    if !worktree_exists(&root, name) {
        bail!("no worktree named `{name}` under .sbxw/worktrees");
    }
    // Resolve the branch checked out in the worktree; fall back to the name itself.
    let target = managed_worktrees(&root)?
        .into_iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, w)| w.branch)
        .unwrap_or_else(|| name.to_string());

    let code = run_interactive("git", &["diff", branch, &target])?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Small git helpers
// ---------------------------------------------------------------------------

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
