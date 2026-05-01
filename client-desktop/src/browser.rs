#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::process::{Command, Output, Stdio};
#[cfg(target_os = "windows")]
use std::thread;
#[cfg(target_os = "windows")]
use std::time::{Duration, Instant};
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::{env, fs};

use serde::Serialize;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use serde_json::Value;
use url::Url;

use crate::event::AppInfo;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserContext {
    pub family: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    pub source: String,
    pub confidence: f32,
}

#[derive(Debug, Clone)]
struct BrowserDefinition {
    family: &'static str,
    name: &'static str,
    bundle_ids: &'static [&'static str],
    processes: &'static [&'static str],
    app_names: &'static [&'static str],
    #[cfg(target_os = "macos")]
    apple_script_name: Option<&'static str>,
}

const BROWSERS: &[BrowserDefinition] = &[
    BrowserDefinition {
        family: "chromium",
        name: "Google Chrome",
        bundle_ids: &[
            "com.google.chrome",
            "com.google.chrome.canary",
            "org.chromium.chromium",
        ],
        processes: &["chrome.exe", "chrome", "chromium.exe", "chromium"],
        app_names: &["google chrome", "chrome", "chromium"],
        #[cfg(target_os = "macos")]
        apple_script_name: Some("Google Chrome"),
    },
    BrowserDefinition {
        family: "chromium",
        name: "Microsoft Edge",
        bundle_ids: &["com.microsoft.edgemac", "com.microsoft.edge"],
        processes: &["msedge.exe", "msedge"],
        app_names: &["microsoft edge", "edge"],
        #[cfg(target_os = "macos")]
        apple_script_name: Some("Microsoft Edge"),
    },
    BrowserDefinition {
        family: "chromium",
        name: "Brave",
        bundle_ids: &[
            "com.brave.browser",
            "com.brave.browser.beta",
            "com.brave.browser.nightly",
        ],
        processes: &["brave.exe", "brave"],
        app_names: &["brave browser", "brave"],
        #[cfg(target_os = "macos")]
        apple_script_name: Some("Brave Browser"),
    },
    BrowserDefinition {
        family: "chromium",
        name: "Opera",
        bundle_ids: &[
            "com.operasoftware.opera",
            "com.operasoftware.operanext",
            "com.operasoftware.operagx",
        ],
        processes: &["opera.exe", "launcher.exe", "opera", "launcher"],
        app_names: &["opera", "opera gx"],
        #[cfg(target_os = "macos")]
        apple_script_name: Some("Opera"),
    },
    BrowserDefinition {
        family: "chromium",
        name: "Vivaldi",
        bundle_ids: &["com.vivaldi.vivaldi"],
        processes: &["vivaldi.exe", "vivaldi"],
        app_names: &["vivaldi"],
        #[cfg(target_os = "macos")]
        apple_script_name: Some("Vivaldi"),
    },
    BrowserDefinition {
        family: "chromium",
        name: "Arc",
        bundle_ids: &["company.thebrowser.browser"],
        processes: &["arc.exe", "arc"],
        app_names: &["arc"],
        #[cfg(target_os = "macos")]
        apple_script_name: Some("Arc"),
    },
    BrowserDefinition {
        family: "chromium",
        name: "Zen Browser",
        bundle_ids: &["app.zen-browser.zen"],
        processes: &["zen.exe", "zen"],
        app_names: &["zen browser", "zen"],
        #[cfg(target_os = "macos")]
        apple_script_name: None,
    },
    BrowserDefinition {
        family: "firefox",
        name: "Firefox",
        bundle_ids: &[
            "org.mozilla.firefox",
            "org.mozilla.firefoxdeveloperedition",
            "net.waterfox.waterfox",
        ],
        processes: &["firefox.exe", "firefox", "waterfox.exe", "waterfox"],
        app_names: &["mozilla firefox", "firefox", "waterfox"],
        #[cfg(target_os = "macos")]
        apple_script_name: None,
    },
    BrowserDefinition {
        family: "webkit",
        name: "Safari",
        bundle_ids: &["com.apple.safari"],
        processes: &["safari.exe", "safari"],
        app_names: &["safari"],
        #[cfg(target_os = "macos")]
        apple_script_name: Some("Safari"),
    },
    BrowserDefinition {
        family: "webkit",
        name: "Orion",
        bundle_ids: &["com.kagi.kagimacOS"],
        processes: &["orion.exe", "orion"],
        app_names: &["orion"],
        #[cfg(target_os = "macos")]
        apple_script_name: None,
    },
    BrowserDefinition {
        family: "chromium",
        name: "QQ Browser",
        bundle_ids: &[],
        processes: &["qqbrowser.exe", "qqbrowser"],
        app_names: &["qq browser", "qqbrowser", "qq浏览器"],
        #[cfg(target_os = "macos")]
        apple_script_name: None,
    },
    BrowserDefinition {
        family: "chromium",
        name: "360 Browser",
        bundle_ids: &[],
        processes: &["360se.exe", "360chrome.exe", "360se", "360chrome"],
        app_names: &["360 browser", "360se", "360chrome", "360浏览器"],
        #[cfg(target_os = "macos")]
        apple_script_name: None,
    },
    BrowserDefinition {
        family: "chromium",
        name: "Sogou Browser",
        bundle_ids: &[],
        processes: &["sogouexplorer.exe", "sogouexplorer"],
        app_names: &["sogou browser", "sogouexplorer", "搜狗浏览器"],
        #[cfg(target_os = "macos")]
        apple_script_name: None,
    },
];

