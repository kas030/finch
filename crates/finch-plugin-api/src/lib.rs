//! Version 1 wire types shared by Finch and independently built executable plugins.
//!
//! This crate contains data and constants only: no process spawning, filesystem
//! access, PDF engine, UI, or transport implementation. Commands travel as
//! newline-delimited JSON-RPC 2.0 over stdin/stdout. Plugin package versions are
//! independent of the protocol version negotiated at initialization.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Version negotiated by the manifest and the `initialize` handshake.
pub const API_VERSION: u32 = 1;
/// Manifest filename inside an installed plugin package.
pub const MANIFEST_FILENAME: &str = "plugin.json";
/// Maximum UTF-8 JSON-RPC message size, excluding the trailing newline.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginCommand {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    /// Platform keys: windows-x86_64, macos-aarch64, macos-x86_64, linux-x86_64, etc.
    pub executables: BTreeMap<String, String>,
    pub commands: Vec<PluginCommand>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginContext {
    pub document: Option<DocumentContext>,
    pub selected_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocumentContext {
    pub id: String,
    pub title: String,
    /// Zero based throughout the protocol.
    pub page_index: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginResult {
    pub text: String,
    #[serde(default)]
    pub navigate_to_page: Option<u32>,
}
