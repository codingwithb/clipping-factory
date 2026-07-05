//! Media inspection (ffprobe) and audio extraction (ffmpeg) — PRD §7.2 steps
//! "Inspect source" and "Extract audio", with the validation and edge cases
//! from §15.

use crate::config::Config;
use crate::domain::SourceInfo;
use crate::util::{run_capture, run_streaming};
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub const MIN_SOURCE_MS: u64 = 20_000;
pub const MAX_SOURCE_MS: u64 = 4 * 3600 * 1000;

/// Inspect and validate the uploaded source. Errors are user-actionable.
pub async fn probe(cfg: &Config, src: &Path, original_filename: &str) -> Result<SourceInfo> {
    let args: Vec<String> = vec![
        "-v".into(),
        "error".into(),
        "-print_format".into(),
        "json".into(),
        "-show_format".into(),
        "-show_streams".into(),
        src.to_string_lossy().into_owned(),
    ];
    let out = run_capture(&cfg.ffprobe, &args)
        .await
        .map_err(|e| anyhow!("This file could not be read as a video. {}", e))?;
    let v: serde_json::Value =
        serde_json::from_str(&out).context("ffprobe returned unparseable output")?;

    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .ok_or_else(|| anyhow!("No video stream found. Attach an MP4 that contains video."))?;
    let audio = streams
        .iter()
        .find(|s| s["codec_type"] == "audio")
        .ok_or_else(|| {
            anyhow!(
                "This MP4 has no audio stream. Clipping Factory needs speech audio to work with."
            )
        })?;

    let duration_s: f64 = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse().ok())
        .or_else(|| video["duration"].as_str().and_then(|d| d.parse().ok()))
        .ok_or_else(|| {
            anyhow!("Could not determine the video duration. The file may be corrupted.")
        })?;
    let duration_ms = (duration_s * 1000.0) as u64;

    if duration_ms < MIN_SOURCE_MS {
        bail!(
            "This video is {}s long. Sources must be at least 20 seconds.",
            duration_ms / 1000
        );
    }
    if duration_ms > MAX_SOURCE_MS {
        bail!("This video is over 4 hours. The MVP supports sources up to 4 hours.");
    }

    let width = video["width"].as_u64().unwrap_or(0) as u32;
    let height = video["height"].as_u64().unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        bail!("Could not read the video dimensions. The file may be corrupted.");
    }

    let fps = parse_rate(video["avg_frame_rate"].as_str().unwrap_or(""))
        .or_else(|| parse_rate(video["r_frame_rate"].as_str().unwrap_or("")))
        .unwrap_or(30.0);

    let size_bytes = v["format"]["size"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::fs::metadata(src).map(|m| m.len()).unwrap_or(0));

    Ok(SourceInfo {
        filename: original_filename.to_string(),
        duration_ms,
        width,
        height,
        fps,
        video_codec: video["codec_name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        audio_codec: audio["codec_name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        size_bytes,
    })
}

fn parse_rate(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/')?;
    let num: f64 = num.parse().ok()?;
    let den: f64 = den.parse().ok()?;
    if den == 0.0 || num == 0.0 {
        None
    } else {
        Some(num / den)
    }
}

/// Extract mono 16kHz WAV for transcription, reporting progress 0–1.
pub async fn extract_audio<F>(
    cfg: &Config,
    src: &Path,
    out_wav: &Path,
    duration_ms: u64,
    cancel: &CancellationToken,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(f32),
{
    let args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        src.to_string_lossy().into_owned(),
        "-vn".into(),
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        "16000".into(),
        "-c:a".into(),
        "pcm_s16le".into(),
        "-progress".into(),
        "pipe:1".into(),
        out_wav.to_string_lossy().into_owned(),
    ];
    let dur_us = (duration_ms as f64) * 1000.0;
    run_streaming(&cfg.ffmpeg, &args, cancel, |is_err, line| {
        // ffmpeg -progress emits `out_time_ms=<microseconds>` lines on stdout.
        if !is_err {
            if let Some(us) = line
                .strip_prefix("out_time_ms=")
                .and_then(|v| v.parse::<f64>().ok())
            {
                if dur_us > 0.0 {
                    on_progress((us / dur_us).clamp(0.0, 1.0) as f32);
                }
            }
        }
    })
    .await
    .map_err(|e| {
        if e.to_string().contains("cancelled") {
            e
        } else {
            anyhow!("Audio extraction failed. {}", e)
        }
    })?;
    Ok(())
}
