//! Small platform boundary for CLI paths, recovery markers, and local instance IPC.

use crossbeam_channel::{Receiver, bounded};
use finch_store::AppPaths;
use fs2::FileExt;
use interprocess::ConnectWaitMode;
use interprocess::local_socket::{
    ConnectOptions, GenericFilePath, GenericNamespaced, ListenerOptions, Name, Stream, prelude::*,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const HELP: &str = "Finch — 本地论文阅读器\n\n用法：finch [选项] [文档.pdf]\n  --data-dir <目录>        指定本地数据目录\n  --pdfium <动态库>        指定 PDFium 动态库\n  --software              使用软件渲染（Windows 默认）\n  --gpu                   使用 GPU 渲染（macOS 默认）\n  --metrics <文件>         写入性能指标\n  --screenshot <文件>      保存 P6 PPM 截图（扩展名改为 .ppm）\n  --quit-after-ms <毫秒>   定时结束测试运行\n  --smoke                 运行冒烟测试\n  --help                  显示帮助\n";

#[derive(Debug, Clone)]
pub struct Config {
    pub file: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub pdfium: Option<PathBuf>,
    pub software: bool,
    pub metrics: Option<PathBuf>,
    pub screenshot: Option<PathBuf>,
    pub quit_after_ms: Option<u64>,
    pub smoke: bool,
}

// Windows defaults to software rendering; deriving would lose that platform default.
#[allow(clippy::derivable_impls)]
impl Default for Config {
    fn default() -> Self {
        Self {
            file: None,
            data_dir: None,
            pdfium: None,
            software: cfg!(target_os = "windows"),
            metrics: None,
            screenshot: None,
            quit_after_ms: None,
            smoke: false,
        }
    }
}

impl Config {
    pub fn parse() -> Result<Self, String> {
        Self::parse_from(std::env::args_os().skip(1))
    }

    /// Parse arguments excluding the executable name. Path values preserve native
    /// encoding and are resolved against the caller's working directory.
    pub fn parse_from<I, S>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut config = Self::default();
        let mut args = args.into_iter().map(Into::into);
        let mut positional_only = false;
        while let Some(arg) = args.next() {
            if !positional_only {
                match arg.to_str() {
                    Some("--") => {
                        positional_only = true;
                        continue;
                    }
                    Some("--help" | "-h") => return Err(HELP.into()),
                    Some("--software") => {
                        config.software = true;
                        continue;
                    }
                    Some("--gpu") => {
                        config.software = false;
                        continue;
                    }
                    Some("--smoke") => {
                        config.smoke = true;
                        continue;
                    }
                    Some("--data-dir" | "--pdfium" | "--metrics" | "--screenshot") => {
                        let value = args
                            .next()
                            .ok_or_else(|| format!("{} requires a path", arg.to_string_lossy()))?;
                        if value.is_empty() {
                            return Err(format!(
                                "{} requires a nonempty path",
                                arg.to_string_lossy()
                            ));
                        }
                        let path =
                            absolute(PathBuf::from(value)).map_err(|error| error.to_string())?;
                        match arg.to_str().unwrap_or_default() {
                            "--data-dir" => config.data_dir = Some(path),
                            "--pdfium" => config.pdfium = Some(path),
                            "--metrics" => config.metrics = Some(path),
                            "--screenshot" => config.screenshot = Some(path),
                            _ => unreachable!(),
                        }
                        continue;
                    }
                    Some("--quit-after-ms") => {
                        let value = args.next().ok_or("--quit-after-ms requires an integer")?;
                        config.quit_after_ms = Some(
                            value
                                .to_str()
                                .ok_or("--quit-after-ms must be UTF-8 digits")?
                                .parse()
                                .map_err(|_| "--quit-after-ms requires a nonnegative integer")?,
                        );
                        continue;
                    }
                    Some(value) if value.starts_with('-') => {
                        return Err(format!(
                            "unknown option: {value}\nUse --help to view options."
                        ));
                    }
                    _ => {}
                }
            }
            if config.file.is_some() {
                return Err("only one PDF can be opened at a time".into());
            }
            config.file = Some(absolute(PathBuf::from(arg)).map_err(|error| error.to_string())?);
        }
        Ok(config)
    }

    pub fn paths(&self) -> finch_store::Result<AppPaths> {
        self.data_dir
            .as_ref()
            .map_or_else(AppPaths::discover, |path| Ok(AppPaths::in_directory(path)))
    }

    /// Explicit CLI path, then environment override, then packaged and development
    /// locations. Never searches PATH or downloads a library at runtime.
    pub fn pdfium_path(&self) -> Result<PathBuf, String> {
        if let Some(path) = &self.pdfium {
            return require_library(path.clone());
        }
        if let Some(path) = std::env::var_os("FINCH_PDFIUM_PATH").filter(|value| !value.is_empty())
        {
            return require_library(absolute(path.into()).map_err(|error| error.to_string())?);
        }
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let executable_dir = exe
            .parent()
            .ok_or("cannot determine executable directory")?;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let candidates = pdfium_candidates(executable_dir, &root);
        candidates
            .into_iter()
            .find(|path| path.is_file())
            .ok_or_else(|| {
                format!(
                    "PDFium is not installed. Place {} beside Finch or use --pdfium <path>.",
                    library_name()
                )
            })
    }
}

