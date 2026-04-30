use crate::guardian::Guardian;
use crate::models::SERVICE_NAME;
use crate::pipe_server::PipeServer;
use log::{error, info, LevelFilter, Log, Metadata, Record};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use time::macros::offset;
use time::OffsetDateTime;
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const MAX_LOG_SIZE: u64 = 300 * 1024 * 1024; // 300MB
const LOG_DIR_NAME: &str = "logs";

pub(crate) struct StartupGate {
    ready: Mutex<bool>,
    condvar: Condvar,
}

impl StartupGate {
    pub(crate) fn new() -> Self {
        Self {
            ready: Mutex::new(false),
            condvar: Condvar::new(),
        }
    }

    pub(crate) fn mark_ready(&self) {
        let mut ready = self.ready.lock().unwrap();
        *ready = true;
        self.condvar.notify_all();
    }

    pub(crate) fn wait_until_ready(&self) {
        let mut ready = self.ready.lock().unwrap();
        while !*ready {
            ready = self.condvar.wait(ready).unwrap();
        }
    }
}

/// 获取日志目录路径
fn get_log_dir() -> PathBuf {
    let exe_path = env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = exe_path.parent().unwrap_or(std::path::Path::new("."));
    exe_dir.join(LOG_DIR_NAME)
}

/// 根据日期获取日志文件路径
fn get_log_file_path(date: &OffsetDateTime) -> PathBuf {
    let log_dir = get_log_dir();
    let date_str = date.format(time::macros::format_description!(
        "[year]-[month]-[day]"
    )).unwrap_or_else(|_| "unknown".to_string());
    log_dir.join(format!("process-guard-service-{}.log", date_str))
}

/// 计算日志目录总大小
fn get_total_log_size(log_dir: &Path) -> u64 {
    if !log_dir.exists() {
        return 0;
    }

    let mut total_size = 0u64;
    if let Ok(entries) = fs::read_dir(log_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    total_size += metadata.len();
                }
            }
        }
    }
    total_size
}

/// 清理旧的日志文件，直到总大小低于限制
fn cleanup_old_logs(log_dir: &Path, max_size: u64) {
    let mut total_size = get_total_log_size(log_dir);

    if total_size <= max_size {
        return;
    }

    // 收集所有日志文件及其修改时间
    let mut log_files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(entries) = fs::read_dir(log_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    if let Ok(modified) = metadata.modified() {
                        log_files.push((path, modified));
                    }
                }
            }
        }
    }

    // 按修改时间排序（最老的在前）
    log_files.sort_by(|a, b| a.1.cmp(&b.1));

    // 删除最老的文件直到低于限制
    for (path, _) in log_files {
        if total_size <= max_size {
            break;
        }

        if let Ok(metadata) = fs::metadata(&path) {
            let file_size = metadata.len();
            if fs::remove_file(&path).is_ok() {
                total_size -= file_size;
                eprintln!("已删除旧日志文件: {:?}", path);
            }
        }
    }
}

/// 自定义日志实现，支持按日期分割和容量控制
struct RotatingLogger {
    level: LevelFilter,
    log_dir: PathBuf,
    current_date: Mutex<OffsetDateTime>,
    file: Mutex<Option<File>>,
}

impl RotatingLogger {
    fn new(level: LevelFilter) -> Self {
        let log_dir = get_log_dir();

        // 创建日志目录
        if let Err(e) = fs::create_dir_all(&log_dir) {
            eprintln!("创建日志目录失败: {:?}", e);
        }

        // 清理旧日志
        cleanup_old_logs(&log_dir, MAX_LOG_SIZE);

        let now = OffsetDateTime::now_utc().to_offset(offset!(+8));
        let log_path = get_log_file_path(&now);

        // 打开或创建日志文件
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();

        if file.is_some() {
            eprintln!("日志文件已打开: {:?}", log_path);
        } else {
            eprintln!("无法打开日志文件: {:?}", log_path);
        }

        Self {
            level,
            log_dir,
            current_date: Mutex::new(now),
            file: Mutex::new(file),
        }
    }

