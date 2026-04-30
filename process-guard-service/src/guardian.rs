use crate::config::load_config;
use crate::models::{ChangeType, Config, ConfigChange, MonitoredProcess, CHECK_INTERVAL_MS};
use crate::session0::{
    check_process_alive, find_process_by_path, kill_process, start_process_in_session0,
};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn should_kill_process_for_change(change_type: ChangeType) -> bool {
    change_type.has_flag(ChangeType::Stop)
}

fn normalize_startup_config(config: Config) -> (Config, bool) {
    (config, false)
}

fn apply_pause_state(
    processes: &mut HashMap<String, MonitoredProcess>,
    config: &mut Config,
    item_id: &str,
) -> bool {
    let mut found = false;

    if let Some(process) = processes.get_mut(item_id) {
        process.item.enabled = false;
        found = true;
        info!(
            "Disabled monitor item at runtime without terminating process: {} ({})",
            process.item.name, item_id
        );
    }

    if let Some(item) = config.items.iter_mut().find(|item| item.id == item_id) {
        item.enabled = false;
        found = true;
        info!("Disabled monitor item in config: {}", item_id);
    }

    found
}

pub struct Guardian {
    processes: Arc<Mutex<HashMap<String, MonitoredProcess>>>,
    config: Arc<Mutex<Config>>,
    pending_changes: Arc<Mutex<Vec<ConfigChange>>>,
    running: Arc<Mutex<bool>>,
    startup_gate: Option<Arc<crate::service::StartupGate>>,
}

#[cfg(test)]
mod tests {
    use super::{
        apply_pause_state, normalize_startup_config, should_kill_process_for_change,
    };
    use crate::models::{ChangeType, Config, MonitorItem, MonitoredProcess};
    use std::collections::HashMap;

    #[test]
    fn pause_change_does_not_require_terminating_process() {
        assert!(!should_kill_process_for_change(ChangeType::Pause));
    }

    #[test]
    fn stop_change_still_requires_terminating_process() {
        assert!(should_kill_process_for_change(ChangeType::Stop));
    }

    #[test]
    fn pause_state_disables_monitor_without_clearing_process_id() {
        let item = MonitorItem {
            id: "EnergyMonitor".to_string(),
            exe_path: r"C:\EnergyMonitor.exe".to_string(),
            args: None,
            name: "EnergyMonitor".to_string(),
            minimize: false,
            no_window: false,
            enabled: true,
            heartbeat_timeout_ms: 15_000,
        };
        let mut processes = HashMap::new();
        let mut process = MonitoredProcess::from_item(item.clone());
        process.process_id = Some(42);
        processes.insert(item.id.clone(), process);

        let mut config = Config {
            items: vec![item.clone()],
        };

        assert!(apply_pause_state(&mut processes, &mut config, &item.id));
        assert!(!processes[&item.id].item.enabled);
        assert_eq!(processes[&item.id].process_id, Some(42));
        assert!(!config.items[0].enabled);
    }

    #[test]
    fn startup_keeps_disabled_monitor_items_disabled() {
        let config = Config {
            items: vec![MonitorItem {
                id: "EnergyMonitor".to_string(),
                exe_path: r"C:\EnergyMonitor.exe".to_string(),
                args: None,
                name: "EnergyMonitor".to_string(),
                minimize: false,
                no_window: false,
                enabled: false,
                heartbeat_timeout_ms: 15_000,
            }],
        };

        let (normalized, modified) = normalize_startup_config(config);
        assert!(!modified);
        assert!(!normalized.items[0].enabled);
    }
}

impl Guardian {
    pub fn new(
        running: Arc<Mutex<bool>>,
        startup_gate: Option<Arc<crate::service::StartupGate>>,
    ) -> Self {
        info!("Initializing guardian");

        let loaded_config = load_config();
        let (config, config_modified) = normalize_startup_config(loaded_config);
        let mut processes = HashMap::new();

        info!("Loaded {} monitor items from config", config.items.len());

        if config_modified {
            if let Err(e) = crate::config::save_config(&config) {
                error!("Failed to persist normalized startup config: {}", e);
            } else {
                info!("Saved normalized startup monitor configuration");
            }
        }

        for item in &config.items {
            let monitored = MonitoredProcess::from_item(item.clone());
            processes.insert(item.id.clone(), monitored);
            info!("Registered monitor item: {} ({})", item.name, item.exe_path);
        }

        Self {
            processes: Arc::new(Mutex::new(processes)),
            config: Arc::new(Mutex::new(config)),
            pending_changes: Arc::new(Mutex::new(Vec::new())),
            running,
            startup_gate,
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
        debug!("Queued config change for {}", item_id);
    }

    pub fn update_heartbeat(&self, item_id: &str) -> bool {
        let mut processes = self.processes.lock().unwrap();
        if let Some(process) = processes.get_mut(item_id) {
            process.update_heartbeat();
            debug!("Heartbeat updated for {} ({})", process.item.name, item_id);
            true
        } else {
            warn!("Heartbeat update failed, item not found: {}", item_id);
            false
        }
    }

    pub fn run(&self) {
        info!("Guardian started");
        info!("Check interval: {} ms", CHECK_INTERVAL_MS);

        if let Some(startup_gate) = &self.startup_gate {
            info!("Waiting for pipe server readiness before starting monitored processes");
            startup_gate.wait_until_ready();
        }

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
            self.process_pending_changes();
            self.check_processes();
        }

        info!("Guardian stopped after {} checks", check_count);
    }

