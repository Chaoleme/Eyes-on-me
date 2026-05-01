use std::env;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use eyes_on_me_shared::PresenceState;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::browser::{BrowserContext, detect_browser_context, page_signature};
use crate::config::CaptureFilters;
use crate::event::{ActivityEnvelope, AppInfo};
use crate::idle;
use crate::platform::{
    apply_capture_filters, is_system_process, normalize_app_info, send_activity,
};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_millis(1200);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxDesktopSession {
    X11,
    Wayland,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxDesktopEnvironment {
    Gnome,
    Kde,
    Sway,
    Hyprland,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LastApp {
    window_id: String,
    pid: Option<u32>,
    page_signature: Option<String>,
}

#[derive(Debug, Clone)]
struct ForegroundApp {
    window_id: String,
    app: AppInfo,
    window_title: Option<String>,
    source: &'static str,
}

#[derive(Debug, Clone)]
struct LastSentState {
    marker: LastApp,
    app: AppInfo,
    window_title: Option<String>,
    browser: Option<BrowserContext>,
    presence: PresenceState,
    sent_at: Instant,
    source: &'static str,
}

pub fn run_foreground_watcher(
    device_id: String,
    agent_name: String,
    capture_filters: CaptureFilters,
    tx: mpsc::Sender<ActivityEnvelope>,
) -> Result<()> {
    let session = current_linux_desktop_session().ok_or_else(|| {
        anyhow!("client-desktop Linux watcher requires an active desktop session")
    })?;
    let desktop_environment = current_linux_desktop_environment();

    match session {
        LinuxDesktopSession::X11 => {
            if !command_available("xprop") {
                bail!("client-desktop Linux X11 watcher requires `xprop` in PATH");
            }
        }
        LinuxDesktopSession::Wayland => {
            if current_linux_active_window_provider(desktop_environment).is_none() {
                bail!(
                    "client-desktop Linux Wayland watcher requires one of gdbus, kdotool, swaymsg, or hyprctl in PATH"
                );
            }
        }
    }

    info!(
        session = ?session,
        desktop_environment = ?desktop_environment,
        provider = current_linux_active_window_provider(desktop_environment).unwrap_or("fallback"),
        "foreground watcher started (Linux polling sampler)"
    );

    let mut last_sent = None::<LastSentState>;
    let mut last_read_error_at = None::<Instant>;

    loop {
        let presence = if idle::is_idle(idle::DEFAULT_IDLE_TIMEOUT_SECS) {
            PresenceState::Idle
        } else {
            PresenceState::Active
        };

        let mut current = match current_foreground_app(session, desktop_environment) {
            Some(current) => {
                last_read_error_at = None;
                current
            }
            None => {
                let now = Instant::now();
                let should_log = last_read_error_at
                    .map(|at| now.duration_since(at) >= Duration::from_secs(30))
                    .unwrap_or(true);
                if should_log {
                    warn!(
                        session = ?session,
                        desktop_environment = ?desktop_environment,
                        "cannot read frontmost app on Linux"
                    );
                    last_read_error_at = Some(now);
                }
                last_sent
                    .as_ref()
                    .map(previous_as_foreground)
                    .unwrap_or_else(|| synthetic_foreground_app(presence))
            }
        };
        if is_system_process(&current.app.name) {
            current = synthetic_foreground_app(presence);
        }

        let browser = stabilize_browser_context(
            detect_browser_context(&current.app, current.window_title.as_deref()),
            last_sent.as_ref(),
            &current.app,
            current.window_title.as_deref(),
        );
        let filtered = apply_capture_filters(
            &capture_filters,
            current.app.clone(),
            current.window_title.clone(),
            browser,
        );
        let marker = LastApp {
            window_id: current.window_id.clone(),
            pid: filtered.app.pid,
            page_signature: page_signature(
                filtered.browser.as_ref(),
                filtered.window_title.as_deref(),
            ),
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

        if marker_changed || presence_changed || sample_due {
            let kind = if marker_changed {
                "foreground_changed"
            } else if presence_changed {
                "presence_changed"
            } else {
                "activity_sample"
            };

            info!(
                app_name = %filtered.app.name,
                app_id = %filtered.app.id,
                pid = ?filtered.app.pid,
                source = current.source,
                presence = ?presence,
                kind,
                "activity sampled"
            );

            let event = ActivityEnvelope::activity(
                &device_id,
                &agent_name,
                "linux",
                current.source,
                kind,
                filtered.app.clone(),
                filtered.window_title.clone(),
                filtered.browser.clone(),
                presence,
            );
            if !send_activity(&tx, event) {
                return Ok(());
            }

            last_sent = Some(LastSentState {
                marker,
                app: filtered.app,
                window_title: filtered.window_title,
                browser: filtered.browser,
                presence,
                sent_at: now,
                source: current.source,
            });
        }

        thread::sleep(POLL_INTERVAL);
    }
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

fn current_foreground_app(
    session: LinuxDesktopSession,
    desktop_environment: LinuxDesktopEnvironment,
) -> Option<ForegroundApp> {
    match session {
        LinuxDesktopSession::X11 => current_foreground_app_x11(),
        LinuxDesktopSession::Wayland => current_foreground_app_wayland(desktop_environment),
    }
}

fn current_foreground_app_x11() -> Option<ForegroundApp> {
    let window_id = active_window_id_x11()?;
    let window_title = window_title_x11(&window_id);
    let pid = window_pid_x11(&window_id);

    let (app_id, app_name, pid) = if let Some(pid) = pid {
        let app_id = process_commandline(pid)
            .or_else(|| process_name(pid))
            .unwrap_or_else(|| format!("pid:{pid}"));
        let app_name = process_name(pid).unwrap_or_else(|| fallback_name(&app_id));
        (app_id, app_name, Some(pid))
    } else {
        let class_name = window_class_x11(&window_id)?;
        let fallback_id = class_name.clone();
        let fallback_name = fallback_name(&class_name);
        (fallback_id, fallback_name, None)
    };

    Some(ForegroundApp {
        window_id,
        app: normalize_app_info(AppInfo {
            id: app_id,
            name: app_name,
            title: window_title.clone(),
            pid,
        }),
        window_title,
        source: "xprop",
    })
}

fn current_foreground_app_wayland(
    desktop_environment: LinuxDesktopEnvironment,
) -> Option<ForegroundApp> {
    let providers: &[fn() -> Option<ForegroundApp>] = match desktop_environment {
        LinuxDesktopEnvironment::Gnome => &[
            current_foreground_app_wayland_gnome,
            current_foreground_app_wayland_sway,
            current_foreground_app_wayland_hyprland,
            current_foreground_app_wayland_kde,
        ],
        LinuxDesktopEnvironment::Kde => &[
            current_foreground_app_wayland_kde,
            current_foreground_app_wayland_gnome,
            current_foreground_app_wayland_sway,
            current_foreground_app_wayland_hyprland,
        ],
        LinuxDesktopEnvironment::Sway => &[
            current_foreground_app_wayland_sway,
            current_foreground_app_wayland_hyprland,
            current_foreground_app_wayland_gnome,
            current_foreground_app_wayland_kde,
        ],
        LinuxDesktopEnvironment::Hyprland => &[
            current_foreground_app_wayland_hyprland,
            current_foreground_app_wayland_sway,
            current_foreground_app_wayland_gnome,
            current_foreground_app_wayland_kde,
        ],
        LinuxDesktopEnvironment::Unknown => &[
            current_foreground_app_wayland_hyprland,
            current_foreground_app_wayland_sway,
            current_foreground_app_wayland_gnome,
            current_foreground_app_wayland_kde,
        ],
    };

    providers.iter().find_map(|provider| provider())
}

fn current_foreground_app_wayland_gnome() -> Option<ForegroundApp> {
    let output = run_command(
        "gdbus",
        &[
            "call",
            "--session",
            "--dest",
            "org.gnome.Shell",
            "--object-path",
            "/org/gnome/shell/extensions/FocusedWindow",
            "--method",
            "org.gnome.shell.extensions.FocusedWindow.Get",
        ],
    )?;
    parse_gnome_focused_window_dbus_output(&output)
}

fn current_foreground_app_wayland_kde() -> Option<ForegroundApp> {
    let window_id = run_command("kdotool", &["getactivewindow"])?;
    let title = run_command("kdotool", &["getwindowname", &window_id])?;
    let class_name = run_command("kdotool", &["getwindowclassname", &window_id])?;
    let pid = run_command("kdotool", &["getwindowpid", &window_id]).and_then(parse_u32);

    build_linux_wayland_foreground(Some(window_id), &title, &class_name, pid, "kdotool")
}

fn current_foreground_app_wayland_sway() -> Option<ForegroundApp> {
    let output = run_command("swaymsg", &["-t", "get_tree", "-r"])?;
    let tree: Value = serde_json::from_str(&output).ok()?;
    let focused = find_focused_sway_node(&tree)?;

    let title = focused.get("name").and_then(|value| value.as_str())?;
    let app_name = focused
        .get("app_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            focused
                .get("window_properties")
                .and_then(|value| value.get("class"))
                .and_then(|value| value.as_str())
        })?;
    let pid = focused
        .get("pid")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok());

    build_linux_wayland_foreground(None, title, app_name, pid, "swaymsg")
}

fn current_foreground_app_wayland_hyprland() -> Option<ForegroundApp> {
    let output = run_command("hyprctl", &["activewindow", "-j"])?;
    let value: Value = serde_json::from_str(&output).ok()?;

    let title = value.get("title").and_then(|value| value.as_str())?;
    let app_name = value.get("class").and_then(|value| value.as_str())?;
    let pid = value
        .get("pid")
        .and_then(|value| value.as_i64())
        .filter(|pid| *pid > 0)
        .and_then(|pid| u32::try_from(pid).ok());

    build_linux_wayland_foreground(None, title, app_name, pid, "hyprctl")
}

fn build_linux_wayland_foreground(
    window_id: Option<String>,
    window_title: &str,
    raw_app_name: &str,
    pid: Option<u32>,
    source: &'static str,
) -> Option<ForegroundApp> {
    let window_title = window_title.trim();
    let raw_app_name = raw_app_name.trim();
    if window_title.is_empty() || raw_app_name.is_empty() {
        return None;
    }

    let app_id = pid
        .and_then(process_commandline)
        .or_else(|| pid.and_then(process_name))
        .unwrap_or_else(|| raw_app_name.to_string());
    let app_name = pid
        .and_then(process_name)
        .unwrap_or_else(|| raw_app_name.to_string());

    Some(ForegroundApp {
        window_id: window_id.unwrap_or_else(|| {
            format!(
                "{source}:{}:{}:{}",
                pid.map(|pid| pid.to_string())
                    .unwrap_or_else(|| "none".to_string()),
                raw_app_name,
                window_title
            )
        }),
        app: normalize_app_info(AppInfo {
            id: app_id,
            name: app_name,
            title: Some(window_title.to_string()),
            pid,
        }),
        window_title: Some(window_title.to_string()),
        source,
    })
}

fn current_linux_active_window_provider(
    desktop_environment: LinuxDesktopEnvironment,
) -> Option<&'static str> {
    let providers: &[(&str, fn() -> bool)] = match desktop_environment {
        LinuxDesktopEnvironment::Gnome => &[
            ("gdbus", is_gnome_wayland_active_window_provider_available),
            ("swaymsg", is_sway_active_window_provider_available),
            ("hyprctl", is_hyprland_active_window_provider_available),
            ("kdotool", is_kde_wayland_active_window_provider_available),
        ],
        LinuxDesktopEnvironment::Kde => &[
            ("kdotool", is_kde_wayland_active_window_provider_available),
            ("gdbus", is_gnome_wayland_active_window_provider_available),
            ("swaymsg", is_sway_active_window_provider_available),
            ("hyprctl", is_hyprland_active_window_provider_available),
        ],
        LinuxDesktopEnvironment::Sway => &[
            ("swaymsg", is_sway_active_window_provider_available),
            ("hyprctl", is_hyprland_active_window_provider_available),
            ("gdbus", is_gnome_wayland_active_window_provider_available),
            ("kdotool", is_kde_wayland_active_window_provider_available),
        ],
        LinuxDesktopEnvironment::Hyprland => &[
            ("hyprctl", is_hyprland_active_window_provider_available),
            ("swaymsg", is_sway_active_window_provider_available),
            ("gdbus", is_gnome_wayland_active_window_provider_available),
            ("kdotool", is_kde_wayland_active_window_provider_available),
        ],
        LinuxDesktopEnvironment::Unknown => &[
            ("hyprctl", is_hyprland_active_window_provider_available),
            ("swaymsg", is_sway_active_window_provider_available),
            ("gdbus", is_gnome_wayland_active_window_provider_available),
            ("kdotool", is_kde_wayland_active_window_provider_available),
        ],
    };

    providers
        .iter()
        .find_map(|(name, probe)| probe().then_some(*name))
}

fn parse_gnome_focused_window_dbus_output(output: &str) -> Option<ForegroundApp> {
    let json_start = output.find('{')?;
    let json_end = output.rfind('}')?;
    let value: Value = serde_json::from_str(&output[json_start..=json_end]).ok()?;

    let window_title = value
        .get("title")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|title| !title.is_empty())?;
    let raw_app_name = value
        .get("wm_class")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|app| !app.is_empty())
        .or_else(|| {
            value
                .get("wm_class_instance")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|app| !app.is_empty())
        })?;
    let pid = value
        .get("pid")
        .and_then(|value| value.as_i64())
        .filter(|pid| *pid > 0)
        .and_then(|pid| u32::try_from(pid).ok());

    build_linux_wayland_foreground(None, window_title, raw_app_name, pid, "gdbus")
}

