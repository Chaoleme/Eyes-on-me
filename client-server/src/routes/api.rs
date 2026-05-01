use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
};
use eyes_on_me_shared::{ActivityEvent, DeviceStatus, StreamMessage};
use serde::Deserialize;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::error;

use crate::app_state::{AnalysisRange, AppState};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/current", get(get_current))
        .route("/api/devices", get(get_devices))
        .route("/api/search/activities", get(search_activities))
        .route("/api/devices/{device_id}", get(get_device_detail))
        .route("/api/analysis", get(get_analysis_overview))
        .route(
            "/api/devices/{device_id}/analysis",
            get(get_device_analysis),
        )
        .route("/api/stream", get(stream))
        .route("/api/agent/activity", post(post_activity))
        .route("/api/agent/status", post(post_status))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ActivityInput {
    Raw(ActivityEvent),
    Envelope(Envelope<ActivityEvent>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StatusInput {
    Raw(DeviceStatus),
    Envelope(Envelope<DeviceStatus>),
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(rename = "type")]
    _message_type: String,
    payload: T,
}

#[derive(Debug, Deserialize, Default)]
struct AnalysisQuery {
    range: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ActivitySearchQuery {
    q: Option<String>,
    device_id: Option<String>,
    limit: Option<u16>,
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn get_current(
    State(state): State<Arc<AppState>>,
) -> Json<eyes_on_me_shared::DashboardSnapshot> {
    Json(state.snapshot())
}

async fn get_devices(
    State(state): State<Arc<AppState>>,
) -> Result<Json<eyes_on_me_shared::DevicesResponse>, StatusCode> {
    state.devices_response().await.map(Json).map_err(|err| {
        error!(error = %err, "failed to load devices response");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

async fn get_device_detail(
    Path(device_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<eyes_on_me_shared::DeviceDetailResponse>, StatusCode> {
    match state.device_detail(&device_id).await {
        Ok(Some(device)) => Ok(Json(device)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(err) => {
            error!(error = %err, device_id, "failed to load device detail");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_analysis_overview(
    Query(query): Query<AnalysisQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<eyes_on_me_shared::AnalysisOverviewResponse>, StatusCode> {
    let range = AnalysisRange::from_query(query.range.as_deref()).ok_or(StatusCode::BAD_REQUEST)?;

    state
        .analysis_overview(range)
        .await
        .map(Json)
        .map_err(|err| {
            error!(error = %err, "failed to load analysis overview");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn get_device_analysis(
    Path(device_id): Path<String>,
    Query(query): Query<AnalysisQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<eyes_on_me_shared::DeviceAnalysisResponse>, StatusCode> {
    let range = AnalysisRange::from_query(query.range.as_deref()).ok_or(StatusCode::BAD_REQUEST)?;

    match state.device_analysis(&device_id, range).await {
        Ok(Some(analysis)) => Ok(Json(analysis)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(err) => {
            error!(error = %err, device_id, "failed to load device analysis");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn search_activities(
    Query(query): Query<ActivitySearchQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<eyes_on_me_shared::ActivitySearchResponse>, StatusCode> {
    let search_query = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let device_id = query
        .device_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let limit = query.limit.unwrap_or(25).clamp(1, 100) as i64;

    state
        .search_activities(search_query, device_id, limit)
        .await
        .map(Json)
        .map_err(|err| {
            error!(error = %err, query = search_query, device_id, "failed to search activities");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn post_activity(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ActivityInput>,
) -> Result<Json<eyes_on_me_shared::DashboardSnapshot>, StatusCode> {
    let payload = match payload {
        ActivityInput::Raw(payload) => payload,
        ActivityInput::Envelope(message) => message.payload,
    };

    if let Err(err) = state.upsert_activity(payload).await {
        error!(error = %err, "failed to persist activity");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(Json(state.snapshot()))
}

async fn post_status(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<StatusInput>,
) -> Result<Json<eyes_on_me_shared::DashboardSnapshot>, StatusCode> {
    let payload = match payload {
        StatusInput::Raw(payload) => payload,
        StatusInput::Envelope(message) => message.payload,
    };

    if let Err(err) = state.update_status(payload).await {
        error!(error = %err, "failed to persist status");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(Json(state.snapshot()))
}

async fn stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let initial_state = state.clone();
    let initial = futures_util::stream::once(async move {
        Ok(Event::default()
            .event("message")
            .json_data(StreamMessage::Snapshot(initial_state.snapshot()))
            .expect("serialize stream snapshot"))
    });

    let updates = BroadcastStream::new(state.subscribe())
        .filter_map(|result| result.ok())
        .map(|message| {
            Ok(Event::default()
                .event("message")
                .json_data(message)
                .expect("serialize stream message"))
        });

    let stream = initial.chain(updates);

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}
