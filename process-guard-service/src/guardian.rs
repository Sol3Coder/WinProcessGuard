use crate::config::load_config;
use crate::models::{ChangeType, Config, ConfigChange, MonitoredProcess, CHECK_INTERVAL_MS};
use crate::session0::{
    check_process_alive, find_process_by_path, get_process_status, kill_process,
    start_process_in_session0, start_process_suspended, start_process_with_raw_token,
};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::System::Threading::ResumeThread;

fn should_kill_process_for_change(change_type: ChangeType) -> bool {
    change_type.has_flag(ChangeType::Stop)
}

fn normalize_startup_config(config: Config) -> (Config, bool) {
    let mut config = config;
    let mut modified = false;

    for item in &mut config.items {
        if !item.enabled {
            item.enabled = true;
            modified = true;
        }
    }

    (config, modified)
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
    use crate::models::{ChangeType, Config, LaunchMethod, MonitorItem, MonitoredProcess};
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
            launch_method: LaunchMethod::Auto,
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
    fn startup_forces_disabled_monitor_items_enabled() {
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
                launch_method: LaunchMethod::Auto,
            }],
        };

        let (normalized, modified) = normalize_startup_config(config);
        assert!(modified);
        assert!(normalized.items[0].enabled);
    }

    #[test]
    fn startup_leaves_already_enabled_monitor_items_unchanged() {
        let config = Config {
            items: vec![MonitorItem {
                id: "EnergyMonitor".to_string(),
                exe_path: r"C:\EnergyMonitor.exe".to_string(),
                args: None,
                name: "EnergyMonitor".to_string(),
                minimize: false,
                no_window: false,
                enabled: true,
                heartbeat_timeout_ms: 15_000,
                launch_method: LaunchMethod::Auto,
            }],
        };

        let (normalized, modified) = normalize_startup_config(config);
        assert!(!modified);
        assert!(normalized.items[0].enabled);
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

        // Run the first check immediately, then sleep between cycles
        loop {
            let running = *self.running.lock().unwrap();
            if !running {
                info!("Guardian stopping");
                break;
            }

            check_count += 1;

            info!("--- Check cycle #{} ---", check_count);
            self.process_pending_changes();
            self.check_processes();

            std::thread::sleep(Duration::from_millis(CHECK_INTERVAL_MS));
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

            let process_status = process.process_id.and_then(get_process_status);
            let process_alive = process_status.as_ref().map_or(false, |s| s.is_alive());
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

            if !process_alive {
                if let Some(status) = &process_status {
                    if status.exit_code != 259 {
                        warn!(
                            "Process {} (PID={:?}) exit code: {} (0x{:08X})",
                            process.item.name, process.process_id, status.exit_code, status.exit_code
                        );
                    }
                } else {
                    warn!(
                        "Process {} (PID={:?}) could not be opened (access denied or already gone)",
                        process.item.name, process.process_id
                    );
                }
            }

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
            if let Err(e) = crate::config::save_config_with_backup(&config) {
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
                    if let Err(e) = crate::config::save_config_with_backup(&config) {
                        error!("Failed to persist enabled config: {}", e);
                    } else {
                        info!("Saved enabled monitor item config: {}", change.item.id);
                    }
                } else {
                    config.items.push(change.item.clone());
                    if let Err(e) = crate::config::save_config_with_backup(&config) {
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

    fn launch_with_method(
        &self,
        method: &crate::models::LaunchMethod,
        exe_path: &str,
        working_dir: Option<&str>,
        args: Option<&str>,
        minimize: bool,
        no_window: bool,
    ) -> Option<u32> {
        use crate::models::LaunchMethod;

        match method {
            LaunchMethod::Auto | LaunchMethod::Direct => {
                start_process_in_session0(
                    exe_path, working_dir, args, minimize, no_window, true,
                )
                .ok()
                .map(|info| info.process_id)
            }
            LaunchMethod::DirectNoEnv => {
                start_process_in_session0(
                    exe_path, working_dir, args, minimize, no_window, false,
                )
                .ok()
                .map(|info| info.process_id)
            }
            LaunchMethod::Suspended => {
                let info = start_process_suspended(
                    exe_path, working_dir, args, minimize, no_window, true, false,
                )
                .ok()?;
                std::thread::sleep(std::time::Duration::from_millis(500));
                unsafe {
                    let _ = ResumeThread(info.thread_handle);
                }
                std::thread::sleep(std::time::Duration::from_millis(1000));
                if check_process_alive(info.process_id) {
                    Some(info.process_id)
                } else {
                    None
                }
            }
            LaunchMethod::SuspendedNoEnv => {
                let info = start_process_suspended(
                    exe_path, working_dir, args, minimize, no_window, false, false,
                )
                .ok()?;
                std::thread::sleep(std::time::Duration::from_millis(500));
                unsafe {
                    let _ = ResumeThread(info.thread_handle);
                }
                std::thread::sleep(std::time::Duration::from_millis(1000));
                if check_process_alive(info.process_id) {
                    Some(info.process_id)
                } else {
                    None
                }
            }
            LaunchMethod::RawToken => {
                start_process_with_raw_token(exe_path, working_dir, args, minimize, no_window)
                    .ok()
                    .map(|info| info.process_id)
            }
            LaunchMethod::ShellLaunch => {
                let cmd_args = format!("/c start \"\" \"{}\"", exe_path);
                start_process_in_session0(
                    "C:\\Windows\\System32\\cmd.exe",
                    None,
                    Some(&cmd_args),
                    minimize,
                    true,
                    true,
                )
                .ok()?;
                std::thread::sleep(std::time::Duration::from_secs(3));
                find_process_by_path(exe_path)
            }
            LaunchMethod::CmdExe => {
                let cmd_args = format!("/c \"{}\"", exe_path);
                start_process_in_session0(
                    "C:\\Windows\\System32\\cmd.exe",
                    working_dir,
                    Some(&cmd_args),
                    minimize,
                    true,
                    true,
                )
                .ok()?;
                for attempt in 0..3 {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if let Some(pid) = find_process_by_path(exe_path) {
                        if check_process_alive(pid) {
                            info!(
                                "CmdExe launch succeeded on attempt {}: PID {}",
                                attempt + 1,
                                pid
                            );
                            return Some(pid);
                        }
                    }
                }
                warn!("CmdExe launch: process not found after 3 attempts");
                None
            }
        }
    }

    fn save_launch_method(
        &self,
        process: &mut MonitoredProcess,
        method: crate::models::LaunchMethod,
    ) {
        process.item.launch_method = method.clone();

        let mut config = self.config.lock().unwrap();
        if let Some(item) = config.items.iter_mut().find(|i| i.id == process.item.id) {
            item.launch_method = method.clone();
        }

        let save_result = crate::config::save_config_with_backup(&config);
        drop(config);

        if let Err(e) = save_result {
            error!("Failed to persist launch method: {}", e);
        } else {
            info!(
                "Saved launch method {:?} for {} to config",
                method, process.item.name
            );
        }
    }

    fn diagnose_launch_failure(
        &self,
        exe_path: &str,
        working_dir: Option<&str>,
        args: Option<&str>,
        minimize: bool,
        no_window: bool,
        process: &mut MonitoredProcess,
    ) {
        use crate::models::LaunchMethod;

        warn!("=== Launch failure diagnostics for {} ===", process.item.name);

        // Test 1: without env block
        warn!("Test 1: without environment block (DirectNoEnv)");
        if let Some(pid) = self.launch_with_method(
            &LaunchMethod::DirectNoEnv, exe_path, working_dir, args, minimize, no_window,
        ) {
            info!("Test 1 PASSED: process alive without env block (PID {})", pid);
            process.process_id = Some(pid);
            process.last_heartbeat = Instant::now();
            process.startup_time = Instant::now();
            self.save_launch_method(process, LaunchMethod::DirectNoEnv);
            return;
        }
        warn!("Test 1 FAILED");

        // Test 2: suspended without desktop
        warn!("Test 2: suspended without desktop (Suspended)");
        if let Some(pid) = self.launch_with_method(
            &LaunchMethod::Suspended, exe_path, working_dir, args, minimize, no_window,
        ) {
            info!("Test 2 PASSED: process alive via suspended launch (PID {})", pid);
            process.process_id = Some(pid);
            process.last_heartbeat = Instant::now();
            process.startup_time = Instant::now();
            self.save_launch_method(process, LaunchMethod::Suspended);
            return;
        }
        warn!("Test 2 FAILED");

        // Test 3: suspended, no env, no desktop
        warn!("Test 3: suspended, no env, no desktop (SuspendedNoEnv)");
        if let Some(pid) = self.launch_with_method(
            &LaunchMethod::SuspendedNoEnv, exe_path, working_dir, args, minimize, no_window,
        ) {
            info!("Test 3 PASSED: process alive via suspended (no env) launch (PID {})", pid);
            process.process_id = Some(pid);
            process.last_heartbeat = Instant::now();
            process.startup_time = Instant::now();
            self.save_launch_method(process, LaunchMethod::SuspendedNoEnv);
            return;
        }
        warn!("Test 3 FAILED");

        // Test 4: raw WTS token
        warn!("Test 4: raw WTS token (RawToken)");
        if let Some(pid) = self.launch_with_method(
            &LaunchMethod::RawToken, exe_path, working_dir, args, minimize, no_window,
        ) {
            info!("Test 4 PASSED: process alive with raw WTS token (PID {})", pid);
            process.process_id = Some(pid);
            process.last_heartbeat = Instant::now();
            process.startup_time = Instant::now();
            self.save_launch_method(process, LaunchMethod::RawToken);
            return;
        }
        warn!("Test 4 FAILED");

        // Test 5: shell launch via cmd.exe /c start
        warn!("Test 5: shell launch via cmd.exe /c start (ShellLaunch)");
        if let Some(pid) = self.launch_with_method(
            &LaunchMethod::ShellLaunch, exe_path, working_dir, args, minimize, no_window,
        ) {
            info!("Test 5 PASSED: process alive via shell launch (PID {})", pid);
            process.process_id = Some(pid);
            process.last_heartbeat = Instant::now();
            process.startup_time = Instant::now();
            self.save_launch_method(process, LaunchMethod::ShellLaunch);
            return;
        }
        warn!("Test 5 FAILED");

        // Test 6: cmd.exe /c <exe>
        warn!("Test 6: cmd.exe /c <exe> (CmdExe)");
        if let Some(pid) = self.launch_with_method(
            &LaunchMethod::CmdExe, exe_path, working_dir, args, minimize, no_window,
        ) {
            info!("Test 6 PASSED: process alive via cmd.exe parent (PID {})", pid);
            process.process_id = Some(pid);
            process.last_heartbeat = Instant::now();
            process.startup_time = Instant::now();
            self.save_launch_method(process, LaunchMethod::CmdExe);
            return;
        }
        warn!("Test 6 FAILED");

        warn!("=== All diagnostic tests FAILED for {} ===", process.item.name);
    }

    /// Quick fallback: launch via cmd.exe /c <exe> — works around
    /// STATUS_STACK_BUFFER_OVERRUN that some runtimes (Flutter/Dart) hit when
    /// launched directly via CreateProcessAsUserW.
    fn try_launch_via_cmd(
        &self,
        exe_path: &str,
        working_dir: Option<&str>,
        minimize: bool,
        process: &mut MonitoredProcess,
    ) -> bool {
        use crate::models::LaunchMethod;
        info!("Falling back to cmd.exe /c launch for {}", process.item.name);
        let args = process.item.args.clone();
        let no_window = process.item.no_window;
        if let Some(pid) = self.launch_with_method(
            &LaunchMethod::CmdExe,
            exe_path,
            working_dir,
            args.as_deref(),
            minimize,
            no_window,
        ) {
            process.process_id = Some(pid);
            process.last_heartbeat = Instant::now();
            process.startup_time = Instant::now();
            self.save_launch_method(process, LaunchMethod::CmdExe);
            true
        } else {
            false
        }
    }

    fn start_process_internal(&self, process: &mut MonitoredProcess) -> Result<(), String> {
        use crate::models::LaunchMethod;

        let exe_path = process.item.exe_path.clone();

        info!("Starting process: {}", exe_path);

        if !std::path::Path::new(&exe_path).exists() {
            error!("Executable not found: {}", exe_path);
            return Err(format!("Executable not found: {}", exe_path));
        }

        if let Some(existing_pid) = find_process_by_path(&exe_path) {
            info!(
                "Found running process {} (PID: {}), reusing it",
                process.item.name, existing_pid
            );
            process.process_id = Some(existing_pid);
            process.last_heartbeat = Instant::now();
            process.startup_time = Instant::now();
            return Ok(());
        }

        let working_dir = std::path::Path::new(&exe_path)
            .parent()
            .and_then(|p| p.to_str())
            .map(|s| s.to_string());

        let args = process.item.args.clone();
        let minimize = process.item.minimize;
        let no_window = process.item.no_window;
        let launch_method = process.item.launch_method.clone();

        // If a specific (non-auto) launch method was saved, try it first
        if launch_method != LaunchMethod::Auto {
            info!(
                "Using recorded launch method {:?} for {}",
                launch_method, process.item.name
            );
            if let Some(pid) = self.launch_with_method(
                &launch_method,
                &exe_path,
                working_dir.as_deref(),
                args.as_deref(),
                minimize,
                no_window,
            ) {
                process.process_id = Some(pid);
                process.last_heartbeat = Instant::now();
                process.startup_time = Instant::now();
                info!(
                    "Started {} with PID {} via {:?}",
                    process.item.name, pid, launch_method
                );
                return Ok(());
            }
            warn!(
                "Recorded launch method {:?} failed for {}, falling back to auto diagnostics",
                launch_method, process.item.name
            );
        }

        // Auto: try Direct first
        match self.launch_with_method(
            &LaunchMethod::Direct,
            &exe_path,
            working_dir.as_deref(),
            args.as_deref(),
            minimize,
            no_window,
        ) {
            Some(pid) => {
                process.process_id = Some(pid);
                process.last_heartbeat = Instant::now();
                process.startup_time = Instant::now();
                info!(
                    "Started monitored process {} with PID {}",
                    process.item.name, pid
                );

                // Check if the process survives the first 500ms
                std::thread::sleep(std::time::Duration::from_millis(500));
                let process_crashed = match get_process_status(pid) {
                    Some(status) if status.is_alive() => {
                        debug!(
                            "Process {} (PID {}) confirmed alive 500ms after launch",
                            process.item.name, pid
                        );
                        false
                    }
                    Some(status) => {
                        warn!(
                            "Process {} (PID {}) exited within 500ms of launch! Exit code: {} (0x{:08X})",
                            process.item.name, pid, status.exit_code, status.exit_code
                        );
                        true
                    }
                    None => {
                        warn!(
                            "Process {} (PID {}) disappeared within 500ms of launch",
                            process.item.name, pid
                        );
                        true
                    }
                };

                if process_crashed {
                    // Fast fallback: try cmd.exe /c (common fix for Flutter/Dart)
                    if !self.try_launch_via_cmd(
                        &exe_path,
                        working_dir.as_deref(),
                        minimize,
                        process,
                    ) {
                        let args_owned = args.clone();
                        self.diagnose_launch_failure(
                            &exe_path,
                            working_dir.as_deref(),
                            args_owned.as_deref(),
                            minimize,
                            no_window,
                            process,
                        );
                    }
                }
            }
            None => {
                // Direct launch failed entirely, run full diagnostics
                warn!(
                    "Direct launch failed for {}, running full diagnostics",
                    process.item.name
                );
                let args_owned = args.clone();
                self.diagnose_launch_failure(
                    &exe_path,
                    working_dir.as_deref(),
                    args_owned.as_deref(),
                    minimize,
                    no_window,
                    process,
                );
            }
        }

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
