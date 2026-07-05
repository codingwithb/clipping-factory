//! Clipping Factory — a local clipping studio.
//! One podcast MP4 in → several faithful, polished 9:16 clips out.
//!
//! Launch with one command: `cargo run --release` (or run the built binary).

mod api;
mod captions;
mod config;
mod domain;
mod frame;
mod media;
mod pipeline;
mod render;
mod select;
mod settings;
mod state;
mod store;
mod transcribe;
mod util;
mod validate;

use config::Config;
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clipping_factory=info,tower_http=warn".into()),
        )
        .init();

    let cfg = Config::resolve();
    std::fs::create_dir_all(cfg.projects_dir()).ok();

    first_run_report(&cfg).await;

    let state = AppState::new(cfg.clone());
    let app = api::router(state);

    let host = if cfg.bind_all { "0.0.0.0" } else { "127.0.0.1" };
    let addr = format!("{}:{}", host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let url = format!("http://localhost:{}", cfg.port);
    println!("\n  Clipping Factory studio ready → {}\n", url);

    if cfg.open_browser {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(opener).arg(&url).spawn();
    }

    axum::serve(listener, app).await?;
    Ok(())
}

/// PRD §7.1: verify FFmpeg, FFprobe, the transcription runtime, and disk space.
async fn first_run_report(cfg: &Config) {
    let check = |ok: bool| if ok { "ok" } else { "MISSING" };
    let ffmpeg_ok = util::run_capture(&cfg.ffmpeg, &["-version".into()])
        .await
        .is_ok();
    let ffmpeg_ass = util::ffmpeg_has_ass(&cfg.ffmpeg).await;
    let ffprobe_ok = util::run_capture(&cfg.ffprobe, &["-version".into()])
        .await
        .is_ok();
    let disk = util::disk_free_gb(&cfg.data_dir).await;

    println!("  Clipping Factory — first-run checks");
    println!("  ├─ ffmpeg        {}", check(ffmpeg_ok));
    println!("  ├─ ASS captions  {}", check(ffmpeg_ass));
    println!("  ├─ ffprobe       {}", check(ffprobe_ok));
    println!(
        "  ├─ whisper-cli   {}",
        cfg.whisper_bin
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "MISSING (brew install whisper-cpp, or set CF_WHISPER_BIN)".into())
    );
    println!(
        "  ├─ whisper model {}",
        cfg.whisper_model
            .as_ref()
            .map(|p| format!(
                "{} ({} MB)",
                p.to_string_lossy(),
                std::fs::metadata(p)
                    .map(|m| m.len() / 1_000_000)
                    .unwrap_or(0)
            ))
            .unwrap_or_else(|| {
                format!(
                    "MISSING — download ggml-base.en.bin (~148 MB) to {}/models/",
                    cfg.data_dir.to_string_lossy()
                )
            })
    );
    println!(
        "  ├─ face model    {}",
        cfg.face_model
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "missing (optional — clips fall back to blur-pad layout)".into())
    );
    println!("  ├─ caption font  {}", cfg.caption_font);
    println!(
        "  ├─ disk free     {}",
        disk.map(|g| format!("{:.1} GB", g))
            .unwrap_or_else(|| "unknown".into())
    );
    println!("  ├─ projects dir  {}", cfg.data_dir.to_string_lossy());
    println!("  └─ output dir    {}", cfg.output_root.to_string_lossy());
}
