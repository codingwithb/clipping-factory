//! Framing analysis (PRD §11.2): sample frames across the candidate interval,
//! detect faces, and decide the layout.
//!
//! - One persistent face  → smoothed vertical crop centered on the face.
//! - Two or more faces    → uncropped source over a blurred background.
//! - No reliable face     → same blurred-background layout.
//!
//! Face detection uses rustface (SeetaFace, pure Rust). If the model file is
//! missing or detection fails, we degrade gracefully to BlurPad — never crash
//! a render over framing.

use crate::config::Config;
use crate::domain::{CropKey, LayoutPlan, SourceInfo};
use crate::util::run_streaming;
use anyhow::Result;
use rustface::ImageData;
use std::path::Path;
use tokio_util::sync::CancellationToken;

const SAMPLE_FPS: f64 = 1.0;
const SAMPLE_WIDTH: u32 = 480;
/// A face cluster must appear in at least this fraction of sampled frames to
/// count as persistent.
const PERSISTENCE: f64 = 0.5;
/// Cluster width as a fraction of frame width.
const CLUSTER_EPS: f32 = 0.18;

pub async fn analyze_layout(
    cfg: &Config,
    src: &Path,
    source: &SourceInfo,
    start_ms: u64,
    end_ms: u64,
    frames_dir: &Path,
    cancel: &CancellationToken,
) -> Result<LayoutPlan> {
    // Portrait-ish sources can't be face-cropped to 9:16 — pad them.
    if (source.width as f64) / (source.height as f64) < 1.05 {
        return Ok(LayoutPlan::BlurPad);
    }
    let Some(model_path) = cfg.face_model.as_ref() else {
        return Ok(LayoutPlan::BlurPad);
    };

    // 1. Sample frames with ffmpeg.
    tokio::fs::remove_dir_all(frames_dir).await.ok();
    tokio::fs::create_dir_all(frames_dir).await?;
    let dur_s = (end_ms - start_ms) as f64 / 1000.0;
    let args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-ss".into(),
        format!("{:.3}", start_ms as f64 / 1000.0),
        "-t".into(),
        format!("{:.3}", dur_s),
        "-i".into(),
        src.to_string_lossy().into_owned(),
        "-vf".into(),
        format!("fps={},scale={}:-2", SAMPLE_FPS, SAMPLE_WIDTH),
        "-q:v".into(),
        "6".into(),
        frames_dir.join("f%04d.jpg").to_string_lossy().into_owned(),
    ];
    run_streaming(&cfg.ffmpeg, &args, cancel, |_, _| {}).await?;

    let mut frame_paths: Vec<std::path::PathBuf> = std::fs::read_dir(frames_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "jpg").unwrap_or(false))
        .collect();
    frame_paths.sort();
    if frame_paths.is_empty() {
        return Ok(LayoutPlan::BlurPad);
    }

    // 2. Detect faces per frame (blocking CPU work off the async runtime).
    let model_path = model_path.clone();
    let paths = frame_paths.clone();
    let detections: Vec<Vec<f32>> = tokio::task::spawn_blocking(move || {
        detect_all(&model_path, &paths)
    })
    .await??;

    tokio::fs::remove_dir_all(frames_dir).await.ok();

    // 3. Decide the layout.
    let n_frames = detections.len();
    let plan = decide_layout(&detections, n_frames);
    tracing::info!(
        frames = n_frames,
        layout = plan.label(),
        "framing analysis complete"
    );
    Ok(plan)
}

/// Per frame, return the normalized x-centers (0–1) of detected faces.
fn detect_all(model_path: &Path, frames: &[std::path::PathBuf]) -> Result<Vec<Vec<f32>>> {
    let mut detector = rustface::create_detector(&model_path.to_string_lossy())
        .map_err(|e| anyhow::anyhow!("face detector init failed: {}", e))?;
    detector.set_min_face_size(24);
    detector.set_score_thresh(2.0);
    detector.set_pyramid_scale_factor(0.8);
    detector.set_slide_window_step(4, 4);

    let mut out = Vec::with_capacity(frames.len());
    for path in frames {
        let centers = match image::open(path) {
            Ok(img) => {
                let gray = img.to_luma8();
                let (w, h) = (gray.width(), gray.height());
                let data = ImageData::new(&gray, w, h);
                detector
                    .detect(&data)
                    .into_iter()
                    .filter(|f| f.score() > 2.0)
                    .map(|f| {
                        let b = f.bbox();
                        (b.x() as f32 + b.width() as f32 / 2.0) / w as f32
                    })
                    .collect()
            }
            Err(_) => Vec::new(),
        };
        out.push(centers);
    }
    Ok(out)
}

