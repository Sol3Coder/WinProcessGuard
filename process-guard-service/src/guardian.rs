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
        info!("正在初始化守护进程...");

        let mut config = load_config();
        let mut processes = HashMap::new();

        info!("从配置加载了 {} 个监控项", config.items.len());

        // Force enable all monitor items on startup
        let mut config_modified = false;
        for item in &mut config.items {
            if !item.enabled {
                item.enabled = true;
                config_modified = true;
                info!("启动时强制启用监控项: {}", item.name);
            }
        }

        // Save config if modified
        if config_modified {
            if let Err(e) = crate::config::save_config(&config) {
                error!("强制启用后保存配置失败: {}", e);
            } else {
                info!("已保存所有监控项强制启用的配置");
            }
        }

        // Add all monitor items to processes (all are now enabled)
        for item in &config.items {
            let monitored = MonitoredProcess::from_item(item.clone());
            processes.insert(item.id.clone(), monitored);
            info!("添加监控项: {} ({})", item.name, item.exe_path);
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
        debug!("为监控项添加待处理变更: {}", item_id);
    }

    pub fn update_heartbeat(&self, item_id: &str) -> bool {
        let mut processes = self.processes.lock().unwrap();
        if let Some(process) = processes.get_mut(item_id) {
            process.update_heartbeat();
            debug!(
                "心跳已更新: {} (监控项: {})",
                process.item.name, item_id
            );
            true
        } else {
            warn!("心跳更新失败, 未找到监控项: {}", item_id);
            false
        }
    }

    pub fn run(&self) {
        info!("守护进程已启动");
        info!("检查间隔: {}毫秒", CHECK_INTERVAL_MS);

        self.start_all_processes();

        let mut check_count: u64 = 0;

        loop {
            let running = *self.running.lock().unwrap();
            if !running {
                info!("守护进程正在停止");
                break;
            }

            std::thread::sleep(Duration::from_millis(CHECK_INTERVAL_MS));
            check_count += 1;

            info!("--- 检查周期 #{} ---", check_count);
            self.check_processes();

            self.process_pending_changes();
        }

        info!("守护进程已停止, 共执行 {} 个检查周期", check_count);
    }

    fn start_all_processes(&self) {
        info!("正在启动所有被监控的进程...");

        let processes = self.processes.lock().unwrap().clone();

        for (id, mut process) in processes {
            if process.item.enabled {
                info!(
                    "启动被监控程序: {} ({})",
                    process.item.name, process.item.exe_path
                );
                // 启动进程，如果已存在则复用
                if let Err(e) = self.start_process_internal(&mut process) {
                    error!("启动被监控程序 {} 失败: {}", process.item.name, e);
                } else {
                    let mut procs = self.processes.lock().unwrap();
                    procs.insert(id, process);
                }
            }
        }

        info!("所有被监控进程启动完成");
    }

    fn check_processes(&self) {
        let mut processes = self.processes.lock().unwrap();

        for (_id, process) in processes.iter_mut() {
            if !process.item.enabled {
                debug!("进程 {} 已禁用, 跳过检查", process.item.name);
                continue;
            }

            // 检查是否在启动宽限期内（5秒）
            let startup_elapsed = process.startup_time.elapsed();
            let in_grace_period = startup_elapsed < Duration::from_secs(5);
            
            // 宽限期内跳过所有检查
            if in_grace_period {
                debug!(
                    "进程 {} 处于启动宽限期内 (已启动 {:.1}秒), 跳过所有检查",
                    process.item.name,
                    startup_elapsed.as_secs_f64()
                );
                continue;
            }

            let process_alive = process
                .process_id
                .map_or(false, |pid| check_process_alive(pid));
            
            let heartbeat_ok = !process.is_heartbeat_timeout();

            info!(
                "检查 [{}]: PID={:?}, 存活={}, 心跳正常={} (上次心跳={:.1}秒前, 超时={}毫秒, 启动={:.1}秒前)",
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
                    "进程不存活"
                } else {
                    "心跳超时"
                };

                // 增加详细的心跳超时调试信息
                if !heartbeat_ok && process_alive {
                    let elapsed_ms = process.last_heartbeat.elapsed().as_millis();
                    let timeout_ms = process.item.heartbeat_timeout_ms;
                    debug!(
                        "心跳超时详情: 程序名称={}, 已过去={}毫秒, 超时阈值={}毫秒, 差值={}毫秒",
                        process.item.name,
                        elapsed_ms,
                        timeout_ms,
                        elapsed_ms as i64 - timeout_ms as i64
                    );
                }

                warn!(
                    "程序未正常运行或主动控制关闭: 程序名称={}, 原因={}, PID={:?}",
                    process.item.name, reason, process.process_id
                );
                warn!(
                    "进程 {} 需要重启: {} (重启次数: {})",
                    process.item.name,
                    if !process_alive {
                        "进程不存活"
                    } else {
                        "心跳超时"
                    },
                    process.restart_count
                );

                if let Some(pid) = process.process_id {
                    if check_process_alive(pid) {
                        info!(
                            "退出被监控程序: {}, PID: {}, 原因: 需要重启",
                            process.item.name, pid
                        );
                        kill_process(pid);
                    }
                }

                // 重启进程，如果已存在则复用
                if let Err(e) = self.start_process_internal(process) {
                    error!("重启进程 {} 失败: {}", process.item.name, e);
                } else {
                    process.restart_count += 1;
                    info!(
                        "进程 {} 重启成功 (重启次数: {})",
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

        info!("正在处理 {} 个待处理变更", changes.len());

        for change in changes {
            self.apply_change(change);
        }
    }

    fn apply_change(&self, change: ConfigChange) {
        let mut processes = self.processes.lock().unwrap();
        let mut config = self.config.lock().unwrap();

        info!(
            "正在应用监控项变更: {} (类型: {:?})",
            change.item.id, change.change_type
        );

        if change.change_type.has_flag(ChangeType::Stop) {
            if let Some(process) = processes.get(&change.item.id) {
                if let Some(pid) = process.process_id {
                    if check_process_alive(pid) {
                        warn!(
                            "程序未正常运行或主动控制关闭: 程序名称={}, 原因=用户主动停止监控, PID={}",
                            process.item.name, pid
                        );
                        info!(
                            "退出被监控程序: {}, PID: {}, 原因: 用户主动停止监控",
                            process.item.name, pid
                        );
                        kill_process(pid);
                    }
                }
            }

            if let Some(process) = processes.get_mut(&change.item.id) {
                process.item.enabled = false;
                info!(
                    "已在运行时禁用监控项: {} ({})",
                    process.item.name, change.item.id
                );
            }

            if let Some(item) = config.items.iter_mut().find(|i| i.id == change.item.id) {
                item.enabled = false;
                if let Err(e) = crate::config::save_config(&config) {
                    error!("停止后保存配置失败: {}", e);
                } else {
                    info!("已保存禁用监控项的配置: {}", change.item.id);
                }
            }
        }

        if change.change_type.has_flag(ChangeType::Remove) {
            if let Some(process) = processes.remove(&change.item.id) {
                info!(
                    "已从运行时移除监控项: {} ({})",
                    process.item.name, change.item.id
                );
            }
            config.items.retain(|i| i.id != change.item.id);
            if let Err(e) = crate::config::save_config(&config) {
                error!("移除后保存配置失败: {}", e);
            }
            info!("已从配置文件中移除监控项: {}", change.item.id);
        }

        if change.change_type.has_flag(ChangeType::Start) {
            let mut monitored = MonitoredProcess::from_item(change.item.clone());

            // 启动进程，如果已存在则复用
            if let Err(e) = self.start_process_internal(&mut monitored) {
                error!("启动进程 {} 失败: {}", change.item.name, e);
            } else {
                processes.insert(change.item.id.clone(), monitored);

                if let Some(item) = config.items.iter_mut().find(|i| i.id == change.item.id) {
                    item.enabled = true;
                    if let Err(e) = crate::config::save_config(&config) {
                        error!("启动后保存配置失败: {}", e);
                    } else {
                        info!("已保存启用监控项的配置: {}", change.item.id);
                    }
                } else {
                    config.items.push(change.item.clone());
                    if let Err(e) = crate::config::save_config(&config) {
                        error!("添加后保存配置失败: {}", e);
                    }
                }

                info!(
                    "开始监控: {} ({})",
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

        info!("正在启动进程: {}", exe_path);

        if !std::path::Path::new(exe_path).exists() {
            error!("可执行文件未找到: {}", exe_path);
            return Err(format!("可执行文件未找到: {}", exe_path));
        }

        // 检查是否已有同名进程在运行，如果存在则复用
        if let Some(existing_pid) = find_process_by_path(exe_path) {
            info!(
                "发现进程 {} 已在运行 (PID: {}), 复用该进程",
                process.item.name, existing_pid
            );
            process.process_id = Some(existing_pid);
            process.last_heartbeat = Instant::now();
            process.startup_time = Instant::now(); // 记录进程启动时间
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
        process.startup_time = Instant::now(); // 记录进程启动时间

        info!(
            "启动被监控程序: {}, PID: {}",
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
