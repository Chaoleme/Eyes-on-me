use std::{thread, time::Duration};

use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::warn;
use url::Url;

use crate::browser::BrowserContext;
use crate::config::CaptureFilters;
use crate::event::{ActivityEnvelope, AppInfo};

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub use macos::run_foreground_watcher;

#[cfg(target_os = "windows")]
pub use windows::run_foreground_watcher;

#[cfg(target_os = "linux")]
pub use linux::run_foreground_watcher;

#[cfg(not(test))]
const CHANNEL_BACKPRESSURE_RETRY: Duration = Duration::from_millis(200);
#[cfg(test)]
const CHANNEL_BACKPRESSURE_RETRY: Duration = Duration::from_millis(1);

#[cfg(not(test))]
const CHANNEL_BACKPRESSURE_MAX_RETRIES: u32 = 15;
#[cfg(test)]
const CHANNEL_BACKPRESSURE_MAX_RETRIES: u32 = 2;

pub(crate) fn send_activity(
    tx: &mpsc::Sender<ActivityEnvelope>,
    mut event: ActivityEnvelope,
) -> bool {
    for _ in 0..CHANNEL_BACKPRESSURE_MAX_RETRIES {
        match tx.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                event = returned;
                thread::sleep(CHANNEL_BACKPRESSURE_RETRY);
            }
            Err(TrySendError::Closed(_)) => {
                warn!("event channel closed, dropping event");
                return false;
            }
        }
    }

    warn!("event channel full after retries, dropping event");
    false
}

pub(crate) struct FilteredCapture {
    pub app: AppInfo,
    pub window_title: Option<String>,
    pub browser: Option<BrowserContext>,
}

pub(crate) fn apply_capture_filters(
    filters: &CaptureFilters,
    mut app: AppInfo,
    mut window_title: Option<String>,
    browser: Option<BrowserContext>,
) -> FilteredCapture {
    if matches_ignored_app(filters, &app) {
        let label = "Ignored Application".to_string();
        return FilteredCapture {
            app: AppInfo {
                id: "filtered.app".to_string(),
                name: label.clone(),
                title: Some(label.clone()),
                pid: None,
            },
            window_title: Some(label),
            browser: None,
        };
    }

    if matches_ignored_domain(filters, browser.as_ref()) {
        let label = "Ignored Website".to_string();
        app.title = Some(label.clone());
        window_title = Some(label);
        return FilteredCapture {
            app,
            window_title,
            browser: None,
        };
    }

    FilteredCapture {
        app,
        window_title,
        browser,
    }
}

pub(crate) fn normalize_app_info(mut app: AppInfo) -> AppInfo {
    let original_name = app.name.trim().to_string();
    let normalized_name = normalize_display_app_name(&original_name);

    if app.title.as_deref().map(str::trim) == Some(original_name.as_str()) {
        app.title = Some(normalized_name.clone());
    }

    app.name = normalized_name;
    app
}

fn matches_ignored_app(filters: &CaptureFilters, app: &AppInfo) -> bool {
    if filters.ignored_apps.is_empty() {
        return false;
    }

    let normalized_name = normalize_match_name(&app.name);
    let normalized_id =
        normalize_match_name(app.id.rsplit(['/', '\\']).next().unwrap_or(app.id.as_str()));

    filters.ignored_apps.iter().any(|rule| {
        let normalized_rule = normalize_match_name(rule);
        !normalized_rule.is_empty()
            && (normalized_name.contains(&normalized_rule)
                || normalized_rule.contains(&normalized_name)
                || normalized_id.contains(&normalized_rule)
                || normalized_rule.contains(&normalized_id))
    })
}

fn matches_ignored_domain(filters: &CaptureFilters, browser: Option<&BrowserContext>) -> bool {
    if filters.ignored_domains.is_empty() {
        return false;
    }

    let target_domain = browser.and_then(|browser| {
        browser
            .domain
            .as_deref()
            .and_then(normalize_domain_rule)
            .or_else(|| browser.url.as_deref().and_then(normalize_domain_rule))
    });

    let Some(target_domain) = target_domain else {
        return false;
    };

    filters.ignored_domains.iter().any(|rule| {
        let Some(rule_domain) = normalize_domain_rule(rule) else {
            return false;
        };
        target_domain == rule_domain || target_domain.ends_with(&format!(".{rule_domain}"))
    })
}

fn normalize_match_name(value: &str) -> String {
    normalize_display_app_name(value)
        .trim()
        .to_ascii_lowercase()
}

fn normalize_domain_rule(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(parsed) = Url::parse(trimmed) {
        return parsed
            .domain()
            .or_else(|| parsed.host_str())
            .map(|domain| domain.to_ascii_lowercase());
    }

    let without_scheme = trimmed
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    let host = host.rsplit_once('@').map(|(_, host)| host).unwrap_or(host);
    let host = host
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|ch| ch.is_ascii_digit()))
        .map(|(host, _)| host)
        .unwrap_or(host)
        .trim_matches('.');

    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

