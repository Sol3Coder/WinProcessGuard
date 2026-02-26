use log::{debug, error, info};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, MAX_PATH};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, OpenProcess, TerminateProcess,
    CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, NORMAL_PRIORITY_CLASS,
    PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_TERMINATE,
    STARTUPINFOW, STARTUPINFOW_FLAGS, PROCESS_VM_READ,
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

fn to_wide_string(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn get_active_session_id() -> u32 {
    unsafe {
        let session_id = WTSGetActiveConsoleSessionId();
        if session_id != 0xFFFFFFFF {
            debug!("Active console session ID: {}", session_id);
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
                    debug!("Found active session via enumeration: {}", active_id);
                    return active_id;
                }
            }
            WTSFreeMemory(session_info as *mut std::ffi::c_void);
        }

        error!("Failed to get active session ID");
        0xFFFFFFFF
    }
}

pub fn start_process_in_session0(
    exe_path: &str,
    working_dir: Option<&str>,
    args: Option<&str>,
    minimize: bool,
    no_window: bool,
) -> Result<ProcessInfo, String> {
    unsafe {
        let mut process_info = ProcessInfo::new();
        let mut h_token = HANDLE::default();
        let mut h_dup_token = HANDLE::default();
        let mut p_env: *mut std::ffi::c_void = ptr::null_mut();

        let session_id = get_active_session_id();
        if session_id == 0xFFFFFFFF {
            return Err("Failed to get active session ID".to_string());
        }

        info!("Starting process in session {}, path: {}", session_id, exe_path);

        let query_result = WTSQueryUserToken(session_id, &mut h_token);
        if query_result == 0 {
            let err = windows::core::Error::from_win32();
            error!("WTSQueryUserToken failed: {:?}", err);
            return Err(format!("WTSQueryUserToken failed: {:?}", err));
        }

        let dup_result = DuplicateTokenEx(
            h_token,
            MAXIMUM_ALLOWED,
            ptr::null_mut(),
            SECURITY_IDENTIFICATION,
            TOKEN_PRIMARY,
            &mut h_dup_token,
        );

        if dup_result == 0 {
            let err = windows::core::Error::from_win32();
            let _ = CloseHandle(h_token);
            error!("DuplicateTokenEx failed: {:?}", err);
            return Err(format!("DuplicateTokenEx failed: {:?}", err));
        }

        let env_result = CreateEnvironmentBlock(&mut p_env, h_dup_token, false);
        if env_result == 0 {
            let _ = CloseHandle(h_token);
            let _ = CloseHandle(h_dup_token);
            error!("CreateEnvironmentBlock failed");
            return Err("CreateEnvironmentBlock failed".to_string());
        }

        let mut startup_info: STARTUPINFOW = std::mem::zeroed();
        startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

        let desktop = to_wide_string("winsta0\\default");
        startup_info.lpDesktop = PWSTR(desktop.as_ptr() as *mut u16);

        if minimize {
            startup_info.dwFlags = STARTUPINFOW_FLAGS(0x00000001);
            startup_info.wShowWindow = 2;
        }

        let mut creation_flags = CREATE_UNICODE_ENVIRONMENT | NORMAL_PRIORITY_CLASS;
        if no_window {
            creation_flags |= CREATE_NO_WINDOW;
        } else {
            creation_flags |= CREATE_NEW_CONSOLE;
        }

        let exe_wide = to_wide_string(exe_path);
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

        let create_result = CreateProcessAsUserW(
            h_dup_token,
            PCWSTR(exe_wide.as_ptr()),
            PWSTR(cmd_line.as_mut_ptr()),
            None,
            None,
            false,
            creation_flags,
            Some(p_env),
            cwd_ptr,
            &mut startup_info,
            &mut proc_info,
        );

        let _ = DestroyEnvironmentBlock(p_env);
        let _ = CloseHandle(h_token);
        let _ = CloseHandle(h_dup_token);

        if create_result.is_err() {
            let err = windows::core::Error::from_win32();
            error!("CreateProcessAsUserW failed: {:?}", err);
            return Err(format!("CreateProcessAsUserW failed: {:?}", err));
        }

        process_info.process_id = proc_info.dwProcessId;
        process_info.thread_id = proc_info.dwThreadId;
        process_info.process_handle = proc_info.hProcess;
        process_info.thread_handle = proc_info.hThread;

        info!(
            "Started process in session 0: {} (PID: {})",
            exe_path, process_info.process_id
        );

        Ok(process_info)
    }
}

pub fn check_process_alive(process_id: u32) -> bool {
    if process_id == 0 {
        return false;
    }

    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_INFORMATION, false, process_id) {
            Ok(h) => h,
            Err(_) => return false,
        };

        if handle.is_invalid() {
            return false;
        }

        let mut exit_code: u32 = 0;
        let result = GetExitCodeProcess(handle, &mut exit_code);
        let _ = CloseHandle(handle);

        result.is_ok() && exit_code == 259
    }
}

pub fn kill_process(process_id: u32) -> bool {
    if process_id == 0 {
        return true;
    }

    info!("Killing process with PID: {}", process_id);

    unsafe {
        let handle = match OpenProcess(PROCESS_TERMINATE, false, process_id) {
            Ok(h) => h,
            Err(_) => {
                debug!("Process {} not found or already terminated", process_id);
                return true;
            }
        };

        if handle.is_invalid() {
            debug!("Process {} not found or already terminated", process_id);
            return true;
        }

        let result = TerminateProcess(handle, 0);
        let _ = CloseHandle(handle);

        if result.is_ok() {
            info!("Process {} terminated successfully", process_id);
        }
        
        result.is_ok()
    }
}

pub fn find_process_by_name(process_name: &str) -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    debug!("Searching for process by name: {}", process_name);

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
                debug!("Found process {} with PID: {}", process_name, pid);
                return Some(pid);
            }

            result = Process32NextW(snapshot, &mut entry);
        }

        let _ = CloseHandle(snapshot);
        debug!("Process {} not found", process_name);
        None
    }
}

pub fn find_process_by_path(exe_path: &str) -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::ProcessStatus::GetModuleFileNameExW;

    debug!("Searching for process by path: {}", exe_path);

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
                            debug!("Found process at {} with PID: {}", exe_path, pid);
                            return Some(pid);
                        }
                    }
                }
            }

            result = Process32NextW(snapshot, &mut entry);
        }

        let _ = CloseHandle(snapshot);
        debug!("Process at path {} not found", exe_path);
        None
    }
}
