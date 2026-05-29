// OS-level sandbox profiles for shell command execution. Mirrors the real CLI's
// `--sandbox <profile>` flag / GROK_SANDBOX env (here: MIMO_SANDBOX).
//
// On macOS we confine commands with the built-in `sandbox-exec -p <policy>` using
// Apple's SBPL (sandbox profile language). On other platforms sandboxing is a no-op
// and the caller falls back to the plain `bash -c` form (see is_supported()).
//
// Profiles:
//   - "danger-full-access" / "none": no sandboxing at all.
//   - "read-only":      allow file reads + process exec; deny ALL file writes + network.
//   - "workspace-write": allow reads everywhere; allow writes only under the cwd and
//                        $TMPDIR; deny network.

use std::path::Path;

/// The set of recognized sandbox profile names.
pub fn profiles() -> &'static [&'static str] {
    &["danger-full-access", "none", "read-only", "workspace-write"]
}

/// True when OS-level sandboxing is available (macOS only, via `sandbox-exec`).
pub fn is_supported() -> bool {
    cfg!(target_os = "macos")
}

/// Build the full argv to execute `command` under the given sandbox `profile`.
///
/// For "none"/"danger-full-access"/unknown profiles (or any non-macOS host) this
/// returns the plain `["bash", "-c", command]` form. For a sandboxed profile on
/// macOS it returns `["sandbox-exec", "-p", <SBPL policy>, "bash", "-c", command]`.
pub fn wrap(command: &str, profile: &str, cwd: &Path) -> Vec<String> {
    let plain = || vec!["bash".to_string(), "-c".to_string(), command.to_string()];

    // No sandboxing requested, or unsupported host: run bare.
    if !is_supported() {
        return plain();
    }
    let policy = match profile {
        "read-only" => Some(read_only_policy()),
        "workspace-write" => Some(workspace_write_policy(cwd)),
        // "none", "danger-full-access", and anything unrecognized: no sandbox.
        _ => None,
    };
    match policy {
        Some(p) => vec![
            "sandbox-exec".to_string(),
            "-p".to_string(),
            p,
            "bash".to_string(),
            "-c".to_string(),
            command.to_string(),
        ],
        None => plain(),
    }
}

/// SBPL: allow everything except file writes and network. Process exec + file reads
/// remain permitted so commands can still run and inspect the filesystem.
fn read_only_policy() -> String {
    let mut p = String::new();
    p.push_str("(version 1)");
    p.push_str("(allow default)");
    p.push_str("(deny file-write*)");
    p.push_str("(deny network*)");
    p
}

/// SBPL: allow reads everywhere and writes only under the cwd and $TMPDIR; deny
/// network. We start from a permissive base, deny all writes + network, then
/// re-allow writes under the permitted subpaths.
fn workspace_write_policy(cwd: &Path) -> String {
    let mut p = String::new();
    p.push_str("(version 1)");
    p.push_str("(allow default)");
    p.push_str("(deny network*)");
    p.push_str("(deny file-write*)");

    let mut roots = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(tmp) = std::env::var("TMPDIR") {
        if !tmp.is_empty() {
            roots.push(tmp);
        }
    } else {
        roots.push("/tmp".to_string());
    }

    p.push_str("(allow file-write*");
    for root in &roots {
        p.push_str(&format!(" (subpath \"{}\")", escape_sbpl(root)));
    }
    p.push(')');
    p
}

/// Escape a literal path for embedding inside an SBPL double-quoted string.
/// Backslashes and double quotes must be escaped; other bytes pass through.
fn escape_sbpl(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/* ## INTEGRATION

Add the module to `src/main.rs` (or wherever the crate's other `mod` lines live):

    mod sandbox;

`tools::run_terminal_command` is stateless (it takes only `&Value`), so there is no
`Config` in scope at call time. The simplest robust way to thread the selected
profile through is a process-global env var set once at startup, then read inside
`run_terminal_command`. (This also matches the real CLI, which honors the
GROK_SANDBOX env var.)

1) Config: add a field and parse the flag/env.

   In `src/config.rs`, add to the `Config` struct:

       pub sandbox: Option<String>,

   and in `Config::load()`'s returned struct literal:

       sandbox: std::env::var("MIMO_SANDBOX").ok().filter(|s| !s.is_empty()),

   (Optionally accept a `--sandbox <PROFILE>` CLI flag in your arg parser and have
   it call `std::env::set_var("MIMO_SANDBOX", profile)` before `Config::load()`,
   so the flag and env converge on the same source of truth. Validate against
   `sandbox::profiles()` and warn on an unknown name.)

2) Startup: ensure the env var is set from cfg so the stateless tool can read it.

   After loading config (in `main`), once:

       if let Some(p) = &cfg.sandbox {
           std::env::set_var("MIMO_SANDBOX", p);
           if !sandbox::is_supported() && p != "none" && p != "danger-full-access" {
               eprintln!("warning: sandbox profile '{p}' requested but OS-level \
                          sandboxing is unsupported on this platform; running unconfined");
           }
       }

3) tools.rs: read the profile and wrap the command. Replace the body of
   `run_terminal_command`'s spawn with:

       fn run_terminal_command(args: &Value) -> Result<String> {
           let cmd = args["command"].as_str().ok_or_else(|| anyhow!("missing command"))?;
           let profile = std::env::var("MIMO_SANDBOX").unwrap_or_else(|_| "none".into());
           let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
           let argv = crate::sandbox::wrap(cmd, &profile, &cwd);
           let out = Command::new(&argv[0]).args(&argv[1..]).output()?;
           // ... unchanged: collect stdout/stderr, exit code, truncate ...
       }

4) bgtask.rs (optional, for parity): the background spawner can wrap the same way:

       let profile = std::env::var("MIMO_SANDBOX").unwrap_or_else(|_| "none".into());
       let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
       let argv = crate::sandbox::wrap(command, &profile, &cwd);
       let mut child = Command::new(&argv[0])
           .args(&argv[1..])
           .stdin(Stdio::null())
           .stdout(Stdio::piped())
           .stderr(Stdio::piped())
           .spawn()?;

   (`tokio::process::Command` has the same `new`/`args` API, so this is a drop-in.)
*/
