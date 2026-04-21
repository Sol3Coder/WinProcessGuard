use crate::models::{Config, MonitorItem, CONFIG_BACKUP_FILE_NAME, CONFIG_FILE_NAME};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub fn get_config_dir() -> PathBuf {
    let exe_path = env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = exe_path.parent().unwrap_or(std::path::Path::new("."));
    exe_dir.to_path_buf()
}

pub fn get_config_file_path() -> PathBuf {
    get_config_dir().join(CONFIG_FILE_NAME)
}

pub fn get_config_backup_file_path() -> PathBuf {
    get_config_dir().join(CONFIG_BACKUP_FILE_NAME)
}

#[cfg(test)]
fn get_config_backup_file_path_for_tests(base_dir: &Path) -> PathBuf {
    base_dir.join(CONFIG_BACKUP_FILE_NAME)
}

pub fn ensure_config_dir() -> io::Result<()> {
    let config_dir = get_config_dir();
    if !config_dir.exists() {
        fs::create_dir_all(&config_dir)?;
        info!("Created config directory: {:?}", config_dir);
    }
    Ok(())
}

pub fn load_config() -> Config {
    ensure_config_dir().ok();

    let config_path = get_config_file_path();
    let backup_path = get_config_backup_file_path();

    info!("Loading config from: {:?}", config_path);
    info!("Backup config path: {:?}", backup_path);

    load_config_from_paths(&config_path, &backup_path)
}

#[derive(Debug)]
enum ConfigLoadError {
    Missing,
    Empty,
    Read(io::Error),
    Parse(serde_json::Error),
}

fn load_config_from_paths(config_path: &Path, backup_path: &Path) -> Config {
    match read_config_file(config_path) {
        Ok(config) => {
            info!("Loaded primary config from: {:?}", config_path);
            return normalize_loaded_config(config, Some(config_path));
        }
        Err(err) => {
            warn!(
                "Primary config unavailable at {:?}: {}. Trying backup.",
                config_path,
                describe_load_error(&err)
            );
        }
    }

    match read_config_file(backup_path) {
        Ok(config) => {
            info!("Recovered config from backup: {:?}", backup_path);
            let config = normalize_loaded_config(config, None);

            if let Err(e) = save_config_to_path(config_path, &config) {
                error!(
                    "Failed to sync recovered config to {:?}: {}",
                    config_path, e
                );
            } else {
                info!("Recovered config synced to primary config: {:?}", config_path);
            }

            config
        }
        Err(err) => {
            warn!(
                "Backup config unavailable at {:?}: {}. Using default config.",
                backup_path,
                describe_load_error(&err)
            );
            Config::new()
        }
    }
}

fn read_config_file(path: &Path) -> Result<Config, ConfigLoadError> {
    match fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => Err(ConfigLoadError::Empty),
        Ok(content) => serde_json::from_str::<Config>(&content).map_err(ConfigLoadError::Parse),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(ConfigLoadError::Missing),
        Err(e) => Err(ConfigLoadError::Read(e)),
    }
}

fn describe_load_error(err: &ConfigLoadError) -> String {
    match err {
        ConfigLoadError::Missing => "file not found".to_string(),
        ConfigLoadError::Empty => "file is empty".to_string(),
        ConfigLoadError::Read(e) => format!("read failed: {}", e),
        ConfigLoadError::Parse(e) => format!("invalid json: {}", e),
    }
}

fn normalize_loaded_config(config: Config, save_path: Option<&Path>) -> Config {
    log_loaded_config(&config);
    deduplicate_exe_paths_with_target(config, save_path)
}

fn log_loaded_config(config: &Config) {
    info!("Loaded {} monitor items from config", config.items.len());
    for item in &config.items {
        info!(
            "  - [{}] {} ({})",
            if item.enabled { "enabled" } else { "disabled" },
            item.name,
            item.exe_path
        );
    }
}