fn find_focused_sway_node<'a>(value: &'a Value) -> Option<&'a Value> {
    if value
        .get("focused")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
        && (value.get("pid").is_some() || value.get("app_id").is_some())
    {
        return Some(value);
    }

    for key in ["nodes", "floating_nodes"] {
        if let Some(nodes) = value.get(key).and_then(|value| value.as_array()) {
            for node in nodes {
                if let Some(found) = find_focused_sway_node(node) {
                    return Some(found);
                }
            }
        }
    }

    None
}

fn previous_as_foreground(previous: &LastSentState) -> ForegroundApp {
    ForegroundApp {
        window_id: previous.marker.window_id.clone(),
        app: previous.app.clone(),
        window_title: previous.window_title.clone(),
        source: previous.source,
    }
}

fn synthetic_foreground_app(presence: PresenceState) -> ForegroundApp {
    let (id, name) = match presence {
        PresenceState::Locked => ("system.locked", "Locked Screen"),
        PresenceState::Idle => ("system.idle", "Idle"),
        PresenceState::Active => ("system.unknown", "Unknown"),
    };

    ForegroundApp {
        window_id: id.to_string(),
        app: AppInfo {
            id: id.to_string(),
            name: name.to_string(),
            title: Some(name.to_string()),
            pid: None,
        },
        window_title: None,
        source: "linux-monitor",
    }
}