pub fn detect_browser_context(app: &AppInfo, window_title: Option<&str>) -> Option<BrowserContext> {
    detect_browser_context_internal(app, window_title, None)
}

pub fn is_browser_app(app: &AppInfo) -> bool {
    match_browser(app).is_some()
}

#[cfg(target_os = "windows")]
pub fn detect_browser_context_for_window(
    app: &AppInfo,
    window_title: Option<&str>,
    hwnd: isize,
) -> Option<BrowserContext> {
    detect_browser_context_internal(app, window_title, Some(hwnd))
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn detect_browser_context_for_window(
    app: &AppInfo,
    window_title: Option<&str>,
    _hwnd: isize,
) -> Option<BrowserContext> {
    detect_browser_context_internal(app, window_title, None)
}

fn detect_browser_context_internal(
    app: &AppInfo,
    window_title: Option<&str>,
    #[allow(unused_variables)] hwnd: Option<isize>,
) -> Option<BrowserContext> {
    let browser = match_browser(app)?;

    let initial_title = infer_page_title(window_title, browser);
    #[cfg(target_os = "macos")]
    let mut title = initial_title;
    #[cfg(not(target_os = "macos"))]
    let title = initial_title;
    let mut url = None;
    let mut domain = None;
    let mut source = "window-title".to_string();
    let mut confidence: f32 = if title.is_some() { 0.42 } else { 0.18 };

    #[cfg(target_os = "macos")]
    if let Some(mac_page) = read_macos_browser_page(browser) {
        if let Some(page_title) = mac_page.page_title {
            title = Some(page_title);
        }
        url = mac_page.url;
        domain = mac_page.domain;
        source = mac_page.source;
        confidence = mac_page.confidence;
    }

    #[cfg(target_os = "windows")]
    if let Some(win_page) = hwnd.and_then(|handle| read_windows_browser_page(window_title, handle))
    {
        url = win_page.url;
        domain = win_page.domain;
        source = win_page.source;
        confidence = win_page.confidence;
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if url.is_none() && browser.family == "firefox" {
        if let Some(title_ref) = window_title {
            if let Some(sessionstore_url) = firefox_family_session_store_url(&app.name, title_ref) {
                domain = url_domain(&sessionstore_url);
                url = Some(sessionstore_url);
                source = "firefox-sessionstore".to_string();
                confidence = confidence.max(0.94);
            }
        }
    }

    if url.is_none() {
        url = window_title.and_then(infer_url_from_title);
        if url.is_some() {
            source = "window-title-url".to_string();
            confidence = confidence.max(0.74);
        }
    }

    if domain.is_none() {
        domain = url.as_deref().and_then(url_domain);
    }

    if domain.is_none() {
        domain = title.as_deref().and_then(infer_domain_from_text);
        if domain.is_some() {
            source = "window-title-domain".to_string();
            confidence = confidence.max(0.56);
        }
    }

    Some(BrowserContext {
        family: browser.family.to_string(),
        name: browser.name.to_string(),
        page_title: title,
        url,
        domain,
        source,
        confidence,
    })
}

#[cfg(target_os = "windows")]
const BROWSER_COMMAND_TIMEOUT: Duration = Duration::from_millis(1200);

#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
struct WindowsBrowserPage {
    url: Option<String>,
    domain: Option<String>,
    source: String,
    confidence: f32,
}

#[cfg(target_os = "windows")]
fn read_windows_browser_page(
    window_title: Option<&str>,
    hwnd: isize,
) -> Option<WindowsBrowserPage> {
    if hwnd == 0 {
        return None;
    }

    let native_result = std::panic::catch_unwind(|| get_url_via_uiautomation(hwnd)).unwrap_or(None);
    if let Some(url) = native_result {
        return Some(WindowsBrowserPage {
            domain: url_domain(&url),
            url: Some(url),
            source: "uiautomation".to_string(),
            confidence: 0.96,
        });
    }

    if let Some(url) = get_url_via_powershell_uia(hwnd) {
        return Some(WindowsBrowserPage {
            domain: url_domain(&url),
            url: Some(url),
            source: "powershell-uia".to_string(),
            confidence: 0.88,
        });
    }

    window_title
        .and_then(infer_url_from_title)
        .map(|url| WindowsBrowserPage {
            domain: url_domain(&url),
            url: Some(url),
            source: "window-title-url".to_string(),
            confidence: 0.74,
        })
}

#[cfg(target_os = "windows")]
fn run_browser_command_with_timeout(command: &mut Command) -> Option<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command.spawn().ok()?;
    let started_at = Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if started_at.elapsed() < BROWSER_COMMAND_TIMEOUT => {
                thread::sleep(Duration::from_millis(50));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn get_url_via_powershell_uia(hwnd: isize) -> Option<String> {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const POWERSHELL_PATH: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";

    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

$hwnd = [IntPtr]::new({hwnd})
if ($hwnd -eq [IntPtr]::Zero) {{ exit 0 }}

$window = [System.Windows.Automation.AutomationElement]::FromHandle($hwnd)
if ($null -eq $window) {{ exit 0 }}

$editCondition = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
    [System.Windows.Automation.ControlType]::Edit
)
$docCondition = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
    [System.Windows.Automation.ControlType]::Document
)
$allConditions = New-Object System.Windows.Automation.OrCondition($editCondition, $docCondition)
$nodes = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, $allConditions)

for ($i = 0; $i -lt $nodes.Count; $i++) {{
    $node = $nodes.Item($i)
    $candidates = New-Object System.Collections.Generic.List[string]

    try {{
        $vp = $node.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
        if ($vp -ne $null -and $vp.Current.Value) {{ [void]$candidates.Add($vp.Current.Value) }}
    }} catch {{ }}

    try {{
        $lp = $node.GetCurrentPattern([System.Windows.Automation.LegacyIAccessiblePattern]::Pattern)
        if ($lp -ne $null -and $lp.Current.Value) {{ [void]$candidates.Add($lp.Current.Value) }}
    }} catch {{ }}

    try {{
        if ($node.Current.Name) {{ [void]$candidates.Add($node.Current.Name) }}
    }} catch {{ }}

    foreach ($raw in $candidates) {{
        if ([string]::IsNullOrWhiteSpace($raw)) {{ continue }}
        $value = $raw.Trim()
        if ($value -match '^(https?://|chrome://|edge://|about:|file:)' -or
            $value -match '^(localhost|([a-zA-Z0-9-]+\.)+[a-zA-Z]{{2,}}|\d{{1,3}}(\.\d{{1,3}}){{3}})(:\d{{2,5}})?([/?#].*)?$') {{
            Write-Output $value
            exit 0
        }}
    }}
}}
"#
    );

    let output = run_browser_command_with_timeout(
        Command::new(POWERSHELL_PATH)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Sta",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &script,
            ])
            .creation_flags(CREATE_NO_WINDOW),
    )?;

    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    normalize_possible_url(&value)
}

