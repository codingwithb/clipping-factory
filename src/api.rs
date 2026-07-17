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
//! POST   /api/projects/{id}/clips/{clipId}/restyle   re-burn captions (style/color)
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
        .route(
            "/api/projects/{id}/clips/{clip}/download",
            get(serve_clip_download),
        )
        .route(
            "/api/projects/{id}/clips/{clip}/restyle",
            post(restyle_clip),
        )
        .route(
            "/api/projects/{id}/open-output-folder",
            post(open_output_folder),
        )
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
            let body = tokio::fs::read_to_string(&disk)
                .await
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
        tokio::fs::read_to_string(&disk)
            .await
            .unwrap_or_else(|_| include_str!("../static/index.html").to_string()),
    )
}

// ---------------------------------------------------------------------------
// Setup & settings
// ---------------------------------------------------------------------------

async fn setup_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cfg = &state.cfg;
    let ffmpeg_ok = crate::util::run_capture(&cfg.ffmpeg, &["-version".into()])
        .await
        .is_ok();
    let ffmpeg_ass = crate::util::ffmpeg_has_ass(&cfg.ffmpeg).await;
    let ffprobe_ok = crate::util::run_capture(&cfg.ffprobe, &["-version".into()])
        .await
        .is_ok();
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
        "caption_fonts": crate::captions::CAPTION_FONTS,
        "accent_palette": crate::accent::ACCENT_PALETTE,
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
        return Err(bad_request(
            "provider must be openai, anthropic, or offline",
        ));
    }
    let updated = {
        let mut s = state.settings.write().unwrap();
        s.provider = body.provider;
        s.model = body.model;
        // Empty key = keep the existing one (lets users switch model without retyping).
        if !body.api_key.trim().is_empty() {
            s.api_key = Some(body.api_key.trim().to_string());
        }
        s.clone()
    };
    // `settings::save` touches the filesystem synchronously (write + chmod
    // 0600) — keep that work off the async workers.
    let data_dir = state.cfg.data_dir.clone();
    let to_save = updated.clone();
    tokio::task::spawn_blocking(move || crate::settings::save(&data_dir, &to_save))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(
        serde_json::to_value(updated.public()).unwrap_or_default(),
    ))
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
    let mut accent_mode = crate::accent::AccentMode::default();
    let mut framing_mode = FramingMode::default();

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
                    accent_color = Some(if v.starts_with('#') {
                        v
                    } else {
                        format!("#{}", v)
                    });
                }
            }
            continue;
        }
        if field.name() == Some("accent_mode") {
            if let Ok(v) = field.text().await {
                accent_mode = match crate::accent::AccentMode::parse(&v) {
                    Some(mode) => mode,
                    None => {
                        tokio::fs::remove_dir_all(state.store.project_dir(&id))
                            .await
                            .ok();
                        return Err(bad_request(
                            "accent_mode must be manual, random, or optimized",
                        ));
                    }
                };
            }
            continue;
        }
        if field.name() == Some("framing_mode") {
            if let Ok(v) = field.text().await {
                framing_mode = match v.trim() {
                    "background" => FramingMode::Background,
                    _ => FramingMode::Fill,
                };
            }
            continue;
        }
        if field.name() != Some("file") {
            continue;
        }
        original_name = field.file_name().unwrap_or("source.mp4").to_string();
        let lower = original_name.to_lowercase();
        if !(lower.ends_with(".mp4") || lower.ends_with(".m4v")) {
            tokio::fs::remove_dir_all(state.store.project_dir(&id))
                .await
                .ok();
            return Err(bad_request(
                "Attach an .mp4 file. Other containers are post-MVP.",
            ));
        }
        // Stream to disk without buffering the whole video in memory (PRD §7.2).
        let mut file = tokio::fs::File::create(&dest)
            .await
            .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| bad_request(format!("upload interrupted: {e}")))?
        {
            wrote_bytes += chunk.len() as u64;
            file.write_all(&chunk)
                .await
                .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
        }
        file.flush().await.ok();
    }

    if wrote_bytes == 0 {
        tokio::fs::remove_dir_all(state.store.project_dir(&id))
            .await
            .ok();
        return Err(bad_request(
            "No file received. Drop one MP4 into the studio.",
        ));
    }

    accent_color = match accent_mode {
        crate::accent::AccentMode::Random => Some(crate::accent::random_accent().to_string()),
        crate::accent::AccentMode::Optimized => match crate::accent::optimized_accent_for_video(
            &state.cfg.ffmpeg,
            &state.cfg.ffprobe,
            &dest,
        )
        .await
        {
            Ok(color) => Some(color.to_string()),
            Err(error) => {
                tracing::warn!("video color optimization failed, using default accent: {error:#}");
                Some(
                    crate::captions::default_accent_hex(crate::captions::CaptionStyle::Impact)
                        .to_string(),
                )
            }
        },
        crate::accent::AccentMode::Manual => accent_color,
    };

    let mut project = Project::new(id.clone(), dest);
    project.caption_style = caption_style;
    project.accent_color = accent_color;
    project.framing_mode = framing_mode;
    state
        .store
        .save_project(&project)
        .await
        .map_err(ApiError::from)?;
    // The original filename lives in a sidecar file; the inspect stage and
    // output-folder naming read it from there.
    tokio::fs::write(
        state.store.project_dir(&id).join("original-name.txt"),
        &original_name,
    )
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
    let mut p = state
        .store
        .load_project(&id)
        .await
        .map_err(ApiError::from)?;
    let handle = state.handle(&id);
    if p.status.is_active() && !handle.is_running() {
        p.status = JobState::Failed;
        p.error = Some("Processing was interrupted (the server restarted). Retry to resume from the last completed stage.".into());
        state.store.save_project(&p).await.ok();
        // A hard interruption can also strand manifest clips in `rendering`;
        // reset them so the next run re-renders instead of skipping them.
        if let Ok(mut m) = state.store.load_manifest(&id).await {
            let mut changed = false;
            for c in &mut m.clips {
                if c.status == ClipStatus::Rendering {
                    c.status = ClipStatus::Pending;
                    changed = true;
                }
            }
            if changed {
                state.store.save_manifest(&id, &m).await.ok();
            }
        }
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
    let original_name =
        tokio::fs::read_to_string(state.store.project_dir(id).join("original-name.txt"))
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
    let rest = BroadcastStream::new(rx)
        .filter_map(|msg| async move { msg.ok().map(|data| Ok(Event::default().data(data))) });
    Ok(Sse::new(first.chain(rest)).keep_alive(KeepAlive::default()))
}

