use crate::guardian::Guardian;
use crate::models::{ChangeType, ConfigChange, PipeRequest, PipeResponse, PIPE_NAME};
use log::{debug, error, info};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::Arc;
use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};

const BUFFER_SIZE: u32 = 65536;
const MAX_INSTANCES: u32 = 10;
const TIMEOUT_MS: u32 = 0;
const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;

fn to_wide_string(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub struct PipeServer {
    guardian: Arc<Guardian>,
    running: Arc<std::sync::Mutex<bool>>,
}

impl PipeServer {
    pub fn new(guardian: Arc<Guardian>, running: Arc<std::sync::Mutex<bool>>) -> Self {
        Self { guardian, running }
    }

    pub fn run(&self) {
        let pipe_name = format!("\\\\.\\pipe\\{}", PIPE_NAME);
        let pipe_name_wide = to_wide_string(&pipe_name);

        info!("Starting pipe server: {}", pipe_name);

        loop {
            let running = *self.running.lock().unwrap();
            if !running {
                info!("Pipe server stopping");
                break;
            }

            let pipe_handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(pipe_name_wide.as_ptr()),
                    FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE.0),
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    MAX_INSTANCES,
                    BUFFER_SIZE,
                    BUFFER_SIZE,
                    TIMEOUT_MS,
                    None,
                )
            };

            if pipe_handle.is_invalid() {
                error!("Failed to create named pipe");
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }

            debug!("Waiting for client connection...");

            let connect_result = unsafe { ConnectNamedPipe(pipe_handle, None) };

            if connect_result.is_err() {
                let err = windows::core::Error::from_win32();
                error!("ConnectNamedPipe failed: {:?}", err);
                unsafe {
                    let _ = CloseHandle(pipe_handle);
                }
                continue;
            }

            info!("Client connected to pipe server");

            let mut buffer = vec![0u8; BUFFER_SIZE as usize];
            let mut bytes_read: u32 = 0;

            let read_result =
                unsafe { ReadFile(pipe_handle, Some(&mut buffer), Some(&mut bytes_read), None) };

            if read_result.is_err() || bytes_read == 0 {
                debug!("Failed to read from pipe or empty request");
                unsafe {
                    let _ = DisconnectNamedPipe(pipe_handle);
                    let _ = CloseHandle(pipe_handle);
                }
                continue;
            }

            let request_data = String::from_utf8_lossy(&buffer[..bytes_read as usize]);
            info!("Received request: {}", request_data);

            let response = self.handle_request(&request_data);
            let response_data = serde_json::to_string(&response).unwrap_or_default();

            let mut bytes_written: u32 = 0;
            let write_result = unsafe {
                WriteFile(
                    pipe_handle,
                    Some(response_data.as_bytes()),
                    Some(&mut bytes_written),
                    None,
                )
            };

            if write_result.is_err() {
                error!("Failed to write response to pipe");
            } else {
                debug!("Response sent: {}", response_data);
            }

            unsafe {
                let _ = DisconnectNamedPipe(pipe_handle);
                let _ = CloseHandle(pipe_handle);
            }

            info!("Client disconnected from pipe server");
        }

        info!("Pipe server stopped");
    }

    fn handle_request(&self, request_data: &str) -> PipeResponse {
        let request: PipeRequest = match serde_json::from_str(request_data) {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to parse request: {}", e);
                return PipeResponse::error(&format!("Invalid JSON: {}", e));
            }
        };

        info!("Handling request type: {}", request.request_type);

        match request.request_type.as_str() {
            "heartbeat" => self.handle_heartbeat(&request),
            "add" => self.handle_add(&request),
            "update" => self.handle_update(&request),
            "remove" => self.handle_remove(&request),
            "stop" => self.handle_stop(&request),
            "start" => self.handle_start(&request),
            "list" => self.handle_list(),
            "status" => self.handle_status(),
            _ => PipeResponse::error(&format!("Unknown request type: {}", request.request_type)),
        }
    }

    fn handle_heartbeat(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(item_id) = &request.item_id {
            if self.guardian.update_heartbeat(item_id) {
                debug!("Heartbeat updated for item: {}", item_id);
                PipeResponse::success("Heartbeat updated")
            } else {
                error!("Heartbeat update failed, item not found: {}", item_id);
                PipeResponse::error("Item not found")
            }
        } else {
            PipeResponse::error("Missing item_id")
        }
    }

    fn handle_add(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(config) = &request.config {
            info!("Adding monitor item: {} ({})", config.name, config.exe_path);

            let config_arc = self.guardian.get_config();
            let mut cfg = config_arc.lock().unwrap();

            if cfg.items.iter().any(|i| i.id == config.id) {
                error!("Item with ID {} already exists", config.id);
                return PipeResponse::error("Item with this ID already exists");
            }

            if cfg
                .items
                .iter()
                .any(|i| i.exe_path.to_lowercase() == config.exe_path.to_lowercase())
            {
                error!("Executable path already monitored: {}", config.exe_path);
                return PipeResponse::error("Executable path already monitored");
            }

            cfg.items.push(config.clone());

            if let Err(e) = crate::config::save_config(&cfg) {
                error!("Failed to save config: {}", e);
                return PipeResponse::error(&format!("Failed to save config: {}", e));
            }

            drop(cfg);

            let change = ConfigChange {
                item: config.clone(),
                change_type: ChangeType::Start,
            };
            self.guardian.add_change(change);

            info!(
                "Monitor item added successfully: {} ({})",
                config.name, config.id
            );
            PipeResponse::success("Item added")
        } else {
            PipeResponse::error("Missing config")
        }
    }

    fn handle_update(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(config) = &request.config {
            info!("Updating monitor item: {} ({})", config.name, config.id);

            let config_arc = self.guardian.get_config();
            let mut cfg = config_arc.lock().unwrap();

            if let Some(existing) = cfg.items.iter_mut().find(|i| i.id == config.id) {
                *existing = config.clone();

                if let Err(e) = crate::config::save_config(&cfg) {
                    error!("Failed to save config: {}", e);
                    return PipeResponse::error(&format!("Failed to save config: {}", e));
                }

                drop(cfg);

                let change = ConfigChange {
                    item: config.clone(),
                    change_type: ChangeType::Stop | ChangeType::Start,
                };
                self.guardian.add_change(change);

                info!(
                    "Monitor item updated successfully: {} ({})",
                    config.name, config.id
                );
                PipeResponse::success("Item updated")
            } else {
                error!("Item not found for update: {}", config.id);
                PipeResponse::error("Item not found")
            }
        } else {
            PipeResponse::error("Missing config")
        }
    }

    fn handle_remove(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(id) = &request.id {
            info!("Removing monitor item: {}", id);

            let config_arc = self.guardian.get_config();
            let mut cfg = config_arc.lock().unwrap();

            let item = cfg.items.iter().find(|i| &i.id == id).cloned();

            if let Some(item) = item {
                cfg.items.retain(|i| &i.id != id);

                if let Err(e) = crate::config::save_config(&cfg) {
                    error!("Failed to save config: {}", e);
                    return PipeResponse::error(&format!("Failed to save config: {}", e));
                }

                drop(cfg);

                let change = ConfigChange {
                    item,
                    change_type: ChangeType::Stop | ChangeType::Remove,
                };
                self.guardian.add_change(change);

                info!("Monitor item removed successfully: {}", id);
                PipeResponse::success("Item removed")
            } else {
                error!("Item not found for removal: {}", id);
                PipeResponse::error("Item not found")
            }
        } else {
            PipeResponse::error("Missing id")
        }
    }

    fn handle_stop(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(id) = &request.id {
            info!("Stopping monitor item: {}", id);

            let config_arc = self.guardian.get_config();
            let cfg = config_arc.lock().unwrap();
            let item = cfg.items.iter().find(|i| &i.id == id).cloned();
            drop(cfg);

            if let Some(item) = item {
                let change = ConfigChange {
                    item,
                    change_type: ChangeType::Stop,
                };
                self.guardian.add_change(change);

                info!("Monitor item stop command sent: {}", id);
                PipeResponse::success("Item stopped")
            } else {
                error!("Item not found for stop: {}", id);
                PipeResponse::error("Item not found")
            }
        } else {
            PipeResponse::error("Missing id")
        }
    }

    fn handle_start(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(id) = &request.id {
            info!("Starting monitor item: {}", id);

            let config_arc = self.guardian.get_config();
            let cfg = config_arc.lock().unwrap();
            let item = cfg.items.iter().find(|i| &i.id == id).cloned();
            drop(cfg);

            if let Some(mut item) = item {
                item.enabled = true;
                let change = ConfigChange {
                    item,
                    change_type: ChangeType::Start,
                };
                self.guardian.add_change(change);

                info!("Monitor item start command sent: {}", id);
                PipeResponse::success("Item started")
            } else {
                error!("Item not found for start: {}", id);
                PipeResponse::error("Item not found")
            }
        } else {
            PipeResponse::error("Missing id")
        }
    }

    fn handle_list(&self) -> PipeResponse {
        debug!("Listing all monitor items");

        let config_arc = self.guardian.get_config();
        let cfg = config_arc.lock().unwrap();
        let items = serde_json::to_value(&cfg.items).unwrap_or(serde_json::json!([]));

        debug!("Found {} monitor items", cfg.items.len());
        PipeResponse::success_with_data("Items list", items)
    }

    fn handle_status(&self) -> PipeResponse {
        debug!("Getting service status");

        let status = self.guardian.get_status();
        PipeResponse::success_with_data("Service status", status)
    }
}
