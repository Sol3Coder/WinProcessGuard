use crate::config::load_config;
use crate::models::{ChangeType, Config, ConfigChange, MonitoredProcess, CHECK_INTERVAL_MS};
use crate::session0::{
    check_process_alive, find_process_by_path, kill_process, start_process_in_session0,
};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Guardian {
    processes: Arc<Mutex<HashMap<String, MonitoredProcess>>>,
    config: Arc<Mutex<Config>>,
    pending_changes: Arc<Mutex<Vec<ConfigChange>>>,
    running: Arc<Mutex<bool>>,
}

impl Guardian {
    pub fn new(running: Arc<Mutex<bool>>) -> Self {
        info!("Initializing Guardian...");

        let mut config = load_config();
        let mut processes = HashMap::new();

        info!("Loaded {} monitor items from config", config.items.len());

        // Force enable all monitor items on startup
        let mut config_modified = false;
        for item in &mut config.items {
            if !item.enabled {
                item.enabled = true;
                config_modified = true;
                info!("Force enabled monitor item on startup: {}", item.name);
            }
        }

        // Save config if modified
        if config_modified {
            if let Err(e) = crate::config::save_config(&config) {
                error!("Failed to save config after force enable: {}", e);
            } else {
                info!("Saved config with all items force enabled");
            }
        }

        // Add all monitor items to processes (all are now enabled)
        for item in &config.items {
            let monitored = MonitoredProcess::from_item(item.clone());
            processes.insert(item.id.clone(), monitored);
            info!("Added monitor item: {} ({})", item.name, item.exe_path);
        }

        Self {
            processes: Arc::new(Mutex::new(processes)),
            config: Arc::new(Mutex::new(config)),
            pending_changes: Arc::new(Mutex::new(Vec::new())),
            running,
        }
    }

    pub fn get_processes(&self) -> Arc<Mutex<HashMap<String, MonitoredProcess>>> {
        self.processes.clone()
    }

    pub fn get_config(&self) -> Arc<Mutex<Config>> {
        self.config.clone()
    }

    pub fn get_pending_changes(&self) -> Arc<Mutex<Vec<ConfigChange>>> {
        self.pending_changes.clone()
    }

    pub fn add_change(&self, change: ConfigChange) {
        let item_id = change.item.id.clone();
        let mut pending = self.pending_changes.lock().unwrap();
        pending.push(change);
        debug!("Added pending change for item: {}", item_id);
    }

    pub fn update_heartbeat(&self, item_id: &str) -> bool {
        let mut processes = self.processes.lock().unwrap();
        if let Some(process) = processes.get_mut(item_id) {
            process.update_heartbeat();
            debug!(
                "Heartbeat updated for: {} (item: {})",
                process.item.name, item_id
            );
            true
        } else {
            warn!("Heartbeat update failed, item not found: {}", item_id);
            false
        }
    }

    pub fn run(&self) {
        info!("Guardian started");
        info!("Check interval: {}ms", CHECK_INTERVAL_MS);

        self.start_all_processes();

        let mut check_count: u64 = 0;

        loop {
            let running = *self.running.lock().unwrap();
            if !running {
                info!("Guardian stopping");
                break;
            }

            std::thread::sleep(Duration::from_millis(CHECK_INTERVAL_MS));
            check_count += 1;

            info!("--- Check cycle #{} ---", check_count);
            self.check_processes();

            self.process_pending_changes();
        }

        info!("Guardian stopped after {} check cycles", check_count);
    }

    fn start_all_processes(&self) {
        info!("Starting all monitored processes...");

        let processes = self.processes.lock().unwrap().clone();

        for (id, mut process) in processes {
            if process.item.enabled {
                info!(
                    "Starting process: {} ({})",
                    process.item.name, process.item.exe_path
                );
                if let Err(e) = self.start_process(&mut process) {
                    error!("Failed to start process {}: {}", process.item.name, e);
                } else {
                    let mut procs = self.processes.lock().unwrap();
                    procs.insert(id, process);
                }
            }
        }

        info!("All processes startup completed");
    }

    fn check_processes(&self) {
        let mut processes = self.processes.lock().unwrap();

        for (_id, process) in processes.iter_mut() {
            if !process.item.enabled {
                debug!("Process {} is disabled, skipping check", process.item.name);
                continue;
            }

            let process_alive = process
                .process_id
                .map_or(false, |pid| check_process_alive(pid));
            let heartbeat_ok = !process.is_heartbeat_timeout();

            info!(
                "Checking [{}]: PID={:?}, alive={}, heartbeat_ok={} (last_heartbeat={:.1}s ago, timeout={}ms)",
                process.item.name,
                process.process_id,
                process_alive,
                heartbeat_ok,
                process.last_heartbeat.elapsed().as_secs_f64(),
                process.item.heartbeat_timeout_ms
            );

            if !process_alive || !heartbeat_ok {
                let reason = if !process_alive {
                    "process not alive"
                } else {
                    "heartbeat timeout"
                };
                warn!(
                    "Process {} needs restart: {} (restart count: {})",
                    process.item.name, reason, process.restart_count
                );

                if let Some(pid) = process.process_id {
                    if check_process_alive(pid) {
                        info!(
                            "Killing existing process {} (PID: {})",
                            process.item.name, pid
                        );
                        kill_process(pid);
                    }
                }

                if let Err(e) = self.start_process_internal(process) {
                    error!("Failed to restart process {}: {}", process.item.name, e);
                } else {
                    process.restart_count += 1;
                    info!(
                        "Restarted process {} successfully (restart count: {})",
                        process.item.name, process.restart_count
                    );
                }
            }

            process.last_check = Instant::now();
        }
    }

