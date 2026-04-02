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

        info!("正在启动管道服务: {}", pipe_name);

        loop {
            let running = *self.running.lock().unwrap();
            if !running {
                info!("管道服务正在停止");
                break;
            }

            let pipe_handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(pipe_name_wide.as_ptr()),
                    FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX),
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    MAX_INSTANCES,
                    BUFFER_SIZE,
                    BUFFER_SIZE,
                    TIMEOUT_MS,
                    None,
                )
            };

            if pipe_handle.is_invalid() {
                error!("创建命名管道失败");
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }

            //  debug!("等待客户端连接...");

            let connect_result = unsafe { ConnectNamedPipe(pipe_handle, None) };

            if connect_result.is_err() {
                let err = windows::core::Error::from_win32();
                if err.code() != windows::Win32::Foundation::ERROR_PIPE_CONNECTED.into() {
                    error!("连接命名管道失败: {:?}", err);
                    unsafe {
                        let _ = CloseHandle(pipe_handle);
                    }
                    continue;
                }
            }

            //   info!("客户端已连接到管道服务");

            let mut buffer = vec![0u8; BUFFER_SIZE as usize];
            let mut bytes_read: u32 = 0;

            let read_result =
                unsafe { ReadFile(pipe_handle, Some(&mut buffer), Some(&mut bytes_read), None) };

            if read_result.is_err() || bytes_read == 0 {
                debug!("从管道读取失败或请求为空");
                unsafe {
                    let _ = DisconnectNamedPipe(pipe_handle);
                    let _ = CloseHandle(pipe_handle);
                }
                continue;
            }

            let request_data = String::from_utf8_lossy(&buffer[..bytes_read as usize]);
            //    info!("接收到请求: {}", request_data);

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
                error!("向管道写入响应失败");
            } else {
                // debug!("响应已发送: {}", response_data);
            }

            unsafe {
                let _ = DisconnectNamedPipe(pipe_handle);
                let _ = CloseHandle(pipe_handle);
            }

            //  info!("客户端已从管道服务断开");
        }

        info!("管道服务已停止");
    }

    fn handle_request(&self, request_data: &str) -> PipeResponse {
        let request: PipeRequest = match serde_json::from_str(request_data) {
            Ok(r) => r,
            Err(e) => {
                error!("解析请求失败: {}", e);
                return PipeResponse::error(&format!("JSON格式错误: {}", e));
            }
        };

        //  info!("正在处理请求类型: {}", request.request_type);

        match request.request_type.as_str() {
            "heartbeat" => self.handle_heartbeat(&request),
            "add" => self.handle_add(&request),
            "update" => self.handle_update(&request),
            "remove" => self.handle_remove(&request),
            "stop" => self.handle_stop(&request),
            "start" => self.handle_start(&request),
            "list" => self.handle_list(),
            "status" => self.handle_status(),
            _ => PipeResponse::error(&format!("未知的请求类型: {}", request.request_type)),
        }
    }

    fn handle_heartbeat(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(item_id) = &request.item_id {
            if self.guardian.update_heartbeat(item_id) {
                //    debug!("监控项心跳已更新: {}", item_id);
                PipeResponse::success("心跳已更新")
            } else {
                error!("心跳更新失败, 未找到监控项: {}", item_id);
                PipeResponse::error("未找到监控项")
            }
        } else {
            PipeResponse::error("缺少item_id")
        }
    }

    fn handle_add(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(config) = &request.config {
            info!("正在添加监控项: {} ({})", config.name, config.exe_path);

            let config_arc = self.guardian.get_config();
            let mut cfg = config_arc.lock().unwrap();

            if cfg.items.iter().any(|i| i.id == config.id) {
                error!("ID为 {} 的监控项已存在", config.id);
                return PipeResponse::error("该ID的监控项已存在");
            }

            if cfg
                .items
                .iter()
                .any(|i| i.exe_path.to_lowercase() == config.exe_path.to_lowercase())
            {
                error!("可执行文件路径已被监控: {}", config.exe_path);
                return PipeResponse::error("可执行文件路径已被监控");
            }

            cfg.items.push(config.clone());

            if let Err(e) = crate::config::save_config(&cfg) {
                error!("保存配置失败: {}", e);
                return PipeResponse::error(&format!("保存配置失败: {}", e));
            }

            drop(cfg);

            let change = ConfigChange {
                item: config.clone(),
                change_type: ChangeType::Start,
            };
            self.guardian.add_change(change);

            info!("监控项添加成功: {} ({})", config.name, config.id);
            PipeResponse::success("监控项已添加")
        } else {
            PipeResponse::error("缺少配置")
        }
    }

    fn handle_update(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(config) = &request.config {
            info!("正在更新监控项: {} ({})", config.name, config.id);

            let config_arc = self.guardian.get_config();
            let mut cfg = config_arc.lock().unwrap();

            if let Some(existing) = cfg.items.iter_mut().find(|i| i.id == config.id) {
                *existing = config.clone();

                if let Err(e) = crate::config::save_config(&cfg) {
                    error!("保存配置失败: {}", e);
                    return PipeResponse::error(&format!("保存配置失败: {}", e));
                }

                drop(cfg);

                let change = ConfigChange {
                    item: config.clone(),
                    change_type: ChangeType::Stop | ChangeType::Start,
                };
                self.guardian.add_change(change);

                info!("监控项更新成功: {} ({})", config.name, config.id);
                PipeResponse::success("监控项已更新")
            } else {
                error!("未找到要更新的监控项: {}", config.id);
                PipeResponse::error("未找到监控项")
            }
        } else {
            PipeResponse::error("缺少配置")
        }
    }

    fn handle_remove(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(id) = &request.id {
            info!("正在移除监控项: {}", id);

            let config_arc = self.guardian.get_config();
            let mut cfg = config_arc.lock().unwrap();

            let item = cfg.items.iter().find(|i| &i.id == id).cloned();

            if let Some(item) = item {
                cfg.items.retain(|i| &i.id != id);

                if let Err(e) = crate::config::save_config(&cfg) {
                    error!("保存配置失败: {}", e);
                    return PipeResponse::error(&format!("保存配置失败: {}", e));
                }

                drop(cfg);

                let change = ConfigChange {
                    item,
                    change_type: ChangeType::Stop | ChangeType::Remove,
                };
                self.guardian.add_change(change);

                info!("监控项移除成功: {}", id);
                PipeResponse::success("监控项已移除")
            } else {
                error!("未找到要移除的监控项: {}", id);
                PipeResponse::error("未找到监控项")
            }
        } else {
            PipeResponse::error("缺少id")
        }
    }

    fn handle_stop(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(id) = &request.id {
            info!("正在停止监控项: {}", id);

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

                info!("监控项停止命令已发送: {}", id);
                PipeResponse::success("监控项已停止")
            } else {
                error!("未找到要停止的监控项: {}", id);
                PipeResponse::error("未找到监控项")
            }
        } else {
            PipeResponse::error("缺少id")
        }
    }

    fn handle_start(&self, request: &PipeRequest) -> PipeResponse {
        if let Some(id) = &request.id {
            info!("正在启动监控项: {}", id);

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

                info!("监控项启动命令已发送: {}", id);
                PipeResponse::success("监控项已启动")
            } else {
                error!("未找到要启动的监控项: {}", id);
                PipeResponse::error("未找到监控项")
            }
        } else {
            PipeResponse::error("缺少id")
        }
    }

    fn handle_list(&self) -> PipeResponse {
        debug!("正在列出所有监控项");

        let config_arc = self.guardian.get_config();
        let cfg = config_arc.lock().unwrap();
        let items = serde_json::to_value(&cfg.items).unwrap_or(serde_json::json!([]));

        debug!("找到 {} 个监控项", cfg.items.len());
        PipeResponse::success_with_data("监控项列表", items)
    }

    fn handle_status(&self) -> PipeResponse {
        debug!("正在获取服务状态");

        let status = self.guardian.get_status();
        PipeResponse::success_with_data("服务状态", status)
    }
}
