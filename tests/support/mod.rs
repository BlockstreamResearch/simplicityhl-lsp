use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tower_lsp_server::lsp_types::Uri;
use tower_lsp_server::UriExt;

const MESSAGE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn file_uri(path: &Path) -> String {
    Uri::from_file_path(path)
        .expect("absolute test path produces a file URI")
        .as_str()
        .to_string()
}

pub struct LspProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
    pending: VecDeque<Value>,
}

impl LspProcess {
    pub fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_simplicityhl-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn language server");
        let stdout = child.stdout.take().expect("language server stdout");
        let stdin = child.stdin.take().expect("language server stdin");
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_message(&mut reader) {
                    Ok(Some(message)) => {
                        if sender.send(Ok(message)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        return;
                    }
                }
            }
        });

        Self {
            child,
            stdin: Some(stdin),
            messages,
            pending: VecDeque::new(),
        }
    }

    pub fn initialize(&mut self, root_uri: &str, initialization_options: &Value) -> Value {
        let response = self.request(
            1,
            "initialize",
            &json!({
                "processId": null,
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": "integration" }],
                "capabilities": {},
                "initializationOptions": initialization_options
            }),
        );
        self.notify("initialized", &json!({}));
        response
    }

    pub fn request(&mut self, id: i32, method: &str, params: &Value) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }));
        self.receive_where(|message| message.get("id") == Some(&json!(id)))
    }

    pub fn notify(&mut self, method: &str, params: &Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }));
    }

    pub fn diagnostics(&mut self, uri: &str, version: i32) -> Value {
        self.receive_where(|message| {
            message.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
                && message.pointer("/params/uri").and_then(Value::as_str) == Some(uri)
                && message.pointer("/params/version").and_then(Value::as_i64)
                    == Some(i64::from(version))
        })
    }

    pub fn shutdown(&mut self) {
        let response = self.request(999, "shutdown", &json!(null));
        assert!(
            response.get("error").is_none(),
            "shutdown failed: {response}"
        );
        self.notify("exit", &json!(null));
        self.stdin.take();

        let deadline = Instant::now() + MESSAGE_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll language server") {
                assert!(status.success(), "language server exited with {status}");
                return;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("language server did not exit after shutdown");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, message: &Value) {
        let body = serde_json::to_vec(&message).expect("serialize JSON-RPC message");
        let stdin = self.stdin.as_mut().expect("language server stdin is open");
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write JSON-RPC header");
        stdin.write_all(&body).expect("write JSON-RPC body");
        stdin.flush().expect("flush JSON-RPC message");
    }

    fn receive_where(&mut self, mut predicate: impl FnMut(&Value) -> bool) -> Value {
        if let Some(index) = self.pending.iter().position(&mut predicate) {
            return self.pending.remove(index).expect("pending message index");
        }

        let deadline = Instant::now() + MESSAGE_TIMEOUT;
        loop {
            let timeout = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(timeout) {
                Ok(Ok(message)) if predicate(&message) => return message,
                Ok(Ok(message)) => self.pending.push_back(message),
                Ok(Err(error)) => panic!("invalid language server output: {error}"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("language server output closed before the expected message")
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for language server message")
                }
            }
        }
    }
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>, String> {
    let mut content_length = None;
    loop {
        let mut header = String::new();
        let read = reader
            .read_line(&mut header)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(None);
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
        if let Some(value) = header
            .strip_prefix("Content-Length:")
            .or_else(|| header.strip_prefix("content-length:"))
        {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| error.to_string())?,
            );
        }
    }

    let length = content_length.ok_or_else(|| "missing Content-Length header".to_string())?;
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| error.to_string())
}
