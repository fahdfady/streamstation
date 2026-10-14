use crate::broadcaster::StationRuntime;
use crate::playlist::{Station, deterministic_order};
use crate::scan;
use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::SystemTime;
use tracing::debug;

pub fn router(rt: Arc<StationRuntime>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/heartbeat", post(heartbeat))
        .route("/api/leave", post(leave))
        .route("/api/rescan", post(rescan))
        .route("/hls/index.m3u8", get(playlist))
        .route("/hls/{name}", get(segment))
        .with_state(rt)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

#[derive(Serialize)]
struct StatusResponse {
    playing: bool,
    viewers: usize,
    count: usize,
    index: Option<usize>,
    title: String,
    next_title: Option<String>,
    offset_secs: f64,
    duration_secs: f64,
    station_total_secs: f64,
    uptime_secs: u64,
}

async fn status(State(rt): State<Arc<StationRuntime>>) -> Json<StatusResponse> {
    let station = rt.station.read().await.clone();
    let position = station.position_at(SystemTime::now());
    let (index, title, next_title, offset_secs, duration_secs) = match position {
        Some(pb) => {
            let entry = &station.entries[pb.index];
            let next = station
                .next_index(pb.index)
                .map(|i| station.entries[i].title.clone());
            (
                Some(pb.index),
                entry.title.clone(),
                next,
                pb.offset.as_secs_f64(),
                entry.duration.as_secs_f64(),
            )
        }
        None => (None, "no programming".to_string(), None, 0.0, 0.0),
    };

    Json(StatusResponse {
        playing: rt.is_playing(),
        viewers: rt.presence.count(),
        count: station.entries.len(),
        index,
        title,
        next_title,
        offset_secs,
        duration_secs,
        station_total_secs: station.total.as_secs_f64(),
        uptime_secs: rt.started.elapsed().as_secs(),
    })
}

#[derive(Deserialize)]
struct Heartbeat {
    session: String,
}

#[derive(Serialize)]
struct ViewersResponse {
    viewers: usize,
}

async fn heartbeat(
    State(rt): State<Arc<StationRuntime>>,
    Json(req): Json<Heartbeat>,
) -> Json<ViewersResponse> {
    let viewers = rt.presence.heartbeat(&req.session);
    debug!("heartbeat from {} -> {} viewer(s)", req.session, viewers);
    Json(ViewersResponse { viewers })
}

async fn leave(
    State(rt): State<Arc<StationRuntime>>,
    Json(req): Json<Heartbeat>,
) -> Json<ViewersResponse> {
    let viewers = rt.presence.leave(&req.session);
    debug!("{} left -> {} viewer(s)", req.session, viewers);
    Json(ViewersResponse { viewers })
}

#[derive(Serialize)]
struct RescanResponse {
    files: usize,
    total_secs: f64,
}

async fn rescan(State(rt): State<Arc<StationRuntime>>) -> Result<Json<RescanResponse>, ApiError> {
    let _guard = rt.rescan_lock.lock().await;
    let cfg = &rt.cfg;
    let cache_path = cfg.cache_dir.join("movies.json");
    let items = scan::scan(&cfg.path, &cfg.extensions, &cache_path).await?;
    let ordered = deterministic_order(items, cfg.shuffle_seed);
    let station = Station::new(ordered, SystemTime::now());
    let response = RescanResponse {
        files: station.entries.len(),
        total_secs: station.total.as_secs_f64(),
    };
    rt.swap_station(Arc::new(station)).await;
    Ok(Json(response))
}

async fn playlist(State(rt): State<Arc<StationRuntime>>) -> Response {
    let path = rt.cfg.temp_dir.join("index.m3u8");
    match tokio::fs::read_to_string(&path).await {
        Ok(body) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            [
                (header::CONTENT_TYPE, "text/plain"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            "stream is idle".to_string(),
        )
            .into_response(),
    }
}

async fn segment(State(rt): State<Arc<StationRuntime>>, Path(name): Path<String>) -> Response {
    let safe = name.ends_with(".ts")
        && std::path::Path::new(&name)
            .file_name()
            .map(|f| f.to_str() == Some(name.as_str()))
            .unwrap_or(false);
    if !safe {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(rt.cfg.temp_dir.join(&name)).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "video/mp2t"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

struct ApiError(anyhow::Error);

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{:#}", self.0),
        )
            .into_response()
    }
}
