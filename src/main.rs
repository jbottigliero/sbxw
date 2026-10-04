use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Output, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List worktree + Docker Sandbox combinations managed by sbxw
    Ls {
        /// Print machine-readable JSON instead of a table
        #[arg(long, default_value_t = false)]
        json: bool,
    },
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
        /// Ensure the worktree + sandbox exist, but don't attach a shell
        #[arg(long, default_value_t = false)]
        no_attach: bool,
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
}

fn main() -> Result<()> {
    let args = Cli::parse();
    match args.command {
        Command::Ls { json } => cmd_ls(json),
        Command::Launch {
            name,
            agent,
            kits,
            no_attach,
        } => cmd_launch(&name, &agent, &kits, no_attach),
        Command::Rm { name, delete_branch } => cmd_rm(&name, delete_branch),
        Command::Compare { name, branch } => cmd_compare(&name, branch.as_deref()),
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
    #[serde(default)]
    workspaces: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SbxList {
    #[serde(default)]
    sandboxes: Vec<SbxEntry>,
}

/// All `sbxw-`-prefixed sandboxes reported by `sbx ls --json`, across every
/// repo on the machine. Sandbox names are global, so this alone does not
/// mean a given entry belongs to *this* repo — see `sandbox_belongs_to_repo`.
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

/// True if `workspace` (one of the paths `sbx ls --json` reports for a
/// sandbox) is this repo's worktree directory for `name`, i.e.
/// `<root>/.sbxw/worktrees/<name>`.
///
/// Canonicalizes both sides first so symlinks and macOS's `/private` prefix
/// don't cause a false mismatch. If either side no longer exists on disk
/// (e.g. the worktree was deleted but the sandbox lingers as an orphan),
/// canonicalization fails and we fall back to comparing the raw paths —
/// `sbx create` was given this exact path at creation time, so a lexical
/// match is still meaningful.
fn workspace_matches(root: &Path, name: &str, workspace: &str) -> bool {
    let expected = worktree_path(root, name);
    let workspace_path = Path::new(workspace);
    match (expected.canonicalize(), workspace_path.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => expected == workspace_path,
    }
}

/// True if any of `entry`'s workspaces is this repo's worktree directory for
/// `name`.
fn sandbox_belongs_to_repo(root: &Path, name: &str, entry: &SbxEntry) -> bool {
    entry
        .workspaces
        .iter()
        .any(|w| workspace_matches(root, name, w))
}

/// All `sbxw-`-prefixed sandboxes that actually belong to this repo (i.e. one
/// of their workspaces is `<root>/.sbxw/worktrees/<name>`), keyed by the
/// `<name>` recovered from the sandbox name.
fn scoped_sandboxes(root: &Path) -> Result<Vec<SbxEntry>> {
    Ok(list_sandboxes()?
        .into_iter()
        .filter(|s| {
            name_from_sandbox(&s.name)
                .is_some_and(|name| sandbox_belongs_to_repo(root, name, s))
        })
        .collect())
}

/// Looks up the sandbox named `sbxw-<name>` anywhere on the machine, not
/// scoped to this repo — sandbox names are global, so this may return a
/// sandbox that belongs to a different repo entirely.
fn find_global_sandbox(name: &str) -> Result<Option<SbxEntry>> {
    let target = sandbox_name(name);
    Ok(list_sandboxes()?.into_iter().find(|s| s.name == target))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_launch(name: &str, agent: &str, kits: &[String], no_attach: bool) -> Result<()> {
    let root = repo_root()?;
    let wt_path = worktree_path(&root, name);
    let wt_path_str = wt_path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid utf-8"))?;
    let sbx = sandbox_name(name);

    std::fs::create_dir_all(worktrees_dir(&root)).context("failed to create .sbxw/worktrees")?;

    let had_worktree = worktree_exists(&root, name);
    let had_sandbox = match find_global_sandbox(name)? {
        None => false,
        Some(s) if sandbox_belongs_to_repo(&root, name, &s) => true,
        Some(s) => bail!(
            "sandbox `{sbx}` already exists but belongs to a different workspace ({}).\n\
             Sandbox names are global, so `sbxw` can't attach to it from here. Remove it with \
             `sbx rm -f {sbx}` if it's stale, or choose a different combo name.",
            s.workspaces.join(", ")
        ),
    };

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

    if no_attach {
        println!("`{name}` ready (worktree + sandbox `{sbx}`).");
        return Ok(());
    }

    // 3. Open an interactive shell in the sandbox.
    println!("Opening shell in `{sbx}` (exit to return)...");
    let code = run_interactive("sbx", &["run", "--name", &sbx])?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct ComboJson {
    name: String,
    branch: Option<String>,
    worktree_path: Option<String>,
    worktree_status: &'static str,
    sandbox_name: String,
    sandbox_status: String,
    sandbox_workspace: Option<String>,
}

fn cmd_ls(json: bool) -> Result<()> {
    let root = repo_root()?;
    let worktrees = managed_worktrees(&root)?;
    let sandboxes = scoped_sandboxes(&root)?;

    // Union of names from both sides so orphans are visible.
    let mut names: Vec<String> = worktrees.iter().map(|(n, _)| n.clone()).collect();
    for s in &sandboxes {
        if let Some(n) = name_from_sandbox(&s.name)
            && !names.iter().any(|x| x == n)
        {
            names.push(n.to_string());
        }
    }
    names.sort();

    if json {
        let combos: Vec<ComboJson> = names
            .into_iter()
            .map(|name| {
                let wt = worktrees.iter().find(|(n, _)| n == &name).map(|(_, w)| w);
                let sandbox = sandboxes
                    .iter()
                    .find(|s| name_from_sandbox(&s.name) == Some(name.as_str()));
                ComboJson {
                    branch: wt.and_then(|w| w.branch.clone()),
                    worktree_path: wt.map(|w| w.path.display().to_string()),
                    worktree_status: if wt.is_some() { "ok" } else { "missing" },
                    sandbox_name: sandbox_name(&name),
                    sandbox_status: sandbox
                        .map(|s| s.status.clone())
                        .unwrap_or_else(|| "missing".to_string()),
                    sandbox_workspace: sandbox.and_then(|s| s.workspaces.first().cloned()),
                    name,
                }
            })
            .collect();
        println!("{}", serde_json::to_string(&combos)?);
        return Ok(());
    }

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

    match find_global_sandbox(name)? {
        Some(s) if sandbox_belongs_to_repo(&root, name, &s) => {
            println!("Removing sandbox `{sbx}`...");
            run("sbx", &["rm", "-f", &sbx])?;
        }
        Some(s) => {
            println!(
                "Sandbox `{sbx}` belongs to a different workspace ({}); leaving it alone.",
                s.workspaces.join(", ")
            );
        }
        None => {
            println!("No sandbox `{sbx}`.");
        }
    }

    // Check git's own worktree registration rather than just the directory:
    // the directory may have been deleted without `git worktree remove`
    // (e.g. manual `rm -rf`, a prior partial `rm`, or sandbox teardown),
    // leaving a stale ("prunable") entry that still blocks `branch -D` with
    // "used by worktree" even though nothing is on disk. `git worktree
    // remove --force` clears that registration fine even when the
    // directory is already gone, and only touches this one worktree.
    let registered = managed_worktrees(&root)?.iter().any(|(n, _)| n == name);
    if registered {
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

    // Diff the base branch against the worktree's working tree (so uncommitted
    // changes show up too), running git from within the worktree.
    let code = run_interactive("git", &["-C", wt_path_str, "diff", &branch])?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh, empty temp directory to use as a fake repo root, scoped to
    /// the test name and process id so parallel test runs don't collide.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sbxw-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn matches_exact_worktree_path() {
        let root = temp_dir("exact");
        let wt = worktree_path(&root, "foo");
        fs::create_dir_all(&wt).unwrap();

        assert!(workspace_matches(&root, "foo", wt.to_str().unwrap()));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rejects_another_repos_worktree() {
        let root_a = temp_dir("repo-a");
        let root_b = temp_dir("repo-b");
        let wt_a = worktree_path(&root_a, "foo");
        fs::create_dir_all(&wt_a).unwrap();

        // Same combo name, but root_b's own worktree for "foo" doesn't exist
        // at all, let alone match root_a's.
        assert!(!workspace_matches(&root_b, "foo", wt_a.to_str().unwrap()));

        fs::remove_dir_all(&root_a).unwrap();
        fs::remove_dir_all(&root_b).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn resolves_symlinks_via_canonicalize() {
        use std::os::unix::fs::symlink;

        let base = temp_dir("symlink-base");
        let real_root = base.join("real-root");
        fs::create_dir_all(&real_root).unwrap();
        fs::create_dir_all(worktree_path(&real_root, "foo")).unwrap();

        // The workspace sbx reports resolves to real_root's worktree through
        // a symlink, mimicking macOS's /private prefix or a symlinked repo.
        let linked_root = base.join("linked-root");
        symlink(&real_root, &linked_root).unwrap();
        let linked_workspace = worktree_path(&linked_root, "foo");

        assert!(workspace_matches(
            &real_root,
            "foo",
            linked_workspace.to_str().unwrap()
        ));

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn falls_back_to_lexical_match_when_worktree_is_gone() {
        // Orphan case: the worktree directory was deleted, but the
        // sandbox's reported workspace still points at where it used to be.
        let root = temp_dir("orphan-match");
        let wt = worktree_path(&root, "foo");

        assert!(workspace_matches(&root, "foo", wt.to_str().unwrap()));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn fallback_still_rejects_an_unrelated_path() {
        let root = temp_dir("orphan-mismatch");

        assert!(!workspace_matches(&root, "foo", "/nonexistent/other/path"));

        fs::remove_dir_all(&root).unwrap();
    }
}
