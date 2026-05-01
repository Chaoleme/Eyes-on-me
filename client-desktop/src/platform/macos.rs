use std::process::Command;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use block2::RcBlock;
use eyes_on_me_shared::PresenceState;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSNotification, NSRunLoop};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::browser::{BrowserContext, detect_browser_context, is_browser_app, page_signature};
use crate::config::CaptureFilters;
use crate::event::{ActivityEnvelope, AppInfo};
use crate::platform::{
    apply_capture_filters, is_system_process, normalize_app_info, send_activity,
};
use crate::{idle, screen_lock};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(15);
const LOG_THROTTLE_INTERVAL: Duration = Duration::from_secs(30);
const FRONT_APP_DELIMITER: &str = "|||AMI|||";

#[derive(Debug, Clone, PartialEq, Eq)]
struct LastApp {
    bundle_id: String,
    pid: Option<u32>,
    page_signature: Option<String>,
}

#[derive(Debug, Clone)]
struct ForegroundApp {
    app: AppInfo,
    window_title: Option<String>,
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
    let last_sent = Arc::new(Mutex::new(None::<LastSentState>));
    let last_read_error_at = Arc::new(Mutex::new(None::<Instant>));

    autoreleasepool(|_| {
        let workspace = NSWorkspace::sharedWorkspace();
        let notification_center = workspace.notificationCenter();

        emit_sample(
            &device_id,
            &agent_name,
            &capture_filters,
            &tx,
            &last_sent,
            &last_read_error_at,
        );

        let block_device_id = device_id.clone();
        let block_agent_name = agent_name.clone();
        let block_capture_filters = capture_filters.clone();
        let block_tx = tx.clone();
        let block_last_sent = Arc::clone(&last_sent);
        let block_last_read_error_at = Arc::clone(&last_read_error_at);
        let observer = RcBlock::new(move |_notification: NonNull<NSNotification>| {
            emit_sample(
                &block_device_id,
                &block_agent_name,
                &block_capture_filters,
                &block_tx,
                &block_last_sent,
                &block_last_read_error_at,
            );
        });

        let _observer_token = unsafe {
            notification_center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceDidActivateApplicationNotification),
                None,
                None,
                &observer,
            )
        };

        info!("foreground watcher started (macOS notification + polling sampler)");

        let run_loop = NSRunLoop::currentRunLoop();
        loop {
            let until = NSDate::dateWithTimeIntervalSinceNow(1.0);
            let mode = unsafe { NSDefaultRunLoopMode };
            let _ = run_loop.runMode_beforeDate(mode, &until);
            emit_sample(
                &device_id,
                &agent_name,
                &capture_filters,
                &tx,
                &last_sent,
                &last_read_error_at,
            );
        }
    })
}

