//! Per-monitor screenshots.
//!
//! v1 capture uses Win32 GDI (`BitBlt` from the screen device context into
//! a 32-bit DIB, converted BGRA -> RGBA) and returns raw pixels. It is
//! real, per-monitor, and DPI-aware: the captured rectangle is the
//! monitor's own rectangle in virtual-desktop physical pixels, so a
//! 150%-scaled portrait panel yields a 2160x3840 image, not a scaled-down
//! one. The API layer encodes (PNG, or downscaled JPEG) from these
//! pixels, so the capture path never round-trips through an encoding.
//!
//! Planned upgrade (documented, not claimed): DXGI Desktop Duplication for
//! high-rate frame streaming. The `capture_monitor` signature already
//! returns raw RGBA, so the duplication backend slots in behind it.

use crate::monitors::Monitor;

/// Capture monitor `id` and return `(width, height, rgba_bytes)` without
/// encoding — the downscale/JPEG path uses raw pixels directly instead
/// of round-tripping through PNG.
pub fn capture_monitor_rgba(monitors: &[Monitor], id: u32) -> Result<(u32, u32, Vec<u8>), String> {
    let monitor = monitors
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| format!("unknown monitor {}", id))?;
    capture_rgba(monitor)
}

#[cfg(windows)]
fn capture_rgba(monitor: &Monitor) -> Result<(u32, u32, Vec<u8>), String> {
    type Hdc = isize;
    type Hbitmap = isize;

    #[repr(C)]
    struct BitmapInfoHeader {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bit_count: u16,
        compression: u32,
        image_size: u32,
        x_pels: i32,
        y_pels: i32,
        clr_used: u32,
        clr_important: u32,
    }

    #[repr(C)]
    struct BitmapInfo {
        header: BitmapInfoHeader,
        colors: [u32; 1],
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn CreateCompatibleDC(hdc: Hdc) -> Hdc;
        fn CreateCompatibleBitmap(hdc: Hdc, width: i32, height: i32) -> Hbitmap;
        fn SelectObject(hdc: Hdc, obj: Hbitmap) -> Hbitmap;
        fn DeleteObject(obj: Hbitmap) -> i32;
        fn DeleteDC(hdc: Hdc) -> i32;
        fn GetDIBits(
            hdc: Hdc,
            hbm: Hbitmap,
            start: u32,
            lines: u32,
            bits: *mut u8,
            info: *mut BitmapInfo,
            usage: u32,
        ) -> i32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetDC(hwnd: isize) -> Hdc;
        fn ReleaseDC(hwnd: isize, hdc: Hdc) -> i32;
        fn BitBlt(
            dest: Hdc,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            src: Hdc,
            sx: i32,
            sy: i32,
            rop: u32,
        ) -> i32;
    }

    const SRCCOPY: u32 = 0x00CC_0020;
    const DIB_RGB_COLORS: u32 = 0;

    let w = monitor.width as i32;
    let h = monitor.height as i32;
    unsafe {
        let screen = GetDC(0);
        if screen == 0 {
            return Err("GetDC failed".to_string());
        }
        let mem = CreateCompatibleDC(screen);
        if mem == 0 {
            ReleaseDC(0, screen);
            return Err("CreateCompatibleDC failed".to_string());
        }
        let bmp = CreateCompatibleBitmap(screen, w, h);
        if bmp == 0 {
            DeleteDC(mem);
            ReleaseDC(0, screen);
            return Err("CreateCompatibleBitmap failed".to_string());
        }
        let old = SelectObject(mem, bmp);
        let blt = BitBlt(mem, 0, 0, w, h, screen, monitor.x, monitor.y, SRCCOPY);
        if blt == 0 {
            SelectObject(mem, old);
            DeleteObject(bmp);
            DeleteDC(mem);
            ReleaseDC(0, screen);
            return Err("BitBlt failed".to_string());
        }
        // Top-down DIB (negative height) so rows arrive in display order.
        let mut info = BitmapInfo {
            header: BitmapInfoHeader {
                size: 40,
                width: w,
                height: -h,
                planes: 1,
                bit_count: 32,
                compression: 0,
                image_size: 0,
                x_pels: 0,
                y_pels: 0,
                clr_used: 0,
                clr_important: 0,
            },
            colors: [0],
        };
        let mut bgra = vec![0u8; (w as usize) * (h as usize) * 4];
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            bgra.as_mut_ptr(),
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(0, screen);
        if lines == 0 {
            return Err("GetDIBits failed".to_string());
        }
        // BGRA -> RGBA in place.
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Ok((monitor.width, monitor.height, bgra))
    }
}

#[cfg(not(windows))]
fn capture_rgba(_monitor: &Monitor) -> Result<(u32, u32, Vec<u8>), String> {
    Err("screen capture requires Windows".to_string())
}
