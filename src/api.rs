//! HTTP API (PRD §14.1) + the studio's static assets.
//!
//! POST   /api/settings/ai            set provider/model/key
//! GET    /api/settings/ai            public status (never the key)
//! POST   /api/settings/ai/test       verify connectivity
//! GET    /api/setup                  first-run environment checks
//! POST   /api/projects               multipart MP4 upload → project (auto-starts)
//! GET    /api/projects/{id}          full project view
//! GET    /api/projects/{id}/events   SSE progress stream
//! POST   /api/projects/{id}/process  start/resume
//! POST   /api/projects/{id}/cancel   stop subprocesses, keep finished clips
//! POST   /api/projects/{id}/retry    re-run failed stage / failed clips only
//! GET    /api/projects/{id}/clips/{clipId}           inline MP4 (Range-aware)
//! GET    /api/projects/{id}/clips/{clipId}/download  attachment
//! POST   /api/projects/{id}/open-output-folder

use crate::domain::*;
use crate::pipeline;
use crate::settings::AiSettings;
use crate::state::AppState;
use axum::extract::{DefaultBodyLimit, Multipart, Path as AxPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream, StreamExt};
use serde_json::json;
use std::convert::Infallible;
use tokio::io::AsyncWriteExt;
use tokio_stream::wrappers::BroadcastStream;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/styles.css", get(styles_css))
        .route("/app.js", get(app_js))
        .route("/api/setup", get(setup_status))
        .route("/api/settings/ai", get(get_settings).post(set_settings))
        .route("/api/settings/ai/test", post(test_settings))
        .route("/api/projects", post(create_project))
        .route("/api/projects/{id}", get(get_project))
        .route("/api/projects/{id}/events", get(project_events))
        .route("/api/projects/{id}/process", post(process_project))
        .route("/api/projects/{id}/cancel", post(cancel_project))
        .route("/api/projects/{id}/retry", post(retry_project))
        .route("/api/projects/{id}/clips/{clip}", get(serve_clip_inline))
        .route("/api/projects/{id}/clips/{clip}/download", get(serve_clip_download))
        .route("/api/projects/{id}/open-output-folder", post(open_output_folder))
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}
fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}

// ---------------------------------------------------------------------------
// Static studio assets: prefer ./static on disk (dev), fall back to embedded.
// ---------------------------------------------------------------------------

macro_rules! static_asset {
    ($fn_name:ident, $file:literal, $ct:literal) => {
        async fn $fn_name() -> Response {
            let disk = std::path::Path::new("static").join($file);
            let body = std::fs::read_to_string(&disk)
                .unwrap_or_else(|_| include_str!(concat!("../static/", $file)).to_string());
            ([(header::CONTENT_TYPE, $ct)], body).into_response()
        }
    };
}
static_asset!(styles_css, "styles.css", "text/css; charset=utf-8");
static_asset!(app_js, "app.js", "application/javascript; charset=utf-8");

async fn index_html() -> Html<String> {
    let disk = std::path::Path::new("static").join("index.html");
    Html(
        std::fs::read_to_string(&disk)
            .unwrap_or_else(|_| include_str!("../static/index.html").to_string()),
    )
}

// ---------------------------------------------------------------------------
// Setup & settings
// ---------------------------------------------------------------------------

async fn setup_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cfg = &state.cfg;
    let ffmpeg_ok = crate::util::run_capture(&cfg.ffmpeg, &["-version".into()]).await.is_ok();
    let ffmpeg_ass = crate::util::ffmpeg_has_ass(&cfg.ffmpeg).await;
    let ffprobe_ok = crate::util::run_capture(&cfg.ffprobe, &["-version".into()]).await.is_ok();
    let model_size = cfg
        .whisper_model
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len() / 1_000_000);
    let disk = crate::util::disk_free_gb(&cfg.data_dir).await;
    Json(json!({
        "ffmpeg": ffmpeg_ok,
        "ffmpeg_ass": ffmpeg_ass,
        "ffprobe": ffprobe_ok,
        "whisper_cli": cfg.whisper_bin.as_ref().map(|p| p.to_string_lossy()).unwrap_or_default(),
        "whisper_ok": cfg.whisper_bin.is_some(),
        "model_ok": cfg.whisper_model.is_some(),
        "model_mb": model_size,
        "face_model_ok": cfg.face_model.is_some(),
        "caption_font": cfg.caption_font,
        "disk_free_gb": disk,
        "data_dir": cfg.data_dir.to_string_lossy(),
        "output_root": cfg.output_root.to_string_lossy(),
    }))
}