fn current_linux_desktop_session() -> Option<LinuxDesktopSession> {
    if env::var_os("WAYLAND_DISPLAY").is_some() {
        Some(LinuxDesktopSession::Wayland)
    } else if env::var_os("DISPLAY").is_some() {
        Some(LinuxDesktopSession::X11)
    } else {
        None
    }
}

fn current_linux_desktop_environment() -> LinuxDesktopEnvironment {
    let value = env::var("XDG_CURRENT_DESKTOP")
        .or_else(|_| env::var("DESKTOP_SESSION"))
        .unwrap_or_default()
        .to_ascii_lowercase();

    if value.contains("gnome") {
        LinuxDesktopEnvironment::Gnome
    } else if value.contains("kde") || value.contains("plasma") {
        LinuxDesktopEnvironment::Kde
    } else if value.contains("sway") {
        LinuxDesktopEnvironment::Sway
    } else if value.contains("hypr") {
        LinuxDesktopEnvironment::Hyprland
    } else {
        LinuxDesktopEnvironment::Unknown
    }
}

fn active_window_id_x11() -> Option<String> {
    let output = run_command("xprop", &["-root", "_NET_ACTIVE_WINDOW"])?;
    let marker = output.split('#').nth(1)?.trim();
    if marker.is_empty() || marker == "0x0" {
        return None;
    }
    Some(marker.to_string())
}

