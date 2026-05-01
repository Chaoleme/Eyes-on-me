use std::time::Duration;

use anyhow::{Result, anyhow};
use reqwest::{Client, StatusCode};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::event::ActivityEnvelope;

pub async fn run_transport(
    server_api_base_url: String,
    api_token: String,
    mut rx: mpsc::Receiver<ActivityEnvelope>,
) -> Result<()> {
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;

    let max_retry_delay = Duration::from_secs(30);
    let mut retry_delay = Duration::from_secs(2);
    let mut pending: Option<ActivityEnvelope> = None;

    loop {
        let event = match pending.take() {
            Some(event) => event,
            None => {
                let Some(event) = rx.recv().await else {
                    info!("event channel closed, transport exiting");
                    return Ok(());
                };
                event
            }
        };

        let endpoint = endpoint_for(&server_api_base_url, event.message_type)?;
        match send_event(&client, &endpoint, &api_token, &event).await {
            Ok(()) => {
                retry_delay = Duration::from_secs(2);
            }
            Err(err) => {
                warn!(
                    error = %err,
                    endpoint = %endpoint,
                    delay_secs = retry_delay.as_secs(),
                    "event delivery failed, scheduling retry"
                );
                pending = Some(event);
                sleep(retry_delay).await;
                retry_delay = std::cmp::min(max_retry_delay, retry_delay.saturating_mul(2));
            }
        }
    }
}

fn endpoint_for(server_api_base_url: &str, message_type: &str) -> Result<String> {
    let path = match message_type {
        "activity" => "/api/agent/activity",
        "status" => "/api/agent/status",
        other => return Err(anyhow!("unsupported agent message type: {other}")),
    };

    Ok(format!("{server_api_base_url}{path}"))
}

async fn send_event(
    client: &Client,
    endpoint: &str,
    api_token: &str,
    event: &ActivityEnvelope,
) -> Result<()> {
    let response = client
        .post(endpoint)
        .bearer_auth(api_token)
        .json(event)
        .send()
        .await?;

    if response.status().is_success() {
        info!(
            endpoint = %endpoint,
            app_id = %event.payload.app.id,
            browser_name = event.payload.browser.as_ref().map(|browser| browser.name.as_str()).unwrap_or("n/a"),
            pid = ?event.payload.app.pid,
            "event sent"
        );
        return Ok(());
    }

    let status = response.status();
    let detail = response
        .text()
        .await
        .unwrap_or_else(|_| "failed to read error response".to_string());

    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(anyhow!(
            "server rejected agent token: HTTP {status} {detail}"
        ));
    }

    Err(anyhow!("server returned HTTP {status}: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::endpoint_for;

    #[test]
    fn resolves_activity_endpoint() {
        let endpoint = endpoint_for("http://127.0.0.1:8787", "activity").unwrap();
        assert_eq!(endpoint, "http://127.0.0.1:8787/api/agent/activity");
    }

    #[test]
    fn rejects_unknown_message_type() {
        let result = endpoint_for("http://127.0.0.1:8787", "unknown");
        assert!(result.is_err());
    }
}