async fn get_settings(State(state): State<AppState>) -> Json<serde_json::Value> {
    let s = state.settings.read().unwrap().public();
    Json(serde_json::to_value(s).unwrap_or_default())
}

#[derive(serde::Deserialize)]
struct SettingsIn {
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    api_key: String,
}

async fn set_settings(
    State(state): State<AppState>,
    Json(body): Json<SettingsIn>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !["openai", "anthropic", "offline"].contains(&body.provider.as_str()) {
        return Err(bad_request("provider must be openai, anthropic, or offline"));
    }
    let updated = {
        let mut s = state.settings.write().unwrap();
        s.provider = body.provider;
        s.model = body.model;
        // Empty key = keep the existing one (lets users switch model without retyping).
        if !body.api_key.trim().is_empty() {
            s.api_key = Some(body.api_key.trim().to_string());
        }
        if s.provider == "offline" {
            // Keys are never needed (or sent anywhere) in offline mode.
        }
        s.clone()
    };
    crate::settings::save(&state.cfg.data_dir, &updated)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::to_value(updated.public()).unwrap_or_default()))
}

async fn test_settings(State(state): State<AppState>) -> Json<serde_json::Value> {
    let settings: AiSettings = state.settings.read().unwrap().clone();
    match crate::select::test_connection(&settings).await {
        Ok(msg) => Json(json!({ "ok": true, "message": msg })),
        Err(e) => Json(json!({ "ok": false, "message": e.to_string() })),
    }
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

async fn create_project(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = crate::util::short_id();
    state.store.create_dirs(&id).await.map_err(ApiError::from)?;
    let dest = state.store.source_path(&id);

    let mut original_name = String::new();
    let mut wrote_bytes: u64 = 0;
    let mut caption_style: Option<String> = None;
    let mut accent_color: Option<String> = None;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| bad_request(format!("upload error: {e}")))?
    {
        if field.name() == Some("caption_style") {
            if let Ok(v) = field.text().await {
                let v = v.trim().to_lowercase();
                if v == "impact" || v == "clean" {
                    caption_style = Some(v);
                }
            }
            continue;
        }
        if field.name() == Some("accent_color") {
            if let Ok(v) = field.text().await {
                let v = v.trim().to_string();
                if crate::captions::hex_to_ass_bgr(&v).is_some() {
                    accent_color = Some(if v.starts_with('#') { v } else { format!("#{}", v) });
                }
            }
            continue;
        }
        if field.name() != Some("file") {
            continue;
        }
        original_name = field.file_name().unwrap_or("source.mp4").to_string();
        let lower = original_name.to_lowercase();
        if !(lower.ends_with(".mp4") || lower.ends_with(".m4v")) {
            tokio::fs::remove_dir_all(state.store.project_dir(&id)).await.ok();
            return Err(bad_request("Attach an .mp4 file. Other containers are post-MVP."));
        }
        // Stream to disk without buffering the whole video in memory (PRD §7.2).
        let mut file = tokio::fs::File::create(&dest).await.map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| bad_request(format!("upload interrupted: {e}")))?
        {
            wrote_bytes += chunk.len() as u64;
            file.write_all(&chunk).await.map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
        }
        file.flush().await.ok();
    }

    if wrote_bytes == 0 {
        tokio::fs::remove_dir_all(state.store.project_dir(&id)).await.ok();
        return Err(bad_request("No file received. Drop one MP4 into the studio."));
    }

    let mut project = Project::new(id.clone(), dest);
    project.caption_style = caption_style;
    project.accent_color = accent_color;
    // Stash the original filename for the inspect stage & output folder name.
    project.source = None;
    state.store.save_project(&project).await.map_err(ApiError::from)?;
    // Keep the original name in a sidecar spot: we thread it through probe by
    // writing it into the project after inspection; store it now in `error`-free
    // metadata via a rename-safe file.
    tokio::fs::write(state.store.project_dir(&id).join("original-name.txt"), &original_name)
        .await
        .ok();

    // Processing begins automatically (PRD §7.2).
    pipeline::start(state.clone(), id.clone()).ok();

    let view = project_view(&state, &id).await.map_err(ApiError::from)?;
    Ok(Json(view))
}