fn absolute(path: PathBuf) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn require_library(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("PDFium library was not found: {}", path.display()))
    }
}

fn library_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else {
        "libpdfium.so"
    }
}

fn pdfium_candidates(executable_dir: &Path, development_root: &Path) -> Vec<PathBuf> {
    let name = library_name();
    let mut candidates = vec![executable_dir.join(name)];
    if cfg!(target_os = "macos") {
        candidates.push(executable_dir.join("../Frameworks").join(name));
    }
    candidates.push(executable_dir.join("runtime/pdfium/bin").join(name));
    candidates.push(executable_dir.join("runtime/pdfium/lib").join(name));
    candidates.push(development_root.join("runtime/pdfium/bin").join(name));
    candidates.push(development_root.join("runtime/pdfium/lib").join(name));
    candidates
}

/// A guard tracks only the initial restore, not every unclean application exit.
/// Never clear it in Drop: a failed restore must remain detectable next launch.
pub struct RestoreGuard {
    path: PathBuf,
}

impl RestoreGuard {
    pub fn new(paths: &AppPaths) -> Self {
        Self {
            path: paths.data_dir.join("restore.inprogress"),
        }
    }
    pub fn was_interrupted(&self) -> bool {
        self.path.exists()
    }
    pub fn mark(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = File::create(&self.path)?;
        writeln!(file, "{}", std::process::id())?;
        file.sync_all()
    }
    pub fn clear(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum InstanceMessage {
    Focus,
    Open(PathBuf),
}

pub enum InstanceClaim {
    Primary(Instance),
    Forwarded,
}

/// Own this until the application exits. One blocked listener thread; no idle
/// timers or network sockets. A lock file's mere existence is not ownership.
pub struct Instance {
    _lock: Arc<File>,
    name: Name<'static>,
    events: Receiver<InstanceMessage>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Instance {
    pub fn claim(paths: &AppPaths, file: Option<PathBuf>) -> Result<InstanceClaim, String> {
        fs::create_dir_all(&paths.data_dir).map_err(|error| error.to_string())?;
        let root = fs::canonicalize(&paths.data_dir).map_err(|error| error.to_string())?;
        let name = socket_name(&root).map_err(|error| error.to_string())?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("instance.lock"))
            .map_err(|error| error.to_string())?;
        if let Err(error) = FileExt::try_lock_exclusive(&lock) {
            if error.raw_os_error() != fs2::lock_contended_error().raw_os_error()
                && error.kind() != io::ErrorKind::WouldBlock
            {
                return Err(format!("cannot lock Finch data directory: {error}"));
            }
            let message = file.map_or(InstanceMessage::Focus, InstanceMessage::Open);
            forward(&name, &message).map_err(|error| {
                format!("Finch is already running, but could not receive the request: {error}")
            })?;
            return Ok(InstanceClaim::Forwarded);
        }
        let listener = ListenerOptions::new()
            .name(name.clone())
            .try_overwrite(true)
            .create_sync()
            .map_err(|error| format!("cannot create Finch local IPC listener: {error}"))?;
        let (sender, events) = bounded(16);
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stopping);
        let lock = Arc::new(lock);
        let listener_lock = Arc::clone(&lock);
        let thread = thread::Builder::new()
            .name("finch-instance".into())
            .spawn(move || {
                let _listener_lock = listener_lock;
                loop {
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = listener.accept() else {
                        break;
                    };
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let result = stream.set_nonblocking(true).and_then(|()| {
                        let line = read_line(&mut stream, Duration::from_millis(500))?;
                        let message: InstanceMessage =
                            serde_json::from_slice(&line).map_err(io::Error::other)?;
                        sender
                            .try_send(message)
                            .map_err(|error| io::Error::other(error.to_string()))?;
                        Ok(())
                    });
                    let ack = match result {
                        Ok(()) => "ok\n".to_owned(),
                        Err(error) => format!("error: {error}\n"),
                    };
                    let _ = write_until(
                        &mut stream,
                        ack.as_bytes(),
                        Instant::now() + Duration::from_millis(250),
                    );
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(InstanceClaim::Primary(Self {
            _lock: lock,
            name,
            events,
            stopping,
            thread: Some(thread),
        }))
    }

    pub fn events(&self) -> Receiver<InstanceMessage> {
        self.events.clone()
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        // Wake a blocked accept. Client creation is bounded, including on Windows.
        let wake = ConnectOptions::new()
            .name(self.name.clone())
            .wait_mode(ConnectWaitMode::Timeout(Duration::from_millis(750)))
            .nonblocking_stream(true)
            .connect_sync();
        if wake.is_ok() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            drop(wake);
        }
    }
}

fn socket_name(root: &Path) -> io::Result<Name<'static>> {
    if GenericNamespaced::is_supported() {
        // Stable FNV-1a over the canonical native path; no process-random seed.
        let hash = root
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
        format!("finch-{hash:016x}")
            .to_ns_name::<GenericNamespaced>()
            .map(Name::into_owned)
    } else {
        root.join("instance.sock")
            .to_fs_name::<GenericFilePath>()
            .map(Name::into_owned)
    }
}

fn forward(name: &Name<'_>, message: &InstanceMessage) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut stream = loop {
        match ConnectOptions::new()
            .name(name.borrow())
            .wait_mode(ConnectWaitMode::Timeout(Duration::from_millis(100)))
            .nonblocking_stream(true)
            .connect_sync()
        {
            Ok(stream) => break stream,
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(15)),
        }
    };
    let mut encoded = serde_json::to_vec(message).map_err(io::Error::other)?;
    if encoded.len() > 64 * 1024 {
        return Err(io::Error::other("IPC request exceeds 64 KiB"));
    }
    encoded.push(b'\n');
    write_until(&mut stream, &encoded, deadline)?;
    let ack = read_line(
        &mut stream,
        deadline.saturating_duration_since(Instant::now()),
    )?;
    if ack == b"ok" {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "the running Finch instance rejected the request: {}",
            String::from_utf8_lossy(&ack)
        )))
    }
}

