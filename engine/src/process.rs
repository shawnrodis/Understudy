//! Process/app control: list running processes, launch an app, kill a
//! process by pid. Windows uses Toolhelp32 + CreateProcessW +
//! TerminateProcess; other platforms use `/proc` for listing and the
//! `kill` helper for termination (dev path only — the engine is a Windows
//! program and its Windows paths are the real ones).

#[cfg(not(windows))]
use std::process::Command;

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
}

pub fn list_processes() -> Result<Vec<ProcessInfo>, String> {
    #[cfg(windows)]
    {
        list_windows()
    }
    #[cfg(not(windows))]
    {
        list_procfs()
    }
}

pub fn launch_app(path: &str, args: &[String]) -> Result<u32, String> {
    #[cfg(windows)]
    {
        launch_windows(path, args)
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new(path);
        cmd.args(args);
        let child = cmd
            .spawn()
            .map_err(|e| format!("cannot launch {}: {}", path, e))?;
        Ok(child.id())
    }
}

pub fn kill_process(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        kill_windows(pid)
    }
    #[cfg(not(windows))]
    {
        let out = Command::new("kill").arg(pid.to_string()).output();
        match out {
            Ok(o) if o.status.success() => Ok(()),
            _ => Err(format!("cannot kill pid {}", pid)),
        }
    }
}

#[cfg(not(windows))]
fn list_procfs() -> Result<Vec<ProcessInfo>, String> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir("/proc").map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid: u32 = match name.to_str().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        let comm = std::fs::read_to_string(entry.path().join("comm"))
            .unwrap_or_default()
            .trim()
            .to_string();
        out.push(ProcessInfo {
            pid,
            name: if comm.is_empty() {
                format!("pid {}", pid)
            } else {
                comm
            },
        });
    }
    Ok(out)
}

#[cfg(windows)]
fn list_windows() -> Result<Vec<ProcessInfo>, String> {
    #[repr(C)]
    struct ProcessEntry {
        size: u32,
        usage: u32,
        pid: u32,
        heap_id: usize,
        module_id: u32,
        threads: u32,
        parent_pid: u32,
        pri_class: i32,
        flags: u32,
        exe_file: [u16; 260],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntry) -> i32;
        fn Process32NextW(snapshot: isize, entry: *mut ProcessEntry) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const INVALID_HANDLE: isize = -1;

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE {
            return Err("CreateToolhelp32Snapshot failed".to_string());
        }
        let mut out = Vec::new();
        let mut entry = ProcessEntry {
            size: 560,
            usage: 0,
            pid: 0,
            heap_id: 0,
            module_id: 0,
            threads: 0,
            parent_pid: 0,
            pri_class: 0,
            flags: 0,
            exe_file: [0; 260],
        };
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let end = entry
                    .exe_file
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.exe_file.len());
                let name = String::from_utf16_lossy(&entry.exe_file[..end]);
                out.push(ProcessInfo {
                    pid: entry.pid,
                    name,
                });
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        Ok(out)
    }
}

#[cfg(windows)]
fn launch_windows(path: &str, args: &[String]) -> Result<u32, String> {
    #[repr(C)]
    struct StartupInfo {
        cb: u32,
        _pad: u32,
        reserved: usize,
        desktop: usize,
        title: usize,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_chars: u32,
        y_chars: u32,
        fill_attr: u32,
        flags: u32,
        show_window: u16,
        reserved2: u16,
        _pad2: u32,
        reserved2_ptr: usize,
        stdin: usize,
        stdout: usize,
        stderr: usize,
    }

    #[repr(C)]
    struct ProcessInfoRaw {
        process: usize,
        thread: usize,
        pid: u32,
        tid: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateProcessW(
            app: *const u16,
            cmdline: *mut u16,
            proc_attrs: usize,
            thread_attrs: usize,
            inherit: i32,
            flags: u32,
            env: usize,
            dir: usize,
            startup: *const StartupInfo,
            out: *mut ProcessInfoRaw,
        ) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    let mut cmdline = String::from(path);
    for a in args {
        cmdline.push(' ');
        // Minimal quoting: quote args containing spaces.
        if a.contains(' ') {
            cmdline.push('"');
            cmdline.push_str(&a.replace('"', "\\\""));
            cmdline.push('"');
        } else {
            cmdline.push_str(a);
        }
    }
    let mut cmd_w = crate::win32::to_wide(&cmdline);
    let si = StartupInfo {
        cb: 104,
        _pad: 0,
        reserved: 0,
        desktop: 0,
        title: 0,
        x: 0,
        y: 0,
        x_size: 0,
        y_size: 0,
        x_chars: 0,
        y_chars: 0,
        fill_attr: 0,
        flags: 0,
        show_window: 0,
        reserved2: 0,
        _pad2: 0,
        reserved2_ptr: 0,
        stdin: 0,
        stdout: 0,
        stderr: 0,
    };
    let mut pi = ProcessInfoRaw {
        process: 0,
        thread: 0,
        pid: 0,
        tid: 0,
    };
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd_w.as_mut_ptr(),
            0,
            0,
            0,
            0,
            0,
            0,
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(format!("CreateProcessW failed for {}", path));
    }
    unsafe {
        CloseHandle(pi.process as isize);
        CloseHandle(pi.thread as isize);
    }
    Ok(pi.pid)
}

#[cfg(windows)]
fn kill_windows(pid: u32) -> Result<(), String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn TerminateProcess(handle: isize, code: u32) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }
    const PROCESS_TERMINATE: u32 = 0x0001;
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h == 0 {
            return Err(format!("cannot open pid {} (not found or access denied)", pid));
        }
        let ok = TerminateProcess(h, 1);
        CloseHandle(h);
        if ok == 0 {
            return Err(format!("TerminateProcess failed for pid {}", pid));
        }
        Ok(())
    }
}
