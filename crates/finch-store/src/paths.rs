use crate::{Result, StoreError};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};

/// Platform-native locations; discovery itself performs no filesystem writes.
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub database: PathBuf,
    pub session: PathBuf,
    pub plugins: PathBuf,
    pub plugin_data: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("org", "Finch", "Finch").ok_or(StoreError::NoDataDirectory)?;
        let mut result = Self::in_directory(dirs.data_local_dir());
        result.cache_dir = dirs.cache_dir().to_path_buf();
        Ok(result)
    }

    /// An explicit root makes portable installations and tests deterministic.
    pub fn in_directory(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            data_dir: root.to_path_buf(),
            cache_dir: root.join("cache"),
            database: root.join("library.sqlite3"),
            session: root.join("session.json"),
            plugins: root.join("plugins"),
            plugin_data: root.join("plugin-data"),
        }
    }
}
