// Best-of-N with git worktree isolation (the flagship Mimo Build feature).
// Spawns N isolated git worktrees, runs the agent once in each to produce a
// candidate diff, asks an LLM judge to pick the best by correctness > quality >
// safety, then applies the winning patch back onto the main workspace.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};

use crate::agent::Agent;
use crate::api::{stream_chat, Message};
use crate::config::{mimo_home, Config};
use crate::event::Emitter;

/// How much of each candidate's summary/diff we feed the judge.
const TRUNC: usize = 6000;

/// Monotonic counter so concurrent/repeat runs never collide on worktree paths.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A single candidate's result.
#[allow(dead_code)] // `branch` is kept for traceability/debugging
struct Candidate {
    index: usize,
    path: PathBuf,
    branch: String,
    summary: String,
    diff: String,
}

/// Run a git command, returning trimmed stdout on success or an error with stderr.
fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| anyhow!("failed to spawn git {:?}: {e}", args))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("git {:?} failed: {}", args, err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Like `git`, but never fails — used for best-effort cleanup.
fn git_quiet(args: &[&str]) {
    let _ = Command::new("git").args(args).output();
}

/// Truncate a string to roughly `max` chars (char-boundary safe), appending a marker.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated]", &s[..end])
}

/// Entry point: run best-of-n for `prompt` with `n` candidates.
pub async fn run(cfg: &Config, prompt: &str, n: usize) -> Result<()> {
    let emitter = Emitter::Stdout;
    let n = n.max(1);

    // 1. Require a git repo.
    let inside = git(&["rev-parse", "--is-inside-work-tree"])
        .map_err(|_| anyhow!("best-of-n requires a git repository; run inside a git work tree"))?;
    if inside != "true" {
        return Err(anyhow!("best-of-n requires a git repository (not inside a work tree)"));
    }

    let main_root = git(&["rev-parse", "--show-toplevel"])?;
    let original_cwd = std::env::current_dir()?;
    let ts = COUNTER.fetch_add(1, Ordering::SeqCst);
    let base = mimo_home().join("worktrees");
    std::fs::create_dir_all(&base)?;

    emitter.info(&format!("best-of-n: spawning {n} candidate worktree(s)"));

    // 2. Create N isolated worktrees.
    let mut worktrees: Vec<(PathBuf, String)> = Vec::new();
    for i in 0..n {
        let path = base.join(format!("{ts}-{i}"));
        let branch = format!("mimo-candidate-{i}");
        let path_str = path.to_string_lossy().to_string();
        // Drop any stale branch/path from a previous aborted run.
        git_quiet(&["worktree", "remove", "--force", &path_str]);
        git_quiet(&["branch", "-D", &branch]);
        if let Err(e) = git(&["worktree", "add", &path_str, "-b", &branch, "HEAD"]) {
            // Clean up what we already made before bailing.
            cleanup(&worktrees);
            return Err(e);
        }
        worktrees.push((path, branch));
    }

    // 3. Run each candidate SEQUENTIALLY (set_current_dir is process-global).
    let mut candidates: Vec<Candidate> = Vec::new();
    for (i, (path, branch)) in worktrees.iter().enumerate() {
        emitter.info(&format!("── candidate {i} ──"));
        let result = run_candidate(cfg, prompt, i, path, branch).await;
        // Always restore cwd, regardless of success.
        let _ = std::env::set_current_dir(&original_cwd);
        match result {
            Ok(c) => candidates.push(c),
            Err(e) => emitter.error(&format!("candidate {i} failed: {e}")),
        }
    }

    if candidates.is_empty() {
        cleanup(&worktrees);
        return Err(anyhow!("all {n} candidates failed to produce a result"));
    }

    // 4. Judge.
    let (winner_idx, scorecard) = match judge(cfg, &candidates, &emitter).await {
        Ok(w) => w,
        Err(e) => {
            emitter.error(&format!("judge failed ({e}); falling back to candidate {}", candidates[0].index));
            (candidates[0].index, "judge unavailable; defaulted to first candidate".to_string())
        }
    };

    let winner = candidates
        .iter()
        .find(|c| c.index == winner_idx)
        .or_else(|| candidates.first())
        .ok_or_else(|| anyhow!("no winning candidate"))?;

    // 5. Apply the winner's diff to the main workspace.
    emitter.info(&format!("── scorecard ──\n{scorecard}"));
    emitter.info(&format!("winner: candidate {}", winner.index));

    if winner.diff.trim().is_empty() {
        emitter.info("winning candidate produced no changes; nothing to apply");
    } else {
        match apply_diff(&main_root, &winner.diff) {
            Ok(()) => emitter.info(&format!("applied candidate {} to {}", winner.index, main_root)),
            Err(e) => {
                // Fall back to copying changed files from the winning worktree.
                emitter.error(&format!("git apply failed ({e}); copying changed files instead"));
                copy_changed_files(&winner.path, Path::new(&main_root))?;
                emitter.info(&format!("copied candidate {} files into {}", winner.index, main_root));
            }
        }
    }

    // 6. Clean up.
    cleanup(&worktrees);
    Ok(())
}