fn read_line(stream: &mut Stream, timeout: Duration) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::with_capacity(256);
    let mut buffer = [0_u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            // A Windows PIPE_NOWAIT byte pipe can report an empty read while
            // still connected. Bound this wait just like WouldBlock; on Unix a
            // premature EOF consequently also terminates within the deadline.
            Ok(0) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "IPC peer did not complete its message",
                    ));
                }
                thread::sleep(Duration::from_millis(2));
            }
            Ok(count) => {
                if let Some(end) = buffer[..count].iter().position(|byte| *byte == b'\n') {
                    bytes.extend_from_slice(&buffer[..end]);
                    if bytes.len() > 64 * 1024 {
                        return Err(io::Error::other("IPC request exceeds 64 KiB"));
                    }
                    return Ok(bytes);
                }
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.len() > 64 * 1024 {
                    return Err(io::Error::other("IPC request exceeds 64 KiB"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "IPC request timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn write_until(stream: &mut Stream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "IPC peer stopped receiving",
                ));
            }
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "IPC send timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_renderer_defaults_and_explicit_overrides() {
        assert_eq!(
            Config::parse_from(std::iter::empty::<&str>())
                .unwrap()
                .software,
            cfg!(target_os = "windows")
        );
        assert!(!Config::parse_from(["--gpu"]).unwrap().software);
        assert!(Config::parse_from(["--software"]).unwrap().software);
        assert!(
            Config::parse_from(["--gpu", "--software"])
                .unwrap()
                .software
        );
        assert!(
            !Config::parse_from(["--software", "--gpu"])
                .unwrap()
                .software
        );
    }

    #[test]
    fn cli_preserves_unicode_paths_and_checks_arguments() {
        let config = Config::parse_from([
            "--data-dir",
            "数据",
            "--software",
            "--metrics",
            "metrics.json",
            "--screenshot",
            "截图.png",
            "--quit-after-ms",
            "1500",
            "--smoke",
            "论文.pdf",
        ])
        .unwrap();
        assert!(config.file.unwrap().ends_with("论文.pdf"));
        assert!(config.data_dir.unwrap().is_absolute());
        assert!(config.software && config.smoke);
        assert_eq!(config.quit_after_ms, Some(1500));
        assert!(Config::parse_from(["--pdfium"]).is_err());
        assert!(Config::parse_from(["--quit-after-ms", "-1"]).is_err());
        assert!(Config::parse_from(["a.pdf", "b.pdf"]).is_err());
        assert!(Config::parse_from(["--unknown"]).is_err());
        assert!(
            Config::parse_from(["--", "-paper.pdf"])
                .unwrap()
                .file
                .unwrap()
                .ends_with("-paper.pdf")
        );
    }

    #[test]
    fn restore_guard_survives_drop_but_not_successful_clear() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = AppPaths::in_directory(temp.path());
        {
            let guard = RestoreGuard::new(&paths);
            assert!(!guard.was_interrupted());
            guard.mark().unwrap();
        }
        let next = RestoreGuard::new(&paths);
        assert!(next.was_interrupted());
        next.clear().unwrap();
        next.clear().unwrap();
        assert!(!next.was_interrupted());
    }

    #[test]
    fn local_instance_forwards_open_and_focus_then_releases_lock() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = AppPaths::in_directory(temp.path());
        let InstanceClaim::Primary(primary) = Instance::claim(&paths, None).unwrap() else {
            panic!("first instance must own lock")
        };
        let events = primary.events();
        let file = temp.path().join("论文.pdf");
        assert!(matches!(
            Instance::claim(&paths, Some(file.clone())).unwrap(),
            InstanceClaim::Forwarded
        ));
        assert_eq!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            InstanceMessage::Open(file)
        );
        assert!(matches!(
            Instance::claim(&paths, None).unwrap(),
            InstanceClaim::Forwarded
        ));
        assert_eq!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            InstanceMessage::Focus
        );
        drop(primary);
        assert!(matches!(
            Instance::claim(&paths, None).unwrap(),
            InstanceClaim::Primary(_)
        ));
    }

    #[test]
    fn pdfium_candidate_order_prefers_packaged_binary() {
        let candidates = pdfium_candidates(Path::new("package"), Path::new("development"));
        assert_eq!(candidates[0], Path::new("package").join(library_name()));
        assert!(
            candidates
                .iter()
                .any(|path| path.starts_with("development"))
        );
    }
}
