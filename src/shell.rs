//! Noninteractive shell execution for native hosts. This is not a sandbox.
//! Unix children run in a separate process group, killed on cancellation or exit.
//! On other platforms only the direct child is terminated; detached processes
//! require host-specific supervision.
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

const STREAM_LIMIT: usize = 32 * 1024;

struct Process(Option<Child>, Option<u32>);

impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.1 {
            // SAFETY: the child was spawned in its own process group with this ID.
            // A group may already have exited, in which case kill returns ESRCH.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        if let Some(mut child) = self.0.take() {
            let _ = child.start_kill();
            // Keep polling wait even when the RPC runtime is idle after abort.
            // kill_on_drop remains the fallback during runtime shutdown.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }
    }
}

async fn capture(mut reader: impl AsyncRead + Unpin) -> Result<String, String> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        let keep = count.min(STREAM_LIMIT - output.len());
        output.extend_from_slice(&buffer[..keep]);
        truncated |= keep < count;
    }
    let mut output = String::from_utf8_lossy(&output).into_owned();
    if truncated {
        output.push_str("\n[TRUNCATED: output exceeds 32 KiB]");
    }
    Ok(output)
}

/// Execute with null stdin, bounded captured output and a 120-second timeout.
pub async fn execute(command: &str, cwd: &Path) -> Result<String, String> {
    tokio::time::timeout(Duration::from_secs(120), execute_inner(command, cwd))
        .await
        .map_err(|_| "Shell timed out after 120 seconds".to_string())?
}

async fn execute_inner(command: &str, cwd: &Path) -> Result<String, String> {
    #[cfg(unix)]
    let mut cmd = {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd.as_std_mut().process_group(0);
        cmd
    };
    #[cfg(not(unix))]
    let mut cmd = {
        let mut cmd = Command::new("cmd");
        cmd.args(["/D", "/S", "/C", command]);
        cmd
    };
    let child = cmd
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Cannot start shell: {e}"))?;
    let pid = child.id();
    let mut process = Process(Some(child), pid);
    let child = process.0.as_mut().expect("owned child");
    let stdout = child.stdout.take().ok_or("Missing stdout pipe")?;
    let stderr = child.stderr.take().ok_or("Missing stderr pipe")?;
    let (status, stdout, stderr) = tokio::try_join!(
        async { child.wait().await.map_err(|e| e.to_string()) },
        capture(stdout),
        capture(stderr)
    )?;
    let result = format!("{status}\nstdout:\n{stdout}\nstderr:\n{stderr}");
    if status.success() {
        Ok(result)
    } else {
        Err(result)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_both_streams_failure_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let output = execute("printf '你好'; printf 'problem' >&2; exit 7", dir.path())
            .await
            .unwrap_err();
        assert!(output.contains("你好") && output.contains("problem") && output.contains('7'));
        let output = execute("head -c 70000 /dev/zero", dir.path())
            .await
            .unwrap();
        assert!(output.contains("TRUNCATED"));
        assert!(output.len() < 34000);
    }

    #[tokio::test]
    async fn cancellation_kills_background_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let task_dir = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            execute("sleep 30 & echo $! > child.pid; wait", &task_dir).await
        });
        let pid_file = dir.path().join("child.pid");
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&pid_file).await
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // Linux can briefly retain a killed descendant as a zombie until init reaps it.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // SAFETY: signal 0 only checks existence of the test-created process.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
                #[cfg(target_os = "linux")]
                if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    && stat
                        .split_once(") ")
                        .is_some_and(|(_, s)| s.starts_with('Z'))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background child survived cancellation");
    }
}
