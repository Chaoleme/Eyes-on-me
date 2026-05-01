use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Macos,
    Windows,
    Linux,
    Android,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    Active,
    Idle,
    Locked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityApp {
    pub id: String,
    pub name: String,
    pub title: Option<String>,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserContext {
    pub family: String,
    pub name: String,
    pub page_title: Option<String>,
    pub url: Option<String>,
    pub domain: Option<String>,
    pub source: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    ForegroundChanged,
    ActivitySample,
    PresenceChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEvent {
    pub event_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub device_id: String,
    pub agent_name: String,
    pub platform: Platform,
    pub kind: ActivityKind,
    pub app: ActivityApp,
    pub window_title: Option<String>,
    pub browser: Option<BrowserContext>,
    pub presence: PresenceState,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub device_id: String,
    pub agent_name: String,
    pub platform: Platform,
    pub status_text: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardSnapshot {
    pub devices: Vec<ActivityEvent>,
    pub latest_status: Option<DeviceStatus>,
    pub recent_activities: Vec<ActivityEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceOverview {
    pub device: ActivityEvent,
    pub latest_status: Option<DeviceStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevicesResponse {
    pub devices: Vec<DeviceOverview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceDetailResponse {
    pub device: ActivityEvent,
    pub latest_status: Option<DeviceStatus>,
    pub recent_activities: Vec<ActivityEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBucket {
    pub key: String,
    pub label: String,
    pub sublabel: Option<String>,
    pub total_tracked_ms: u64,
    pub sessions: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageUsageBucket {
    pub key: String,
    pub label: String,
    pub url: Option<String>,
    pub total_tracked_ms: u64,
    pub sessions: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainUsageBucket {
    pub key: String,
    pub label: String,
    pub total_tracked_ms: u64,
    pub sessions: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: OffsetDateTime,
    pub pages: Vec<PageUsageBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserUsageBucket {
    pub key: String,
    pub label: String,
    pub family: String,
    pub total_tracked_ms: u64,
    pub sessions: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: OffsetDateTime,
    pub domains: Vec<DomainUsageBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAnalysisSummary {
    pub device_id: String,
    pub platform: Platform,
    pub current_label: String,
    pub latest_status_text: Option<String>,
    pub total_tracked_ms: u64,
    pub event_count: usize,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisOverviewResponse {
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub device_count: usize,
    pub total_tracked_ms: u64,
    pub work_tracked_ms: u64,
    pub browser_tracked_ms: u64,
    pub app_count: usize,
    pub devices: Vec<DeviceAnalysisSummary>,
    pub top_app_usage: Vec<UsageBucket>,
    pub top_domain_usage: Vec<UsageBucket>,
    pub top_browser_usage: Vec<BrowserUsageBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAnalysisResponse {
    pub device_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub total_tracked_ms: u64,
    pub work_tracked_ms: u64,
    pub browser_tracked_ms: u64,
    pub app_count: usize,
    pub event_count: usize,
    pub current_label: Option<String>,
    pub latest_status: Option<DeviceStatus>,
    pub app_usage: Vec<UsageBucket>,
    pub domain_usage: Vec<UsageBucket>,
    pub browser_usage: Vec<BrowserUsageBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySearchHit {
    pub activity: ActivityEvent,
    pub snippet: Option<String>,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySearchResponse {
    pub query: String,
    pub device_id: Option<String>,
    pub total: usize,
    pub results: Vec<ActivitySearchHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum StreamMessage {
    Snapshot(DashboardSnapshot),
    Ping {
        #[serde(with = "time::serde::rfc3339")]
        ts: OffsetDateTime,
    },
}

impl DashboardSnapshot {
    pub fn demo() -> Self {
        let now = OffsetDateTime::now_utc();
        let activity = ActivityEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            ts: now,
            device_id: "my-mac".to_string(),
            agent_name: "client-desktop".to_string(),
            platform: Platform::Macos,
            kind: ActivityKind::ForegroundChanged,
            app: ActivityApp {
                id: "com.apple.Safari".to_string(),
                name: "Safari".to_string(),
                title: Some("eyes-on-me".to_string()),
                pid: Some(4242),
            },
            window_title: Some("am-i-okay dashboard".to_string()),
            browser: Some(BrowserContext {
                family: "webkit".to_string(),
                name: "Safari".to_string(),
                page_title: Some("eyes-on-me".to_string()),
                url: Some("https://example.com/eyes-on-me".to_string()),
                domain: Some("example.com".to_string()),
                source: "demo".to_string(),
                confidence: 0.9,
            }),
            presence: PresenceState::Active,
            source: "demo".to_string(),
        };

        let status = DeviceStatus {
            ts: now,
            device_id: "my-mac".to_string(),
            agent_name: "client-desktop".to_string(),
            platform: Platform::Macos,
            status_text: "building Eyes on Me".to_string(),
            source: "demo".to_string(),
        };

        Self {
            devices: vec![activity.clone()],
            latest_status: Some(status),
            recent_activities: vec![activity],
        }
    }
}
