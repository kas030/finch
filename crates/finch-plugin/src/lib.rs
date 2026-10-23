//! A local, trusted executable plugin host. This is fault isolation, not a sandbox.
//!
//! All host methods are synchronous and must run on the application's blocking worker.
//! Discovery never starts plugins. Child workers exist only after explicit invocation.

mod process;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

pub use finch_plugin_api::{
    API_VERSION, DocumentContext, MANIFEST_FILENAME, MAX_MESSAGE_BYTES, PluginCommand,
    PluginContext, PluginManifest, PluginResult,
};

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;

pub type Result<T> = std::result::Result<T, PluginError>;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("Plugin I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid plugin JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid plugin manifest: {0}")]
    Manifest(String),
    #[error("Plugin {0} is not installed")]
    NotFound(String),
    #[error("Plugin {0} is disabled; enable only plugins you trust")]
    Disabled(String),
    #[error("Plugin {0} has no executable for this platform")]
    UnsupportedPlatform(String),
    #[error("Unknown command: {0}")]
    UnknownCommand(String),
    #[error("Plugin protocol error: {0}")]
    Protocol(String),
    #[error("Plugin {0} timed out")]
    Timeout(String),
    #[error("Plugin process stopped: {0}")]
    Exited(String),
    #[error("Plugin reported error {code}: {message}")]
    Remote { code: i64, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginInfo {
    pub manifest: PluginManifest,
    pub enabled: bool,
    pub available: bool,
}

#[derive(Debug, Clone)]
pub struct HostOptions {
    pub startup_timeout: Duration,
    pub command_timeout: Duration,
    pub idle_timeout: Duration,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(60),
        }
    }
}

pub struct PluginHost {
    plugins_dir: PathBuf,
    data_dir: PathBuf,
    options: HostOptions,
    workers: HashMap<String, process::Worker>,
    discovery_errors: Vec<String>,
}

impl PluginHost {
    /// Does not touch disk or start workers until a method is called.
    pub fn new(plugins_dir: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self::with_options(plugins_dir, data_dir, HostOptions::default())
    }

    pub fn with_options(
        plugins_dir: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        options: HostOptions,
    ) -> Self {
        Self {
            plugins_dir: plugins_dir.into(),
            data_dir: data_dir.into(),
            options,
            workers: HashMap::new(),
            discovery_errors: Vec::new(),
        }
    }

    /// Call when opening the plugin interface, never on the critical startup path.
    /// A malformed package does not hide healthy packages; inspect discovery_errors().
    pub fn discover(&mut self) -> Result<Vec<PluginInfo>> {
        self.discovery_errors.clear();
        let entries = match fs::read_dir(&self.plugins_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut plugins = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_dir() || !path.join(MANIFEST_FILENAME).is_file() {
                continue;
            }
            match self.inspect_package(&path) {
                Ok(info) => plugins.push(info),
                Err(error) => self
                    .discovery_errors
                    .push(format!("{}: {error}", path.display())),
            }
        }
        plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        Ok(plugins)
    }

    pub fn discovery_errors(&self) -> &[String] {
        &self.discovery_errors
    }

