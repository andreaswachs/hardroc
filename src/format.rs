//! Document formatting via the official Roc formatter (`roc fmt --stdin`).

/// Format `source` by piping it through `roc fmt --stdin`.  Returning `None`
/// means the formatter failed; callers must not guess at formatting.
pub async fn format_source(
    roc_path: &str,
    source: &str,
    timeout_secs: u64,
) -> Result<String, String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    use tokio::time::{timeout, Duration};

    let mut child = Command::new(roc_path)
        .arg("fmt")
        .arg("--stdin")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("failed to run roc fmt: {error}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(source.as_bytes())
            .await
            .map_err(|error| format!("failed to write roc fmt stdin: {error}"))?;
        drop(stdin);
    }

    let output = timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .map_err(|_| format!("roc fmt timed out after {timeout_secs}s"))?
        .map_err(|error| format!("failed to wait for roc fmt: {error}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    String::from_utf8(output.stdout).map_err(|_| "roc fmt produced invalid UTF-8".to_string())
}