    /// 检查是否需要轮转（跨天）
    fn check_and_rotate(&self) {
        let now = OffsetDateTime::now_utc().to_offset(offset!(+8));
        let current_date = *self.current_date.lock().unwrap();

        // 检查是否跨天
        if now.date() != current_date.date() {
            let new_log_path = get_log_file_path(&now);

            // 尝试打开新文件
            if let Ok(new_file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&new_log_path)
            {
                let mut file_guard = self.file.lock().unwrap();
                *file_guard = Some(new_file);

                let mut date_guard = self.current_date.lock().unwrap();
                *date_guard = now;

                eprintln!("日志已轮转至新文件: {:?}", new_log_path);
            }
        }

        // 检查容量限制
        cleanup_old_logs(&self.log_dir, MAX_LOG_SIZE);
    }

    /// 格式化日志记录
    fn format_log(&self, record: &Record) -> String {
        let now = OffsetDateTime::now_utc().to_offset(offset!(+8));
        let time_str = now
            .format(time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second]"
            ))
            .unwrap_or_else(|_| "unknown".to_string());

        format!(
            "[{}] [{}] {}\n",
            time_str,
            record.level(),
            record.args()
        )
    }
}

impl Log for RotatingLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        // 检查是否需要轮转
        self.check_and_rotate();

        let log_line = self.format_log(record);

        // 写入文件
        if let Ok(mut file_guard) = self.file.lock() {
            if let Some(ref mut file) = *file_guard {
                let _ = file.write_all(log_line.as_bytes());
                let _ = file.flush();
            }
        }

        // 同时输出到控制台（用于调试）
        eprintln!("{}", log_line.trim());
    }

    fn flush(&self) {
        if let Ok(mut file_guard) = self.file.lock() {
            if let Some(ref mut file) = *file_guard {
                let _ = file.flush();
            }
        }
    }
}

fn init_logger() {
    let logger = RotatingLogger::new(LevelFilter::Debug);

    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(LevelFilter::Debug);
        info!("日志初始化完成, 日志目录: {:?}", get_log_dir());
    } else {
        eprintln!("日志初始化失败");
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    init_logger();
    info!("========================================");
    info!("进程守护服务启动...");
    info!("========================================");

    let running = Arc::new(Mutex::new(true));
    let running_clone = running.clone();
    let running_for_pipe = running.clone();
    let running_for_guardian = running.clone();
    let pipe_ready = Arc::new(StartupGate::new());

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                info!("接收到服务控制管理器的停止信号");
                let mut running = running_clone.lock().unwrap();
                *running = false;
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .expect("注册服务控制处理器失败");

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });

    info!("服务状态已设置为运行中");

    let guardian = Arc::new(Guardian::new(
        running_for_guardian,
        Some(pipe_ready.clone()),
    ));
    let guardian_for_pipe = guardian.clone();

    let guardian_handle = std::thread::spawn(move || {
        info!("守护线程已启动, 进入运行循环");
        guardian.run();
        info!("守护线程已退出");
    });

    let pipe_ready_for_pipe = pipe_ready.clone();
    let pipe_server = PipeServer::new(
        guardian_for_pipe,
        running_for_pipe,
        Some(pipe_ready_for_pipe),
    );
    let pipe_handle = std::thread::spawn(move || {
        info!("管道服务线程已启动");
        pipe_server.run();
        info!("管道服务线程已退出");
    });

    info!("服务正在运行并监控进程中");

    loop {
        let r = running.lock().unwrap();
        if !*r {
            info!("服务主循环接收到停止信号");
            break;
        }
        drop(r);
        std::thread::sleep(Duration::from_millis(100));
    }

    info!("服务正在停止...");

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });

    let _ = guardian_handle.join();
    let _ = pipe_handle.join();

    info!("========================================");
    info!("进程守护服务已停止");
    info!("========================================");
}

pub fn run_service() -> Result<(), windows_service::Error> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

pub fn install_service(exe_path: &str) -> Result<(), String> {
    info!("正在安装服务: {}", exe_path);

    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let service_manager =
        ServiceManager::local_computer(None::<&str>, manager_access).map_err(|e| {
            error!("连接服务管理器失败: {:?}", e);
            format!("连接服务管理器失败: {:?}", e)
        })?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("Process Guard Service"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::path::PathBuf::from(exe_path),
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };

    service_manager
        .create_service(&service_info, ServiceAccess::empty())
        .map_err(|e| {
            error!("创建服务失败: {:?}", e);
            format!("创建服务失败: {:?}", e)
        })?;

    info!("服务安装成功");
    Ok(())
}