fn deduplicate_exe_paths_with_target(mut config: Config, save_path: Option<&Path>) -> Config {
    let original_len = config.items.len();

    let mut seen_paths: HashMap<String, usize> = HashMap::new();
    let mut duplicates_found = false;

    for (index, item) in config.items.iter().enumerate() {
        let path_lower = item.exe_path.to_lowercase();
        if let Some(&prev_index) = seen_paths.get(&path_lower) {
            info!(
                "Duplicate exe_path found: {} (indices {} and {}), keeping the last one",
                item.exe_path, prev_index, index
            );
            duplicates_found = true;
        }
        seen_paths.insert(path_lower, index);
    }

    if duplicates_found {
        let mut path_to_last_item: HashMap<String, MonitorItem> = HashMap::new();

        for item in config.items.into_iter() {
            let path_lower = item.exe_path.to_lowercase();
            path_to_last_item.insert(path_lower, item);
        }

        let mut sorted_items: Vec<(String, MonitorItem)> = path_to_last_item.into_iter().collect();
        sorted_items.sort_by(|a, b| {
            let a_lower = a.0.to_lowercase();
            let b_lower = b.0.to_lowercase();
            a_lower.cmp(&b_lower)
        });

        config.items = sorted_items.into_iter().map(|(_, item)| item).collect();

        warn!(
            "Removed {} duplicate exe_path entries from config",
            original_len - config.items.len()
        );

        if let Some(path) = save_path {
            if let Err(e) = save_config_to_path(path, &config) {
                error!("Failed to save deduplicated config to {:?}: {}", path, e);
            } else {
                info!("Config file updated with deduplicated items: {:?}", path);
            }
        }
    }

    config
}

pub fn save_config(config: &Config) -> io::Result<()> {
    ensure_config_dir()?;

    let config_path = get_config_file_path();

    info!("Saving config to: {:?}", config_path);

    save_config_to_path(&config_path, config)?;

    info!("Config saved successfully ({} items)", config.items.len());

    debug!("Saved config: {:?}", config);
    Ok(())
}

fn save_config_to_path(path: &Path, config: &Config) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let content = serde_json::to_string_pretty(config)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    fs::write(path, content)
}

pub fn add_item(config: &mut Config, item: MonitorItem) -> io::Result<()> {
    info!("Adding new monitor item: {} ({})", item.name, item.id);

    if config.items.iter().any(|i| i.id == item.id) {
        error!("Item with id {} already exists", item.id);
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("Item with id {} already exists", item.id),
        ));
    }

    config.items.push(item);
    save_config(config)?;

    info!("Item added successfully");
    Ok(())
}

pub fn update_item(config: &mut Config, item: MonitorItem) -> io::Result<()> {
    info!("Updating monitor item: {} ({})", item.name, item.id);

    if let Some(existing) = config.items.iter_mut().find(|i| i.id == item.id) {
        *existing = item;
        save_config(config)?;
        info!("Item updated successfully");
        Ok(())
    } else {
        error!("Item with id {} not found", item.id);
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Item with id {} not found", item.id),
        ))
    }
}

pub fn remove_item(config: &mut Config, id: &str) -> io::Result<()> {
    info!("Removing monitor item: {}", id);

    let initial_len = config.items.len();
    config.items.retain(|i| i.id != id);

    if config.items.len() == initial_len {
        error!("Item with id {} not found", id);
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Item with id {} not found", id),
        ));
    }

    save_config(config)?;
    info!("Item removed successfully");
    Ok(())
}

pub fn get_item<'a>(config: &'a Config, id: &str) -> Option<&'a MonitorItem> {
    config.items.iter().find(|i| i.id == id)
}