fn emit_sample(
    device_id: &str,
    agent_name: &str,
    capture_filters: &CaptureFilters,
    tx: &mpsc::Sender<ActivityEnvelope>,
    last_sent: &Arc<Mutex<Option<LastSentState>>>,
    last_read_error_at: &Arc<Mutex<Option<Instant>>>,
) {
    let presence = current_presence();
    let previous = last_sent.lock().expect("snapshot mutex poisoned").clone();
    let now = Instant::now();
    let sample_due = previous
        .as_ref()
        .map(|state| now.duration_since(state.sent_at) >= SAMPLE_INTERVAL)
        .unwrap_or(true);
    let presence_changed = previous
        .as_ref()
        .map(|state| state.presence != presence)
        .unwrap_or(true);

    let current_identity = current_frontmost_identity();
    let app_changed = match (previous.as_ref(), current_identity.as_ref()) {
        (Some(previous), Some(current)) => {
            previous.app.id != current.id || previous.app.pid != current.pid
        }
        (None, Some(_)) | (Some(_), None) => true,
        (None, None) => true,
    };

    if !(app_changed || presence_changed || sample_due) {
        return;
    }

    let needs_full_snapshot = app_changed || previous.is_none();
    let mut current = if needs_full_snapshot {
        match current_foreground_app() {
            Some(current) => {
                *last_read_error_at.lock().expect("error mutex poisoned") = None;
                current
            }
            None => {
                throttle_read_error(last_read_error_at);
                previous
                    .as_ref()
                    .map(previous_as_foreground)
                    .unwrap_or_else(|| synthetic_foreground_app(presence))
            }
        }
    } else {
        previous
            .as_ref()
            .map(previous_as_foreground)
            .unwrap_or_else(|| synthetic_foreground_app(presence))
    };
    if is_system_process(&current.app.name) {
        current = synthetic_foreground_app(presence);
    }

    let browser = if app_changed || is_browser_app(&current.app) {
        stabilize_browser_context(
            detect_browser_context(&current.app, current.window_title.as_deref()),
            previous.as_ref(),
            &current.app,
            current.window_title.as_deref(),
        )
    } else {
        previous.as_ref().and_then(|state| state.browser.clone())
    };

    let filtered = apply_capture_filters(
        capture_filters,
        current.app.clone(),
        current.window_title.clone(),
        browser,
    );

    let marker = LastApp {
        bundle_id: filtered.app.id.clone(),
        pid: filtered.app.pid,
        page_signature: page_signature(filtered.browser.as_ref(), filtered.window_title.as_deref()),
    };
    let marker_changed = previous
        .as_ref()
        .map(|state| state.marker != marker)
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
            bundle_id = %filtered.app.id,
            pid = ?filtered.app.pid,
            presence = ?presence,
            kind,
            "activity sampled"
        );
    }

    let event = ActivityEnvelope::activity(
        device_id,
        agent_name,
        "macos",
        "nsworkspace",
        kind,
        filtered.app.clone(),
        filtered.window_title.clone(),
        filtered.browser.clone(),
        presence,
    );

    if !send_activity(tx, event) {
        return;
    }

    *last_sent.lock().expect("snapshot mutex poisoned") = Some(LastSentState {
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

fn throttle_read_error(last_read_error_at: &Arc<Mutex<Option<Instant>>>) {
    let now = Instant::now();
    let mut guard = last_read_error_at.lock().expect("error mutex poisoned");
    let should_log = guard
        .map(|at| now.duration_since(at) >= LOG_THROTTLE_INTERVAL)
        .unwrap_or(true);
    if should_log {
        warn!("cannot read frontmost app on macOS");
        *guard = Some(now);
    }
}

fn current_foreground_app() -> Option<ForegroundApp> {
    let script_snapshot = read_frontmost_app_snapshot();

    autoreleasepool(|_| {
        let workspace = NSWorkspace::sharedWorkspace();
        let frontmost = workspace.frontmostApplication();

        match (script_snapshot, frontmost) {
            (Some((app_name, pid, window_title)), _) => {
                let running = NSRunningApplication::runningApplicationWithProcessIdentifier(pid);
                Some(ForegroundApp {
                    app: running
                        .as_deref()
                        .map(|app| app_from_running_app(app, Some(app_name.clone()), pid))
                        .unwrap_or_else(|| fallback_app_info(app_name, pid)),
                    window_title,
                })
            }
            (None, Some(app)) => Some(ForegroundApp {
                app: app_from_running_app(&app, None, app.processIdentifier() as i32),
                window_title: None,
            }),
            (None, None) => None,
        }
    })
}

fn current_frontmost_identity() -> Option<AppInfo> {
    autoreleasepool(|_| {
        let workspace = NSWorkspace::sharedWorkspace();
        let app = workspace.frontmostApplication()?;
        Some(app_from_running_app(
            &app,
            None,
            app.processIdentifier() as i32,
        ))
    })
}

fn read_frontmost_app_snapshot() -> Option<(String, i32, Option<String>)> {
    let script = format!(
        r#"
tell application "System Events"
    set frontApp to first application process whose frontmost is true
    set appName to name of frontApp
    set appPid to unix id of frontApp
    set windowTitle to ""
    try
        set windowTitle to name of front window of frontApp
    end try
    return appName & "{delimiter}" & (appPid as string) & "{delimiter}" & windowTitle
end tell
"#,
        delimiter = FRONT_APP_DELIMITER
    );

    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.splitn(3, FRONT_APP_DELIMITER);
    let app_name = parts.next()?.trim().to_string();
    let pid = parts.next()?.trim().parse::<i32>().ok()?;
    let window_title = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    Some((app_name, pid, window_title))
}

fn app_from_running_app(
    app: &NSRunningApplication,
    preferred_name: Option<String>,
    pid: i32,
) -> AppInfo {
    let bundle_id = app
        .bundleIdentifier()
        .map(|id| id.to_string())
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| format!("pid:{pid}"));
    let name = preferred_name.unwrap_or_else(|| {
        app.localizedName()
            .map(|name| name.to_string())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| bundle_id.clone())
    });

    normalize_app_info(AppInfo {
        id: bundle_id,
        name: name.clone(),
        title: Some(name),
        pid: u32::try_from(pid).ok(),
    })
}

fn fallback_app_info(app_name: String, pid: i32) -> AppInfo {
    normalize_app_info(AppInfo {
        id: format!("pid:{pid}"),
        name: app_name.clone(),
        title: Some(app_name),
        pid: u32::try_from(pid).ok(),
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
    }
}
