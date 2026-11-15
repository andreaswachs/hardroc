#![allow(dead_code)]
//! End-to-end LSP harness for `hardroc`.
//!
//! The harness spawns the real server binary (`CARGO_BIN_EXE_hardroc`) over
//! stdio, speaks the LSP base protocol with `Content-Length` framing, and
//! exposes a small async API that later feature tests can reuse. Responses are
//! routed by JSON-RPC id and notifications are kept in a shared history, so a
//! test never polls and a notification that arrives before the test starts
//! waiting is still observed.
//!
//! All waiting helpers take an explicit timeout and panic with the messages
//! received so far (plus the server's stderr) instead of hanging CI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use lsp_types::{Position, Range, Url};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Notify};

/// Timeout used by [`Harness::request`] for ordinary LSP requests.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Timeout used by [`Harness::shutdown`] while waiting for the process to exit.
const DEFAULT_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// A received server notification or server-to-client request.
#[derive(Debug, Clone)]
struct Notification {
    seq: u64,
    method: String,
    params: Value,
    /// True when the server sent this as a request that we answered.
    is_request: bool,
}

/// Why a response future produced no result.
type PendingResult = Result<Value, Value>;

/// A message for the stdout writer task.
enum Outgoing {
    /// A JSON-RPC message to frame and write.
    Message(Value),
    /// Close the server's stdin, which makes tower-lsp's serve loop finish.
    CloseStdin,
}

/// An initialized connection to a `hardroc` subprocess.
///
/// Create one with [`Harness::spawn`], [`Harness::initialize`], and
/// [`Harness::initialized`], or with the [`Harness::start`] shortcut. The
/// process is killed when the harness is dropped, including after an assertion
/// failure, so tests do not leak servers.
pub struct Harness {
    tx: mpsc::UnboundedSender<Outgoing>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<PendingResult>>>>,
    notifications: Arc<Mutex<Vec<Notification>>>,
    notify: Arc<Notify>,
    log: Arc<Mutex<Vec<String>>>,
    next_id: AtomicI64,
    child: Option<Child>,
    exit_status: Option<ExitStatus>,
}