#[cfg(target_os = "windows")]
fn get_url_via_uiautomation(hwnd: isize) -> Option<String> {
    use uiautomation::UIAutomation;
    use uiautomation::patterns::{UILegacyIAccessiblePattern, UIValuePattern};
    use uiautomation::types::{ControlType, Handle};

    let automation = UIAutomation::new().ok()?;
    let window_element = automation.element_from_handle(Handle::from(hwnd)).ok()?;

    let mut best_match: Option<(i32, String)> = None;

    let inspect_control = |control: uiautomation::UIElement,
                           best_match: &mut Option<(i32, String)>| {
        let control_type = match control.get_control_type() {
            Ok(control_type) => control_type,
            Err(_) => return,
        };

        if control_type != ControlType::Edit && control_type != ControlType::Document {
            return;
        }

        let name = control.get_name().unwrap_or_default();
        let class_name = control.get_classname().unwrap_or_default();
        let name_lower = name.to_lowercase();
        let class_lower = class_name.to_lowercase();
        let address_like = name_lower.contains("address")
            || name_lower.contains("地址")
            || name_lower.contains("location")
            || name_lower.contains("omnibox")
            || class_lower.contains("omnibox")
            || class_lower.contains("address");

        let mut candidates = Vec::new();
        if let Ok(pattern) = control.get_pattern::<UIValuePattern>() {
            if let Ok(value) = pattern.get_value() {
                candidates.push(value);
            }
        }
        if let Ok(pattern) = control.get_pattern::<UILegacyIAccessiblePattern>() {
            if let Ok(value) = pattern.get_value() {
                candidates.push(value);
            }
        }
        candidates.push(name.clone());

        for raw in candidates {
            let Some(url) = normalize_possible_url(&raw) else {
                continue;
            };

            let mut score = match control_type {
                ControlType::Edit => 35,
                ControlType::Document => 15,
                _ => 0,
            };

            if address_like {
                score += 50;
            }
            if raw.starts_with("http://") || raw.starts_with("https://") {
                score += 30;
            } else if raw == class_name || raw == name {
                score += 5;
            }

            if score >= 60
                && best_match
                    .as_ref()
                    .map(|(best_score, _)| score > *best_score)
                    .unwrap_or(true)
            {
                *best_match = Some((score, url));
            }
        }
    };

    if let Ok(edits) = automation
        .create_matcher()
        .from(window_element.clone())
        .control_type(ControlType::Edit)
        .timeout(300)
        .find_all()
    {
        for edit in edits {
            inspect_control(edit, &mut best_match);
        }
    }

    if let Some((score, url)) = &best_match {
        if *score >= 85 {
            return Some(url.clone());
        }
    }

    if let Ok(docs) = automation
        .create_matcher()
        .from(window_element)
        .control_type(ControlType::Document)
        .timeout(300)
        .find_all()
    {
        for doc in docs {
            inspect_control(doc, &mut best_match);
        }
    }

    best_match.map(|(_, url)| url)
}