/// Run the agent in one worktree and capture (summary, staged diff).
async fn run_candidate(
    cfg: &Config,
    prompt: &str,
    index: usize,
    path: &Path,
    branch: &str,
) -> Result<Candidate> {
    std::env::set_current_dir(path)
        .map_err(|e| anyhow!("could not enter worktree {}: {e}", path.display()))?;

    let mut agent = Agent::new_with(cfg.clone(), Emitter::Stdout);
    let summary = agent.run_turn(prompt).await?;

    // Stage everything so the diff captures new files too.
    git(&["add", "-A"])?;
    let diff = git(&["diff", "--cached"])?;

    Ok(Candidate {
        index,
        path: path.to_path_buf(),
        branch: branch.to_string(),
        summary,
        diff,
    })
}

/// Ask the LLM judge to compare candidates and pick a winner. Returns
/// (winner_index, scorecard_text).
async fn judge(cfg: &Config, candidates: &[Candidate], emitter: &Emitter) -> Result<(usize, String)> {
    let mut user = String::from(
        "Compare the following candidate solutions to the same task. \
         Pick the single best one, judging by (1) correctness, then (2) code quality, \
         then (3) safety. Reply with a line exactly `WINNER: <number>` (the candidate \
         number) followed by a short scorecard (one line per candidate).\n\n",
    );
    for c in candidates {
        user.push_str(&format!(
            "### Candidate {}\nSummary:\n{}\n\nDiff:\n{}\n\n",
            c.index,
            truncate(&c.summary, TRUNC),
            truncate(&c.diff, TRUNC),
        ));
    }

    let messages = vec![
        Message::system(
            "You are a rigorous code-review judge. Choose the best candidate by \
             correctness first, then code quality, then safety. Be decisive.",
        ),
        Message::user(user),
    ];

    let assistant = stream_chat(cfg, &messages, vec![], emitter).await?;
    let reply = assistant.content;

    let valid: Vec<usize> = candidates.iter().map(|c| c.index).collect();
    let winner = parse_winner(&reply, &valid).unwrap_or_else(|| valid[0]);
    Ok((winner, reply.trim().to_string()))
}

/// Parse `WINNER: <n>` from the judge reply, accepting only a valid candidate index.
fn parse_winner(reply: &str, valid: &[usize]) -> Option<usize> {
    let re = regex::Regex::new(r"(?i)WINNER\s*:?\s*#?\s*(\d+)").ok()?;
    let caps = re.captures(reply)?;
    let n: usize = caps.get(1)?.as_str().parse().ok()?;
    if valid.contains(&n) {
        Some(n)
    } else {
        None
    }
}

/// Apply a unified diff to the main workspace via `git -C <root> apply`.
fn apply_diff(root: &str, diff: &str) -> Result<()> {
    use std::io::Write;
    // Write the patch to a temp file under the worktree base to avoid stdin plumbing.
    let tmp = std::env::temp_dir().join(format!("mimo-bestofn-{}.patch", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(diff.as_bytes())?;
    }
    let tmp_str = tmp.to_string_lossy().to_string();
    let res = git(&["-C", root, "apply", "--whitespace=nowarn", &tmp_str]).map(|_| ());
    let _ = std::fs::remove_file(&tmp);
    res
}

/// Fallback: copy files the winning worktree changed (vs HEAD) into the main tree.
fn copy_changed_files(worktree: &Path, main_root: &Path) -> Result<()> {
    // List staged paths relative to the worktree root.
    let out = Command::new("git")
        .args(["-C", &worktree.to_string_lossy(), "diff", "--cached", "--name-only"])
        .output()?;
    if !out.status.success() {
        return Err(anyhow!("could not list changed files in worktree"));
    }
    let names = String::from_utf8_lossy(&out.stdout);
    for rel in names.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let src = worktree.join(rel);
        let dst = main_root.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if src.exists() {
            std::fs::copy(&src, &dst)
                .map_err(|e| anyhow!("copy {} -> {} failed: {e}", src.display(), dst.display()))?;
        }
    }
    Ok(())
}

/// Best-effort removal of all worktrees and their branches.
fn cleanup(worktrees: &[(PathBuf, String)]) {
    for (path, branch) in worktrees {
        let path_str = path.to_string_lossy().to_string();
        git_quiet(&["worktree", "remove", "--force", &path_str]);
        git_quiet(&["branch", "-D", branch]);
    }
    git_quiet(&["worktree", "prune"]);
}