impl Harness {
    /// Spawn `hardroc --stdio` and start the reader/writer tasks.
    ///
    /// This does not initialize the protocol; call [`Harness::initialize`] and
    /// [`Harness::initialized`] next.
    pub async fn spawn() -> Self {
        let binary = env!("CARGO_BIN_EXE_hardroc");
        let mut child = Command::new(binary)
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|error| panic!("failed to spawn `{binary} --stdio`: {error}"));

        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => panic!("hardroc was spawned without a stdin pipe"),
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => panic!("hardroc was spawned without a stdout pipe"),
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => panic!("hardroc was spawned without a stderr pipe"),
        };

        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<PendingResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let log = Arc::new(Mutex::new(Vec::new()));

        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(writer_loop(stdin, rx, Arc::clone(&log)));
        tokio::spawn(reader_loop(
            stdout,
            Arc::clone(&pending),
            Arc::clone(&notifications),
            Arc::clone(&notify),
            tx.clone(),
            Arc::clone(&log),
        ));
        tokio::spawn(stderr_loop(stderr, Arc::clone(&log)));

        Self {
            tx,
            pending,
            notifications,
            notify,
            log,
            next_id: AtomicI64::new(1),
            child: Some(child),
            exit_status: None,
        }
    }

    /// Send `initialize` for `root` using the hermetic test options from
    /// [`hermetic_initialization_options`], and return the `InitializeResult`.
    ///
    /// The options disable compiler checks and package-cache inspection so
    /// tests stay fast and network-free. Use
    /// [`Harness::initialize_with_options`] to exercise other configurations.
    pub async fn initialize(&self, root: impl AsRef<Path>) -> Value {
        self.initialize_with_options(root, hermetic_initialization_options())
            .await
    }

    /// Send `initialize` with explicit `initializationOptions`.
    pub async fn initialize_with_options(&self, root: impl AsRef<Path>, options: Value) -> Value {
        self.initialize_with_workspace_folders(&[root.as_ref().to_path_buf()], options)
            .await
    }

    /// Send `initialize` for several workspace folders.
    pub async fn initialize_with_workspace_folders(
        &self,
        roots: &[PathBuf],
        options: Value,
    ) -> Value {
        let mut folders = Vec::new();
        for root in roots {
            let uri = Url::from_directory_path(root).unwrap_or_else(|_| {
                panic!(
                    "fixture root is not a valid directory path: {}",
                    root.display()
                )
            });
            folders.push(json!({ "uri": uri, "name": "fixture" }));
        }
        let root_uri = folders
            .first()
            .and_then(|folder| folder.get("uri").cloned())
            .unwrap_or(Value::Null);
        let params = json!({
            "processId": Value::Null,
            "clientInfo": {
                "name": "hardroc-test-harness",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "rootUri": root_uri,
            "capabilities": {},
            "workspaceFolders": folders,
            "initializationOptions": options,
        });
        self.request("initialize", params).await
    }

    /// Send the `initialized` notification.
    ///
    /// The server may immediately issue server-to-client requests (this one
    /// registers a file watcher). We wait briefly for the first such request so
    /// its response is queued before a test can initiate shutdown; losing that
    /// response would leave the server's `initialized` handler pending.
    pub async fn initialized(&self) {
        let before = self.notification_seq();
        self.notify("initialized", json!({})).await;
        let _ = self
            .await_server_request_since(before, Duration::from_secs(5))
            .await;
    }

    /// Spawn, initialize with hermetic options, and send `initialized`.
    pub async fn start(root: impl AsRef<Path>) -> Self {
        let harness = Self::spawn().await;
        harness.initialize(root.as_ref()).await;
        harness.initialized().await;
        harness
    }

    /// Notify the server that `text` is open at `version`.
    pub async fn open(&self, uri: &Url, text: &str, version: i32) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "roc",
                    "version": version,
                    "text": text,
                }
            }),
        )
        .await;
    }

    /// Replace the whole document with `text` (`TextDocumentSyncKind::Full`).
    pub async fn change_full(&self, uri: &Url, text: &str, version: i32) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {
                    "uri": uri,
                    "version": version,
                },
                "contentChanges": [{ "text": text }],
            }),
        )
        .await;
    }

    /// Apply one ranged change, using LSP UTF-16 positions.
    pub async fn change_incremental(&self, uri: &Url, range: Range, text: &str, version: i32) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {
                    "uri": uri,
                    "version": version,
                },
                "contentChanges": [{
                    "range": {
                        "start": {
                            "line": range.start.line,
                            "character": range.start.character,
                        },
                        "end": {
                            "line": range.end.line,
                            "character": range.end.character,
                        },
                    },
                    "text": text,
                }],
            }),
        )
        .await;
    }

    /// Send `textDocument/didSave`, optionally including the full text.
    pub async fn save(&self, uri: &Url, text: Option<&str>) {
        let mut params = json!({
            "textDocument": { "uri": uri },
        });
        if let Some(text) = text {
            params["text"] = json!(text);
        }
        self.notify("textDocument/didSave", params).await;
    }

    /// Send `textDocument/didClose`.
    pub async fn close(&self, uri: &Url) {
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await;
    }

    /// Send a request and return its `result`, panicking on a protocol error or
    /// timeout.
    pub async fn request(&self, method: &str, params: Value) -> Value {
        self.request_with_timeout(method, params, DEFAULT_REQUEST_TIMEOUT)
            .await
    }

    /// Like [`Harness::request`], with an explicit timeout.
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = lock(&self.pending);
            pending.insert(id, tx);
        }
        let mut message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });
        if !params.is_null() {
            message["params"] = params;
        }
        self.send(message);

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => result,
            Ok(Ok(Err(error))) => panic!(
                "`{method}` failed: server returned an error: {error}; {}",
                self.received_summary()
            ),
            Ok(Err(_)) => {
                lock(&self.pending).remove(&id);
                panic!(
                    "`{method}` failed: the server closed the response channel; {}",
                    self.received_summary()
                );
            }
            Err(_) => {
                lock(&self.pending).remove(&id);
                panic!(
                    "timed out after {timeout:?} waiting for the `{method}` response; {}",
                    self.received_summary()
                );
            }
        }
    }

    /// Send a notification without waiting for a response.
    ///
    /// A `null` `params` value is omitted from the message, as required by
    /// parameterless protocol methods such as `shutdown` and `exit`.
    pub async fn notify(&self, method: &str, params: Value) {
        let mut message = json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        if !params.is_null() {
            message["params"] = params;
        }
        self.send(message);
    }

    /// The sequence number of the most recent notification, or `0` when none
    /// has arrived. Pass it to [`Harness::await_diagnostics_since`] to ignore
    /// notifications received before an action.
    pub fn notification_seq(&self) -> u64 {
        lock(&self.notifications)
            .last()
            .map(|notification| notification.seq)
            .unwrap_or(0)
    }

    /// Wait until a `textDocument/publishDiagnostics` notification for `uri`
    /// satisfies `predicate`, then return its diagnostics.
    ///
    /// Notifications are kept in a history, so one that arrived before this
    /// call is still considered. Panics with the received messages on timeout.
    pub async fn await_diagnostics<F>(
        &self,
        uri: &Url,
        predicate: F,
        timeout: Duration,
    ) -> Vec<Value>
    where
        F: Fn(&[Value]) -> bool + Send + Sync,
    {
        self.await_diagnostics_inner(None, uri, &predicate, timeout)
            .await
    }

    /// Like [`Harness::await_diagnostics`], but only notifications with a
    /// sequence greater than `since` are considered.
    pub async fn await_diagnostics_since<F>(
        &self,
        since: u64,
        uri: &Url,
        predicate: F,
        timeout: Duration,
    ) -> Vec<Value>
    where
        F: Fn(&[Value]) -> bool + Send + Sync,
    {
        self.await_diagnostics_inner(Some(since), uri, &predicate, timeout)
            .await
    }

    async fn await_diagnostics_inner<F>(
        &self,
        since: Option<u64>,
        uri: &Url,
        predicate: &F,
        timeout: Duration,
    ) -> Vec<Value>
    where
        F: Fn(&[Value]) -> bool + Send + Sync,
    {
        let uri_text = uri.as_str().to_string();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            // Register interest before taking the snapshot. `notify_one` stores
            // a permit, so a notification between the snapshot and the await is
            // not lost.
            let notified = self.notify.notified();
            let snapshot = self.notifications_snapshot();
            for notification in &snapshot {
                if let Some(since) = since {
                    if notification.seq <= since {
                        continue;
                    }
                }
                if notification.method != "textDocument/publishDiagnostics" {
                    continue;
                }
                if notification.params.get("uri").and_then(Value::as_str) != Some(uri_text.as_str())
                {
                    continue;
                }
                let diagnostics = notification
                    .params
                    .get("diagnostics")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if predicate(&diagnostics) {
                    return diagnostics;
                }
            }

            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!(
                    "timed out after {timeout:?} waiting for diagnostics for {uri}; {}",
                    self.received_summary()
                );
            }
        }
    }

    /// Wait for the first server-to-client request after `since`.
    ///
    /// Returns `None` on timeout instead of panicking, because not every server
    /// sends initialization requests. The response is queued before the request
    /// is recorded, so once this returns the reply is ahead of any later
    /// shutdown/exit messages in the writer queue.
    async fn await_server_request_since(&self, since: u64, timeout: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            let snapshot = self.notifications_snapshot();
            for notification in &snapshot {
                if notification.seq > since && notification.is_request {
                    return Some(notification.params.clone());
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return None;
            }
        }
    }

    /// Send `shutdown`, send `exit`, and wait for the process to exit.
    ///
    /// Returns the `shutdown` response. Panics if the process does not exit
    /// within `timeout`.
    pub async fn shutdown(&mut self, timeout: Duration) -> Value {
        let response = self.request("shutdown", Value::Null).await;
        self.notify("exit", Value::Null).await;
        // tower-lsp stops serving when stdin reaches EOF, which is how a
        // normal editor tears the process down after `exit`.
        self.close_stdin();
        let _status = self.wait_for_exit(timeout).await;
        response
    }

    /// Wait for the server process to exit and return its status.
    pub async fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        if let Some(status) = self.exit_status {
            return status;
        }
        let Some(mut child) = self.child.take() else {
            panic!("hardroc process was already reaped");
        };
        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => {
                self.exit_status = Some(status);
                status
            }
            Ok(Err(error)) => panic!(
                "failed waiting for hardroc to exit: {error}; {}",
                self.received_summary()
            ),
            Err(_) => {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                panic!(
                    "timed out after {timeout:?} waiting for hardroc to exit; {}",
                    self.received_summary()
                );
            }
        }
    }

    /// The exit status once the process has been reaped.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit_status
    }

    /// The tail of the server's stderr, useful in assertion messages.
    pub fn stderr_tail(&self) -> String {
        let log = lock(&self.log).join("");
        let tail: String = log
            .chars()
            .rev()
            .take(4096)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        tail
    }

    fn send(&self, message: Value) {
        if self.tx.send(Outgoing::Message(message)).is_err() {
            panic!(
                "hardroc stdin is closed (did the server exit?); {}",
                self.received_summary()
            );
        }
    }

    fn close_stdin(&self) {
        let _ = self.tx.send(Outgoing::CloseStdin);
    }

    fn notifications_snapshot(&self) -> Vec<Notification> {
        lock(&self.notifications).clone()
    }

    fn received_summary(&self) -> String {
        let snapshot = self.notifications_snapshot();
        let mut summary = String::from("received notifications: [");
        if snapshot.is_empty() {
            summary.push_str("none");
        }
        for (index, notification) in snapshot.iter().rev().take(8).rev().enumerate() {
            if index > 0 {
                summary.push_str(", ");
            }
            let params = serde_json::to_string(&notification.params)
                .unwrap_or_else(|_| "<unserializable>".to_string());
            let params: String = params.chars().take(240).collect();
            summary.push_str(&format!(
                "#{} {} {}",
                notification.seq, notification.method, params
            ));
        }
        summary.push(']');
        let stderr = self.stderr_tail();
        if !stderr.trim().is_empty() {
            summary.push_str("; server stderr: ");
            summary.push_str(stderr.trim());
        }
        summary
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

/// The workspace fixture root (`tests/fixtures/workspace`).
pub fn fixture_workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("workspace")
}

