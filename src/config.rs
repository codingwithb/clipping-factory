//! Runtime configuration resolved from environment variables with sensible
//! discovery-based defaults. Every external dependency can be overridden:
//!
//! - `CF_PORT`            — HTTP port (default 4571)
//! - `CF_DATA_DIR`        — project state root (default ~/.clipping-factory)
//! - `CF_OUTPUT_DIR`      — finished clip root (default ~/Downloads/Clipping Factory)
//! - `CF_FFMPEG` / `CF_FFPROBE`
//! - `CF_WHISPER_BIN`     — whisper.cpp `whisper-cli`
//! - `CF_WHISPER_MODEL`   — ggml model path
//! - `CF_FONTS_DIR`       — directory containing caption fonts
//! - `CF_FACE_MODEL`      — rustface seeta model path
//! - `CF_THREADS`         — transcription threads (default = physical cores)
//! - `CF_BIND_ALL=1`      — listen on 0.0.0.0 instead of 127.0.0.1
//! - `CF_NO_OPEN=1`       — don't try to open the browser on start

use crate::util::which;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub bind_all: bool,
    pub open_browser: bool,
    pub data_dir: PathBuf,
    pub output_root: PathBuf,
    pub ffmpeg: String,
    pub ffprobe: String,
    pub whisper_bin: Option<PathBuf>,
    pub whisper_model: Option<PathBuf>,
    pub fonts_dir: Option<PathBuf>,
    pub caption_font: String,
    /// Default caption style when a project doesn't specify one: "impact" | "clean".
    pub caption_style: String,
    pub face_model: Option<PathBuf>,
    pub threads: usize,
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

fn first_existing(cands: Vec<PathBuf>) -> Option<PathBuf> {
    cands.into_iter().find(|p| p.is_file())
}

impl Config {
    pub fn resolve() -> Config {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let data_dir = env_path("CF_DATA_DIR").unwrap_or_else(|| home.join(".clipping-factory"));
        let output_root = env_path("CF_OUTPUT_DIR")
            .unwrap_or_else(|| home.join("Downloads").join("Clipping Factory"));

        // Homebrew's regular FFmpeg omits libass; prefer its full build when installed.
        let homebrew_full = PathBuf::from("/opt/homebrew/opt/ffmpeg-full/bin");
        let ffmpeg = std::env::var("CF_FFMPEG").unwrap_or_else(|_| {
            let binary = homebrew_full.join("ffmpeg");
            if binary.is_file() {
                binary.to_string_lossy().into_owned()
            } else {
                "ffmpeg".into()
            }
        });
        let ffprobe = std::env::var("CF_FFPROBE").unwrap_or_else(|_| {
            let binary = homebrew_full.join("ffprobe");
            if binary.is_file() {
                binary.to_string_lossy().into_owned()
            } else {
                "ffprobe".into()
            }
        });

        // whisper.cpp binary: env → PATH → common local build locations.
        let whisper_bin = env_path("CF_WHISPER_BIN")
            .filter(|p| p.is_file())
            .or_else(|| which("whisper-cli"))
            .or_else(|| which("whisper-cpp"))
            .or_else(|| {
                first_existing(vec![
                    cwd.join("whisper.cpp/build/bin/whisper-cli"),
                    cwd.join("../whisper.cpp/build/bin/whisper-cli"),
                    PathBuf::from("/opt/homebrew/bin/whisper-cli"),
                    PathBuf::from("/usr/local/bin/whisper-cli"),
                ])
            });

        // Model: env → data dir → common local locations.
        let whisper_model = env_path("CF_WHISPER_MODEL")
            .filter(|p| p.is_file())
            .or_else(|| {
                first_existing(vec![
                    data_dir.join("models/ggml-base.en.bin"),
                    data_dir.join("models/ggml-small.en.bin"),
                    cwd.join("models/ggml-base.en.bin"),
                    cwd.join("../models/ggml-base.en.bin"),
                ])
            });

        // Caption fonts: bundled assets dir preferred.
        let fonts_dir = env_path("CF_FONTS_DIR").filter(|p| p.is_dir()).or_else(|| {
            let d = cwd.join("assets/fonts");
            if d.is_dir() {
                Some(d)
            } else {
                None
            }
        });
        let caption_font = if fonts_dir
            .as_ref()
            .and_then(|d| std::fs::read_dir(d).ok())
            .map(|entries| {
                entries.flatten().any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .to_lowercase()
                        .starts_with("inter")
                })
            })
            .unwrap_or(false)
        {
            "Inter".to_string()
        } else {
            "DejaVu Sans".to_string()
        };

        let face_model = env_path("CF_FACE_MODEL")
            .filter(|p| p.is_file())
            .or_else(|| {
                first_existing(vec![
                    cwd.join("assets/models/seeta_fd_frontal_v1.0.bin"),
                    data_dir.join("models/seeta_fd_frontal_v1.0.bin"),
                ])
            });

        let threads = std::env::var("CF_THREADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
            });

        Config {
            port: std::env::var("CF_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4571),
            bind_all: std::env::var("CF_BIND_ALL")
                .map(|v| v == "1")
                .unwrap_or(false),
            open_browser: std::env::var("CF_NO_OPEN")
                .map(|v| v != "1")
                .unwrap_or(true),
            data_dir,
            output_root,
            ffmpeg,
            ffprobe,
            whisper_bin,
            whisper_model,
            fonts_dir,
            caption_font,
            caption_style: std::env::var("CF_CAPTION_STYLE").unwrap_or_else(|_| "impact".into()),
            face_model,
            threads,
        }
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.data_dir.join("projects")
    }
}
