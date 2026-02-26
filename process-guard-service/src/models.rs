use serde::{Deserialize, Serialize};
use std::ops::BitOr;
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorItem {
    pub id: String,
    pub exe_path: String,
    pub args: Option<String>,
    pub name: String,
    pub minimize: bool,
    pub no_window: bool,
    pub enabled: bool,
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout_ms: u64,
}

fn default_heartbeat_timeout() -> u64 {
    1000
}

impl MonitorItem {
    pub fn new(exe_path: String, name: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            exe_path,
            args: None,
            name,
            minimize: false,
            no_window: false,
            enabled: true,
            heartbeat_timeout_ms: 1000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MonitoredProcess {
    pub item: MonitorItem,
    pub process_id: Option<u32>,
    pub last_heartbeat: Instant,
    pub last_check: Instant,
    pub restart_count: u32,
}

impl MonitoredProcess {
    pub fn from_item(item: MonitorItem) -> Self {
        Self {
            item,
            process_id: None,
            last_heartbeat: Instant::now(),
            last_check: Instant::now(),
            restart_count: 0,
        }
    }

    pub fn is_heartbeat_timeout(&self) -> bool {
        let timeout = std::time::Duration::from_millis(self.item.heartbeat_timeout_ms);
        self.last_heartbeat.elapsed() > timeout
    }

    pub fn update_heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub items: Vec<MonitorItem>,
}

impl Config {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipeRequest {
    #[serde(rename = "type")]
    pub request_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<MonitorItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipeResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl PipeResponse {
    pub fn success(message: &str) -> Self {
        Self {
            success: true,
            message: Some(message.to_string()),
            data: None,
        }
    }

    pub fn success_with_data(message: &str, data: serde_json::Value) -> Self {
        Self {
            success: true,
            message: Some(message.to_string()),
            data: Some(data),
        }
    }

    pub fn error(message: &str) -> Self {
        Self {
            success: false,
            message: Some(message.to_string()),
            data: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeType {
    None = 0,
    Start = 1,
    Stop = 2,
    Remove = 4,
}

impl BitOr for ChangeType {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        unsafe { std::mem::transmute((self as u8) | (rhs as u8)) }
    }
}

impl ChangeType {
    pub fn has_flag(&self, other: ChangeType) -> bool {
        (*self as u8) & (other as u8) != 0
    }
}

#[derive(Debug, Clone)]
pub struct ConfigChange {
    pub item: MonitorItem,
    pub change_type: ChangeType,
}

pub const SERVICE_NAME: &str = "ProcessGuardService";
pub const PIPE_NAME: &str = "ProcessGuardService";
pub const CONFIG_FILE_NAME: &str = "config.json";
pub const CHECK_INTERVAL_MS: u64 = 3000;
pub const DEFAULT_HEARTBEAT_TIMEOUT_MS: u64 = 1000;
