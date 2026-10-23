use finch_plugin_api::{API_VERSION, MAX_MESSAGE_BYTES, PluginContext, PluginResult};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut initialized = false;
    for line in stdin.lock().lines() {
        let line = line?;
        if line.len() > MAX_MESSAGE_BYTES {
            return Err("message too large".into());
        }
        let request: Value = serde_json::from_str(&line)?;
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let response = if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            failure(id, -32600, "Expected JSON-RPC 2.0")
        } else {
            match request.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    if request["params"]["api_version"].as_u64() == Some(u64::from(API_VERSION)) {
                        initialized = true;
                        json!({"jsonrpc":"2.0","id":id,"result":{"api_version":API_VERSION}})
                    } else {
                        failure(id, -32602, "Unsupported Finch API version")
                    }
                }
                Some("command.execute") if initialized => {
                    if request["params"]["command"].as_str() != Some("count-selection") {
                        failure(id, -32602, "Unknown command")
                    } else {
                        match serde_json::from_value::<PluginContext>(
                            request["params"]["context"].clone(),
                        ) {
                            Ok(context) => {
                                json!({"jsonrpc":"2.0","id":id,"result":count_selection(&context.selected_text)})
                            }
                            Err(_) => failure(id, -32602, "Invalid command context"),
                        }
                    }
                }
                Some("command.execute") => failure(id, -32000, "Initialize the plugin first"),
                _ => failure(id, -32601, "Unknown method"),
            }
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

fn failure(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn count_selection(text: &str) -> PluginResult {
    let result = if text.is_empty() {
        "请先在 PDF 中选择一段文字。\nSelect text in the PDF to count it.".to_owned()
    } else {
        format!(
            "选中文本统计 · Selected text\n\n字符 / Characters: {}\n非空白字符 / Non-whitespace: {}\n空白分词 / Whitespace-separated words: {}\n行 / Lines: {}",
            text.chars().count(),
            text.chars().filter(|c| !c.is_whitespace()).count(),
            text.split_whitespace().count(),
            text.lines().count()
        )
    };
    PluginResult {
        text: result,
        navigate_to_page: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_unicode_scalars_without_counting_utf8_bytes() {
        let result = count_selection("你好 Rust\nPDF");
        assert!(result.text.contains("Characters: 11"));
        assert!(result.text.contains("Non-whitespace: 9"));
        assert!(result.text.contains("words: 3"));
        assert!(result.text.contains("Lines: 2"));
    }
    #[test]
    fn empty_selection_has_a_useful_message() {
        assert!(count_selection("").text.contains("Select text"));
    }
}
