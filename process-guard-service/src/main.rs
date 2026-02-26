mod config;
mod guardian;
mod models;
mod pipe_server;
mod service;
mod session0;

use std::env;

fn print_usage() {
    println!("Process Guard Service - Windows Process Guardian");
    println!();
    println!("Usage:");
    println!("  process-guard-service.exe              Run as Windows service");
    println!("  process-guard-service.exe --install    Install the service");
    println!("  process-guard-service.exe --uninstall  Uninstall the service");
    println!("  process-guard-service.exe --start      Start the service");
    println!("  process-guard-service.exe --stop       Stop the service");
    println!("  process-guard-service.exe --status     Check service status");
    println!("  process-guard-service.exe --help       Show this help message");
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 {
        match args[1].as_str() {
            "--install" => {
                let exe_path = env::current_exe()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "process-guard-service.exe".to_string());
                
                match service::install_service(&exe_path) {
                    Ok(_) => println!("Service installed successfully"),
                    Err(e) => eprintln!("Failed to install service: {}", e),
                }
            }
            "--uninstall" => {
                match service::uninstall_service() {
                    Ok(_) => println!("Service uninstalled successfully"),
                    Err(e) => eprintln!("Failed to uninstall service: {}", e),
                }
            }
            "--start" => {
                match service::start_service() {
                    Ok(_) => println!("Service started successfully"),
                    Err(e) => eprintln!("Failed to start service: {}", e),
                }
            }
            "--stop" => {
                match service::stop_service() {
                    Ok(_) => println!("Service stopped successfully"),
                    Err(e) => eprintln!("Failed to stop service: {}", e),
                }
            }
            "--status" => {
                let installed = service::is_service_installed();
                let running = service::is_service_running();
                
                println!("Service Status:");
                println!("  Installed: {}", if installed { "Yes" } else { "No" });
                println!("  Running: {}", if running { "Yes" } else { "No" });
            }
            "--help" | "-h" | "/?" => {
                print_usage();
            }
            _ => {
                eprintln!("Unknown argument: {}", args[1]);
                print_usage();
            }
        }
    } else {
        match service::run_service() {
            Ok(_) => {}
            Err(e) => {
                eprintln!("Service error: {:?}", e);
                eprintln!("Note: This program should be run as a Windows service.");
                eprintln!("Use --install to install the service first.");
            }
        }
    }
}