/// Cluster detections across frames and apply the PRD §11.2 decision table.
pub fn decide_layout(detections: &[Vec<f32>], n_frames: usize) -> LayoutPlan {
    if n_frames == 0 {
        return LayoutPlan::BlurPad;
    }
    // 1D clustering of face centers across all frames.
    let mut clusters: Vec<Vec<(usize, f32)>> = Vec::new(); // (frame_idx, cx)
    for (fi, centers) in detections.iter().enumerate() {
        for &cx in centers {
            match clusters.iter_mut().find(|cl| {
                let mean = cl.iter().map(|(_, x)| x).sum::<f32>() / cl.len() as f32;
                (mean - cx).abs() < CLUSTER_EPS
            }) {
                Some(cl) => cl.push((fi, cx)),
                None => clusters.push(vec![(fi, cx)]),
            }
        }
    }
    let persistent: Vec<&Vec<(usize, f32)>> = clusters
        .iter()
        .filter(|cl| {
            let distinct: std::collections::HashSet<usize> =
                cl.iter().map(|(fi, _)| *fi).collect();
            distinct.len() as f64 / n_frames as f64 >= PERSISTENCE
        })
        .collect();

    match persistent.len() {
        1 => {
            let cluster = persistent[0];
            // Per-frame center, forward-filled for frames without a detection.
            let mut per_frame: Vec<Option<f32>> = vec![None; n_frames];
            for (fi, cx) in cluster {
                let slot = &mut per_frame[*fi];
                *slot = Some(slot.map(|v: f32| (v + cx) / 2.0).unwrap_or(*cx));
            }
            let mut filled: Vec<f32> = Vec::with_capacity(n_frames);
            let first = cluster[0].1;
            let mut last = first;
            for slot in per_frame {
                if let Some(v) = slot {
                    last = v;
                }
                filled.push(last);
            }
            // Smooth with a centered moving average (PRD: no rapid movement).
            let smoothed: Vec<f32> = (0..filled.len())
                .map(|i| {
                    let lo = i.saturating_sub(2);
                    let hi = (i + 3).min(filled.len());
                    filled[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
                })
                .collect();
            // Keyframe every ~2s, dropping near-identical neighbours.
            let mut keyframes: Vec<CropKey> = Vec::new();
            for (i, cx) in smoothed.iter().enumerate().step_by(2) {
                let t_ms = (i as f64 / SAMPLE_FPS * 1000.0) as u64;
                if keyframes
                    .last()
                    .map(|k| (k.cx - cx).abs() > 0.012)
                    .unwrap_or(true)
                {
                    keyframes.push(CropKey { t_ms, cx: *cx });
                }
            }
            if keyframes.is_empty() {
                keyframes.push(CropKey { t_ms: 0, cx: smoothed[0] });
            }
            // Cap keyframe count to keep the ffmpeg expression sane.
            while keyframes.len() > 12 {
                keyframes = keyframes.iter().step_by(2).cloned().collect();
            }
            LayoutPlan::FaceCrop { keyframes }
        }
        // 0 → no reliable face; ≥2 → don't guess the active speaker (MVP).
        _ => LayoutPlan::BlurPad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_faces_means_blur_pad() {
        let det: Vec<Vec<f32>> = vec![vec![]; 30];
        assert_eq!(decide_layout(&det, 30), LayoutPlan::BlurPad);
    }

    #[test]
    fn two_persistent_faces_mean_blur_pad() {
        let det: Vec<Vec<f32>> = (0..30).map(|_| vec![0.3, 0.7]).collect();
        assert_eq!(decide_layout(&det, 30), LayoutPlan::BlurPad);
    }

    #[test]
    fn one_persistent_face_means_smoothed_crop() {
        // Face drifts slowly from x=0.40 to x=0.46.
        let det: Vec<Vec<f32>> = (0..30).map(|i| vec![0.40 + i as f32 * 0.002]).collect();
        match decide_layout(&det, 30) {
            LayoutPlan::FaceCrop { keyframes } => {
                assert!(!keyframes.is_empty() && keyframes.len() <= 12);
                assert!(keyframes.windows(2).all(|w| w[1].t_ms > w[0].t_ms));
                for k in &keyframes {
                    assert!(k.cx > 0.35 && k.cx < 0.5);
                }
            }
            other => panic!("expected FaceCrop, got {:?}", other),
        }
    }

    #[test]
    fn flickering_detection_below_persistence_is_blur_pad() {
        // Face appears in only 30% of frames.
        let det: Vec<Vec<f32>> = (0..30)
            .map(|i| if i % 10 < 3 { vec![0.5] } else { vec![] })
            .collect();
        assert_eq!(decide_layout(&det, 30), LayoutPlan::BlurPad);
    }
}