    /// Copies a manually selected package into the application plugin directory.
    /// New packages are disabled. Existing IDs are not silently overwritten.
    pub fn install(&mut self, source: &Path) -> Result<PluginInfo> {
        let source = if source.is_file() {
            source
                .parent()
                .ok_or_else(|| PluginError::Manifest("missing package directory".into()))?
        } else {
            source
        };
        let source = source.canonicalize()?;
        let manifest = read_manifest(&source.join(MANIFEST_FILENAME))?;
        // Check all packaged entry points, not just the current platform.
        for executable in manifest.executables.values() {
            package_executable(&source, executable)?;
        }
        fs::create_dir_all(&self.plugins_dir)?;
        if self.plugins_dir.canonicalize()?.starts_with(&source) {
            return Err(PluginError::Manifest(
                "installation directory cannot be inside the selected package".into(),
            ));
        }
        let destination = self.plugins_dir.join(&manifest.id);
        if destination.exists() {
            return Err(PluginError::Manifest(format!(
                "{} is already installed",
                manifest.id
            )));
        }
        let staging = self.plugins_dir.join(format!(".install-{}", manifest.id));
        // Never reuse a partially copied package or replace arbitrary contents.
        if staging.exists() {
            return Err(PluginError::Manifest(format!(
                "unfinished installation exists: {}",
                staging.display()
            )));
        }
        fs::create_dir(&staging)?;
        let result = copy_package(&source, &staging, &mut 0);
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        // A source package must not be able to opt itself into trusted execution.
        let marker = staging.join(".enabled");
        if marker.exists() {
            fs::remove_file(marker)?;
        }
        if let Err(error) = fs::rename(&staging, &destination) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error.into());
        }
        self.inspect_package(&destination)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        self.load(id)?;
        let marker = self.plugins_dir.join(id).join(".enabled");
        if enabled {
            fs::write(marker, b"This executable plugin was enabled by the user.\n")?;
        } else {
            if let Some(mut worker) = self.workers.remove(id) {
                worker.stop();
            }
            match fs::remove_file(marker) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Blocks this caller, never the GUI. Protocol failures discard the process;
    /// a subsequent explicit invocation starts a clean one.
    pub fn invoke(
        &mut self,
        plugin_id: &str,
        command_id: &str,
        context: PluginContext,
    ) -> Result<PluginResult> {
        let info = self.load(plugin_id)?;
        if !info.enabled {
            return Err(PluginError::Disabled(plugin_id.into()));
        }
        if !info.manifest.commands.iter().any(|c| c.id == command_id) {
            return Err(PluginError::UnknownCommand(command_id.into()));
        }
        if self
            .workers
            .get(plugin_id)
            .is_some_and(process::Worker::is_finished)
        {
            self.workers.remove(plugin_id);
        }
        self.ensure_worker(plugin_id, &info)?;
        let (sender, receiver) = mpsc::channel();
        let send_result = self.workers[plugin_id].sender.send(process::Invocation {
            command_id: command_id.into(),
            context,
            reply: sender,
        });
        if let Err(unsent) = send_result {
            // Idle expiry may race the is_finished check. Retry only an invocation
            // that was never accepted, never a command that may have executed.
            self.workers.remove(plugin_id);
            self.ensure_worker(plugin_id, &info)?;
            self.workers[plugin_id]
                .sender
                .send(unsent.0)
                .map_err(|_| PluginError::Exited(plugin_id.into()))?;
        }
        let result = receiver
            .recv()
            .unwrap_or_else(|_| Err(PluginError::Exited(plugin_id.into())));
        if result.is_err()
            && let Some(mut worker) = self.workers.remove(plugin_id)
        {
            worker.stop();
        }
        result
    }

    pub fn shutdown(&mut self) {
        for (_, mut worker) in self.workers.drain() {
            worker.stop();
        }
    }

    fn ensure_worker(&mut self, id: &str, info: &PluginInfo) -> Result<()> {
        if self.workers.contains_key(id) {
            return Ok(());
        }
        let relative = info
            .manifest
            .executables
            .get(&current_platform())
            .ok_or_else(|| PluginError::UnsupportedPlatform(id.into()))?;
        let package = self.plugins_dir.join(id).canonicalize()?;
        let executable = package_executable(&package, relative)?;
        let storage = self.data_dir.join(id);
        fs::create_dir_all(&storage)?;
        let worker = process::Worker::start(
            executable,
            package,
            storage.canonicalize()?,
            self.options.clone(),
        );
        self.workers.insert(id.into(), worker);
        Ok(())
    }

    fn load(&self, id: &str) -> Result<PluginInfo> {
        validate_id(id)?;
        let path = self.plugins_dir.join(id);
        if !path.is_dir() {
            return Err(PluginError::NotFound(id.into()));
        }
        self.inspect_package(&path)
    }

    fn inspect_package(&self, path: &Path) -> Result<PluginInfo> {
        let manifest = read_manifest(&path.join(MANIFEST_FILENAME))?;
        if path.file_name().and_then(|s| s.to_str()) != Some(&manifest.id) {
            return Err(PluginError::Manifest(
                "installed directory must match plugin id".into(),
            ));
        }
        let available = manifest
            .executables
            .get(&current_platform())
            .is_some_and(|entry| package_executable(path, entry).is_ok());
        Ok(PluginInfo {
            manifest,
            enabled: path.join(".enabled").is_file(),
            available,
        })
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn current_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn read_manifest(path: &Path) -> Result<PluginManifest> {
    if fs::metadata(path)?.len() > MAX_MANIFEST_BYTES {
        return Err(PluginError::Manifest("manifest exceeds 64 KiB".into()));
    }
    let manifest: PluginManifest = serde_json::from_slice(&fs::read(path)?)?;
    validate_id(&manifest.id)?;
    if manifest.api_version != API_VERSION {
        return Err(PluginError::Manifest(format!(
            "unsupported API version {}",
            manifest.api_version
        )));
    }
    if manifest.name.trim().is_empty()
        || manifest.version.trim().is_empty()
        || manifest.commands.is_empty()
    {
        return Err(PluginError::Manifest(
            "name, version, and commands are required".into(),
        ));
    }
    if manifest.executables.is_empty() {
        return Err(PluginError::Manifest(
            "at least one platform executable is required".into(),
        ));
    }
    let mut ids = std::collections::HashSet::new();
    for command in &manifest.commands {
        validate_id(&command.id)?;
        if command.title.trim().is_empty() || !ids.insert(&command.id) {
            return Err(PluginError::Manifest(
                "command titles must be present and ids unique".into(),
            ));
        }
    }
    for path in manifest.executables.values() {
        validate_relative(path)?;
    }
    Ok(manifest)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 100
        || !id.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.' || c == b'_'
        })
        || id.starts_with('.')
        || id.ends_with('.')
        || id.contains("..")
    {
        return Err(PluginError::Manifest(format!("invalid identifier: {id}")));
    }
    // Windows device names are not portable package directory names.
    let stem = id.split('.').next().unwrap_or(id).to_ascii_uppercase();
    if [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ]
    .contains(&stem.as_str())
    {
        return Err(PluginError::Manifest("reserved platform identifier".into()));
    }
    Ok(())
}

