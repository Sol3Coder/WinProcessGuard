use crate::guardian::Guardian;
use crate::models::SERVICE_NAME;
use crate::pipe_server::PipeServer;
use log::{error, info, LevelFilter};
use simplelog::{Config as LogConfig, WriteLogger};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
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

    match File::create(&log_path) {
        Ok(file) => {
            let _ = WriteLogger::init(LevelFilter::Debug, LogConfig::default(), file);
            info!("Logger initialized, log file: {:?}", log_path);
        }
        Err(e) => {
            eprintln!("Failed to create log file: {:?}", e);
        }
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    init_logger();
    info!("========================================");
    info!("Process Guard Service starting...");
    info!("========================================");

    let running = Arc::new(Mutex::new(true));
    let running_clone = running.clone();
    let running_for_pipe = running.clone();
    let running_for_guardian = running.clone();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                info!("Received stop signal from service control manager");
                let mut running = running_clone.lock().unwrap();
                *running = false;
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .expect("Failed to register service control handler");

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });

    info!("Service status set to RUNNING");

    let guardian = Arc::new(Guardian::new(running_for_guardian));
    let guardian_for_pipe = guardian.clone();

    let guardian_handle = std::thread::spawn(move || {
        info!("Guardian thread started, entering run loop");
        guardian.run();
        info!("Guardian thread exited");
    });

    let pipe_server = PipeServer::new(guardian_for_pipe, running_for_pipe);
    let pipe_handle = std::thread::spawn(move || {
        info!("Pipe server thread started");
        pipe_server.run();
        info!("Pipe server thread exited");
    });

    info!("Service is now running and monitoring processes");

    loop {
        let r = running.lock().unwrap();
        if !*r {
            info!("Service main loop received stop signal");
            break;
        }
        drop(r);
        std::thread::sleep(Duration::from_millis(100));
    }

    info!("Service stopping...");

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
    info!("Process Guard Service stopped");
    info!("========================================");
}

pub fn run_service() -> Result<(), windows_service::Error> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

pub fn install_service(exe_path: &str) -> Result<(), String> {
    info!("Installing service from: {}", exe_path);

    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let service_manager =
        ServiceManager::local_computer(None::<&str>, manager_access).map_err(|e| {
            error!("Failed to connect to service manager: {:?}", e);
            format!("Failed to connect to service manager: {:?}", e)
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
            error!("Failed to create service: {:?}", e);
            format!("Failed to create service: {:?}", e)
        })?;

    info!("Service installed successfully");
    Ok(())
}

pub fn uninstall_service() -> Result<(), String> {
    info!("Uninstalling service");

    let manager_access = ServiceManagerAccess::CONNECT;
    let service_manager =
        ServiceManager::local_computer(None::<&str>, manager_access).map_err(|e| {
            error!("Failed to connect to service manager: {:?}", e);
            format!("Failed to connect to service manager: {:?}", e)
        })?;

    let service_access = ServiceAccess::DELETE | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS;
    let service = service_manager
        .open_service(SERVICE_NAME, service_access)
        .map_err(|e| {
            error!("Failed to open service: {:?}", e);
            format!("Failed to open service: {:?}", e)
        })?;

    let status = service.query_status().map_err(|e| {
        error!("Failed to query service status: {:?}", e);
        format!("Failed to query service status: {:?}", e)
    })?;

    if status.current_state == ServiceState::Running {
        info!("Service is running, stopping it first");
        service.stop().map_err(|e| {
            error!("Failed to stop service: {:?}", e);
            format!("Failed to stop service: {:?}", e)
        })?;

        std::thread::sleep(Duration::from_secs(2));
    }

    service.delete().map_err(|e| {
        error!("Failed to delete service: {:?}", e);
        format!("Failed to delete service: {:?}", e)
    })?;

    info!("Service uninstalled successfully");
    Ok(())
}

pub fn start_service() -> Result<(), String> {
    info!("Starting service");

    let manager_access = ServiceManagerAccess::CONNECT;
    let service_manager =
        ServiceManager::local_computer(None::<&str>, manager_access).map_err(|e| {
            error!("Failed to connect to service manager: {:?}", e);
            format!("Failed to connect to service manager: {:?}", e)
        })?;

    let service_access = ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let service = service_manager
        .open_service(SERVICE_NAME, service_access)
        .map_err(|e| {
            error!("Failed to open service: {:?}", e);
            format!("Failed to open service: {:?}", e)
        })?;

    service.start(&[] as &[OsString]).map_err(|e| {
        error!("Failed to start service: {:?}", e);
        format!("Failed to start service: {:?}", e)
    })?;

    info!("Service started successfully");
    Ok(())
}

pub fn stop_service() -> Result<(), String> {
    info!("Stopping service");

    let manager_access = ServiceManagerAccess::CONNECT;
    let service_manager =
        ServiceManager::local_computer(None::<&str>, manager_access).map_err(|e| {
            error!("Failed to connect to service manager: {:?}", e);
            format!("Failed to connect to service manager: {:?}", e)
        })?;

    let service_access = ServiceAccess::STOP | ServiceAccess::QUERY_STATUS;
    let service = service_manager
        .open_service(SERVICE_NAME, service_access)
        .map_err(|e| {
            error!("Failed to open service: {:?}", e);
            format!("Failed to open service: {:?}", e)
        })?;

    service.stop().map_err(|e| {
        error!("Failed to stop service: {:?}", e);
        format!("Failed to stop service: {:?}", e)
    })?;

    info!("Service stopped successfully");
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
