use finch_plugin::{PluginContext, PluginHost, current_platform};
use serde_json::json;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn packaged_example_runs_through_the_real_host_protocol() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("finch-example-test-{}-{nonce}", std::process::id()));
    let source = directory.join("source");
    fs::create_dir_all(&source).unwrap();
    let binary_name = format!("finch-text-stats{}", std::env::consts::EXE_SUFFIX);
    fs::copy(
        env!("CARGO_BIN_EXE_finch-text-stats"),
        source.join(&binary_name),
    )
    .unwrap();
    fs::write(
        source.join("plugin.json"),
        serde_json::to_vec(&json!({
            "id":"finch.text-stats", "name":"Text statistics", "version":"0.1.0", "api_version":1,
            "executables":{current_platform():binary_name},
            "commands":[{"id":"count-selection","title":"Count selected text"}],
        }))
        .unwrap(),
    )
    .unwrap();
    let mut host = PluginHost::new(directory.join("plugins"), directory.join("data"));
    host.install(&source).unwrap();
    host.set_enabled("finch.text-stats", true).unwrap();
    let result = host
        .invoke(
            "finch.text-stats",
            "count-selection",
            PluginContext {
                selected_text: "你好 Rust\nPDF".into(),
                ..PluginContext::default()
            },
        )
        .unwrap();
    assert!(result.text.contains("Characters: 11"));
    assert!(result.text.contains("Lines: 2"));
    assert_eq!(result.navigate_to_page, None);
    host.shutdown();
    let absolute = directory.canonicalize().unwrap();
    assert!(absolute.starts_with(std::env::temp_dir().canonicalize().unwrap()));
    assert!(
        absolute
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("finch-example-test-")
    );
    fs::remove_dir_all(absolute).unwrap();
}
