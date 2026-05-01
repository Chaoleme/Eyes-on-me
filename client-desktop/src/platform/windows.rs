use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use eyes_on_me_shared::PresenceState;
use tokio::sync::mpsc;
use tracing::{info, warn};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
};

use crate::browser::{BrowserContext, detect_browser_context_for_window, page_signature};
use crate::config::CaptureFilters;
use crate::event::{ActivityEnvelope, AppInfo};
use crate::platform::{
    apply_capture_filters, is_system_process, normalize_app_info, send_activity,
};
use crate::{idle, screen_lock};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
struct LastApp {
    process_path: String,
    pid: Option<u32>,
    page_signature: Option<String>,
}

#[derive(Debug, Clone)]
struct ForegroundApp {
    app: AppInfo,
    window_title: Option<String>,
    hwnd: isize,
}

#[derive(Debug, Clone)]
struct LastSentState {
    marker: LastApp,
    app: AppInfo,
    window_title: Option<String>,
    browser: Option<BrowserContext>,
    presence: PresenceState,
    sent_at: Instant,
}

pub fn run_foreground_watcher(
    device_id: String,
    agent_name: String,
    capture_filters: CaptureFilters,
    tx: mpsc::Sender<ActivityEnvelope>,
) -> Result<()> {
    info!("foreground watcher started (Windows polling sampler)");

    let mut last_sent: Option<LastSentState> = None;

    loop {
        emit_sample(
            &device_id,
            &agent_name,
            &capture_filters,
            &tx,
            &mut last_sent,
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn emit_sample(
    device_id: &str,
    agent_name: &str,
    capture_filters: &CaptureFilters,
    tx: &mpsc::Sender<ActivityEnvelope>,
    last_sent: &mut Option<LastSentState>,
) {
    let presence = current_presence();
    let mut current = current_foreground_app()
        .or_else(|| last_sent.as_ref().map(previous_as_foreground))
        .unwrap_or_else(|| synthetic_foreground_app(presence));
    if is_system_process(&current.app.name) {
        current = synthetic_foreground_app(presence);
    }

    let browser = stabilize_browser_context(
        detect_browser_context_for_window(
            &current.app,
            current.window_title.as_deref(),
            current.hwnd,
        ),
        last_sent.as_ref(),
        &current.app,
        current.window_title.as_deref(),
    );

    let filtered = apply_capture_filters(
        capture_filters,
        current.app.clone(),
        current.window_title.clone(),
        browser,
    );

    let marker = LastApp {
        process_path: filtered.app.id.clone(),
        pid: filtered.app.pid,
        page_signature: page_signature(filtered.browser.as_ref(), filtered.window_title.as_deref()),
    };

    let now = Instant::now();
    let marker_changed = last_sent
        .as_ref()
        .map(|state| state.marker != marker)
        .unwrap_or(true);
    let presence_changed = last_sent
        .as_ref()
        .map(|state| state.presence != presence)
        .unwrap_or(true);
    let sample_due = last_sent
        .as_ref()
        .map(|state| now.duration_since(state.sent_at) >= SAMPLE_INTERVAL)
        .unwrap_or(true);

    if !(marker_changed || presence_changed || sample_due) {
        return;
    }

    let kind = if marker_changed {
        "foreground_changed"
    } else if presence_changed {
        "presence_changed"
    } else {
        "activity_sample"
    };

    if marker_changed || presence_changed {
        info!(
            app_name = %filtered.app.name,
            process_path = %filtered.app.id,
            pid = ?filtered.app.pid,
            presence = ?presence,
            kind,
            "activity sampled"
        );
    }

    let event = ActivityEnvelope::activity(
        device_id,
        agent_name,
        "windows",
        "polling",
        kind,
        filtered.app.clone(),
        filtered.window_title.clone(),
        filtered.browser.clone(),
        presence,
    );

    if !send_activity(tx, event) {
        return;
    }

    *last_sent = Some(LastSentState {
        marker,
        app: filtered.app,
        window_title: filtered.window_title,
        browser: filtered.browser,
        presence,
        sent_at: now,
    });
}

fn current_presence() -> PresenceState {
    if screen_lock::is_locked() {
        PresenceState::Locked
    } else if idle::is_idle(idle::DEFAULT_IDLE_TIMEOUT_SECS) {
        PresenceState::Idle
    } else {
        PresenceState::Active
    }
}

fn current_foreground_app() -> Option<ForegroundApp> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }

    let pid = process_id_from_hwnd(hwnd)?;
    let process_path = process_path(pid).unwrap_or_else(|| format!("pid:{pid}"));
    let app_name = Path::new(&process_path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
        .unwrap_or_else(|| format!("pid-{pid}"));

    Some(ForegroundApp {
        app: normalize_app_info(AppInfo {
            id: process_path,
            name: app_name,
            title: window_title(hwnd),
            pid: Some(pid),
        }),
        window_title: window_title(hwnd),
        hwnd: hwnd as isize,
    })
}

fn stabilize_browser_context(
    browser: Option<BrowserContext>,
    previous: Option<&LastSentState>,
    app: &AppInfo,
    window_title: Option<&str>,
) -> Option<BrowserContext> {
    let same_window = previous
        .map(|state| {
            state.app.id == app.id
                && state.window_title.as_deref() == window_title
                && state.browser.is_some()
        })
        .unwrap_or(false);

    match (browser, previous.and_then(|state| state.browser.clone())) {
        (Some(mut current), Some(prev)) if same_window => {
            if current.url.is_none() {
                current.url = prev.url.clone();
            }
            if current.domain.is_none() {
                current.domain = prev.domain.clone();
            }
            if current.page_title.is_none() {
                current.page_title = prev.page_title.clone();
            }
            Some(current)
        }
        (Some(current), _) => Some(current),
        (None, Some(prev)) if same_window => Some(prev),
        (None, _) => None,
    }
}

fn previous_as_foreground(previous: &LastSentState) -> ForegroundApp {
    ForegroundApp {
        app: previous.app.clone(),
        window_title: previous.window_title.clone(),
        hwnd: 0,
    }
}

fn synthetic_foreground_app(presence: PresenceState) -> ForegroundApp {
    let (id, name) = match presence {
        PresenceState::Locked => ("system.locked", "Locked Screen"),
        PresenceState::Idle => ("system.idle", "Idle"),
        PresenceState::Active => ("system.unknown", "Unknown"),
    };

    ForegroundApp {
        app: AppInfo {
            id: id.to_string(),
            name: name.to_string(),
            title: Some(name.to_string()),
            pid: None,
        },
        window_title: None,
        hwnd: 0,
    }
}

fn process_id_from_hwnd(hwnd: HWND) -> Option<u32> {
    let mut pid = 0_u32;
    let _thread_id = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 { None } else { Some(pid) }
}

fn process_path(pid: u32) -> Option<String> {
    let process_handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process_handle.is_null() {
        return None;
    }

    let path = query_process_image_name(process_handle);
    let _ = unsafe { CloseHandle(process_handle) };
    path
}

fn query_process_image_name(process_handle: HANDLE) -> Option<String> {
    let mut buffer = vec![0_u16; 32768];
    let mut len = buffer.len() as u32;
    let ok =
        unsafe { QueryFullProcessImageNameW(process_handle, 0, buffer.as_mut_ptr(), &mut len) };
    if ok == 0 || len == 0 {
        return None;
    }

    Some(String::from_utf16_lossy(&buffer[..len as usize]))
}

fn window_title(hwnd: HWND) -> Option<String> {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return None;
    }

    let mut buffer = vec![0_u16; len as usize + 1];
    let written = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if written <= 0 {
        return None;
    }

    let raw = String::from_utf16_lossy(&buffer[..written as usize]);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