fn window_pid_x11(window_id: &str) -> Option<u32> {
    let output = run_command("xprop", &["-id", window_id, "_NET_WM_PID"])?;
    output.split('=').nth(1)?.trim().parse::<u32>().ok()
}

fn window_title_x11(window_id: &str) -> Option<String> {
    let output = run_command("xprop", &["-id", window_id, "_NET_WM_NAME", "WM_NAME"])?;

    for line in output.lines() {
        if let Some(title) = quoted_value(line) {
            return Some(title);
        }
    }

    None
}

fn window_class_x11(window_id: &str) -> Option<String> {
    let output = run_command("xprop", &["-id", window_id, "WM_CLASS"])?;
    parse_wm_class(&output)
}

fn quoted_value(line: &str) -> Option<String> {
    let start = line.find('"')?;
    let end = line.rfind('"')?;
    if end <= start {
        return None;
    }
    let value = &line[start + 1..end];
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.replace("\\\"", "\""))
}

fn parse_wm_class(output: &str) -> Option<String> {
    let parts: Vec<&str> = output.split('"').collect();
    if parts.len() >= 4 {
        Some(parts[3].to_string())
    } else if parts.len() >= 2 {
        Some(parts[1].to_string())
    } else {
        None
    }
}

fn process_commandline(pid: u32) -> Option<String> {
    let output = run_command("ps", &["-p", &pid.to_string(), "-o", "args="])?;
    let command = output.lines().next()?.trim();
    if command.is_empty() {
        return None;
    }

    let first = command.split_whitespace().next().unwrap_or(command);
    Some(first.to_string())
}