    fn process_pending_changes(&self) {
        let mut pending = self.pending_changes.lock().unwrap();
        if pending.is_empty() {
            return;
        }

        let changes: Vec<ConfigChange> = pending.drain(..).collect();
        drop(pending);

        info!("Processing {} pending changes", changes.len());

        for change in changes {
            self.apply_change(change);
        }
    }

    fn apply_change(&self, change: ConfigChange) {
        let mut processes = self.processes.lock().unwrap();
        let mut config = self.config.lock().unwrap();

        info!(
            "Applying change for item: {} (type: {:?})",
            change.item.id, change.change_type
        );

        if change.change_type.has_flag(ChangeType::Stop) {
            if let Some(process) = processes.get(&change.item.id) {
                if let Some(pid) = process.process_id {
                    if check_process_alive(pid) {
                        info!("Stopping process {} (PID: {})", process.item.name, pid);
                        kill_process(pid);
                    }
                }
            }

            if let Some(process) = processes.get_mut(&change.item.id) {
                process.item.enabled = false;
                info!(
                    "Disabled monitor item in runtime: {} ({})",
                    process.item.name, change.item.id
                );
            }

            if let Some(item) = config.items.iter_mut().find(|i| i.id == change.item.id) {
                item.enabled = false;
                if let Err(e) = crate::config::save_config(&config) {
                    error!("Failed to save config after stop: {}", e);
                } else {
                    info!("Saved config with disabled item: {}", change.item.id);
                }
            }
        }

        if change.change_type.has_flag(ChangeType::Remove) {
            if let Some(process) = processes.remove(&change.item.id) {
                info!(
                    "Removed monitor item from runtime: {} ({})",
                    process.item.name, change.item.id
                );
            }
            config.items.retain(|i| i.id != change.item.id);
            if let Err(e) = crate::config::save_config(&config) {
                error!("Failed to save config after remove: {}", e);
            }
            info!("Removed item from config file: {}", change.item.id);
        }

        if change.change_type.has_flag(ChangeType::Start) {
            let mut monitored = MonitoredProcess::from_item(change.item.clone());

            if let Err(e) = self.start_process_internal(&mut monitored) {
                error!("Failed to start process {}: {}", change.item.name, e);
            } else {
                processes.insert(change.item.id.clone(), monitored);

                if let Some(item) = config.items.iter_mut().find(|i| i.id == change.item.id) {
                    item.enabled = true;
                    if let Err(e) = crate::config::save_config(&config) {
                        error!("Failed to save config after start: {}", e);
                    } else {
                        info!("Saved config with enabled item: {}", change.item.id);
                    }
                } else {
                    config.items.push(change.item.clone());
                    if let Err(e) = crate::config::save_config(&config) {
                        error!("Failed to save config after add: {}", e);
                    }
                }

                info!(
                    "Started monitoring: {} ({})",
                    change.item.name, change.item.id
                );
            }
        }
    }

    fn start_process(&self, process: &mut MonitoredProcess) -> Result<(), String> {
        self.start_process_internal(process)
    }

    fn start_process_internal(&self, process: &mut MonitoredProcess) -> Result<(), String> {
        let exe_path = &process.item.exe_path;

        info!("Starting process: {}", exe_path);

        if !std::path::Path::new(exe_path).exists() {
            error!("Executable not found: {}", exe_path);
            return Err(format!("Executable not found: {}", exe_path));
        }

        if let Some(existing_pid) = find_process_by_path(exe_path) {
            warn!(
                "Process already running (PID: {}), killing before restart",
                existing_pid
            );
            kill_process(existing_pid);
            std::thread::sleep(Duration::from_millis(500));
        }

        let working_dir = std::path::Path::new(exe_path)
            .parent()
            .and_then(|p| p.to_str())
            .map(|s| s.to_string());

        let args = process.item.args.as_deref();

        let proc_info = start_process_in_session0(
            exe_path,
            working_dir.as_deref(),
            args,
            process.item.minimize,
            process.item.no_window,
        )?;

        process.process_id = Some(proc_info.process_id);
        process.last_heartbeat = Instant::now();

        info!(
            "Process started successfully: {} (PID: {})",
            process.item.name, proc_info.process_id
        );

        Ok(())
    }

    pub fn get_status(&self) -> serde_json::Value {
        let processes = self.processes.lock().unwrap();
        let items: Vec<serde_json::Value> = processes
            .iter()
            .map(|(id, p)| {
                serde_json::json!({
                    "id": id,
                    "name": p.item.name,
                    "exe_path": p.item.exe_path,
                    "enabled": p.item.enabled,
                    "process_id": p.process_id,
                    "last_heartbeat_ms": p.last_heartbeat.elapsed().as_millis(),
                    "heartbeat_timeout_ms": p.item.heartbeat_timeout_ms,
                    "restart_count": p.restart_count,
                    "is_alive": p.process_id.map_or(false, |pid| check_process_alive(pid)),
                    "is_heartbeat_ok": !p.is_heartbeat_timeout(),
                })
            })
            .collect();

        serde_json::json!({
            "service_running": true,
            "total_items": items.len(),
            "items": items,
        })
    }
}