async fn get_project(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.store.exists(&id) {
        return Err(not_found("Project not found."));
    }
    // Detect interrupted runs (server restarted mid-processing).
    let mut p = state.store.load_project(&id).await.map_err(ApiError::from)?;
    let handle = state.handle(&id);
    if p.status.is_active() && !handle.is_running() {
        p.status = JobState::Failed;
        p.error = Some("Processing was interrupted (the server restarted). Retry to resume from the last completed stage.".into());
        state.store.save_project(&p).await.ok();
    }
    let view = project_view(&state, &id).await.map_err(ApiError::from)?;
    Ok(Json(view))
}

async fn project_view(state: &AppState, id: &str) -> anyhow::Result<serde_json::Value> {
    let p = state.store.load_project(id).await?;
    let handle = state.handle(id);
    let live = handle.live.lock().unwrap().clone();
    let selection = state.store.load_selection(id).await.ok();
    let manifest = state.store.load_manifest(id).await.ok();
    let original_name = tokio::fs::read_to_string(state.store.project_dir(id).join("original-name.txt"))
        .await
        .unwrap_or_default();

    let rejected_summary: Vec<serde_json::Value> = selection
        .as_ref()
        .map(|s| {
            s.rejected
                .iter()
                .take(12)
                .map(|r| {
                    json!({
                        "headline": r.candidate.headline,
                        "start_ms": r.candidate.start_ms,
                        "end_ms": r.candidate.end_ms,
                        "reasons": r.reasons,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(json!({
        "project": p,
        "original_name": original_name.trim(),
        "running": handle.is_running(),
        "live": live,
        "accepted": selection.as_ref().map(|s| s.accepted.len()).unwrap_or(0),
        "rejected": selection.as_ref().map(|s| s.rejected.len()).unwrap_or(0),
        "rejected_summary": rejected_summary,
        "selector": p.selector,
        "clips": manifest.as_ref().map(|m| m.clips.clone()).unwrap_or_default(),
        "output_dir": manifest.as_ref().and_then(|m| m.output_dir.clone()),
    }))
}

async fn process_project(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.store.exists(&id) {
        return Err(not_found("Project not found."));
    }
    match pipeline::start(state.clone(), id) {
        Ok(()) => Ok(Json(json!({ "started": true }))),
        Err(msg) => Err(ApiError(StatusCode::CONFLICT, msg)),
    }
}

async fn cancel_project(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.store.exists(&id) {
        return Err(not_found("Project not found."));
    }
    let was_running = pipeline::cancel(&state, &id);
    Ok(Json(json!({ "cancelling": was_running })))
}

async fn retry_project(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.store.exists(&id) {
        return Err(not_found("Project not found."));
    }
    match pipeline::retry(state.clone(), id).await {
        Ok(()) => Ok(Json(json!({ "restarted": true }))),
        Err(msg) => Err(ApiError(StatusCode::CONFLICT, msg)),
    }
}

// ---------------------------------------------------------------------------
// SSE
// ---------------------------------------------------------------------------

async fn project_events(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    if !state.store.exists(&id) {
        return Err(not_found("Project not found."));
    }
    let handle = state.handle(&id);
    let rx = handle.events.subscribe();

    let snapshot = project_view(&state, &id)
        .await
        .map(|v| json!({ "type": "snapshot", "view": v }).to_string())
        .unwrap_or_else(|_| json!({ "type": "snapshot" }).to_string());

    let first = stream::once(async move { Ok(Event::default().data(snapshot)) });
    let rest = BroadcastStream::new(rx).filter_map(|msg| async move {
        msg.ok().map(|data| Ok(Event::default().data(data)))
    });
    Ok(Sse::new(first.chain(rest)).keep_alive(KeepAlive::default()))
}

// ---------------------------------------------------------------------------
// Clip serving (Range-aware so <video> scrubbing works, esp. Safari)
// ---------------------------------------------------------------------------

async fn find_clip(
    state: &AppState,
    id: &str,
    clip_id: &str,
) -> Result<(ClipRecord, std::path::PathBuf), ApiError> {
    let manifest = state
        .store
        .load_manifest(id)
        .await
        .map_err(|_| not_found("No rendered clips for this project yet."))?;
    let clip = manifest
        .clips
        .iter()
        .find(|c| c.id == clip_id)
        .cloned()
        .ok_or_else(|| not_found("Clip not found."))?;
    let path = state.store.clips_dir(id).join(&clip.filename);
    if !path.is_file() {
        return Err(not_found("Clip file is not on disk (render may have failed)."));
    }
    Ok((clip, path))
}

async fn serve_clip_inline(
    State(state): State<AppState>,
    AxPath((id, clip_id)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (_clip, path) = find_clip(&state, &id, &clip_id).await?;
    serve_video(&path, &headers, None).await
}

async fn serve_clip_download(
    State(state): State<AppState>,
    AxPath((id, clip_id)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (clip, path) = find_clip(&state, &id, &clip_id).await?;
    serve_video(&path, &headers, Some(clip.filename)).await
}

async fn serve_video(
    path: &std::path::Path,
    headers: &HeaderMap,
    download_name: Option<String>,
) -> Result<Response, ApiError> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
    let len = file
        .metadata()
        .await
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))?
        .len();

    // Parse a simple `bytes=start-end` range.
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|spec| {
            let (a, b) = spec.split_once('-')?;
            let start: u64 = a.parse().ok()?;
            let end: u64 = if b.is_empty() { len.saturating_sub(1) } else { b.parse().ok()? };
            if start > end || end >= len {
                None
            } else {
                Some((start, end))
            }
        });

    let mut builder = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "video/mp4");
    if let Some(name) = &download_name {
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", name),
        );
    }

    let (start, end, status) = match range {
        Some((s, e)) => (s, e, StatusCode::PARTIAL_CONTENT),
        None => (0, len.saturating_sub(1), StatusCode::OK),
    };
    let read_len = end - start + 1;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;

    // Read the requested window in one buffered pass (clips are tens of MB max;
    // range requests keep individual reads small during scrubbing).
    let mut buf = Vec::with_capacity(read_len.min(64 * 1024 * 1024) as usize);
    let mut limited = file.take(read_len);
    limited
        .read_to_end(&mut buf)
        .await
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;

    builder = builder
        .status(status)
        .header(header::CONTENT_LENGTH, buf.len().to_string());
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end, len),
        );
    }
    Ok(builder
        .body(axum::body::Body::from(buf))
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))?)
}

// ---------------------------------------------------------------------------

async fn open_output_folder(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let manifest = state.store.load_manifest(&id).await.ok();
    let dir = manifest
        .and_then(|m| m.output_dir)
        .unwrap_or_else(|| state.cfg.output_root.to_string_lossy().into_owned());
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let opened = std::process::Command::new(opener)
        .arg(&dir)
        .spawn()
        .is_ok();
    Ok(Json(json!({ "opened": opened, "path": dir })))
}