// ---------------------------------------------------------------------------
// Post-render caption restyling
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct RestyleIn {
    /// "impact" | "clean". Omitted = keep the clip's current style.
    #[serde(default)]
    style: Option<String>,
    /// `#RRGGBB`. Omitted = keep the clip's current accent.
    #[serde(default)]
    accent_color: Option<String>,
    /// Curated caption font. Omitted = keep the clip's current font.
    #[serde(default)]
    font: Option<String>,
}

/// Releases the per-clip restyle lock on every exit path.
struct RestyleGuard {
    state: AppState,
    key: String,
}
impl Drop for RestyleGuard {
    fn drop(&mut self) {
        self.state.end_restyle(&self.key);
    }
}

/// Re-burn one rendered clip's captions with a new style and/or accent color.
/// Fast path: re-encode from the framed, uncaptioned base intermediate.
/// Older projects without a base rebuild it from the source first (one time).
async fn restyle_clip(
    State(state): State<AppState>,
    AxPath((id, clip_id)): AxPath<(String, String)>,
    Json(body): Json<RestyleIn>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::captions::{
        accent_bgr_for, build_ass, caption_font_name, default_accent_hex, hex_to_ass_bgr,
        words_in_interval, CaptionInput, CaptionStyle,
    };

    if !state.store.exists(&id) {
        return Err(not_found("Project not found."));
    }
    if state.handle(&id).is_running() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Processing is still running. Restyle clips once rendering finishes.".into(),
        ));
    }
    let key = format!("{id}/{clip_id}");
    if !state.try_begin_restyle(&key) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "A restyle is already running for this clip.".into(),
        ));
    }
    let _guard = RestyleGuard {
        state: state.clone(),
        key,
    };

    let mut manifest = state
        .store
        .load_manifest(&id)
        .await
        .map_err(|_| not_found("No rendered clips for this project yet."))?;
    let idx = manifest
        .clips
        .iter()
        .position(|c| c.id == clip_id)
        .ok_or_else(|| not_found("Clip not found."))?;
    let clip = manifest.clips[idx].clone();
    if clip.status != ClipStatus::Ready {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Only rendered clips can be restyled.".into(),
        ));
    }

    let cfg = &state.cfg;

    let style = match body.style.as_deref() {
        Some(s) => CaptionStyle::parse_strict(s)
            .ok_or_else(|| bad_request("style must be \"impact\" or \"clean\""))?,
        None => CaptionStyle::from_str(clip.caption_style.as_deref().unwrap_or("impact")),
    };
    let accent_hex = match body.accent_color.as_deref() {
        Some(c) => {
            hex_to_ass_bgr(c).ok_or_else(|| bad_request("accent_color must be #RRGGBB"))?;
            let c = c.trim();
            if c.starts_with('#') {
                c.to_string()
            } else {
                format!("#{c}")
            }
        }
        None => clip
            .accent_color
            .clone()
            .unwrap_or_else(|| default_accent_hex(style).to_string()),
    };
    let caption_font = match body.font.as_deref() {
        Some(font) => caption_font_name(font)
            .ok_or_else(|| bad_request("font must be one of the curated caption fonts"))?
            .to_string(),
        None => clip
            .caption_font
            .clone()
            .unwrap_or_else(|| cfg.caption_font.clone()),
    };

    let p = state
        .store
        .load_project(&id)
        .await
        .map_err(ApiError::from)?;
    let transcript = state.store.load_transcript(&id).await.map_err(|_| {
        ApiError(
            StatusCode::CONFLICT,
            "The transcript is no longer on disk, so captions cannot be rebuilt.".into(),
        )
    })?;
    let cancel = tokio_util::sync::CancellationToken::new();

    // Ensure the framed, uncaptioned base exists (projects rendered before
    // base intermediates existed rebuild it here from the source, one time).
    let base_path = state.store.base_clip_path(&id, &clip.id);
    if !base_path.is_file() {
        let source = p.source.clone().ok_or_else(|| {
            ApiError(
                StatusCode::CONFLICT,
                "Source metadata is missing; re-run this project.".into(),
            )
        })?;
        if !p.source_path.is_file() {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "The original video is no longer on disk, so this clip cannot be restyled.".into(),
            ));
        }
        tokio::fs::create_dir_all(state.store.base_dir(&id))
            .await
            .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
        crate::render::render_base_clip(
            cfg,
            &p.source_path,
            &source,
            &clip.layout,
            clip.start_ms,
            clip.end_ms,
            &base_path,
            &cancel,
            |_| {},
        )
        .await
        .map_err(ApiError::from)?;
    }

    // Build the new captions and burn them onto the base.
    let words = words_in_interval(&transcript.words, clip.start_ms, clip.end_ms);
    let ass = build_ass(
        &CaptionInput {
            words: &words,
            clip_start_ms: clip.start_ms,
            clip_end_ms: clip.end_ms,
            headline: &clip.headline,
            font: &caption_font,
            accent_bgr: accent_bgr_for(style, Some(&accent_hex)),
        },
        style,
    );
    let clips_dir = state.store.clips_dir(&id);
    let ass_path = clips_dir.join(format!("{}.restyle.ass", clip.id));
    let tmp_out = clips_dir.join(format!("{}.restyle.tmp.mp4", clip.id));
    tokio::fs::write(&ass_path, &ass)
        .await
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
    let burn = crate::render::burn_captions(
        cfg,
        &base_path,
        &ass_path,
        &tmp_out,
        clip.end_ms.saturating_sub(clip.start_ms),
        &cancel,
        |_| {},
    )
    .await;
    tokio::fs::remove_file(&ass_path).await.ok();
    if let Err(e) = burn {
        tokio::fs::remove_file(&tmp_out).await.ok();
        return Err(ApiError::from(e));
    }

    // Swap the restyled clip into place, then refresh the copy in the
    // user-facing output folder (best-effort, mirroring the render stage).
    let final_path = clips_dir.join(&clip.filename);
    tokio::fs::rename(&tmp_out, &final_path)
        .await
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
    if let Some(dir) = manifest.output_dir.clone() {
        tokio::fs::copy(&final_path, std::path::Path::new(&dir).join(&clip.filename))
            .await
            .ok();
    }

    manifest.clips[idx].caption_style = Some(style.label().to_string());
    manifest.clips[idx].accent_color = Some(accent_hex);
    manifest.clips[idx].caption_font = Some(caption_font);
    state
        .store
        .save_manifest(&id, &manifest)
        .await
        .map_err(ApiError::from)?;
    state
        .handle(&id)
        .emit(json!({"type": "clip", "clip": manifest.clips[idx]}));
    Ok(Json(
        serde_json::to_value(&manifest.clips[idx]).unwrap_or_default(),
    ))
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
        return Err(not_found(
            "Clip file is not on disk (render may have failed).",
        ));
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
            let end: u64 = if b.is_empty() {
                len.saturating_sub(1)
            } else {
                b.parse().ok()?
            };
            if start > end || end >= len {
                None
            } else {
                Some((start, end))
            }
        });

    let mut builder = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        // Clip bytes change in place when captions are restyled — never let
        // the browser reuse a cached copy.
        .header(header::CACHE_CONTROL, "no-store")
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
    builder
        .body(axum::body::Body::from(buf))
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))
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
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let opened = std::process::Command::new(opener).arg(&dir).spawn().is_ok();
    Ok(Json(json!({ "opened": opened, "path": dir })))
}
