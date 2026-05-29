// Background command execution + task registry. Backs the real tool set's
// run_terminal_command(background), get_command_or_subagent_output,
// wait_commands_or_subagents, and kill_command_or_subagent.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct BgTask {
    pub command: String,
    output: Arc<Mutex<String>>,
    done: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    child: Arc<Mutex<Child>>,
}

impl BgTask {
    async fn snapshot(&self) -> (String, bool, Option<i32>) {
        (
            self.output.lock().await.clone(),
            self.done.load(Ordering::SeqCst),
            *self.exit_code.lock().await,
        )
    }
}

#[derive(Default)]
pub struct TaskRegistry {
    tasks: HashMap<String, BgTask>,
    counter: u32,
}

impl TaskRegistry {
    /// Spawn a command in the background; returns its task_id.
    pub async fn spawn(&mut self, command: &str) -> std::io::Result<String> {
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let output = Arc::new(Mutex::new(String::new()));
        let done = Arc::new(AtomicBool::new(false));
        let exit_code = Arc::new(Mutex::new(None));

        // Stream stdout + stderr into the shared buffer.
        if let Some(stdout) = child.stdout.take() {
            let buf = output.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut b = buf.lock().await;
                    b.push_str(&line);
                    b.push('\n');
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let buf = output.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut b = buf.lock().await;
                    b.push_str(&line);
                    b.push('\n');
                }
            });
        }

        let child = Arc::new(Mutex::new(child));
        // Waiter: poll for exit, then record the code.
        {
            let child = child.clone();
            let done = done.clone();
            let exit_code = exit_code.clone();
            tokio::spawn(async move {
                loop {
                    {
                        let mut c = child.lock().await;
                        if let Ok(Some(status)) = c.try_wait() {
                            *exit_code.lock().await = status.code();
                            done.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            });
        }

        self.counter += 1;
        let id = format!("task_{}", self.counter);
        self.tasks.insert(
            id.clone(),
            BgTask { command: command.to_string(), output, done, exit_code, child },
        );
        Ok(id)
    }

    pub async fn output(&self, task_id: &str) -> String {
        let Some(t) = self.tasks.get(task_id) else {
            return format!("error: no such task '{task_id}'");
        };
        let (out, done, code) = t.snapshot().await;
        let status = if done {
            format!("[exited, code {}]", code.map(|c| c.to_string()).unwrap_or("?".into()))
        } else {
            "[running]".to_string()
        };
        format!("{status} `{}`\n{out}", t.command)
    }

    /// Wait until all given tasks finish or the timeout elapses.
    pub async fn wait(&self, ids: &[String], timeout_ms: u64) -> String {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            let pending: Vec<&String> = ids
                .iter()
                .filter(|id| {
                    self.tasks.get(*id).map(|t| !t.done.load(Ordering::SeqCst)).unwrap_or(false)
                })
                .collect();
            if pending.is_empty() || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let mut report = String::new();
        for id in ids {
            report.push_str(&self.output(id).await);
            report.push_str("\n---\n");
        }
        report
    }

    pub async fn kill(&mut self, task_id: &str) -> String {
        let Some(t) = self.tasks.get(task_id) else {
            return format!("error: no such task '{task_id}'");
        };
        let _ = t.child.lock().await.start_kill();
        t.done.store(true, Ordering::SeqCst);
        format!("Killed {task_id} (`{}`)", t.command)
    }
}