pub fn page_signature(
    browser: Option<&BrowserContext>,
    window_title: Option<&str>,
) -> Option<String> {
    let browser = browser?;

    browser
        .url
        .as_deref()
        .map(str::to_string)
        .or_else(|| browser.page_title.as_deref().map(str::to_string))
        .or_else(|| window_title.map(str::to_string))
}

fn match_browser(app: &AppInfo) -> Option<&'static BrowserDefinition> {
    let normalized_id = normalize(&app.id);
    let normalized_name = normalize(&app.name);
    let normalized_title = app.title.as_deref().map(normalize);
    let process_name = process_name(&app.id);

    BROWSERS.iter().find(|browser| {
        browser
            .bundle_ids
            .iter()
            .map(|value| normalize(value))
            .any(|value| normalized_id == value)
            || browser
                .processes
                .iter()
                .map(|value| normalize(value))
                .any(|value| normalized_id.ends_with(&value) || process_name == value)
            || browser
                .app_names
                .iter()
                .map(|value| normalize(value))
                .any(|value| {
                    normalized_name == value
                        || normalized_title
                            .as_deref()
                            .map(|title| title == value.as_str())
                            .unwrap_or(false)
                })
    })
}

fn infer_page_title(window_title: Option<&str>, browser: &BrowserDefinition) -> Option<String> {
    let raw = window_title?.trim();
    if raw.is_empty() {
        return None;
    }

    for separator in [" - ", " — ", " – ", " | ", " · "] {
        if let Some((head, tail)) = raw.rsplit_once(separator) {
            let normalized_tail = normalize(tail);
            if tail_matches_browser(&normalized_tail, browser) {
                return clean_page_title(head);
            }
        }

        if let Some((head, tail)) = raw.split_once(separator) {
            let normalized_head = normalize(head);
            if tail_matches_browser(&normalized_head, browser) {
                return clean_page_title(tail);
            }
        }
    }

    clean_page_title(raw)
}

fn tail_matches_browser(value: &str, browser: &BrowserDefinition) -> bool {
    if value == normalize(browser.name) || value.contains(&normalize(browser.name)) {
        return true;
    }

    browser
        .app_names
        .iter()
        .any(|candidate| value.contains(&normalize(candidate)))
}

fn clean_page_title(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if normalize_possible_url(trimmed).is_some() {
        return None;
    }

    Some(trimmed.to_string())
}

fn url_domain(value: &str) -> Option<String> {
    let parsed = Url::parse(value).ok()?;
    parsed
        .domain()
        .or_else(|| parsed.host_str())
        .map(str::to_string)
}

fn infer_url_from_title(window_title: &str) -> Option<String> {
    let title = window_title.trim();
    if title.is_empty() {
        return None;
    }

    if let Some(url) = title
        .split_whitespace()
        .next()
        .and_then(normalize_possible_url)
    {
        return Some(url);
    }

    for separator in [" - ", " — ", " – ", " | ", " · "] {
        for part in title.rsplit(separator) {
            if let Some(url) = normalize_possible_url(part) {
                return Some(url);
            }
        }
    }

    infer_domain_from_text(title).and_then(|domain| normalize_possible_url(&domain))
}

