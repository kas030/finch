use finch_plugin_api::{
    API_VERSION, DocumentContext, MANIFEST_FILENAME, MAX_MESSAGE_BYTES, PluginContext,
    PluginManifest, PluginResult,
};
use serde_json::json;

#[test]
fn context_preserves_the_v1_wire_contract() {
    let context = PluginContext {
        document: Some(DocumentContext {
            id: "paper-1".into(),
            title: "论文".into(),
            page_index: 4,
        }),
        selected_text: "你好 Rust\nPDF".into(),
    };
    let wire = json!({
        "document": {"id":"paper-1", "title":"论文", "page_index":4},
        "selected_text":"你好 Rust\nPDF"
    });
    assert_eq!(serde_json::to_value(&context).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<PluginContext>(wire).unwrap(),
        context
    );
    assert_eq!(
        serde_json::to_value(PluginContext::default()).unwrap(),
        json!({"document":null, "selected_text":""})
    );
}

#[test]
fn v1_result_keeps_optional_navigation_and_rejects_invalid_commands() {
    let result: PluginResult = serde_json::from_value(json!({"text":"结果"})).unwrap();
    assert_eq!(result.navigate_to_page, None);
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        json!({"text":"结果", "navigate_to_page":null})
    );
    for invalid in [
        json!({"text":"", "navigate_to_page":-1}),
        json!({"text":"", "navigate_to_page":4294967296_u64}),
        json!({"text":"", "execute":"unexpected"}),
    ] {
        assert!(serde_json::from_value::<PluginResult>(invalid).is_err());
    }
}

#[test]
fn v1_manifest_and_protocol_constants_are_unchanged() {
    let mut wire = json!({
        "id":"finch.text-stats", "name":"Text statistics", "version":"0.1.0",
        "api_version":1, "executables":{"windows-x86_64":"finch-text-stats.exe"},
        "commands":[{"id":"count-selection", "title":"Count selected text"}]
    });
    let manifest: PluginManifest = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(API_VERSION, 1);
    assert_eq!(MANIFEST_FILENAME, "plugin.json");
    assert_eq!(MAX_MESSAGE_BYTES, 1_048_576);
    assert_eq!(manifest.api_version, API_VERSION);
    assert_eq!(serde_json::to_value(manifest).unwrap(), wire);
    wire["unsupported"] = json!(true);
    assert!(serde_json::from_value::<PluginManifest>(wire).is_err());
}