fn process_name(pid: u32) -> Option<String> {
    let output = run_command("ps", &["-p", &pid.to_string(), "-o", "comm="])?;
    let value = output.lines().next()?.trim();
    if value.is_empty() {
        return None;
    }
    Some(fallback_name(value))
}

fn fallback_name(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
        .unwrap_or_else(|| value.to_string())
}

fn parse_u32(value: String) -> Option<u32> {
    value.trim().parse::<u32>().ok()
}

fn is_gnome_wayland_active_window_provider_available() -> bool {
    command_available("gdbus")
}

fn is_kde_wayland_active_window_provider_available() -> bool {
    command_available("kdotool")
}

fn is_sway_active_window_provider_available() -> bool {
    command_available("swaymsg")
}

fn is_hyprland_active_window_provider_available() -> bool {
    command_available("hyprctl")
}

fn command_available(name: &str) -> bool {
    let mut command = Command::new(name);
    command.stdout(Stdio::null()).stderr(Stdio::null());

    run_command_output(&mut command).is_some()
}

fn run_command(program: &str, args: &[&str]) -> Option<String> {
    let output = run_command_output(Command::new(program).args(args))?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()
        .map(|stdout| stdout.trim().to_string())
        .filter(|stdout| !stdout.is_empty())
}

fn run_command_output(command: &mut Command) -> Option<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command.spawn().ok()?;
    let started_at = Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if started_at.elapsed() < COMMAND_TIMEOUT => {
                thread::sleep(COMMAND_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[allow(dead_code)]
fn command_error(program: &str, args: &[&str]) -> Result<String> {
    let output = run_command_output(Command::new(program).args(args))
        .ok_or_else(|| anyhow!("command failed: {program} {}", args.join(" ")))?;
    if !output.status.success() {
        return Err(anyhow!("command failed: {program} {}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