    fn start_all_processes(&self) {
        info!("Starting all monitored processes");

        let processes = self.processes.lock().unwrap().clone();

        for (id, mut process) in processes {
            if process.item.enabled {
                info!(
                    "Starting monitored process {} ({})",
                    process.item.name, process.item.exe_path
                );
                if let Err(e) = self.start_process_internal(&mut process) {
                    error!("Failed to start monitored process {}: {}", process.item.name, e);
                } else {
                    let mut procs = self.processes.lock().unwrap();
                    procs.insert(id, process);
                }
            }
        }

        info!("Finished starting monitored processes");
    }

    fn check_processes(&self) {
        let mut processes = self.processes.lock().unwrap();

        for process in processes.values_mut() {
            if !process.item.enabled {
                debug!("Process {} is disabled, skipping check", process.item.name);
                continue;
            }

            let startup_elapsed = process.startup_time.elapsed();
            let in_grace_period = startup_elapsed < Duration::from_secs(5);

            if in_grace_period {
                debug!(
                    "Process {} is in startup grace period ({:.1}s), skipping checks",
                    process.item.name,
                    startup_elapsed.as_secs_f64()
                );
                continue;
            }

            let process_alive = process
                .process_id
                .map_or(false, check_process_alive);
            let heartbeat_ok = !process.is_heartbeat_timeout();

            info!(
                "Check [{}]: PID={:?}, alive={}, heartbeat_ok={} (last_heartbeat={:.1}s ago, timeout={}ms, startup={:.1}s ago)",
                process.item.name,
                process.process_id,
                process_alive,
                heartbeat_ok,
                process.last_heartbeat.elapsed().as_secs_f64(),
                process.item.heartbeat_timeout_ms,
                startup_elapsed.as_secs_f64()
            );

            if !process_alive || !heartbeat_ok {
                let reason = if !process_alive {
                    "process not alive"
                } else {
                    "heartbeat timeout"
                };

                if !heartbeat_ok && process_alive {
                    let elapsed_ms = process.last_heartbeat.elapsed().as_millis();
                    let timeout_ms = process.item.heartbeat_timeout_ms;
                    debug!(
                        "Heartbeat timeout detail: name={}, elapsed={}ms, timeout={}ms, delta={}ms",
                        process.item.name,
                        elapsed_ms,
                        timeout_ms,
                        elapsed_ms as i64 - timeout_ms as i64
                    );
                }

                warn!(
                    "Process unhealthy or intentionally controlled: name={}, reason={}, pid={:?}",
                    process.item.name, reason, process.process_id
                );
                warn!(
                    "Process {} needs restart because {} (restart_count={})",
                    process.item.name, reason, process.restart_count
                );

                if let Some(pid) = process.process_id {
                    if check_process_alive(pid) {
                        info!(
                            "Stopping monitored process: {}, PID: {}, reason: restart required",
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
                        "Process {} restarted successfully (restart_count={})",
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
            "Applying config change: {} ({:?})",
            change.item.id, change.change_type
        );

        if change.change_type.has_flag(ChangeType::Stop)
            || change.change_type.has_flag(ChangeType::Pause)
        {
            let should_kill = should_kill_process_for_change(change.change_type);

            if let Some(process) = processes.get(&change.item.id) {
                if should_kill {
                    if let Some(pid) = process.process_id {
                        if check_process_alive(pid) {
                            warn!(
                                "Process {} will be terminated because monitoring was stopped by user, pid={}",
                                process.item.name, pid
                            );
                            info!(
                                "Stopping monitored process: {}, PID: {}, reason: user stop",
                                process.item.name, pid
                            );
                            kill_process(pid);
                        }
                    }
                } else if apply_pause_state(&mut processes, &mut config, &change.item.id) {
                    info!(
                        "Pausing monitor item while keeping process alive: {} ({})",
                        change.item.name, change.item.id
                    );
                }
            }

            if should_kill {
                if let Some(process) = processes.get_mut(&change.item.id) {
                    process.process_id = None;
                    process.item.enabled = false;
                    info!(
                        "Disabled monitor item at runtime: {} ({})",
                        process.item.name, change.item.id
                    );
                }

                if let Some(item) = config.items.iter_mut().find(|i| i.id == change.item.id) {
                    item.enabled = false;
                }
            }

            if let Err(e) = crate::config::save_config(&config) {
                error!("Failed to persist disabled config: {}", e);
            } else {
                info!("Saved disabled monitor item config: {}", change.item.id);
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
                error!("Failed to persist removal: {}", e);
            }
            info!("Removed monitor item from config: {}", change.item.id);
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
                        error!("Failed to persist enabled config: {}", e);
                    } else {
                        info!("Saved enabled monitor item config: {}", change.item.id);
                    }
                } else {
                    config.items.push(change.item.clone());
                    if let Err(e) = crate::config::save_config(&config) {
                        error!("Failed to persist added config: {}", e);
                    }
                }

                info!(
                    "Started monitoring {} ({})",
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
            info!(
                "Found running process {} (PID: {}), reusing it",
                process.item.name, existing_pid
            );
            process.process_id = Some(existing_pid);
            process.last_heartbeat = Instant::now();
            process.startup_time = Instant::now();
            return Ok(());
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
        process.startup_time = Instant::now();

        info!(
            "Started monitored process {} with PID {}",
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
                    "is_alive": p.process_id.map_or(false, check_process_alive),
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
