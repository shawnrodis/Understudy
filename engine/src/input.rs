//! Mouse/keyboard injection.
//!
//! The public surface is monitor-aware: every pointer action carries a
//! monitor id plus logical (DIP) coordinates on that monitor, resolved to
//! `SendInput` absolute coordinates through `monitors::api_point_to_absolute`.
//! Windows injection uses raw `SendInput` FFI (user32). Other platforms
//! return a clear error — input injection is a Windows capability of this
//! engine, not something to fake.

use crate::monitors::{self, Monitor};

/// A pointer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn from_str(s: &str) -> Option<Button> {
        match s.to_ascii_lowercase().as_str() {
            "left" => Some(Button::Left),
            "right" => Some(Button::Right),
            "middle" => Some(Button::Middle),
            _ => None,
        }
    }
}

/// One resolved absolute mouse position (0..=65535 in each axis).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbsolutePoint {
    pub x: u16,
    pub y: u16,
}

/// Resolve an API pointer target to absolute coordinates, or an error
/// message naming what was wrong (unknown monitor / outside monitor).
pub fn resolve_point(
    monitors: &[Monitor],
    monitor_id: u32,
    lx: f64,
    ly: f64,
) -> Result<AbsolutePoint, String> {
    monitors::api_point_to_absolute(monitors, monitor_id, lx, ly)
        .map(|(x, y)| AbsolutePoint { x, y })
        .ok_or_else(|| {
            format!(
                "point ({}, {}) is outside monitor {} or the monitor does not exist",
                lx, ly, monitor_id
            )
        })
}

/// Inject one absolute mouse move.
pub fn move_to(p: AbsolutePoint) -> Result<(), String> {
    #[cfg(windows)]
    {
        send_mouse(p, MouseEvent::Move)
    }
    #[cfg(not(windows))]
    {
        let _ = p;
        Err("mouse injection requires Windows".to_string())
    }
}

/// Inject a click (or double-click) at an absolute point.
pub fn click(p: AbsolutePoint, button: Button, double: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        send_mouse(p, MouseEvent::Move)?;
        let (down, up) = match button {
            Button::Left => (MouseEvent::LeftDown, MouseEvent::LeftUp),
            Button::Right => (MouseEvent::RightDown, MouseEvent::RightUp),
            Button::Middle => (MouseEvent::MiddleDown, MouseEvent::MiddleUp),
        };
        send_mouse(p, down)?;
        send_mouse(p, up)?;
        if double {
            send_mouse(p, down)?;
            send_mouse(p, up)?;
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (p, button, double);
        Err("mouse injection requires Windows".to_string())
    }
}

/// Inject a vertical scroll at an absolute point (`delta` in wheel clicks;
/// positive = up, matching Win32's positive WHEEL_DELTA direction).
pub fn scroll(p: AbsolutePoint, delta: i32) -> Result<(), String> {
    #[cfg(windows)]
    {
        send_mouse_wheel(p, delta)
    }
    #[cfg(not(windows))]
    {
        let _ = (p, delta);
        Err("mouse injection requires Windows".to_string())
    }
}

/// Type text via Unicode `SendInput` keystrokes (no layout dependence).
pub fn type_text(text: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        send_unicode_text(text)
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        Err("keyboard injection requires Windows".to_string())
    }
}

