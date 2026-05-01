pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 180;

pub fn is_idle(timeout_secs: u64) -> bool {
    get_idle_seconds() >= timeout_secs
}

#[cfg(target_os = "windows")]
fn get_idle_seconds() -> u64 {
    use std::mem::size_of;
    use winapi::um::sysinfoapi::GetTickCount;
    use winapi::um::winuser::{GetLastInputInfo, LASTINPUTINFO};

    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };

        if GetLastInputInfo(&mut lii) == 0 {
            return 0;
        }

        let current_tick = GetTickCount();
        let idle_ms = if current_tick >= lii.dwTime {
            current_tick - lii.dwTime
        } else {
            (u32::MAX - lii.dwTime) + current_tick + 1
        };

        (idle_ms / 1000) as u64
    }
}

#[cfg(target_os = "macos")]
fn get_idle_seconds() -> u64 {
    use core_graphics::event_source::CGEventSourceStateID;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(
            state_id: CGEventSourceStateID,
            event_type: u32,
        ) -> f64;
    }

    const K_CG_ANY_INPUT_EVENT_TYPE: u32 = !0u32;

    let idle_time = unsafe {
        CGEventSourceSecondsSinceLastEventType(
            CGEventSourceStateID::HIDSystemState,
            K_CG_ANY_INPUT_EVENT_TYPE,
        )
    };

    if idle_time.is_sign_negative() {
        0
    } else {
        idle_time as u64
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn get_idle_seconds() -> u64 {
    use std::{env, process::Command};

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum LinuxDesktopSession {
        Wayland,
        X11,
        Unknown,
    }

    let session = if env::var_os("WAYLAND_DISPLAY").is_some() {
        LinuxDesktopSession::Wayland
    } else if env::var_os("DISPLAY").is_some() {
        LinuxDesktopSession::X11
    } else {
        LinuxDesktopSession::Unknown
    };

    if session == LinuxDesktopSession::Wayland {
        let output = Command::new("dbus-send")
            .args([
                "--session",
                "--print-reply",
                "--dest=org.freedesktop.ScreenSaver",
                "/org/freedesktop/ScreenSaver",
                "org.freedesktop.ScreenSaver.GetSessionIdleTime",
            ])
            .output();

        if let Ok(result) = output {
            if result.status.success() {
                let stdout = String::from_utf8_lossy(&result.stdout);
                if let Some(idle_ms) = stdout
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .windows(2)
                    .find_map(|parts| match parts {
                        ["uint32", value] => value.parse::<u64>().ok(),
                        ["uint64", value] => value.parse::<u64>().ok(),
                        _ => None,
                    })
                {
                    return idle_ms / 1000;
                }
            }
        }
    }

    let output = Command::new("xprintidle").output();
    match output {
        Ok(result) if result.status.success() => String::from_utf8_lossy(&result.stdout)
            .trim()
            .parse::<u64>()
            .map(|idle_ms| idle_ms / 1000)
            .unwrap_or(0),
        _ => 0,
    }
}
