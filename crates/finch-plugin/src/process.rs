use crate::{
    API_VERSION, HostOptions, MAX_MESSAGE_BYTES, PluginContext, PluginError, PluginResult, Result,
};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(crate) struct Invocation {
    pub command_id: String,
    pub context: PluginContext,
    pub reply: mpsc::Sender<Result<PluginResult>>,
}

pub(crate) struct Worker {
    pub sender: mpsc::Sender<Invocation>,
    cancelled: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(
        executable: PathBuf,
        package: PathBuf,
        storage: PathBuf,
        options: HostOptions,
    ) -> Self {
        let (sender, receiver) = mpsc::channel::<Invocation>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = cancelled.clone();
        let thread = thread::spawn(move || {
            // Wait for the first command before starting even this process.
            let Ok(first) = receiver.recv() else {
                return;
            };
            let mut process = match Process::spawn(executable, package, cancellation) {
                Ok(process) => process,
                Err(error) => {
                    let _ = first.reply.send(Err(error));
                    return;
                }
            };
            let handshake = process
                .rpc(
                    "initialize",
                    json!({
                        "api_version": API_VERSION,
                        "data_directory": storage,
                        "host": "Finch",
                    }),
                    options.startup_timeout,
                )
                .and_then(|value| {
                    if value.get("api_version").and_then(Value::as_u64)
                        != Some(u64::from(API_VERSION))
                    {
                        Err(PluginError::Protocol(
                            "unsupported handshake API version".into(),
                        ))
                    } else {
                        Ok(())
                    }
                });
            if let Err(error) = handshake {
                let _ = first.reply.send(Err(error));
                return;
            }
            let mut next = Some(first);
            loop {
                let invocation = match next.take() {
                    Some(invocation) => invocation,
                    None => match receiver.recv_timeout(options.idle_timeout) {
                        Ok(invocation) => invocation,
                        Err(_) => break,
                    },
                };
                let result = process
                    .rpc(
                        "command.execute",
                        json!({
                            "command": invocation.command_id,
                            "context": invocation.context,
                        }),
                        options.command_timeout,
                    )
                    .and_then(|value| serde_json::from_value(value).map_err(PluginError::from));
                let failed = result.is_err();
                let _ = invocation.reply.send(result);
                if failed {
                    break;
                }
            }
            // Process::drop always kills and reaps, including idle timeout.
        });
        Self {
            sender,
            cancelled,
            thread: Some(thread),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    pub fn stop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let (replacement, _) = mpsc::channel();
        self.sender = replacement; // Disconnect wakes an idle worker immediately.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

struct WriteRequest {
    message: Vec<u8>,
    reply: mpsc::Sender<std::io::Result<()>>,
}

struct Process {
    child: Child,
    writer: Option<mpsc::Sender<WriteRequest>>,
    reader: Option<mpsc::Receiver<std::result::Result<Vec<u8>, String>>>,
    io_threads: Vec<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    cancellation: Arc<AtomicBool>,
    next_id: u64,
}

impl Process {
    fn spawn(executable: PathBuf, package: PathBuf, cancellation: Arc<AtomicBool>) -> Result<Self> {
        let mut command = Command::new(executable);
        command
            .current_dir(package)
            .arg("--finch-plugin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");
        let (write_sender, write_receiver) = mpsc::channel::<WriteRequest>();
        let writer_thread = thread::spawn(move || {
            while let Ok(request) = write_receiver.recv() {
                let result = stdin
                    .write_all(&request.message)
                    .and_then(|()| stdin.flush());
                let failed = result.is_err();
                let _ = request.reply.send(result);
                if failed {
                    break;
                }
            }
        });
        let (read_sender, read_receiver) = mpsc::sync_channel(8);
        let reader_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_bounded_line(&mut reader) {
                    Ok(Some(line)) => {
                        if read_sender.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = read_sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_tail = stderr.clone();
        let stderr_thread = thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(count) = stderr_pipe.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                if let Ok(mut tail) = stderr_tail.lock() {
                    tail.extend_from_slice(&buffer[..count]);
                    if tail.len() > 16 * 1024 {
                        let trim = tail.len() - 16 * 1024;
                        tail.drain(..trim);
                    }
                }
            }
        });
        Ok(Self {
            child,
            writer: Some(write_sender),
            reader: Some(read_receiver),
            io_threads: vec![writer_thread, reader_thread, stderr_thread],
            stderr,
            cancellation,
            next_id: 1,
        })
    }

    fn rpc(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = serde_json::to_vec(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )?;
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(PluginError::Protocol("request exceeds 1 MiB".into()));
        }
        message.push(b'\n');
        let deadline = Instant::now() + timeout;
        let (sender, receiver) = mpsc::channel();
        self.writer
            .as_ref()
            .expect("live writer")
            .send(WriteRequest {
                message,
                reply: sender,
            })
            .map_err(|_| self.exited())?;
        // A separate writer makes a child that stops reading stdin subject to the same deadline.
        wait_until(&receiver, deadline, &self.cancellation, method)?.map_err(PluginError::Io)?;
        let bytes = wait_until(
            self.reader.as_ref().expect("live reader"),
            deadline,
            &self.cancellation,
            method,
        )
        .map_err(|error| {
            if matches!(error, PluginError::Exited(_)) {
                self.exited()
            } else {
                error
            }
        })?
        .map_err(PluginError::Protocol)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || value.get("id").and_then(Value::as_u64) != Some(id)
        {
            return Err(PluginError::Protocol(
                "response version or request id mismatch".into(),
            ));
        }
        match (value.get("result"), value.get("error")) {
            (Some(result), None) => Ok(result.clone()),
            (None, Some(error)) => {
                let code = error
                    .get("code")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| PluginError::Protocol("error code must be an integer".into()))?;
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        PluginError::Protocol("error message must be a string".into())
                    })?;
                Err(PluginError::Remote {
                    code,
                    message: message.into(),
                })
            }
            _ => Err(PluginError::Protocol(
                "response requires exactly one result or error".into(),
            )),
        }
    }

    fn exited(&self) -> PluginError {
        let tail = self
            .stderr
            .lock()
            .map(|s| String::from_utf8_lossy(&s).trim().to_owned())
            .unwrap_or_default();
        PluginError::Exited(if tail.is_empty() {
            "pipe closed".into()
        } else {
            tail
        })
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.writer.take();
        self.reader.take(); // Unblock a reader waiting to enqueue unsolicited output.
        for thread in self.io_threads.drain(..) {
            // A trusted plugin must not leave descendants holding its stdio pipes.
            // Do not block app exit if it does; detached readers release when pipes close.
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

fn wait_until<T>(
    receiver: &mpsc::Receiver<T>,
    deadline: Instant,
    cancelled: &AtomicBool,
    operation: &str,
) -> Result<T> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(PluginError::Exited("host shutdown".into()));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(PluginError::Timeout(operation.into()));
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(value) => return Ok(value),
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(PluginError::Exited("pipe closed".into()));
            }
        }
    }
}

fn read_bounded_line(reader: &mut impl BufRead) -> std::result::Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().map_err(|e| e.to_string())?;
        if buffer.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err("unterminated JSON-RPC line".into())
            };
        }
        let newline = buffer.iter().position(|b| *b == b'\n');
        let consumed = newline.map_or(buffer.len(), |position| position + 1);
        let payload = newline.unwrap_or(buffer.len());
        if line.len() + payload > MAX_MESSAGE_BYTES {
            return Err("response exceeds 1 MiB".into());
        }
        line.extend_from_slice(&buffer[..payload]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_line_does_not_accept_truncated_or_oversized_messages() {
        assert!(read_bounded_line(&mut &b"{}"[..]).is_err());
        assert_eq!(
            read_bounded_line(&mut &b"{}\nnext"[..]).unwrap(),
            Some(b"{}".to_vec())
        );
        assert!(read_bounded_line(&mut &vec![b'x'; MAX_MESSAGE_BYTES + 1][..]).is_err());
    }
}