/// Press a chord: modifier names plus one final key, e.g.
/// `["ctrl", "l"]`, `["win", "r"]`, `["ctrl", "shift", "esc"]`.
/// Names: ctrl, alt, shift, win, plus any single character or a few named
/// keys (enter, tab, esc, space, backspace, delete, up, down, left, right,
/// f1..f12).
pub fn key_chord(keys: &[String]) -> Result<(), String> {
    #[cfg(windows)]
    {
        send_chord(keys)
    }
    #[cfg(not(windows))]
    {
        let _ = keys;
        Err("keyboard injection requires Windows".to_string())
    }
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum MouseEvent {
    Move,
    LeftDown,
    LeftUp,
    RightDown,
    RightUp,
    MiddleDown,
    MiddleUp,
}

#[cfg(windows)]
mod win {
    use super::AbsolutePoint;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MouseInput {
        dx: i32,
        dy: i32,
        mouse_data: u32,
        flags: u32,
        time: u32,
        extra: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeybdInput {
        vk: u16,
        scan: u16,
        flags: u32,
        time: u32,
        extra: usize,
    }

    #[repr(C)]
    union InputUnion {
        mi: MouseInput,
        ki: KeybdInput,
    }

    #[repr(C)]
    struct Input {
        ty: u32,
        _pad: u32,
        u: InputUnion,
    }

    const _: () = assert!(std::mem::size_of::<Input>() == 40);

    const INPUT_MOUSE: u32 = 0;
    const INPUT_KEYBOARD: u32 = 1;
    const MOUSEEVENTF_MOVE: u32 = 0x0001;
    const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
    const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
    const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
    const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
    const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
    const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
    const MOUSEEVENTF_WHEEL: u32 = 0x0800;
    const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
    const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const KEYEVENTF_UNICODE: u32 = 0x0004;

    #[link(name = "user32")]
    extern "system" {
        fn SendInput(c_inputs: u32, p_inputs: *const Input, cb_size: i32) -> u32;
    }

    fn send(inputs: &[Input]) -> Result<(), String> {
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<Input>() as i32,
            )
        };
        if sent as usize != inputs.len() {
            return Err(format!(
                "SendInput sent {}/{} inputs",
                sent,
                inputs.len()
            ));
        }
        Ok(())
    }

    fn mouse_input(p: AbsolutePoint, flags: u32, data: u32) -> Input {
        Input {
            ty: INPUT_MOUSE,
            _pad: 0,
            u: InputUnion {
                mi: MouseInput {
                    dx: p.x as i32,
                    dy: p.y as i32,
                    mouse_data: data,
                    flags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    extra: 0,
                },
            },
        }
    }

    pub fn send_mouse(p: AbsolutePoint, ev: super::MouseEvent) -> Result<(), String> {
        use super::MouseEvent::*;
        let flags = match ev {
            Move => MOUSEEVENTF_MOVE,
            LeftDown => MOUSEEVENTF_LEFTDOWN,
            LeftUp => MOUSEEVENTF_LEFTUP,
            RightDown => MOUSEEVENTF_RIGHTDOWN,
            RightUp => MOUSEEVENTF_RIGHTUP,
            MiddleDown => MOUSEEVENTF_MIDDLEDOWN,
            MiddleUp => MOUSEEVENTF_MIDDLEUP,
        };
        send(&[mouse_input(p, flags, 0)])
    }

    pub fn send_mouse_wheel(p: AbsolutePoint, clicks: i32) -> Result<(), String> {
        // WHEEL_DELTA is 120 per click.
        let data = (clicks.saturating_mul(120)) as u32;
        send(&[mouse_input(p, MOUSEEVENTF_WHEEL, data)])
    }

    fn unicode_key(unit: u16, up: bool) -> Input {
        Input {
            ty: INPUT_KEYBOARD,
            _pad: 0,
            u: InputUnion {
                ki: KeybdInput {
                    vk: 0,
                    scan: unit,
                    flags: KEYEVENTF_UNICODE | if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    extra: 0,
                },
            },
        }
    }

    pub fn send_unicode_text(text: &str) -> Result<(), String> {
        let mut inputs = Vec::new();
        for unit in text.encode_utf16() {
            inputs.push(unicode_key(unit, false));
            inputs.push(unicode_key(unit, true));
        }
        send(&inputs)
    }

    fn vk_for(name: &str) -> Option<u16> {
        // Virtual-key codes (WinUser.h). Modifiers are handled by the caller
        // through this same table.
        let vk = match name.to_ascii_lowercase().as_str() {
            "ctrl" => 0x11,
            "alt" => 0x12,
            "shift" => 0x10,
            "win" => 0x5B,
            "enter" => 0x0D,
            "tab" => 0x09,
            "esc" | "escape" => 0x1B,
            "space" => 0x20,
            "backspace" => 0x08,
            "delete" => 0x2E,
            "up" => 0x26,
            "down" => 0x28,
            "left" => 0x25,
            "right" => 0x27,
            "home" => 0x24,
            "end" => 0x23,
            "pageup" => 0x21,
            "pagedown" => 0x22,
            "f1" => 0x70,
            "f2" => 0x71,
            "f3" => 0x72,
            "f4" => 0x73,
            "f5" => 0x74,
            "f6" => 0x75,
            "f7" => 0x76,
            "f8" => 0x77,
            "f9" => 0x78,
            "f10" => 0x79,
            "f11" => 0x7A,
            "f12" => 0x7B,
            s if s.len() == 1 => {
                let c = s.chars().next().unwrap();
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase() as u16
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        Some(vk)
    }

    fn vk_key(vk: u16, up: bool) -> Input {
        Input {
            ty: INPUT_KEYBOARD,
            _pad: 0,
            u: InputUnion {
                ki: KeybdInput {
                    vk,
                    scan: 0,
                    flags: if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    extra: 0,
                },
            },
        }
    }

    pub fn send_chord(keys: &[String]) -> Result<(), String> {
        if keys.is_empty() {
            return Err("empty key chord".to_string());
        }
        let mut vks = Vec::new();
        for k in keys {
            vks.push(
                vk_for(k)
                    .ok_or_else(|| format!("unknown key name: {}", k))?,
            );
        }
        let mut inputs = Vec::new();
        for &vk in &vks {
            inputs.push(vk_key(vk, false));
        }
        for &vk in vks.iter().rev() {
            inputs.push(vk_key(vk, true));
        }
        send(&inputs)
    }
}

#[cfg(windows)]
use win::{send_chord, send_mouse, send_mouse_wheel, send_unicode_text};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitors::Monitor;

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
    fn button_names() {
        assert_eq!(Button::from_str("LEFT"), Some(Button::Left));
        assert_eq!(Button::from_str("right"), Some(Button::Right));
        assert_eq!(Button::from_str("middle"), Some(Button::Middle));
        assert_eq!(Button::from_str("side"), None);
    }

    #[test]
    fn resolve_point_uses_monitor_scale() {
        let d = desk();
        let p = resolve_point(&d, 1, 720.0, 1280.0).unwrap();
        // Global (3640, 1920) over (0,0,4720,3840).
        assert_eq!(p.x, (3640u32 * 65535 / 4719) as u16);
        assert_eq!(p.y, (1920u32 * 65535 / 3839) as u16);
    }

    #[test]
    fn resolve_point_rejects_bad_targets() {
        let d = desk();
        assert!(resolve_point(&d, 7, 10.0, 10.0).is_err());
        assert!(resolve_point(&d, 0, 3000.0, 10.0).is_err());
    }
}
