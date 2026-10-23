use finch_plugin::{HostOptions, PluginContext, PluginError, PluginHost, current_platform};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
static FIXTURE: OnceLock<PathBuf> = OnceLock::new();

fn fixture() -> &'static Path {
    FIXTURE.get_or_init(|| {
        let directory =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-test-fixture");
        fs::create_dir_all(&directory).unwrap();
        let output = directory.join(format!(
            "fixture-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX
        ));
        let mut compiler =
            Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
        #[cfg(all(windows, target_env = "gnu"))]
        compiler.arg("-Clink-self-contained=yes");
        let result = compiler
            .args(["--edition=2024", "-O"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/child.rs"))
            .arg("-o")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "fixture compile failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        output
    })
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "finch-plugin-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn package(&self, executable_name: &str) -> PathBuf {
        let package = self.0.join("source");
        fs::create_dir(&package).unwrap();
        let executable_name = format!("{executable_name}{}", std::env::consts::EXE_SUFFIX);
        fs::copy(fixture(), package.join(&executable_name)).unwrap();
        let commands = [
            "ok",
            "hang",
            "crash",
            "malformed",
            "oversized",
            "mismatch",
            "remote-error",
            "stderr",
        ]
        .iter()
        .map(|id| json!({"id":id,"title":id}))
        .collect::<Vec<_>>();
        fs::write(
            package.join("plugin.json"),
            serde_json::to_vec(&json!({
                "id":"test.plugin", "name":"Test plugin", "version":"0.1.0", "api_version":1,
                "executables":{current_platform():executable_name}, "commands":commands,
            }))
            .unwrap(),
        )
        .unwrap();
        package
    }
    fn host(&self) -> PluginHost {
        PluginHost::with_options(
            self.0.join("installed"),
            self.0.join("data"),
            HostOptions {
                startup_timeout: Duration::from_secs(2),
                command_timeout: Duration::from_millis(300),
                idle_timeout: Duration::from_millis(150),
            },
        )
    }
    fn enabled(&self, executable: &str) -> PluginHost {
        let mut host = self.host();
        host.install(&self.package(executable)).unwrap();
        host.set_enabled("test.plugin", true).unwrap();
        host
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        if let (Ok(path), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize())
            && path.starts_with(temp)
            && path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("finch-plugin-test-")
        {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[test]
fn new_host_is_lazy_install_requires_trust_and_discovery_survives_bad_packages() {
    let directory = Directory::new();
    let mut host = directory.host();
    assert!(!directory.0.join("installed").exists());
    assert!(host.discover().unwrap().is_empty());
    let source = directory.package("fixture");
    fs::write(
        source.join(".enabled"),
        "untrusted package cannot grant trust",
    )
    .unwrap();
    let info = host.install(&source.join("plugin.json")).unwrap();
    assert!(!info.enabled);
    assert!(info.available);
    assert!(!directory.0.join("data").exists());
    assert!(matches!(
        host.invoke("test.plugin", "ok", PluginContext::default()),
        Err(PluginError::Disabled(_))
    ));
    assert!(host.install(&source).is_err());
    fs::create_dir(directory.0.join("installed/broken")).unwrap();
    fs::write(directory.0.join("installed/broken/plugin.json"), "{").unwrap();
    assert_eq!(host.discover().unwrap().len(), 1);
    assert_eq!(host.discovery_errors().len(), 1);
}

#[test]
fn actual_child_returns_result_and_restarts_after_idle_or_disable() {
    let directory = Directory::new();
    let mut host = directory.enabled("fixture");
    let first = host
        .invoke("test.plugin", "ok", PluginContext::default())
        .unwrap();
    assert_eq!(first.navigate_to_page, Some(3));
    assert_eq!(
        first.text,
        host.invoke("test.plugin", "ok", PluginContext::default())
            .unwrap()
            .text
    );
    std::thread::sleep(Duration::from_millis(350));
    let second = host
        .invoke("test.plugin", "ok", PluginContext::default())
        .unwrap();
    assert_ne!(first.text, second.text);
    host.set_enabled("test.plugin", false).unwrap();
    assert!(matches!(
        host.invoke("test.plugin", "ok", PluginContext::default()),
        Err(PluginError::Disabled(_))
    ));
    host.set_enabled("test.plugin", true).unwrap();
    assert_ne!(
        second.text,
        host.invoke("test.plugin", "ok", PluginContext::default())
            .unwrap()
            .text
    );
}

#[test]
fn hung_crashed_and_invalid_children_fail_without_poisoning_next_invocation() {
    let directory = Directory::new();
    let mut host = directory.enabled("fixture");
    for command in [
        "hang",
        "crash",
        "malformed",
        "oversized",
        "mismatch",
        "remote-error",
    ] {
        let start = Instant::now();
        let result = host.invoke("test.plugin", command, PluginContext::default());
        assert!(result.is_err(), "command {command} unexpectedly succeeded");
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(
            host.invoke("test.plugin", "ok", PluginContext::default())
                .is_ok()
        );
    }
}

#[test]
fn stderr_flood_is_drained_and_drop_cleans_up_promptly() {
    let directory = Directory::new();
    let mut host = directory.enabled("fixture");
    assert!(
        host.invoke("test.plugin", "stderr", PluginContext::default())
            .is_ok()
    );
    let start = Instant::now();
    drop(host);
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn incompatible_handshake_and_blocked_stdin_are_bounded() {
    for executable in ["bad-version", "blocked-writer", "no-read"] {
        let directory = Directory::new();
        let mut host = directory.enabled(executable);
        let start = Instant::now();
        let context = PluginContext {
            selected_text: "x".repeat(900_000),
            ..PluginContext::default()
        };
        assert!(host.invoke("test.plugin", "ok", context).is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}

#[test]
fn rejects_path_traversal_and_unknown_commands_before_execution() {
    let directory = Directory::new();
    let mut host = directory.enabled("fixture");
    assert!(matches!(
        host.invoke("../test.plugin", "ok", PluginContext::default()),
        Err(PluginError::Manifest(_))
    ));
    assert!(matches!(
        host.invoke("test.plugin", "missing", PluginContext::default()),
        Err(PluginError::UnknownCommand(_))
    ));
    let manifest_path = directory.0.join("installed/test.plugin/plugin.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["executables"][current_platform()] = "../outside.exe".into();
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(matches!(
        host.invoke("test.plugin", "ok", PluginContext::default()),
        Err(PluginError::Manifest(_))
    ));
}
