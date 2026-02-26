use crate::models::{Config, MonitorItem, CONFIG_FILE_NAME};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

pub fn get_config_dir() -> PathBuf {
    let exe_path = env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = exe_path.parent().unwrap_or(std::path::Path::new("."));
    exe_dir.to_path_buf()
}

pub fn get_config_file_path() -> PathBuf {
    get_config_dir().join(CONFIG_FILE_NAME)
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

    info!("Loading config from: {:?}", config_path);

    if !config_path.exists() {
        info!("Config file not found, creating default config");
        let default_config = Config::new();
        save_config(&default_config).ok();
        return default_config;
    }

    match fs::read_to_string(&config_path) {
        Ok(content) => {
            if content.trim().is_empty() {
                warn!("Config file is empty, using default config");
                return Config::new();
            }

            match serde_json::from_str::<Config>(&content) {
                Ok(config) => {
                    info!("Loaded {} monitor items from config", config.items.len());
                    for item in &config.items {
                        info!(
                            "  - [{}] {} ({})",
                            if item.enabled { "enabled" } else { "disabled" },
                            item.name,
                            item.exe_path
                        );
                    }

                    let config = deduplicate_exe_paths(config);
                    config
                }
                Err(e) => {
                    error!("Failed to parse config file: {}, using default config", e);
                    Config::new()
                }
            }
        }
        Err(e) => {
            error!("Failed to read config file: {}, using default config", e);
            Config::new()
        }
    }
}

fn deduplicate_exe_paths(mut config: Config) -> Config {
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
        let mut unique_items: Vec<MonitorItem> = Vec::new();
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

        unique_items = sorted_items.into_iter().map(|(_, item)| item).collect();

        config.items = unique_items;

        warn!(
            "Removed {} duplicate exe_path entries from config",
            original_len - config.items.len()
        );

        if let Err(e) = save_config(&config) {
            error!("Failed to save deduplicated config: {}", e);
        } else {
            info!("Config file updated with deduplicated items");
        }
    }

    config
}

pub fn save_config(config: &Config) -> io::Result<()> {
    ensure_config_dir()?;

    let config_path = get_config_file_path();

    info!("Saving config to: {:?}", config_path);

    let content = serde_json::to_string_pretty(config)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    fs::write(&config_path, content)?;

    info!("Config saved successfully ({} items)", config.items.len());

    debug!("Saved config: {:?}", config);
    Ok(())
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
