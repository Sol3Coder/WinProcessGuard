use log::{debug, error, info};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, MAX_PATH};
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TokenElevationType, TokenLinkedToken,
    TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, OpenProcess, TerminateProcess,
    CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    DETACHED_PROCESS, NORMAL_PRIORITY_CLASS, PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION,
    PROCESS_TERMINATE, STARTUPINFOW, STARTUPINFOW_FLAGS, PROCESS_VM_READ,
};

const MAXIMUM_ALLOWED: u32 = 0x02000000;
const SECURITY_IDENTIFICATION: i32 = 1;
const TOKEN_PRIMARY: i32 = 1;

#[repr(C)]
struct WTS_SESSION_INFO {
    session_id: u32,
    p_win_station_name: *mut u16,
    state: WTS_CONNECTSTATE_CLASS,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
enum WTS_CONNECTSTATE_CLASS {
    WTSActive,
    WTSConnected,
    WTSConnectQuery,
    WTSShadow,
    WTSDisconnected,
    WTSIdle,
    WTSListen,
    WTSReset,
    WTSDown,
    WTSInit,
}

#[link(name = "wtsapi32")]
extern "system" {
    fn WTSGetActiveConsoleSessionId() -> u32;
    fn WTSQueryUserToken(session_id: u32, ph_token: *mut HANDLE) -> u32;
    fn WTSEnumerateSessionsW(
        h_server: HANDLE,
        reserved: u32,
        version: u32,
        pp_session_info: *mut *mut WTS_SESSION_INFO,
        p_count: *mut u32,
    ) -> u32;
    fn WTSFreeMemory(p_memory: *mut std::ffi::c_void);
}

#[link(name = "advapi32")]
extern "system" {
    fn DuplicateTokenEx(
        h_existing_token: HANDLE,
        dw_desired_access: u32,
        lp_token_attributes: *mut std::ffi::c_void,
        token_impersonation_level: i32,
        token_type: i32,
        ph_new_token: *mut HANDLE,
    ) -> i32;
}

#[link(name = "userenv")]
extern "system" {
    fn CreateEnvironmentBlock(
        lp_environment: *mut *mut std::ffi::c_void,
        h_token: HANDLE,
        b_inherit: bool,
    ) -> i32;
    fn DestroyEnvironmentBlock(lp_environment: *mut std::ffi::c_void) -> i32;
}

pub struct ProcessInfo {
    pub process_id: u32,
    pub thread_id: u32,
    pub process_handle: HANDLE,
    pub thread_handle: HANDLE,
}

impl ProcessInfo {
    pub fn new() -> Self {
        Self {
            process_id: 0,
            thread_id: 0,
            process_handle: HANDLE::default(),
            thread_handle: HANDLE::default(),
        }
    }
}

impl Drop for ProcessInfo {
    fn drop(&mut self) {
        unsafe {
            if !self.process_handle.is_invalid() {
                let _ = CloseHandle(self.process_handle);
            }
            if !self.thread_handle.is_invalid() {
                let _ = CloseHandle(self.thread_handle);
            }
        }
    }
}

use std::io::{Read, Seek, SeekFrom};

const IMAGE_SUBSYSTEM_WINDOWS_CUI: u16 = 3;

struct RvaInfo {
    e_lfanew: u32,
}

fn read_dos_rva(exe_path: &str) -> Option<RvaInfo> {
    let mut file = std::fs::File::open(exe_path).ok()?;
    let mut dos_header = [0u8; 64];
    file.read_exact(&mut dos_header).ok()?;
    let e_lfanew = u32::from_le_bytes([
        dos_header[0x3C],
        dos_header[0x3D],
        dos_header[0x3E],
        dos_header[0x3F],
    ]);
    Some(RvaInfo { e_lfanew })
}

fn read_pe_subsystem(exe_path: &str) -> Option<u16> {
    let rva = read_dos_rva(exe_path)?;
    let mut file = std::fs::File::open(exe_path).ok()?;
    // PE sig (4) + COFF header (20) + subsystem offset in optional header (68) = 92
    file.seek(SeekFrom::Start(rva.e_lfanew as u64 + 4 + 20 + 68))
        .ok()?;
    let mut buf = [0u8; 2];
    file.read_exact(&mut buf).ok()?;
    Some(u16::from_le_bytes(buf))
}

fn to_wide_string(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn describe_token_elevation(token: HANDLE) -> String {
    unsafe {
        let mut returned = 0u32;
        let mut elevation = TOKEN_ELEVATION::default();
        let elevation_result = GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );

        let mut elevation_type = TOKEN_ELEVATION_TYPE(0);
        let elevation_type_result = GetTokenInformation(
            token,
            TokenElevationType,
            Some((&mut elevation_type as *mut TOKEN_ELEVATION_TYPE).cast()),
            std::mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut returned,
        );

        let elevated = if elevation_result.is_ok() {
            if elevation.TokenIsElevated != 0 {
                "elevated"
            } else {
                "not-elevated"
            }
        } else {
            "elevation-query-failed"
        };

        let elevation_type_name = if elevation_type_result.is_ok() {
            match elevation_type.0 {
                1 => "default",
                2 => "full",
                3 => "limited",
                _ => "unknown",
            }
        } else {
            "type-query-failed"
        };

        format!("{}, type={}", elevated, elevation_type_name)
    }
}

fn should_prefer_linked_token(elevated: bool, elevation_type_raw: i32) -> bool {
    !elevated && elevation_type_raw == 3
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenLaunchSource {
    Original,
    Linked,
}

fn choose_token_launch_source(
    elevated: bool,
    elevation_type_raw: i32,
    linked_token_available: bool,
) -> TokenLaunchSource {
    if should_prefer_linked_token(elevated, elevation_type_raw) && linked_token_available {
        TokenLaunchSource::Linked
    } else {
        TokenLaunchSource::Original
    }
}

fn read_token_elevation_state(token: HANDLE) -> (bool, i32) {
    unsafe {
        let mut returned = 0u32;
        let mut elevation = TOKEN_ELEVATION::default();
        let elevation_result = GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );

        let mut elevation_type = TOKEN_ELEVATION_TYPE(0);
        let elevation_type_result = GetTokenInformation(
            token,
            TokenElevationType,
            Some((&mut elevation_type as *mut TOKEN_ELEVATION_TYPE).cast()),
            std::mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut returned,
        );

        let elevated = elevation_result.is_ok() && elevation.TokenIsElevated != 0;
        let elevation_type_raw = if elevation_type_result.is_ok() {
            elevation_type.0
        } else {
            0
        };

        (elevated, elevation_type_raw)
    }
}

fn try_get_linked_token(token: HANDLE) -> Option<HANDLE> {
    unsafe {
        let mut returned = 0u32;
        let mut linked = TOKEN_LINKED_TOKEN::default();
        let result = GetTokenInformation(
            token,
            TokenLinkedToken,
            Some((&mut linked as *mut TOKEN_LINKED_TOKEN).cast()),
            std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut returned,
        );

        if result.is_ok() && !linked.LinkedToken.is_invalid() {
            Some(linked.LinkedToken)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{should_prefer_linked_token, TokenLaunchSource, choose_token_launch_source};

    #[test]
    fn prefers_linked_token_for_limited_non_elevated_admin_token() {
        assert!(should_prefer_linked_token(false, 3));
    }

    #[test]
    fn does_not_prefer_linked_token_for_full_elevated_token() {
        assert!(!should_prefer_linked_token(true, 2));
    }

    #[test]
    fn does_not_prefer_linked_token_for_default_token_type() {
        assert!(!should_prefer_linked_token(false, 1));
    }

    #[test]
    fn chooses_linked_launch_source_for_limited_token_when_linked_token_exists() {
        assert_eq!(
            choose_token_launch_source(false, 3, true),
            TokenLaunchSource::Linked
        );
    }

    #[test]
    fn falls_back_to_original_launch_source_when_linked_token_is_missing() {
        assert_eq!(
            choose_token_launch_source(false, 3, false),
            TokenLaunchSource::Original
        );
    }
}

fn get_active_session_id() -> u32 {
    unsafe {
        let session_id = WTSGetActiveConsoleSessionId();
        if session_id != 0xFFFFFFFF {
            debug!("活动控制台会话ID: {}", session_id);
            return session_id;
        }

        let mut session_info: *mut WTS_SESSION_INFO = ptr::null_mut();
        let mut count: u32 = 0;

        let result = WTSEnumerateSessionsW(HANDLE::default(), 0, 1, &mut session_info, &mut count);

        if result != 0 && !session_info.is_null() {
            let sessions = std::slice::from_raw_parts(session_info, count as usize);
            for session in sessions {
                if session.state == WTS_CONNECTSTATE_CLASS::WTSActive {
                    let active_id = session.session_id;
                    WTSFreeMemory(session_info as *mut std::ffi::c_void);
                    debug!("通过枚举找到活动会话: {}", active_id);
                    return active_id;
                }
            }
            WTSFreeMemory(session_info as *mut std::ffi::c_void);
        }

        error!("获取活动会话ID失败");
        0xFFFFFFFF
    }
}

pub fn start_process_in_session0(
    exe_path: &str,
    working_dir: Option<&str>,
    args: Option<&str>,
    minimize: bool,
    no_window: bool,
    use_env_block: bool,
) -> Result<ProcessInfo, String> {
    start_process_internal(exe_path, working_dir, args, minimize, no_window, use_env_block, true, false, true)
}

/// Diagnostic: creates process suspended for crash isolation
pub fn start_process_suspended(
    exe_path: &str,
    working_dir: Option<&str>,
    args: Option<&str>,
    minimize: bool,
    no_window: bool,
    use_env_block: bool,
    set_desktop: bool,
) -> Result<ProcessInfo, String> {
    start_process_internal(exe_path, working_dir, args, minimize, no_window, use_env_block, set_desktop, true, true)
}

/// Launch using the raw WTS token (no DuplicateTokenEx)
pub fn start_process_with_raw_token(
    exe_path: &str,
    working_dir: Option<&str>,
    args: Option<&str>,
    minimize: bool,
    no_window: bool,
) -> Result<ProcessInfo, String> {
    start_process_internal(exe_path, working_dir, args, minimize, no_window, true, true, false, false)
}

fn start_process_internal(
    exe_path: &str,
    working_dir: Option<&str>,
    args: Option<&str>,
    minimize: bool,
    no_window: bool,
    use_env_block: bool,
    set_desktop: bool,
    suspended: bool,
    use_duplication: bool,
) -> Result<ProcessInfo, String> {
    unsafe {
        let mut process_info = ProcessInfo::new();
        let mut h_token = HANDLE::default();
        let mut h_linked_token = HANDLE::default();
        let mut h_dup_token = HANDLE::default();
        let mut p_env: *mut std::ffi::c_void = ptr::null_mut();

        let session_id = get_active_session_id();
        if session_id == 0xFFFFFFFF {
            return Err("获取活动会话ID失败".to_string());
        }

        info!("正在会话 {} 中启动进程, 路径: {}", session_id, exe_path);

        let query_result = WTSQueryUserToken(session_id, &mut h_token);
        if query_result == 0 {
            let err = windows::core::Error::from_win32();
            error!("WTSQueryUserToken 失败: {:?}", err);
            return Err(format!("WTSQueryUserToken 失败: {:?}", err));
        }
        info!("Session {} user token diagnostics before duplication: {}", session_id, describe_token_elevation(h_token));

        let (is_elevated, elevation_type_raw) = read_token_elevation_state(h_token);
        let linked_token = if should_prefer_linked_token(is_elevated, elevation_type_raw) {
            try_get_linked_token(h_token)
        } else {
            None
        };
        let launch_source = choose_token_launch_source(
            is_elevated,
            elevation_type_raw,
            linked_token.is_some(),
        );
        let duplicate_source_token = match launch_source {
            TokenLaunchSource::Linked => {
                h_linked_token = linked_token.expect("linked token should exist for linked launch source");
                info!(
                    "Session {} using linked elevated token for process launch: {}",
                    session_id,
                    describe_token_elevation(h_linked_token)
                );
                h_linked_token
            }
            TokenLaunchSource::Original => {
                if should_prefer_linked_token(is_elevated, elevation_type_raw) {
                    info!(
                        "Session {} token is limited but linked token is unavailable; falling back to original token",
                        session_id
                    );
                }
                h_token
            }
        };

        if use_duplication {
            let dup_result = DuplicateTokenEx(
                duplicate_source_token,
                MAXIMUM_ALLOWED,
                ptr::null_mut(),
                SECURITY_IDENTIFICATION,
                TOKEN_PRIMARY,
                &mut h_dup_token,
            );

            if dup_result == 0 {
                let err = windows::core::Error::from_win32();
                let _ = CloseHandle(h_token);
                if !h_linked_token.is_invalid() {
                    let _ = CloseHandle(h_linked_token);
                }
                error!("DuplicateTokenEx 失败: {:?}", err);
                return Err(format!("DuplicateTokenEx 失败: {:?}", err));
            }
            info!("Session {} duplicated primary token diagnostics: {}", session_id, describe_token_elevation(h_dup_token));
        } else {
            // Use the raw WTS token directly
            info!("Session {} using raw WTS token (no duplication), diagnostics: {}", session_id, describe_token_elevation(duplicate_source_token));
        }

        let effective_token = if use_duplication { h_dup_token } else { duplicate_source_token };

        let env_result = if use_env_block {
            CreateEnvironmentBlock(&mut p_env, effective_token, false)
        } else {
            1 // non-zero = success, skip env block (pass NULL to inherit)
        };
        if env_result == 0 {
            let _ = CloseHandle(h_token);
            if !h_linked_token.is_invalid() {
                let _ = CloseHandle(h_linked_token);
            }
            if use_duplication {
                let _ = CloseHandle(h_dup_token);
            }
            error!("CreateEnvironmentBlock 失败");
            return Err("CreateEnvironmentBlock 失败".to_string());
        }

        if !use_env_block {
            info!("Skipping environment block, process will inherit service environment");
        }

        let mut startup_info: STARTUPINFOW = std::mem::zeroed();
        startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

        let desktop;
        if set_desktop {
            desktop = to_wide_string("winsta0\\default");
            startup_info.lpDesktop = PWSTR(desktop.as_ptr() as *mut u16);
            debug!("Setting lpDesktop to winsta0\\default");
        } else {
            debug!("Leaving lpDesktop as NULL (inherit default)");
        }

        if minimize {
            startup_info.dwFlags = STARTUPINFOW_FLAGS(0x00000001);
            startup_info.wShowWindow = 2;
        }

        let subsystem = read_pe_subsystem(exe_path);
        let mut creation_flags = NORMAL_PRIORITY_CLASS;
        if use_env_block {
            creation_flags |= CREATE_UNICODE_ENVIRONMENT;
        }
        if suspended {
            creation_flags |= CREATE_SUSPENDED;
            debug!("Using CREATE_SUSPENDED for diagnostic process creation");
        }
        if no_window {
            creation_flags |= CREATE_NO_WINDOW;
        } else {
            match subsystem {
                Some(IMAGE_SUBSYSTEM_WINDOWS_CUI) => {
                    creation_flags |= CREATE_NEW_CONSOLE;
                    debug!("exe is console subsystem, using CREATE_NEW_CONSOLE");
                }
                _ => {
                    creation_flags |= DETACHED_PROCESS;
                    debug!("exe is GUI subsystem (or unknown), using DETACHED_PROCESS");
                }
            }
        }

        let mut cmd_line: Vec<u16> = if let Some(a) = args {
            let cmd = format!("\"{}\" {}", exe_path, a);
            to_wide_string(&cmd)
        } else {
            to_wide_string(&format!("\"{}\"", exe_path))
        };

        let cwd_wide = working_dir.map(|d| to_wide_string(d));
        let cwd_ptr = cwd_wide
            .as_ref()
            .map(|w| PCWSTR(w.as_ptr()))
            .unwrap_or(PCWSTR::null());

        let mut proc_info: PROCESS_INFORMATION = std::mem::zeroed();

        let env_arg: Option<*const std::ffi::c_void> = if use_env_block {
            Some(p_env as *const std::ffi::c_void)
        } else {
            None
        };

        // Pass NULL for lpApplicationName — let Windows resolve the exe from the
        // command line. Passing both lpApplicationName AND the exe path in lpCommandLine
        // can confuse some runtimes (e.g. Flutter/Dart) during initialization.
        let create_result = CreateProcessAsUserW(
            effective_token,
            PCWSTR::null(),
            PWSTR(cmd_line.as_mut_ptr()),
            None,
            None,
            false,
            creation_flags,
            env_arg,
            cwd_ptr,
            &mut startup_info,
            &mut proc_info,
        );

        if use_env_block {
            let _ = DestroyEnvironmentBlock(p_env);
        }
        let _ = CloseHandle(h_token);
        if !h_linked_token.is_invalid() {
            let _ = CloseHandle(h_linked_token);
        }
        if use_duplication {
            let _ = CloseHandle(h_dup_token);
        }

        if create_result.is_err() {
            let err = windows::core::Error::from_win32();
            error!("CreateProcessAsUserW 失败: {:?}", err);
            return Err(format!("CreateProcessAsUserW 失败: {:?}", err));
        }

        process_info.process_id = proc_info.dwProcessId;
        process_info.thread_id = proc_info.dwThreadId;
        process_info.process_handle = proc_info.hProcess;
        process_info.thread_handle = proc_info.hThread;

        info!(
            "已在会话0中启动进程: {} (PID: {})",
            exe_path, process_info.process_id
        );

        Ok(process_info)
    }
}

pub fn check_process_alive(process_id: u32) -> bool {
    get_process_status(process_id).map_or(false, |s| s.is_alive())
}

pub struct ProcessStatus {
    pub alive: bool,
    pub exit_code: u32,
}

impl ProcessStatus {
    pub fn is_alive(&self) -> bool {
        self.alive
    }
}

const STILL_ACTIVE: u32 = 259;

pub fn get_process_status(process_id: u32) -> Option<ProcessStatus> {
    if process_id == 0 {
        return None;
    }

    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_INFORMATION, false, process_id) {
            Ok(h) => h,
            Err(_) => return None,
        };

        if handle.is_invalid() {
            return None;
        }

        let mut exit_code: u32 = 0;
        let result = GetExitCodeProcess(handle, &mut exit_code);
        let _ = CloseHandle(handle);

        if result.is_err() {
            return None;
        }

        Some(ProcessStatus {
            alive: exit_code == STILL_ACTIVE,
            exit_code,
        })
    }
}

pub fn kill_process(process_id: u32) -> bool {
    if process_id == 0 {
        return true;
    }

    info!("正在终止进程, PID: {}", process_id);

    unsafe {
        let handle = match OpenProcess(PROCESS_TERMINATE, false, process_id) {
            Ok(h) => h,
            Err(_) => {
                debug!("进程 {} 未找到或已终止", process_id);
                return true;
            }
        };

        if handle.is_invalid() {
            debug!("进程 {} 未找到或已终止", process_id);
            return true;
        }

        let result = TerminateProcess(handle, 0);
        let _ = CloseHandle(handle);

        if result.is_ok() {
            info!("进程 {} 终止成功", process_id);
        }
        
        result.is_ok()
    }
}

pub fn find_process_by_name(process_name: &str) -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    debug!("正在按名称搜索进程: {}", process_name);

    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return None,
        };

        if snapshot.is_invalid() {
            return None;
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut result = Process32FirstW(snapshot, &mut entry);
        let target_name = process_name.to_lowercase();

        while result.is_ok() {
            let exe_name = String::from_utf16_lossy(
                &entry.szExeFile
                    [..entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len())],
            );

            if exe_name.to_lowercase() == target_name
                || exe_name.to_lowercase().ends_with(&format!("\\{}", target_name))
            {
                let pid = entry.th32ProcessID;
                let _ = CloseHandle(snapshot);
                debug!("找到进程 {} , PID: {}", process_name, pid);
                return Some(pid);
            }

            result = Process32NextW(snapshot, &mut entry);
        }

        let _ = CloseHandle(snapshot);
        debug!("未找到进程 {}", process_name);
        None
    }
}