/// Build a `file://` URI for `root/relative`.
pub fn fixture_uri(root: &Path, relative: &str) -> Url {
    let path = root.join(relative);
    Url::from_file_path(&path)
        .unwrap_or_else(|_| panic!("fixture path is not a valid file URI: {}", path.display()))
}

/// Read a fixture as UTF-8 text.
pub fn read_fixture(root: &Path, relative: &str) -> String {
    let path = root.join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read fixture {}: {error}", path.display()))
}

/// Initialization options that keep tests hermetic: no compiler subprocess and
/// no package-cache inspection.
pub fn hermetic_initialization_options() -> Value {
    json!({
        "compilerDiagnostics": false,
        "inspectPackages": false,
        "syntaxDiagnostics": true,
        "checkDebounceMs": 0,
    })
}

/// True when a capability is present and enabled, accepting both the boolean
/// and options-object protocol forms.
fn provider_enabled(capabilities: &Value, key: &str) -> bool {
    match capabilities.get(key) {
        Some(Value::Bool(enabled)) => *enabled,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

/// The advertised text-sync change kind, accepting both `2` and
/// `{"change": 2}`.
fn sync_change_kind(capabilities: &Value) -> Option<i64> {
    match capabilities.get("textDocumentSync") {
        Some(Value::Number(kind)) => kind.as_i64(),
        Some(Value::Object(options)) => options.get("change").and_then(Value::as_i64),
        _ => None,
    }
}

/// Flatten the names in a `DocumentSymbolResponse` (nested or flat form).
fn symbol_names(response: &Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(items) = response.as_array() {
        for item in items {
            collect_symbol_names(item, &mut names);
        }
    }
    names
}

fn collect_symbol_names(value: &Value, names: &mut Vec<String>) {
    if let Some(name) = value.get("name").and_then(Value::as_str) {
        names.push(name.to_string());
    }
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        for child in children {
            collect_symbol_names(child, names);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

async fn writer_loop(
    mut stdin: ChildStdin,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
    log: Arc<Mutex<Vec<String>>>,
) {
    while let Some(outgoing) = rx.recv().await {
        let message = match outgoing {
            Outgoing::Message(message) => message,
            Outgoing::CloseStdin => break,
        };
        let body = match serde_json::to_vec(&message) {
            Ok(body) => body,
            Err(error) => {
                lock(&log).push(format!("[stdin] failed to serialize message: {error}"));
                continue;
            }
        };
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        if let Err(error) = stdin.write_all(header.as_bytes()).await {
            lock(&log).push(format!("[stdin] failed to write header: {error}"));
            break;
        }
        if let Err(error) = stdin.write_all(&body).await {
            lock(&log).push(format!("[stdin] failed to write body: {error}"));
            break;
        }
        if let Err(error) = stdin.flush().await {
            lock(&log).push(format!("[stdin] failed to flush: {error}"));
            break;
        }
    }
}

async fn reader_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<PendingResult>>>>,
    notifications: Arc<Mutex<Vec<Notification>>>,
    notify: Arc<Notify>,
    tx: mpsc::UnboundedSender<Outgoing>,
    log: Arc<Mutex<Vec<String>>>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_frame(&mut reader).await {
            Ok(Some(message)) => handle_message(&message, &pending, &notifications, &notify, &tx),
            Ok(None) => break,
            Err(error) => {
                lock(&log).push(format!("[protocol] failed to read a message: {error}"));
                break;
            }
        }
    }
    // Dropping the senders wakes in-flight requests instead of leaving them to
    // time out after the process has gone away.
    lock(&pending).clear();
}

async fn stderr_loop(stderr: ChildStderr, log: Arc<Mutex<Vec<String>>>) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => lock(&log).push(format!("[stderr] {}", line.trim_end())),
            Err(_) => break,
        }
    }
}

