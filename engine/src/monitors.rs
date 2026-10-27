//! Per-monitor enumeration and DPI-aware coordinate mapping.
//!
//! Coordinate systems (see CONTROL_API.md):
//! * A monitor is described by its origin (`x`, `y`) and size in the Windows
//!   virtual-desktop space, in PHYSICAL pixels, plus its DPI scale factor
//!   (1.0 = 100%, 1.25 = 125%, ...).
//! * The control API speaks PER-MONITOR LOGICAL (DIP) coordinates: `x`, `y`
//!   measured from that monitor's own top-left corner, in the monitor's
//!   scaled (logical) space. The engine converts to physical pixels, adds the
//!   monitor origin, and normalizes to the 0..=65535 absolute range that
//!   Win32 `SendInput` expects.
//! * Nothing in this module treats the desktop as one stretched virtual
//!   screen: every conversion goes through a specific monitor and its scale.
//!
//! The pure mapping functions are platform-independent and unit-tested.
//! Enumeration itself is Windows-only (`enumerate_windows`); other platforms
//!   get a single synthetic monitor so the API and tests still run.

use std::fmt;

/// One physical display.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// Stable index in enumeration order (0 = first enumerated monitor).
    pub id: u32,
    /// Virtual-desktop origin, physical pixels (may be negative when a
    /// monitor sits left of / above the primary).
    pub x: i32,
    pub y: i32,
    /// Size in physical pixels.
    pub width: u32,
    pub height: u32,
    /// DPI scale factor: physical pixels per logical (DIP) pixel.
    pub scale: f64,
    /// True when this is the primary monitor.
    pub is_primary: bool,
}

impl Monitor {
    /// Logical (DIP) width of this monitor.
    pub fn logical_width(&self) -> f64 {
        self.width as f64 / self.scale
    }

    /// Logical (DIP) height of this monitor.
    pub fn logical_height(&self) -> f64 {
        self.height as f64 / self.scale
    }

    /// Does this monitor contain the given virtual-desktop physical point?
    pub fn contains_physical(&self, gx: i32, gy: i32) -> bool {
        gx >= self.x
            && gx < self.x + self.width as i32
            && gy >= self.y
            && gy < self.y + self.height as i32
    }

    /// Convert a per-monitor logical point to virtual-desktop physical
    /// coordinates. Returns `None` when the point is outside this monitor.
    pub fn logical_to_global_physical(&self, lx: f64, ly: f64) -> Option<(i32, i32)> {
        if lx < 0.0 || ly < 0.0 || lx >= self.logical_width() || ly >= self.logical_height() {
            return None;
        }
        let px = logical_to_physical(lx, self.scale);
        let py = logical_to_physical(ly, self.scale);
        Some((self.x + px, self.y + py))
    }
}

impl fmt::Display for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "monitor {}: origin=({}, {}) size={}x{} scale={:.2} primary={}",
            self.id, self.x, self.y, self.width, self.height, self.scale, self.is_primary
        )
    }
}

/// Logical (DIP) pixels -> physical pixels, rounded to the nearest pixel.
pub fn logical_to_physical(logical: f64, scale: f64) -> i32 {
    (logical * scale).round() as i32
}

/// Physical pixels -> logical (DIP) pixels.
pub fn physical_to_logical(physical: i32, scale: f64) -> f64 {
    physical as f64 / scale
}

/// Virtual-desktop bounding box over all monitors, in physical pixels:
/// `(min_x, min_y, width, height)`.
pub fn virtual_desktop_bounds(monitors: &[Monitor]) -> Option<(i32, i32, u32, u32)> {
    if monitors.is_empty() {
        return None;
    }
    let min_x = monitors.iter().map(|m| m.x).min().unwrap();
    let min_y = monitors.iter().map(|m| m.y).min().unwrap();
    let max_x = monitors
        .iter()
        .map(|m| m.x + m.width as i32)
        .max()
        .unwrap();
    let max_y = monitors
        .iter()
        .map(|m| m.y + m.height as i32)
        .max()
        .unwrap();
    Some((
        min_x,
        min_y,
        (max_x - min_x) as u32,
        (max_y - min_y) as u32,
    ))
}

/// Which monitor contains a virtual-desktop physical point, if any.
pub fn monitor_at_physical<'a>(monitors: &'a [Monitor], gx: i32, gy: i32) -> Option<&'a Monitor> {
    monitors.iter().find(|m| m.contains_physical(gx, gy))
}

/// Normalize a virtual-desktop physical coordinate to the 0..=65535 range
/// `SendInput` absolute mouse input expects. `origin`/`size` come from
/// `virtual_desktop_bounds`. The result is clamped to the valid range.
pub fn normalize_absolute(coord: i32, origin: i32, size: u32) -> u16 {
    if size <= 1 {
        return 0;
    }
    let rel = (coord - origin).clamp(0, size as i32 - 1) as u32;
    ((rel as u64 * 65535) / (size as u64 - 1)) as u16
}

