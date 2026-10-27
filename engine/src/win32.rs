//! Shared Win32 helpers. This module only compiles on Windows; other
//! platforms get no symbols from it (callers gate with `#[cfg(windows)]`).

/// Convert a Rust string to a null-terminated UTF-16 vector for Win32 APIs.
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MonitorInfoEx {
    size: u32,
    monitor: Rect,
    work: Rect,
    flags: u32,
    device: [u16; 32],
}

/// Enumerate monitors via `EnumDisplayMonitors` + `GetMonitorInfoW` +
/// `GetDpiForMonitor` (effective DPI). The callback collects monitor rects
/// through LPARAM; DPI and the primary flag are resolved afterwards.
pub fn enumerate_monitors() -> Vec<crate::monitors::Monitor> {
    type Hmonitor = isize;
    type Hdc = isize;
    type Lparam = isize;

    #[link(name = "user32")]
    extern "system" {
        fn EnumDisplayMonitors(
            hdc: Hdc,
            clip: *const Rect,
            callback: extern "system" fn(Hmonitor, Hdc, *mut Rect, Lparam) -> i32,
            data: Lparam,
        ) -> i32;
        fn GetMonitorInfoW(monitor: Hmonitor, info: *mut MonitorInfoEx) -> i32;
    }

    #[link(name = "shcore")]
    extern "system" {
        fn GetDpiForMonitor(monitor: Hmonitor, dpi_type: i32, dpi_x: *mut u32, dpi_y: *mut u32)
            -> i32;
    }

    extern "system" fn collect(
        monitor: Hmonitor,
        _hdc: Hdc,
        _rect: *mut Rect,
        data: Lparam,
    ) -> i32 {
        let list = unsafe { &mut *(data as *mut Vec<Hmonitor>) };
        list.push(monitor);
        1
    }

    const MONITORINFOF_PRIMARY: u32 = 1;
    const MDT_EFFECTIVE_DPI: i32 = 0;

    let mut handles: Vec<Hmonitor> = Vec::new();
    let data = &mut handles as *mut Vec<Hmonitor> as Lparam;
    let ok = unsafe { EnumDisplayMonitors(0, std::ptr::null(), collect, data) };
    if ok == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (id, handle) in handles.into_iter().enumerate() {
        let mut info = MonitorInfoEx {
            size: 104,
            monitor: Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            work: Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            flags: 0,
            device: [0; 32],
        };
        let got = unsafe { GetMonitorInfoW(handle, &mut info) };
        if got == 0 {
            continue;
        }
        let mut dpi_x: u32 = 96;
        let mut dpi_y: u32 = 96;
        let hr = unsafe {
            GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y)
        };
        let scale = if hr == 0 && dpi_x > 0 {
            dpi_x as f64 / 96.0
        } else {
            1.0
        };
        out.push(crate::monitors::Monitor {
            id: id as u32,
            x: info.monitor.left,
            y: info.monitor.top,
            width: (info.monitor.right - info.monitor.left).max(0) as u32,
            height: (info.monitor.bottom - info.monitor.top).max(0) as u32,
            scale,
            is_primary: info.flags & MONITORINFOF_PRIMARY != 0,
        });
    }
    out
}

/// Fill `buf` with OS randomness via `CryptGenRandom` (advapi32).
pub fn os_random(buf: &mut [u8]) -> bool {
    #[link(name = "advapi32")]
    extern "system" {
        fn CryptAcquireContextW(
            ph_prov: *mut usize,
            psz_container: *const u16,
            psz_provider: *const u16,
            dw_prov_type: u32,
            dw_flags: u32,
        ) -> i32;
        fn CryptGenRandom(h_prov: usize, dw_len: u32, pb_buffer: *mut u8) -> i32;
        fn CryptReleaseContext(h_prov: usize, dw_flags: u32) -> i32;
    }
    const PROV_RSA_FULL: u32 = 1;
    const CRYPT_VERIFYCONTEXT: u32 = 0xF000_0000;
    unsafe {
        let mut prov: usize = 0;
        if CryptAcquireContextW(&mut prov, std::ptr::null(), std::ptr::null(), PROV_RSA_FULL, CRYPT_VERIFYCONTEXT)
            == 0
        {
            return false;
        }
        let ok = CryptGenRandom(prov, buf.len() as u32, buf.as_mut_ptr()) != 0;
        CryptReleaseContext(prov, 0);
        ok
    }
}