pub fn get_item_mut<'a>(config: &'a mut Config, id: &str) -> Option<&'a mut MonitorItem> {
    config.items.iter_mut().find(|i| i.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct ConfigTestHarness {
        root: PathBuf,
        main: PathBuf,
        backup: PathBuf,
    }

    impl ConfigTestHarness {
        fn new() -> Self {
            let unique = format!(
                "pg-config-tests-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let root = std::env::temp_dir().join(unique);
            fs::create_dir_all(&root).unwrap();
            Self {
                main: root.join(CONFIG_FILE_NAME),
                backup: root.join("config_bak.json"),
                root,
            }
        }

        fn main_path(&self) -> &Path {
            &self.main
        }

        fn backup_path(&self) -> &Path {
            &self.backup
        }

        fn write_main(&self, content: &str) {
            fs::write(&self.main, content).unwrap();
        }

        fn write_backup(&self, content: &str) {
            fs::write(&self.backup, content).unwrap();
        }
    }

    impl Drop for ConfigTestHarness {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn backup_config_path_uses_config_bak_json_name() {
        let dir = PathBuf::from(r"F:\temp\pg-config-test");
        let path = get_config_backup_file_path_for_tests(&dir);
        assert!(path.ends_with("config_bak.json"));
    }

    #[test]
    fn missing_main_uses_backup_and_syncs_main() {
        let harness = ConfigTestHarness::new();
        harness.write_backup(valid_single_item_json());

        let config = load_config_from_paths(harness.main_path(), harness.backup_path());

        assert_eq!(config.items.len(), 1);
        assert!(harness.main_path().exists());
        let synced = fs::read_to_string(harness.main_path()).unwrap();
        assert!(synced.contains(r#""exe_path": "C:\\App.exe""#));
    }

    #[test]
    fn empty_main_uses_backup_and_syncs_main() {
        let harness = ConfigTestHarness::new();
        harness.write_main("   ");
        harness.write_backup(valid_single_item_json());

        let config = load_config_from_paths(harness.main_path(), harness.backup_path());

        assert_eq!(config.items.len(), 1);
        let synced = fs::read_to_string(harness.main_path()).unwrap();
        assert!(synced.contains(r#""name": "App""#));
    }

    #[test]
    fn invalid_main_uses_backup_and_syncs_main() {
        let harness = ConfigTestHarness::new();
        harness.write_main("{invalid-json");
        harness.write_backup(valid_single_item_json());

        let config = load_config_from_paths(harness.main_path(), harness.backup_path());

        assert_eq!(config.items.len(), 1);
        let synced = fs::read_to_string(harness.main_path()).unwrap();
        assert!(synced.contains(r#""items""#));
    }

    #[test]
    fn missing_main_and_missing_backup_returns_default_without_panic() {
        let harness = ConfigTestHarness::new();

        let config = load_config_from_paths(harness.main_path(), harness.backup_path());

        assert!(config.items.is_empty());
        assert!(!harness.main_path().exists());
    }

    #[test]
    fn missing_main_and_empty_backup_returns_default_without_panic() {
        let harness = ConfigTestHarness::new();
        harness.write_backup(" \n\t ");

        let config = load_config_from_paths(harness.main_path(), harness.backup_path());

        assert!(config.items.is_empty());
    }

    #[test]
    fn missing_main_and_invalid_backup_returns_default_without_panic() {
        let harness = ConfigTestHarness::new();
        harness.write_backup("{broken");

        let config = load_config_from_paths(harness.main_path(), harness.backup_path());

        assert!(config.items.is_empty());
    }

    #[test]
    fn missing_main_and_unreadable_backup_returns_default_without_panic() {
        let harness = ConfigTestHarness::new();
        fs::create_dir_all(harness.backup_path()).unwrap();

        let config = load_config_from_paths(harness.main_path(), harness.backup_path());

        assert!(config.items.is_empty());
    }

    #[test]
    fn backup_file_remains_unchanged_after_recovery() {
        let harness = ConfigTestHarness::new();
        let backup = valid_single_item_json();
        harness.write_backup(backup);

        let _ = load_config_from_paths(harness.main_path(), harness.backup_path());

        let backup_after = fs::read_to_string(harness.backup_path()).unwrap();
        assert_eq!(backup_after, backup);
    }

    #[test]
    fn duplicate_backup_entries_are_deduplicated_in_main_only() {
        let harness = ConfigTestHarness::new();
        let backup = r#"{
  "items": [
    {"id":"1","exe_path":"C:\\App.exe","args":null,"name":"App A","minimize":false,"no_window":false,"enabled":true,"heartbeat_timeout_ms":10000},
    {"id":"2","exe_path":"C:\\App.exe","args":null,"name":"App B","minimize":false,"no_window":false,"enabled":true,"heartbeat_timeout_ms":10000}
  ]
}"#;
        harness.write_backup(backup);

        let config = load_config_from_paths(harness.main_path(), harness.backup_path());

        assert_eq!(config.items.len(), 1);
        let main_after = fs::read_to_string(harness.main_path()).unwrap();
        let backup_after = fs::read_to_string(harness.backup_path()).unwrap();
        assert!(main_after.contains("App B"));
        assert_eq!(backup_after, backup);
    }

    fn valid_single_item_json() -> &'static str {
        r#"{"items":[{"id":"1","exe_path":"C:\\App.exe","args":null,"name":"App","minimize":false,"no_window":false,"enabled":true,"heartbeat_timeout_ms":10000}]}"#
    }
}
