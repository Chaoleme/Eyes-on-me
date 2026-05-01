use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::browser::BrowserContext;
use eyes_on_me_shared::PresenceState;

#[derive(Debug, Clone, Serialize)]
pub struct ActivityEnvelope {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub payload: ActivityPayload,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityPayload {
    pub event_id: String,
    pub ts: String,
    pub device_id: String,
    pub agent_name: String,
    pub platform: &'static str,
    pub kind: &'static str,
    pub app: AppInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserContext>,
    pub presence: PresenceState,
    pub source: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl ActivityEnvelope {
    pub fn activity(
        device_id: &str,
        agent_name: &str,
        platform: &'static str,
        source: &'static str,
        kind: &'static str,
        mut app: AppInfo,
        window_title: Option<String>,
        browser: Option<BrowserContext>,
        presence: PresenceState,
    ) -> Self {
        if app.title.is_none() {
            app.title = window_title.clone();
        }

        Self {
            message_type: "activity",
            payload: ActivityPayload {
                event_id: Uuid::new_v4().to_string(),
                ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                device_id: device_id.to_string(),
                agent_name: agent_name.to_string(),
                platform,
                kind,
                app,
                window_title,
                browser,
                presence,
                source,
            },
        }
    }
}