async fn read_frame(reader: &mut BufReader<ChildStdout>) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                let value = value.trim();
                let length = value.parse::<usize>().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid Content-Length: {value}"),
                    )
                })?;
                content_length = Some(length);
            }
        }
    }

    let length = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message header had no Content-Length",
        )
    })?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map(Some).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid JSON body: {error}"),
        )
    })
}

fn handle_message(
    message: &Value,
    pending: &Mutex<HashMap<i64, oneshot::Sender<PendingResult>>>,
    notifications: &Mutex<Vec<Notification>>,
    notify: &Notify,
    tx: &mpsc::UnboundedSender<Outgoing>,
) {
    match message.get("method").and_then(Value::as_str) {
        Some(method) => {
            let id = message.get("id");
            if let Some(id) = id {
                // Server-to-client request. Queue the reply first so it is
                // ahead of any shutdown messages a test sends after observing
                // this request. `workspace/configuration` expects one entry
                // per item.
                let result = if method == "workspace/configuration" {
                    let count = message
                        .get("params")
                        .and_then(|params| params.get("items"))
                        .and_then(Value::as_array)
                        .map(|items| items.len())
                        .unwrap_or(0);
                    Value::Array(vec![Value::Null; count])
                } else {
                    Value::Null
                };
                let _ = tx.send(Outgoing::Message(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                })));
            }
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let mut queue = lock(notifications);
            let seq = queue.last().map(|entry| entry.seq + 1).unwrap_or(1);
            queue.push(Notification {
                seq,
                method: method.to_string(),
                params,
                is_request: id.is_some(),
            });
            drop(queue);
            notify.notify_one();
        }
        None => {
            let Some(id) = message.get("id").and_then(Value::as_i64) else {
                return;
            };
            let Some(sender) = lock(pending).remove(&id) else {
                return;
            };
            let outcome = match message.get("error") {
                Some(error) => Err(error.clone()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = sender.send(outcome);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_advertises_expected_capabilities() {
    let root = fixture_workspace_root();
    let mut harness = Harness::spawn().await;
    let initialize = harness.initialize(&root).await;
    harness.initialized().await;

    let capabilities = initialize
        .get("capabilities")
        .unwrap_or_else(|| panic!("initialize result had no capabilities: {initialize}"));
    let pretty =
        serde_json::to_string_pretty(capabilities).unwrap_or_else(|_| capabilities.to_string());

    assert_eq!(
        capabilities.get("positionEncoding").and_then(Value::as_str),
        Some("utf-16"),
        "expected UTF-16 position encoding; got:\n{pretty}"
    );
    assert_eq!(
        sync_change_kind(capabilities),
        Some(2),
        "expected incremental text sync (kind 2); got:\n{pretty}"
    );

    for key in [
        "completionProvider",
        "hoverProvider",
        "definitionProvider",
        "declarationProvider",
        "typeDefinitionProvider",
        "implementationProvider",
        "referencesProvider",
        "documentSymbolProvider",
        "workspaceSymbolProvider",
        "codeActionProvider",
        "documentFormattingProvider",
        "documentRangeFormattingProvider",
        "foldingRangeProvider",
        "selectionRangeProvider",
        "semanticTokensProvider",
        "inlayHintProvider",
        "documentLinkProvider",
        "executeCommandProvider",
    ] {
        assert!(
            provider_enabled(capabilities, key),
            "capabilities.{key} missing or disabled; got:\n{pretty}"
        );
    }

    let completion = capabilities
        .get("completionProvider")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("completionProvider was not an options object:\n{pretty}"));
    assert_eq!(
        completion.get("resolveProvider").and_then(Value::as_bool),
        Some(true),
        "completion resolve was not advertised; got:\n{pretty}"
    );

    let advertised_commands = capabilities
        .get("executeCommandProvider")
        .and_then(|provider| provider.get("commands"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        !advertised_commands
            .iter()
            .any(|command| command.as_str() == Some("hardroc.showReferences")),
        "parameterized showReferences command must not be advertised: {pretty}"
    );

    let rename = capabilities
        .get("renameProvider")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("renameProvider was not an options object:\n{pretty}"));
    assert_eq!(
        rename.get("prepareProvider").and_then(Value::as_bool),
        Some(true),
        "prepareRename was not advertised; got:\n{pretty}"
    );

    let commands = capabilities
        .get("executeCommandProvider")
        .and_then(|provider| provider.get("commands"))
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("executeCommandProvider had no command list:\n{pretty}"));
    assert!(
        commands
            .iter()
            .any(|command| command.as_str() == Some("hardroc.version")),
        "expected the hardroc.version command; got:\n{pretty}"
    );

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publishes_parser_diagnostics_and_clears_clean_files() {
    let root = fixture_workspace_root();
    let mut harness = Harness::start(&root).await;

    let broken_uri = fixture_uri(&root, "broken.roc");
    let broken_text = read_fixture(&root, "broken.roc");
    harness.open(&broken_uri, &broken_text, 1).await;
    let diagnostics = harness
        .await_diagnostics(
            &broken_uri,
            |diagnostics| !diagnostics.is_empty(),
            Duration::from_secs(10),
        )
        .await;
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic
                .get("message")
                .and_then(Value::as_str)
                .map(|message| !message.is_empty())
                .unwrap_or(false)
        }),
        "a malformed fixture produced only empty diagnostics: {diagnostics:?}"
    );

    let color_uri = fixture_uri(&root, "Color.roc");
    let color_text = read_fixture(&root, "Color.roc");
    harness.open(&color_uri, &color_text, 1).await;
    let clean = harness
        .await_diagnostics(
            &color_uri,
            |diagnostics| diagnostics.is_empty(),
            Duration::from_secs(10),
        )
        .await;
    assert!(
        clean.is_empty(),
        "a clean fixture still reported diagnostics: {clean:?}"
    );

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn utf16_incremental_edit_after_emoji_keeps_the_document_consistent() {
    let root = fixture_workspace_root();
    let mut harness = Harness::start(&root).await;

    let uri = fixture_uri(&root, "Color.roc");
    let source = format!("# 😀\n{}", read_fixture(&root, "Color.roc"));
    harness.open(&uri, &source, 1).await;
    // Wait for the open to be processed before taking a sequence number.
    harness
        .await_diagnostics(&uri, |_| true, Duration::from_secs(10))
        .await;
    let before_edit = harness.notification_seq();

    // Line 0 is `# 😀`: `#`, space, and the emoji (two UTF-16 code units) put
    // the insertion point at character 4. A server that treated columns as
    // bytes would split the surrogate pair or insert in the wrong place.
    harness
        .change_incremental(
            &uri,
            Range::new(Position::new(0, 4), Position::new(0, 4)),
            "\nafter_emoji = 42\n",
            2,
        )
        .await;
    harness
        .await_diagnostics_since(before_edit, &uri, |_| true, Duration::from_secs(10))
        .await;

    let symbols = harness
        .request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await;
    let names = symbol_names(&symbols);
    assert!(
        names.iter().any(|name| name.as_str() == "after_emoji"),
        "the UTF-16 edit after the emoji was not applied to the server document; \
         symbols were {names:?}"
    );

    let hover = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 1, "character": 2 },
            }),
        )
        .await;
    assert!(
        hover.is_null() || hover.is_object(),
        "hover after a UTF-16 edit returned an unexpected shape: {hover}"
    );

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_symbol_returns_type_and_definition_symbols() {
    let root = fixture_workspace_root();
    let mut harness = Harness::start(&root).await;

    let uri = fixture_uri(&root, "Color.roc");
    let text = read_fixture(&root, "Color.roc");
    harness.open(&uri, &text, 1).await;
    harness
        .await_diagnostics(&uri, |_| true, Duration::from_secs(10))
        .await;

    let symbols = harness
        .request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await;
    let names = symbol_names(&symbols);

    assert!(
        names.iter().any(|name| name.as_str() == "Color"),
        "documentSymbol did not include the top-level Color type; got {names:?}"
    );
    assert!(
        names.iter().any(|name| name.as_str() == "palette_name"),
        "documentSymbol did not include the top-level palette_name definition; got {names:?}"
    );

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn formatting_returns_an_edit_when_roc_is_available() {
    if !roc_available() {
        eprintln!("skipping formatting test: `roc` is not on PATH");
        return;
    }

    let root = fixture_workspace_root();
    let mut harness = Harness::start(&root).await;
    let uri = fixture_uri(&root, "Formatting.roc");
    let source =
        "app [main!] {\n\troc: \"nightly-2026-11-09-9e85de7\",\n}\n\nmain! = |_| {Ok({})}\n";
    harness.open(&uri, source, 1).await;

    let edits = harness
        .request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": uri },
                "options": {
                    "tabSize": 4,
                    "insertSpaces": false,
                },
            }),
        )
        .await;
    let edits = edits
        .as_array()
        .unwrap_or_else(|| panic!("expected formatting edits, but the server returned {edits}"));
    assert!(
        !edits.is_empty(),
        "formatting a badly formatted document returned no edits"
    );
    let new_text = edits[0]
        .get("newText")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("formatting edit had no newText: {:?}", edits[0]));
    assert_ne!(new_text, source, "formatting returned the original text");
    assert!(
        new_text.contains("main! = |_| {"),
        "formatted text did not contain the expected definition: {new_text:?}"
    );

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn range_formatting_only_edits_the_requested_lines() {
    if !roc_available() {
        eprintln!("skipping range formatting test: `roc` is not on PATH");
        return;
    }

    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    let path = root.join("main.roc");
    let source = "app [main!] {\n\troc: \"nightly-2026-11-09-9e85de7\",\n}\n\nmain! = |_| {\n\tOk({})\n}\n\nsettings =   { a: 1 }\n";
    std::fs::write(&path, source).expect("write temporary Roc module");
    let uri = Url::from_file_path(&path).expect("temporary Roc file URI");

    let mut harness = Harness::spawn().await;
    harness
        .initialize_with_options(
            root,
            json!({
                "compilerDiagnostics": false,
                "inspectPackages": false,
                "syntaxDiagnostics": true,
                "checkDebounceMs": 0,
            }),
        )
        .await;
    harness.initialized().await;
    harness.open(&uri, source, 1).await;

    let edits = harness
        .request(
            "textDocument/rangeFormatting",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 8, "character": 0 },
                    "end": { "line": 9, "character": 0 },
                },
                "options": {
                    "tabSize": 4,
                    "insertSpaces": false,
                },
            }),
        )
        .await;
    let edits = edits
        .as_array()
        .unwrap_or_else(|| panic!("expected range formatting edits, got {edits}"));
    assert!(!edits.is_empty(), "range formatting returned no edits");
    for edit in edits {
        let start = edit["range"]["start"]["line"].as_u64().unwrap_or(0);
        let end = edit["range"]["end"]["line"].as_u64().unwrap_or(0);
        assert!(
            start >= 8 && end <= 9,
            "range formatting edited outside the requested range: {edit}"
        );
    }

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publishes_compiler_type_diagnostics_when_roc_is_available() {
    if !roc_available() {
        eprintln!("skipping compiler diagnostics test: `roc` is not on PATH");
        return;
    }

    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    let path = root.join("main.roc");
    let source = "module [main]\n\nmain = 1 + \"x\"\n";
    std::fs::write(&path, source).expect("write temporary Roc module");
    let uri = Url::from_file_path(&path).expect("temporary Roc file URI");

    let mut harness = Harness::spawn().await;
    harness
        .initialize_with_options(
            root,
            json!({
                "compilerDiagnostics": true,
                "inspectPackages": false,
                "syntaxDiagnostics": true,
                "checkDebounceMs": 0,
                "checkTimeoutSecs": 30,
            }),
        )
        .await;
    harness.initialized().await;
    harness.open(&uri, source, 1).await;

    let diagnostics = harness
        .await_diagnostics(
            &uri,
            |diagnostics| {
                diagnostics.iter().any(|diagnostic| {
                    diagnostic.get("source").and_then(Value::as_str) == Some("roc")
                })
            },
            Duration::from_secs(40),
        )
        .await;
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.get("source").and_then(Value::as_str) == Some("roc")
                && diagnostic
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| {
                        message.contains("plus method") || message.contains("type")
                    })
        }),
        "compiler diagnostics did not include the type error: {diagnostics:?}"
    );

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compiler_diagnostics_use_unsaved_buffer_text() {
    if !roc_available() {
        eprintln!("skipping unsaved-buffer diagnostics test: `roc` is not on PATH");
        return;
    }

    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    let path = root.join("main.roc");
    let clean = "module [main]\n\nmain = 1\n";
    std::fs::write(&path, clean).expect("write temporary Roc module");
    let uri = Url::from_file_path(&path).expect("temporary Roc file URI");

    let mut harness = Harness::spawn().await;
    harness
        .initialize_with_options(
            root,
            json!({
                "compilerDiagnostics": true,
                "inspectPackages": false,
                "syntaxDiagnostics": true,
                "checkDebounceMs": 0,
                "checkTimeoutSecs": 30,
            }),
        )
        .await;
    harness.initialized().await;
    harness.open(&uri, clean, 1).await;
    harness
        .await_diagnostics(&uri, |_| true, Duration::from_secs(10))
        .await;

    // Change the in-memory buffer without saving it. The compiler must see the
    // overlay, not the still-clean file on disk.
    let dirty = "module [main]\n\nmain = 1 + \"x\"\n";
    harness.change_full(&uri, dirty, 2).await;
    let diagnostics = harness
        .await_diagnostics(
            &uri,
            |diagnostics| {
                diagnostics.iter().any(|diagnostic| {
                    diagnostic.get("source").and_then(Value::as_str) == Some("roc")
                        && diagnostic
                            .get("message")
                            .and_then(Value::as_str)
                            .is_some_and(|message| message.contains("plus method"))
                })
            },
            Duration::from_secs(40),
        )
        .await;

    assert!(
        std::fs::read_to_string(&path).expect("read clean file") == clean,
        "the unsaved editor buffer was written to disk"
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.get("source").and_then(Value::as_str) == Some("roc") }),
        "unsaved buffer did not produce compiler diagnostics: {diagnostics:?}"
    );

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_resolve_fills_documentation_over_lsp() {
    let root = fixture_workspace_root();
    let mut harness = Harness::spawn().await;
    harness.initialize(&root).await;
    harness.initialized().await;

    let resolved = harness
        .request(
            "completionItem/resolve",
            json!({
                "label": "Str",
                "data": { "kind": "builtin", "name": "Str" },
            }),
        )
        .await;
    assert!(
        resolved.get("documentation").is_some() || resolved.get("detail").is_some(),
        "completion resolve returned no documentation or detail: {resolved}"
    );

    let uri = fixture_uri(&root, "main.roc");
    let action = harness
        .request(
            "codeAction/resolve",
            json!({
                "title": "Import `Color`",
                "kind": "quickfix",
                "data": {
                    "kind": "import",
                    "path": "Color",
                    "uri": uri,
                },
            }),
        )
        .await;
    assert!(
        action.get("edit").is_some(),
        "code action resolve returned no edit: {action}"
    );

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_folders_add_and_remove_roots() {
    let first = tempfile::tempdir().expect("first workspace");
    let second = tempfile::tempdir().expect("second workspace");
    std::fs::write(first.path().join("One.roc"), "module [one]\n\none = 1\n")
        .expect("write first module");
    std::fs::write(second.path().join("Two.roc"), "module [two]\n\ntwo = 2\n")
        .expect("write second module");

    let mut harness = Harness::spawn().await;
    harness
        .initialize_with_workspace_folders(
            &[first.path().to_path_buf()],
            hermetic_initialization_options(),
        )
        .await;
    harness.initialized().await;

    let first_symbols = symbol_names(
        &harness
            .request("workspace/symbol", json!({ "query": "one" }))
            .await,
    );
    assert!(
        first_symbols.iter().any(|name| name == "one"),
        "initial workspace did not expose one: {first_symbols:?}"
    );

    let second_uri = Url::from_directory_path(second.path()).expect("second folder uri");
    harness
        .notify(
            "workspace/didChangeWorkspaceFolders",
            json!({
                "event": {
                    "added": [{ "uri": second_uri, "name": "second" }],
                    "removed": [],
                }
            }),
        )
        .await;
    let mut second_symbols = Vec::new();
    for _ in 0..40 {
        second_symbols = symbol_names(
            &harness
                .request("workspace/symbol", json!({ "query": "two" }))
                .await,
        );
        if second_symbols.iter().any(|name| name == "two") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        second_symbols.iter().any(|name| name == "two"),
        "added workspace folder was not indexed: {second_symbols:?}"
    );

    harness
        .notify(
            "workspace/didChangeConfiguration",
            json!({
                "settings": {
                    "compilerDiagnostics": false,
                    "inspectPackages": false,
                }
            }),
        )
        .await;
    let after_config = symbol_names(
        &harness
            .request("workspace/symbol", json!({ "query": "one" }))
            .await,
    );
    assert!(
        after_config.iter().any(|name| name == "one"),
        "configuration change dropped the workspace index: {after_config:?}"
    );

    harness
        .notify(
            "workspace/didChangeWorkspaceFolders",
            json!({
                "event": {
                    "added": [],
                    "removed": [{ "uri": second_uri, "name": "second" }],
                }
            }),
        )
        .await;
    let mut removed_symbols = Vec::new();
    for _ in 0..40 {
        removed_symbols = symbol_names(
            &harness
                .request("workspace/symbol", json!({ "query": "two" }))
                .await,
        );
        if removed_symbols.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        removed_symbols.is_empty(),
        "removed workspace folder stayed indexed: {removed_symbols:?}"
    );

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_compiler_diagnostics_clears_cached_compiler_errors() {
    if !roc_available() {
        eprintln!("skipping compiler diagnostics test: `roc` is not on PATH");
        return;
    }

    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path();
    let path = root.join("main.roc");
    let source =
        "app [main!] {\n\troc: \"nightly-2026-11-09-9e85de7\",\n}\n\nvalue : Str\nvalue = 1\n";
    std::fs::write(&path, source).expect("write module");
    let uri = Url::from_file_path(&path).expect("uri");

    let mut harness = Harness::spawn().await;
    harness
        .initialize_with_options(
            root,
            json!({
                "compilerDiagnostics": true,
                "inspectPackages": false,
                "checkDebounceMs": 0,
            }),
        )
        .await;
    harness.initialized().await;
    harness.open(&uri, source, 1).await;
    harness
        .await_diagnostics(
            &uri,
            |diagnostics| {
                diagnostics.iter().any(|diagnostic| {
                    diagnostic.get("source").and_then(Value::as_str) == Some("roc")
                })
            },
            Duration::from_secs(20),
        )
        .await;

    let before = harness.notification_seq();
    harness
        .notify(
            "workspace/didChangeConfiguration",
            json!({
                "settings": {
                    "compilerDiagnostics": false,
                    "checkDebounceMs": 0,
                    "inspectPackages": false,
                }
            }),
        )
        .await;
    harness
        .await_diagnostics_since(
            before,
            &uri,
            |diagnostics| {
                !diagnostics.iter().any(|diagnostic| {
                    diagnostic.get("source").and_then(Value::as_str) == Some("roc")
                })
            },
            Duration::from_secs(10),
        )
        .await;

    harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_responds_and_the_process_exits() {
    let root = fixture_workspace_root();
    let mut harness = Harness::spawn().await;
    harness.initialize(&root).await;
    harness.initialized().await;

    let response = harness.shutdown(DEFAULT_EXIT_TIMEOUT).await;
    assert!(
        response.is_null(),
        "shutdown should return a null result, got {response}"
    );
    let status = harness
        .exit_status()
        .unwrap_or_else(|| panic!("the server process did not exit after shutdown/exit"));
    assert!(
        status.success(),
        "the server exited unsuccessfully after shutdown: {status:?}; stderr: {}",
        harness.stderr_tail()
    );
}

fn roc_available() -> bool {
    std::process::Command::new("roc")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