pub(crate) fn is_system_process(app_name: &str) -> bool {
    let name_lower = app_name.trim().to_lowercase();
    let name_lower = name_lower.trim_end_matches(".exe");

    matches!(
        name_lower,
        "desktop"
            | "lockapp"
            | "logonui"
            | "searchapp"
            | "searchhost"
            | "shellexperiencehost"
            | "startmenuexperiencehost"
            | "textinputhost"
            | "applicationframehost"
            | "dwm"
            | "csrss"
            | "taskmgr"
            | "loginwindow"
            | "screensaverengine"
            | "screensaver"
            | "cinnamon-session"
            | "cinnamon-screensaver"
            | "gnome-shell"
            | "gnome-screensaver"
            | "plasmashell"
            | "kscreenlocker"
            | "xscreensaver"
            | "i3lock"
            | "swaylock"
            | "xfce4-session"
    )
}

pub(crate) fn normalize_display_app_name(app_name: &str) -> String {
    let trimmed = app_name
        .trim()
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .trim();
    let normalized = trimmed.to_lowercase();

    match normalized.as_str() {
        "chrome" | "google chrome" => "Google Chrome".to_string(),
        "msedge" | "edge" | "microsoft edge" => "Microsoft Edge".to_string(),
        "brave" | "brave browser" => "Brave Browser".to_string(),
        "firefox" | "mozilla firefox" => "Firefox".to_string(),
        "safari" => "Safari".to_string(),
        "opera" | "opera gx" => "Opera".to_string(),
        "vivaldi" => "Vivaldi".to_string(),
        "chromium" => "Chromium".to_string(),
        "arc" => "Arc".to_string(),
        "zen browser" | "zen" => "Zen Browser".to_string(),
        "orion" => "Orion".to_string(),
        "qqbrowser" | "qq browser" | "qq浏览器" => "QQ Browser".to_string(),
        "360se" | "360chrome" | "360 browser" | "360浏览器" => "360 Browser".to_string(),
        "sogouexplorer" | "sogou browser" | "搜狗浏览器" => "Sogou Browser".to_string(),
        "code" | "visual studio code" => "Visual Studio Code".to_string(),
        "cursor" => "Cursor".to_string(),
        "wechat" => "WeChat".to_string(),
        _ => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use eyes_on_me_shared::PresenceState;

    use crate::{
        browser::BrowserContext,
        config::CaptureFilters,
        event::{ActivityEnvelope, AppInfo},
    };

    use super::{
        apply_capture_filters, is_system_process, normalize_display_app_name, send_activity,
    };

    #[test]
    fn normalizes_common_app_names() {
        assert_eq!(normalize_display_app_name("chrome.exe"), "Google Chrome");
        assert_eq!(normalize_display_app_name("msedge"), "Microsoft Edge");
        assert_eq!(normalize_display_app_name("code"), "Visual Studio Code");
    }

    #[test]
    fn detects_system_processes() {
        assert!(is_system_process("ScreenSaverEngine"));
        assert!(is_system_process("dwm.exe"));
        assert!(!is_system_process("Google Chrome"));
    }

    #[test]
    fn drops_event_when_channel_stays_full() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(sample_event()).expect("fill channel");

        assert!(!send_activity(&tx, sample_event()));

        let _ = rx.try_recv().expect("original event remains buffered");
    }

    #[test]
    fn filters_ignored_apps_to_generic_activity() {
        let filtered = apply_capture_filters(
            &CaptureFilters {
                ignored_apps: vec!["WeChat".to_string()],
                ignored_domains: Vec::new(),
            },
            AppInfo {
                id: "/Applications/WeChat.app".to_string(),
                name: "WeChat".to_string(),
                title: Some("Team Chat".to_string()),
                pid: Some(42),
            },
            Some("Team Chat".to_string()),
            None,
        );

        assert_eq!(filtered.app.name, "Ignored Application");
        assert_eq!(
            filtered.window_title.as_deref(),
            Some("Ignored Application")
        );
        assert!(filtered.browser.is_none());
    }

    #[test]
    fn filters_ignored_domains_without_hiding_browser_app() {
        let filtered = apply_capture_filters(
            &CaptureFilters {
                ignored_apps: Vec::new(),
                ignored_domains: vec!["github.com".to_string()],
            },
            AppInfo {
                id: "com.google.Chrome".to_string(),
                name: "Google Chrome".to_string(),
                title: Some("GitHub".to_string()),
                pid: Some(42),
            },
            Some("GitHub - Pull Requests".to_string()),
            Some(BrowserContext {
                family: "chromium".to_string(),
                name: "Google Chrome".to_string(),
                page_title: Some("GitHub - Pull Requests".to_string()),
                url: Some("https://docs.github.com/en".to_string()),
                domain: Some("docs.github.com".to_string()),
                source: "test".to_string(),
                confidence: 0.9,
            }),
        );

        assert_eq!(filtered.app.name, "Google Chrome");
        assert_eq!(filtered.window_title.as_deref(), Some("Ignored Website"));
        assert!(filtered.browser.is_none());
    }

    fn sample_event() -> ActivityEnvelope {
        ActivityEnvelope::activity(
            "device-1",
            "client-desktop",
            "macos",
            "test",
            "activity_sample",
            AppInfo {
                id: "com.google.Chrome".to_string(),
                name: "Google Chrome".to_string(),
                title: Some("GitHub".to_string()),
                pid: Some(42),
            },
            Some("GitHub".to_string()),
            None,
            PresenceState::Active,
        )
    }
}