fn infer_domain_from_text(value: &str) -> Option<String> {
    value
        .split(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '|' | '—' | '–' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
                )
        })
        .find_map(domain_from_candidate)
}

fn domain_from_candidate(candidate: &str) -> Option<String> {
    let trimmed = trim_url_candidate(candidate)
        .trim_matches(|ch: char| ch.is_control() || matches!(ch, '\u{200b}' | '\u{feff}' | '。'))
        .trim_end_matches('.');
    if trimmed.is_empty() || trimmed.contains(' ') {
        return None;
    }

    normalize_possible_url(trimmed).and_then(|url| url_domain(&url))
}

fn normalize_possible_url(value: &str) -> Option<String> {
    let candidate = trim_url_candidate(value)
        .trim_matches(|ch: char| ch.is_control() || matches!(ch, '\u{200b}' | '\u{feff}'))
        .trim_end_matches('.');

    if candidate.is_empty() || candidate.contains(' ') {
        return None;
    }

    let candidate_lower = candidate.to_ascii_lowercase();
    if candidate_lower.starts_with("http://") || candidate_lower.starts_with("https://") {
        return Some(candidate.to_string());
    }

    if candidate.contains("://")
        || candidate_lower.starts_with("about:")
        || candidate_lower.starts_with("chrome:")
        || candidate_lower.starts_with("edge:")
        || candidate_lower.starts_with("file:")
    {
        return Some(candidate.to_string());
    }

    let (host, _) = split_host_and_rest(candidate);
    if is_probable_host(host) {
        let host_without_port = split_host_port(host).0;
        let scheme = if host_without_port.eq_ignore_ascii_case("localhost")
            || is_probable_ipv4(host_without_port)
        {
            "http://"
        } else {
            "https://"
        };
        return Some(format!("{}{}", scheme, candidate.trim_end_matches('/')));
    }

    None
}

fn trim_url_candidate(value: &str) -> &str {
    value.trim().trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
        )
    })
}

fn split_host_and_rest(value: &str) -> (&str, &str) {
    if let Some(index) = value.find(|ch| ['/', '?', '#'].contains(&ch)) {
        (&value[..index], &value[index..])
    } else {
        (value, "")
    }
}

fn split_host_port(value: &str) -> (&str, Option<&str>) {
    if let Some(index) = value.rfind(':') {
        let host = &value[..index];
        let port = &value[index + 1..];
        if !host.is_empty() && !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()) {
            return (host, Some(port));
        }
    }

    (value, None)
}

fn is_probable_ipv4(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() != 4 {
        return false;
    }

    parts.iter().all(|part| {
        !part.is_empty()
            && part.len() <= 3
            && part.chars().all(|ch| ch.is_ascii_digit())
            && part.parse::<u8>().is_ok()
    })
}

fn is_probable_host(value: &str) -> bool {
    let host = value.trim().trim_end_matches('.');
    if host.is_empty() {
        return false;
    }

    if host.contains("://") || host.contains('@') {
        return false;
    }

    let (host_without_port, _) = split_host_port(host);
    host_without_port.eq_ignore_ascii_case("localhost")
        || is_probable_ipv4(host_without_port)
        || looks_like_domain(host_without_port)
}

fn looks_like_domain(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() < 2 || parts.iter().any(|part| part.is_empty()) {
        return false;
    }

    let suffix = parts
        .last()
        .copied()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        suffix.as_str(),
        "rs" | "vue" | "tsx" | "ts" | "js" | "json" | "md" | "txt" | "pdf" | "html" | "css"
    ) {
        return false;
    }

    suffix.len() >= 2 && suffix.chars().all(|ch| ch.is_ascii_alphabetic())
}