/// Full pipeline for an API input point: per-monitor logical coordinates ->
/// `SendInput` absolute (0..=65535) pair. Returns `None` when the monitor id
/// is unknown or the point is outside that monitor.
pub fn api_point_to_absolute(
    monitors: &[Monitor],
    monitor_id: u32,
    lx: f64,
    ly: f64,
) -> Option<(u16, u16)> {
    let monitor = monitors.iter().find(|m| m.id == monitor_id)?;
    let (gx, gy) = monitor.logical_to_global_physical(lx, ly)?;
    let (ox, oy, vw, vh) = virtual_desktop_bounds(monitors)?;
    Some((
        normalize_absolute(gx, ox, vw),
        normalize_absolute(gy, oy, vh),
    ))
}

/// Enumerate monitors on this machine. Windows: real enumeration via
/// `EnumDisplayMonitors` + `GetDpiForMonitor`. Other platforms: one
/// synthetic 1920x1080 monitor so the API, tests, and controller UI still run.
pub fn enumerate() -> Vec<Monitor> {
    #[cfg(windows)]
    {
        crate::win32::enumerate_monitors()
    }
    #[cfg(not(windows))]
    {
        vec![Monitor {
            id: 0,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            is_primary: true,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shawn's desk shape: landscape 2560x1440 primary at 100%, plus a
    /// portrait 2160x3840 panel to its right at 150% scale.
    fn desk() -> Vec<Monitor> {
        vec![
            Monitor {
                id: 0,
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                scale: 1.0,
                is_primary: true,
            },
            Monitor {
                id: 1,
                x: 2560,
                y: 0,
                width: 2160,
                height: 3840,
                scale: 1.5,
                is_primary: false,
            },
        ]
    }

    #[test]
    fn primary_scale_one_is_identity() {
        let m = &desk()[0];
        assert_eq!(m.logical_to_global_physical(100.0, 200.0), Some((100, 200)));
        assert!((m.logical_width() - 2560.0).abs() < f64::EPSILON);
    }

    #[test]
    fn portrait_logical_maps_through_scale_and_origin() {
        // Portrait logical size is 2160/1.5 = 1440 wide, 3840/1.5 = 2560 tall.
        let m = &desk()[1];
        assert!((m.logical_width() - 1440.0).abs() < f64::EPSILON);
        assert!((m.logical_height() - 2560.0).abs() < f64::EPSILON);
        // Logical (720, 1280) -> physical (1080, 1920) -> global (3640, 1920).
        assert_eq!(
            m.logical_to_global_physical(720.0, 1280.0),
            Some((2560 + 1080, 1920))
        );
    }

    #[test]
    fn points_outside_a_monitor_are_rejected() {
        let m = &desk()[1];
        assert_eq!(m.logical_to_global_physical(-1.0, 100.0), None);
        assert_eq!(m.logical_to_global_physical(1440.0, 100.0), None); // at edge
        assert_eq!(m.logical_to_global_physical(100.0, 2560.0), None);
    }

    #[test]
    fn rounding_is_nearest_pixel() {
        assert_eq!(logical_to_physical(10.5, 1.5), 16); // 15.75 -> 16
        assert_eq!(logical_to_physical(10.0, 1.25), 13); // 12.5 -> 13 (round half away)
        assert_eq!(physical_to_logical(16, 1.5), 16.0 / 1.5);
    }

    #[test]
    fn virtual_bounds_span_both_monitors() {
        assert_eq!(
            virtual_desktop_bounds(&desk()),
            Some((0, 0, 2560 + 2160, 3840))
        );
    }

    #[test]
    fn monitor_hit_testing() {
        let d = desk();
        assert_eq!(monitor_at_physical(&d, 100, 100).map(|m| m.id), Some(0));
        assert_eq!(monitor_at_physical(&d, 3000, 3000).map(|m| m.id), Some(1));
        assert_eq!(monitor_at_physical(&d, 100, 2000), None); // below primary
    }

    #[test]
    fn absolute_normalization_edges() {
        // Virtual width 4720: x=0 -> 0, x=4719 -> 65535.
        assert_eq!(normalize_absolute(0, 0, 4720), 0);
        assert_eq!(normalize_absolute(4719, 0, 4720), 65535);
        // Out-of-range input clamps instead of wrapping.
        assert_eq!(normalize_absolute(-50, 0, 4720), 0);
        assert_eq!(normalize_absolute(99999, 0, 4720), 65535);
    }

    #[test]
    fn api_point_pipeline_end_to_end() {
        let d = desk();
        let (ax, ay) = api_point_to_absolute(&d, 1, 720.0, 1280.0).unwrap();
        // Global (3640, 1920) over bounds (0,0,4720,3840).
        assert_eq!(ax, normalize_absolute(3640, 0, 4720));
        assert_eq!(ay, normalize_absolute(1920, 0, 3840));
        assert_eq!(api_point_to_absolute(&d, 9, 10.0, 10.0), None); // bad monitor
        assert_eq!(api_point_to_absolute(&d, 0, 99999.0, 10.0), None); // outside
    }
}
