use crate::guardian::Guardian;
use crate::models::SERVICE_NAME;
use crate::pipe_server::PipeServer;
use log::{error, info, LevelFilter};
use simplelog::{ConfigBuilder, WriteLogger};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::macros::offset;
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

fn get_log_file_path() -> PathBuf {
    let exe_path = env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = exe_path.parent().unwrap_or(std::path::Path::new("."));
    exe_dir.join("process-guard-service.log")
}

fn init_logger() {
    let log_path = get_log_file_path();

    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    // 使用追加模式打开日志文件
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            // 配置日志时间格式为北京时间 (UTC+8)
            let config = ConfigBuilder::new()
                .set_time_offset(offset!(+8))
                .build();
            
            let _ = WriteLogger::init(LevelFilter::Debug, config, file);
            info!("日志初始化完成, 日志文件: {:?}", log_path);
        }
        Err(e) => {
            eprintln!("创建日志文件失败: {:?}", e);
        }
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

    let guardian = Arc::new(Guardian::new(running_for_guardian));
    let guardian_for_pipe = guardian.clone();

    let guardian_handle = std::thread::spawn(move || {
        info!("守护线程已启动, 进入运行循环");
        guardian.run();
        info!("守护线程已退出");
    });

    let pipe_server = PipeServer::new(guardian_for_pipe, running_for_pipe);
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