fn normalize(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn process_name(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .next()
        .map(normalize)
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
struct MacBrowserPage {
    page_title: Option<String>,
    url: Option<String>,
    domain: Option<String>,
    source: String,
    confidence: f32,
}

#[cfg(target_os = "macos")]
fn read_macos_browser_page(browser: &BrowserDefinition) -> Option<MacBrowserPage> {
    let app_name = browser.apple_script_name?;
    let script_lines = if browser.family == "webkit" {
        vec![
            format!("tell application \"{app_name}\""),
            "if it is not running then return \"\"".to_string(),
            "if (count of windows) = 0 then return \"\"".to_string(),
            "set tabTitle to name of current tab of front window".to_string(),
            "set tabUrl to URL of current tab of front window".to_string(),
            "return tabTitle & \"|||AMI|||\" & tabUrl".to_string(),
            "end tell".to_string(),
        ]
    } else {
        vec![
            format!("tell application \"{app_name}\""),
            "if it is not running then return \"\"".to_string(),
            "if (count of windows) = 0 then return \"\"".to_string(),
            "set activeTabRef to active tab of front window".to_string(),
            "set tabTitle to title of activeTabRef".to_string(),
            "set tabUrl to URL of activeTabRef".to_string(),
            "return tabTitle & \"|||AMI|||\" & tabUrl".to_string(),
            "end tell".to_string(),
        ]
    };

    let mut command = std::process::Command::new("osascript");
    for line in script_lines {
        command.arg("-e").arg(line);
    }

    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (page_title, url) = trimmed
        .split_once("|||AMI|||")
        .map(|(title, url)| (clean_page_title(title), clean_url(url)))
        .unwrap_or((clean_page_title(trimmed), None));
    let domain = url.as_deref().and_then(url_domain);
    let has_domain = domain.is_some();

    Some(MacBrowserPage {
        page_title,
        url,
        domain,
        source: "applescript".to_string(),
        confidence: if has_domain { 0.94 } else { 0.78 },
    })
}

#[cfg(target_os = "macos")]
fn clean_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    Url::parse(trimmed).ok().map(|url| url.to_string())
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn firefox_family_session_store_url(app_name: &str, window_title: &str) -> Option<String> {
    let app_lower = app_name.to_ascii_lowercase();
    let base_dir = firefox_family_session_store_base_dir(&app_lower)?;
    let ini_content = fs::read_to_string(base_dir.join("profiles.ini")).ok()?;
    let profile_dir = firefox_family_profile_dir_from_ini(&base_dir, &ini_content)?;

    for session_path in [
        profile_dir
            .join("sessionstore-backups")
            .join("recovery.jsonlz4"),
        profile_dir.join("sessionstore.jsonlz4"),
    ] {
        let Ok(raw) = fs::read(&session_path) else {
            continue;
        };
        let Ok(decoded) = decode_mozlz4_bytes(&raw) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&decoded) else {
            continue;
        };
        if let Some(url) = extract_active_tab_url_from_session_store_value(&value, window_title) {
            return Some(url);
        }
    }

    None
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn firefox_family_session_store_base_dir(app_lower: &str) -> Option<PathBuf> {
    let home_dir = home_dir()?;

    #[cfg(target_os = "macos")]
    {
        let app_support_dir = home_dir.join("Library").join("Application Support");
        return if app_lower.contains("zen") {
            Some(app_support_dir.join("Zen"))
        } else if app_lower.contains("waterfox") {
            Some(app_support_dir.join("Waterfox"))
        } else if app_lower.contains("firefox") {
            Some(app_support_dir.join("Firefox"))
        } else {
            None
        };
    }

    #[cfg(target_os = "linux")]
    {
        return if app_lower.contains("librewolf") {
            Some(home_dir.join(".librewolf"))
        } else if app_lower.contains("waterfox") {
            Some(home_dir.join(".waterfox"))
        } else if app_lower.contains("zen") {
            Some(home_dir.join(".zen"))
        } else if app_lower.contains("firefox") {
            Some(home_dir.join(".mozilla").join("firefox"))
        } else {
            None
        };
    }

    #[cfg(all(test, not(any(target_os = "macos", target_os = "linux"))))]
    {
        return if app_lower.contains("zen") {
            Some(
                home_dir
                    .join("Library")
                    .join("Application Support")
                    .join("Zen"),
            )
        } else if app_lower.contains("waterfox") {
            Some(
                home_dir
                    .join("Library")
                    .join("Application Support")
                    .join("Waterfox"),
            )
        } else if app_lower.contains("firefox") {
            Some(
                home_dir
                    .join("Library")
                    .join("Application Support")
                    .join("Firefox"),
            )
        } else {
            None
        };
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", test)))]
    None
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn firefox_family_profile_dir_from_ini(base_dir: &Path, ini_content: &str) -> Option<PathBuf> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SectionKind {
        Other,
        Install,
        Profile,
    }

    let mut section = SectionKind::Other;
    let mut install_default_path: Option<PathBuf> = None;
    let mut profile_path: Option<String> = None;
    let mut profile_is_relative = true;
    let mut profile_is_default = false;
    let mut default_profile_path: Option<PathBuf> = None;
    let mut first_profile_path: Option<PathBuf> = None;

    let finalize_profile = |profile_path: &mut Option<String>,
                            profile_is_relative: &mut bool,
                            profile_is_default: &mut bool,
                            default_profile_path: &mut Option<PathBuf>,
                            first_profile_path: &mut Option<PathBuf>| {
        let Some(path) = profile_path.take() else {
            *profile_is_relative = true;
            *profile_is_default = false;
            return;
        };

        let resolved = if *profile_is_relative {
            base_dir.join(&path)
        } else {
            PathBuf::from(&path)
        };

        if first_profile_path.is_none() {
            *first_profile_path = Some(resolved.clone());
        }
        if *profile_is_default {
            *default_profile_path = Some(resolved);
        }

        *profile_is_relative = true;
        *profile_is_default = false;
    };

    for raw_line in ini_content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            if section == SectionKind::Profile {
                finalize_profile(
                    &mut profile_path,
                    &mut profile_is_relative,
                    &mut profile_is_default,
                    &mut default_profile_path,
                    &mut first_profile_path,
                );
            }

            let section_name = &line[1..line.len() - 1];
            section = if section_name.starts_with("Install") {
                SectionKind::Install
            } else if section_name.starts_with("Profile") {
                SectionKind::Profile
            } else {
                SectionKind::Other
            };
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        match section {
            SectionKind::Install if key == "Default" => {
                install_default_path = Some(base_dir.join(value));
            }
            SectionKind::Profile => match key {
                "Path" => profile_path = Some(value.to_string()),
                "IsRelative" => profile_is_relative = value != "0",
                "Default" => profile_is_default = value == "1",
                _ => {}
            },
            SectionKind::Other | SectionKind::Install => {}
        }
    }

    if section == SectionKind::Profile {
        finalize_profile(
            &mut profile_path,
            &mut profile_is_relative,
            &mut profile_is_default,
            &mut default_profile_path,
            &mut first_profile_path,
        );
    }

    install_default_path
        .or(default_profile_path)
        .or(first_profile_path)
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn decode_mozlz4_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    const HEADER: &[u8; 8] = b"mozLz40\0";

    if data.len() < 12 {
        return Err("mozlz4 data too short".to_string());
    }
    if &data[..8] != HEADER {
        return Err("invalid mozlz4 header".to_string());
    }

    let expected_len = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let src = &data[12..];
    let mut out = Vec::with_capacity(expected_len);
    let mut index = 0usize;

    while index < src.len() {
        let token = src[index];
        index += 1;

        let mut literal_len = (token >> 4) as usize;
        if literal_len == 15 {
            loop {
                let extra = *src
                    .get(index)
                    .ok_or_else(|| "mozlz4 literal length overflow".to_string())?;
                index += 1;
                literal_len += extra as usize;
                if extra != 255 {
                    break;
                }
            }
        }

        let literal_end = index + literal_len;
        if literal_end > src.len() {
            return Err("mozlz4 literal block overflow".to_string());
        }
        out.extend_from_slice(&src[index..literal_end]);
        index = literal_end;

        if index >= src.len() {
            break;
        }

        let offset = u16::from_le_bytes([
            *src.get(index)
                .ok_or_else(|| "mozlz4 offset overflow".to_string())?,
            *src.get(index + 1)
                .ok_or_else(|| "mozlz4 offset overflow".to_string())?,
        ]) as usize;
        index += 2;

        if offset == 0 || offset > out.len() {
            return Err("invalid mozlz4 offset".to_string());
        }

        let mut match_len = (token & 0x0F) as usize;
        if match_len == 15 {
            loop {
                let extra = *src
                    .get(index)
                    .ok_or_else(|| "mozlz4 match length overflow".to_string())?;
                index += 1;
                match_len += extra as usize;
                if extra != 255 {
                    break;
                }
            }
        }
        match_len += 4;

        let mut match_index = out.len() - offset;
        for _ in 0..match_len {
            let value = *out
                .get(match_index)
                .ok_or_else(|| "mozlz4 match reference overflow".to_string())?;
            out.push(value);
            match_index += 1;
        }
    }

    if out.len() != expected_len {
        return Err(format!(
            "mozlz4 decoded length mismatch: expected={expected_len}, actual={}",
            out.len()
        ));
    }

    Ok(out)
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn normalize_session_store_title(value: &str) -> String {
    [
        " - Mozilla Firefox",
        " - Firefox",
        " - Firefox Developer Edition",
        " - Waterfox",
        " - LibreWolf",
        " - Zen Browser",
        " - Zen",
    ]
    .into_iter()
    .fold(value, |current, suffix| {
        current.split(suffix).next().unwrap_or(current)
    })
    .trim()
    .to_string()
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn extract_active_tab_url_from_session_store_value(
    value: &Value,
    window_title: &str,
) -> Option<String> {
    let windows = value.get("windows")?.as_array()?;
    if windows.is_empty() {
        return None;
    }

    let selected_window_index = value
        .get("selectedWindow")
        .and_then(|value| value.as_u64())
        .unwrap_or(1)
        .saturating_sub(1) as usize;
    let normalized_window_title = normalize_session_store_title(window_title);
    let mut best_match: Option<(i32, u64, String)> = None;

    for (window_index, window) in windows.iter().enumerate() {
        let Some(tabs) = window.get("tabs").and_then(|value| value.as_array()) else {
            continue;
        };

        let selected_tab_index = window
            .get("selected")
            .and_then(|value| value.as_u64())
            .unwrap_or(1)
            .saturating_sub(1) as usize;

        for (tab_index, tab) in tabs.iter().enumerate() {
            let Some(entries) = tab.get("entries").and_then(|value| value.as_array()) else {
                continue;
            };
            if entries.is_empty() {
                continue;
            }

            let selected_entry_index = tab
                .get("index")
                .and_then(|value| value.as_u64())
                .unwrap_or(1)
                .saturating_sub(1) as usize;
            let entry = entries
                .get(selected_entry_index)
                .or_else(|| entries.last())
                .unwrap_or(&entries[0]);

            let Some(raw_url) = entry.get("url").and_then(|value| value.as_str()) else {
                continue;
            };
            let Some(url) = normalize_possible_url(raw_url) else {
                continue;
            };

            let entry_title = entry
                .get("title")
                .and_then(|value| value.as_str())
                .map(normalize_session_store_title)
                .unwrap_or_default();
            let last_accessed = tab
                .get("lastAccessed")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);

            let mut score = 0;
            if !normalized_window_title.is_empty() && !entry_title.is_empty() {
                if entry_title == normalized_window_title {
                    score += 1_000;
                } else if entry_title.contains(&normalized_window_title)
                    || normalized_window_title.contains(&entry_title)
                {
                    score += 600;
                }
            }
            if window_index == selected_window_index {
                score += 120;
            }
            if tab_index == selected_tab_index {
                score += 80;
            }
            if !tab
                .get("hidden")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                score += 20;
            }
            if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
                score += 20;
            }

            let replace = best_match
                .as_ref()
                .map(|(best_score, best_last_accessed, _)| {
                    score > *best_score
                        || (score == *best_score && last_accessed > *best_last_accessed)
                })
                .unwrap_or(true);

            if replace {
                best_match = Some((score, last_accessed, url));
            }
        }
    }

    best_match.map(|(_, _, url)| url)
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::{
        decode_mozlz4_bytes, extract_active_tab_url_from_session_store_value,
        firefox_family_profile_dir_from_ini,
    };

    #[test]
    fn decodes_mozlz4_literal_blocks() {
        let data = [
            b'm', b'o', b'z', b'L', b'z', b'4', b'0', 0, 5, 0, 0, 0, 0x50, b'h', b'e', b'l', b'l',
            b'o',
        ];

        let decoded = decode_mozlz4_bytes(&data).expect("decode mozlz4");
        assert_eq!(decoded, b"hello");
    }

    #[test]
    fn decodes_mozlz4_match_blocks() {
        let data = [
            b'm', b'o', b'z', b'L', b'z', b'4', b'0', 0, 9, 0, 0, 0, 0x32, b'a', b'b', b'c', 0x03,
            0x00,
        ];

        let decoded = decode_mozlz4_bytes(&data).expect("decode repeated sequence");
        assert_eq!(decoded, b"abcabcabc");
    }

    #[test]
    fn extracts_active_tab_url_from_sessionstore() {
        let value = json!({
            "selectedWindow": 1,
            "windows": [
                {
                    "selected": 2,
                    "tabs": [
                        {
                            "index": 1,
                            "entries": [
                                {"url": "https://example.com/older", "title": "旧页面"}
                            ]
                        },
                        {
                            "index": 2,
                            "lastAccessed": 12345,
                            "entries": [
                                {"url": "https://example.com/step-1", "title": "步骤 1"},
                                {"url": "https://example.com/final?q=1", "title": "最终页面"}
                            ]
                        }
                    ]
                }
            ]
        });

        let url =
            extract_active_tab_url_from_session_store_value(&value, "最终页面 - Mozilla Firefox");

        assert_eq!(url.as_deref(), Some("https://example.com/final?q=1"));
    }

    #[test]
    fn resolves_default_profile_from_profiles_ini() {
        let base_dir = Path::new("/tmp/Firefox");
        let ini = r#"
[Install123]
Default=Profiles/install-default

[Profile0]
Name=default-release
IsRelative=1
Path=Profiles/first

[Profile1]
Name=work
IsRelative=1
Path=Profiles/default
Default=1
"#;

        let profile_dir =
            firefox_family_profile_dir_from_ini(base_dir, ini).expect("resolve profile dir");

        assert_eq!(profile_dir, base_dir.join("Profiles/install-default"));
    }
}