fn validate_relative(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\\')
        || value.contains(':')
        || !path.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(PluginError::Manifest(
            "executable must be a relative path inside its package, using / separators".into(),
        ));
    }
    Ok(())
}

fn package_executable(package: &Path, relative: &str) -> Result<PathBuf> {
    validate_relative(relative)?;
    let package = package.canonicalize()?;
    let path = package.join(relative).canonicalize()?;
    if !path.starts_with(&package) || !path.is_file() {
        return Err(PluginError::Manifest(
            "executable escapes package or is not a file".into(),
        ));
    }
    Ok(path)
}

fn copy_package(source: &Path, destination: &Path, bytes: &mut u64) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if entry.file_name() == ".enabled" {
            continue;
        }
        if kind.is_symlink() {
            return Err(PluginError::Manifest(
                "package symlinks are unsupported".into(),
            ));
        }
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            fs::create_dir(&target)?;
            copy_package(&entry.path(), &target, bytes)?;
        } else if kind.is_file() {
            *bytes = bytes.saturating_add(entry.metadata()?.len());
            if *bytes > MAX_PACKAGE_BYTES {
                return Err(PluginError::Manifest("package exceeds 256 MiB".into()));
            }
            fs::copy(entry.path(), target)?;
        } else {
            return Err(PluginError::Manifest(
                "package contains a special file".into(),
            ));
        }
    }
    Ok(())
}