pub fn find_process_by_path(exe_path: &str) -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::ProcessStatus::GetModuleFileNameExW;

    debug!("正在按路径搜索进程: {}", exe_path);

    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return None,
        };

        if snapshot.is_invalid() {
            return None;
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut result = Process32FirstW(snapshot, &mut entry);
        let target_path = exe_path.to_lowercase();

        while result.is_ok() {
            let handle = OpenProcess(
                PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                false,
                entry.th32ProcessID,
            );

            if let Ok(handle) = handle {
                if !handle.is_invalid() {
                    let mut buffer = [0u16; MAX_PATH as usize];
                    let len = GetModuleFileNameExW(handle, HMODULE::default(), &mut buffer);
                    let _ = CloseHandle(handle);

                    if len > 0 {
                        let path = String::from_utf16_lossy(&buffer[..len as usize]);
                        if path.to_lowercase() == target_path {
                            let pid = entry.th32ProcessID;
                            let _ = CloseHandle(snapshot);
                            debug!("找到路径为 {} 的进程, PID: {}", exe_path, pid);
                            return Some(pid);
                        }
                    }
                }
            }

            result = Process32NextW(snapshot, &mut entry);
        }

        let _ = CloseHandle(snapshot);
        debug!("未找到路径为 {} 的进程", exe_path);
        None
    }
}