pub fn uninstall_service() -> Result<(), String> {
    info!("正在卸载服务");

    let manager_access = ServiceManagerAccess::CONNECT;
    let service_manager =
        ServiceManager::local_computer(None::<&str>, manager_access).map_err(|e| {
            error!("连接服务管理器失败: {:?}", e);
            format!("连接服务管理器失败: {:?}", e)
        })?;

    let service_access = ServiceAccess::DELETE | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS;
    let service = service_manager
        .open_service(SERVICE_NAME, service_access)
        .map_err(|e| {
            error!("打开服务失败: {:?}", e);
            format!("打开服务失败: {:?}", e)
        })?;

    let status = service.query_status().map_err(|e| {
        error!("查询服务状态失败: {:?}", e);
        format!("查询服务状态失败: {:?}", e)
    })?;

    if status.current_state == ServiceState::Running {
        info!("服务正在运行, 先停止服务");
        service.stop().map_err(|e| {
            error!("停止服务失败: {:?}", e);
            format!("停止服务失败: {:?}", e)
        })?;

        std::thread::sleep(Duration::from_secs(2));
    }

    service.delete().map_err(|e| {
        error!("删除服务失败: {:?}", e);
        format!("删除服务失败: {:?}", e)
    })?;

    info!("服务卸载成功");
    Ok(())
}

pub fn start_service() -> Result<(), String> {
    info!("正在启动服务");

    let manager_access = ServiceManagerAccess::CONNECT;
    let service_manager =
        ServiceManager::local_computer(None::<&str>, manager_access).map_err(|e| {
            error!("连接服务管理器失败: {:?}", e);
            format!("连接服务管理器失败: {:?}", e)
        })?;

    let service_access = ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let service = service_manager
        .open_service(SERVICE_NAME, service_access)
        .map_err(|e| {
            error!("打开服务失败: {:?}", e);
            format!("打开服务失败: {:?}", e)
        })?;

    service.start(&[] as &[OsString]).map_err(|e| {
        error!("启动服务失败: {:?}", e);
        format!("启动服务失败: {:?}", e)
    })?;

    info!("服务启动成功");
    Ok(())
}

pub fn stop_service() -> Result<(), String> {
    info!("正在停止服务");

    let manager_access = ServiceManagerAccess::CONNECT;
    let service_manager =
        ServiceManager::local_computer(None::<&str>, manager_access).map_err(|e| {
            error!("连接服务管理器失败: {:?}", e);
            format!("连接服务管理器失败: {:?}", e)
        })?;

    let service_access = ServiceAccess::STOP | ServiceAccess::QUERY_STATUS;
    let service = service_manager
        .open_service(SERVICE_NAME, service_access)
        .map_err(|e| {
            error!("打开服务失败: {:?}", e);
            format!("打开服务失败: {:?}", e)
        })?;

    service.stop().map_err(|e| {
        error!("停止服务失败: {:?}", e);
        format!("停止服务失败: {:?}", e)
    })?;

    info!("服务停止成功");
    Ok(())
}

pub fn is_service_installed() -> bool {
    let manager_access = ServiceManagerAccess::CONNECT;
    if let Ok(service_manager) = ServiceManager::local_computer(None::<&str>, manager_access) {
        let service_access = ServiceAccess::QUERY_STATUS;
        service_manager
            .open_service(SERVICE_NAME, service_access)
            .is_ok()
    } else {
        false
    }
}

pub fn is_service_running() -> bool {
    let manager_access = ServiceManagerAccess::CONNECT;
    if let Ok(service_manager) = ServiceManager::local_computer(None::<&str>, manager_access) {
        let service_access = ServiceAccess::QUERY_STATUS;
        if let Ok(service) = service_manager.open_service(SERVICE_NAME, service_access) {
            if let Ok(status) = service.query_status() {
                return status.current_state == ServiceState::Running;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::StartupGate;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn startup_gate_blocks_until_pipe_is_ready() {
        let gate = Arc::new(StartupGate::new());
        let unblocked = Arc::new(AtomicBool::new(false));

        let gate_for_thread = gate.clone();
        let unblocked_for_thread = unblocked.clone();
        let waiter = thread::spawn(move || {
            gate_for_thread.wait_until_ready();
            unblocked_for_thread.store(true, Ordering::SeqCst);
        });

        thread::sleep(Duration::from_millis(50));
        assert!(!unblocked.load(Ordering::SeqCst));

        gate.mark_ready();
        waiter.join().unwrap();

        assert!(unblocked.load(Ordering::SeqCst));
    }
}