// ── Windows Service management ──────────────────────────────────────────

/// Check whether a Windows service is in the SERVICE_RUNNING state.
pub fn check_service_running(service_name: &str) -> bool {
    use windows::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
        SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS,
    };
    use windows::core::PCWSTR;

    unsafe {
        let scm = OpenSCManagerW(
            PCWSTR::null(),
            PCWSTR::null(),
            SC_MANAGER_CONNECT,
        );
        if scm.is_err() {
            return false;
        }
        let scm = scm.unwrap();
        if scm.is_invalid() {
            return false;
        }

        let name_wide: Vec<u16> = std::ffi::OsStr::new(service_name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let svc = OpenServiceW(scm, PCWSTR(name_wide.as_ptr()), SERVICE_QUERY_STATUS);
        if svc.is_err() {
            let _ = CloseServiceHandle(scm);
            return false;
        }
        let svc = svc.unwrap();
        if svc.is_invalid() {
            let _ = CloseServiceHandle(scm);
            return false;
        }

        let mut status = SERVICE_STATUS::default();
        let result = QueryServiceStatus(svc, &mut status);

        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);

        result.is_ok() && status.dwCurrentState == SERVICE_RUNNING
    }
}

/// Start a Windows service by name. Returns Ok once the start command is
/// sent successfully (does NOT wait for the service to reach running state).
pub fn start_windows_service(service_name: &str) -> Result<(), String> {
    use windows::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, StartServiceW,
        SC_MANAGER_CONNECT, SERVICE_START,
    };
    use windows::core::PCWSTR;

    unsafe {
        let scm = OpenSCManagerW(
            PCWSTR::null(),
            PCWSTR::null(),
            SC_MANAGER_CONNECT,
        );
        if scm.is_err() {
            return Err(format!("OpenSCManagerW failed: {:?}", scm.unwrap_err()));
        }
        let scm = scm.unwrap();
        if scm.is_invalid() {
            return Err("OpenSCManagerW returned invalid handle".to_string());
        }

        let name_wide: Vec<u16> = std::ffi::OsStr::new(service_name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let svc = OpenServiceW(scm, PCWSTR(name_wide.as_ptr()), SERVICE_START);
        if svc.is_err() {
            let _ = CloseServiceHandle(scm);
            return Err(format!("OpenServiceW failed for '{}'", service_name));
        }
        let svc = svc.unwrap();
        if svc.is_invalid() {
            let _ = CloseServiceHandle(scm);
            return Err(format!("Service '{}' not found", service_name));
        }

        let result = StartServiceW(svc, None);
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);

        if result.is_err() {
            let err = windows::core::Error::from_win32();
            return Err(format!("StartServiceW failed for '{}': {:?}", service_name, err));
        }
        Ok(())
    }
}

/// Ensure a Windows service is running. If it is already running the call
/// returns immediately. Otherwise it attempts to start the service and
/// polls every second for up to 30 seconds waiting for SERVICE_RUNNING.
pub fn ensure_service_running(service_name: &str) -> Result<(), String> {
    if check_service_running(service_name) {
        debug!("Service '{}' is already running", service_name);
        return Ok(());
    }

    info!("Service '{}' is not running, attempting to start", service_name);
    start_windows_service(service_name)?;

    // Wait for the service to reach the running state.
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if check_service_running(service_name) {
            info!("Service '{}' started successfully", service_name);
            return Ok(());
        }
    }

    Err(format!(
        "Service '{}' did not reach running state within 30 seconds",
        service_name
    ))
}
