//! Clip rendering (PRD §11–12): one continuous source interval → polished
//! 1080×1920 H.264/AAC MP4 with burned captions, using FFmpeg filtergraphs.
//!
//! Two layouts (house style, §11.2/11.3):
//! - BlurPad:  source centered over a blurred, darkened copy of itself.
//! - FaceCrop: smoothed vertical crop following one persistent face,
//!             expressed as a piecewise-linear x(t) crop expression.

use crate::config::Config;
use crate::domain::{CropKey, LayoutPlan, SourceInfo};
use crate::util::run_streaming;
use anyhow::{anyhow, Result};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const OUT_W: u32 = 1080;
const OUT_H: u32 = 1920;

pub async fn render_clip<F>(
    cfg: &Config,
    src: &Path,
    source: &SourceInfo,
    layout: &LayoutPlan,
    start_ms: u64,
    end_ms: u64,
    ass_path: &Path,
    out_path: &Path,
    cancel: &CancellationToken,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(f32),
{
    let dur_s = (end_ms.saturating_sub(start_ms)) as f64 / 1000.0;
    let graph = build_graph(cfg, source, layout, ass_path)?;

    let mut args: Vec<String> = vec![
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
        "-filter_complex".into(),
        graph,
        "-map".into(),
        "[v]".into(),
        "-map".into(),
        "0:a:0".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-crf".into(),
        "19".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "160k".into(),
        "-ar".into(),
        "48000".into(),
        "-movflags".into(),
        "+faststart".into(),
    ];
    // Preserve source frame rate when practical, otherwise 30 fps (PRD §11.1).
    if !(20.0..=60.0).contains(&source.fps) {
        args.push("-r".into());
        args.push("30".into());
    }
    args.push("-progress".into());
    args.push("pipe:1".into());
    args.push(out_path.to_string_lossy().into_owned());

    let dur_us = dur_s * 1_000_000.0;
    run_streaming(&cfg.ffmpeg, &args, cancel, |is_err, line| {
        if !is_err {
            if let Some(us) = line.strip_prefix("out_time_ms=").and_then(|v| v.parse::<f64>().ok())
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
            anyhow!("Render failed. {}", e)
        }
    })?;

    // Sanity: the output must exist and be non-trivial.
    let size = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
    if size < 10_000 {
        return Err(anyhow!("Render produced an empty file. Retry this clip."));
    }
    Ok(())
}

fn build_graph(
    cfg: &Config,
    source: &SourceInfo,
    layout: &LayoutPlan,
    ass_path: &Path,
) -> Result<String> {
    let ass = ff_escape_path(ass_path);
    let fonts = cfg
        .fonts_dir
        .as_ref()
        .map(|d| format!(":fontsdir='{}'", ff_escape_str(&d.to_string_lossy())))
        .unwrap_or_default();
    let subtitles = format!("ass='{}'{}", ass, fonts);

    Ok(match layout {
        LayoutPlan::BlurPad => format!(
            "[0:v]split=2[bga][fga];\
             [bga]scale={w}:{h}:force_original_aspect_ratio=increase:force_divisible_by=2,\
             crop={w}:{h},gblur=sigma=26,eq=brightness=-0.14:saturation=0.8[bg];\
             [fga]scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2[fg];\
             [bg][fg]overlay=(W-w)/2:(H-h)/2,{subs}[v]",
            w = OUT_W,
            h = OUT_H,
            subs = subtitles
        ),
        LayoutPlan::FaceCrop { keyframes } => {
            // Scale so height fills 1920, then crop a 1080-wide window whose
            // x follows the smoothed face track.
            let scaled_w = {
                let w = (source.width as f64) * (OUT_H as f64) / (source.height as f64);
                (w / 2.0).round() as u64 * 2
            };
            if scaled_w < OUT_W as u64 {
                // Shouldn't happen (frame.rs guards portrait), but stay safe.
                return build_graph(cfg, source, &LayoutPlan::BlurPad, ass_path);
            }
            let expr = crop_x_expr(keyframes, scaled_w);
            format!(
                "[0:v]scale=-2:{h}:force_divisible_by=2,\
                 crop={w}:{h}:x='{expr}':y=0,{subs}[v]",
                w = OUT_W,
                h = OUT_H,
                expr = expr,
                subs = subtitles
            )
        }
    })
}

/// Piecewise-linear x(t) between keyframes, clamped to the scaled frame.
/// `t` in the crop filter is the output timestamp in seconds (0 at clip start).
pub fn crop_x_expr(keyframes: &[CropKey], scaled_w: u64) -> String {
    let max_x = (scaled_w - OUT_W as u64) as f64;
    let px = |cx: f32| -> f64 { ((cx as f64) * scaled_w as f64 - (OUT_W as f64) / 2.0).clamp(0.0, max_x) };

    match keyframes.len() {
        0 => format!("{:.1}", max_x / 2.0),
        1 => format!("{:.1}", px(keyframes[0].cx)),
        _ => {
            // Innermost value: hold the last keyframe.
            let mut expr = format!("{:.1}", px(keyframes[keyframes.len() - 1].cx));
            for pair in keyframes.windows(2).rev() {
                let (a, b) = (&pair[0], &pair[1]);
                let (t0, t1) = (a.t_ms as f64 / 1000.0, b.t_ms as f64 / 1000.0);
                let (x0, x1) = (px(a.cx), px(b.cx));
                if t1 <= t0 {
                    continue;
                }
                expr = format!(
                    "if(lt(t\\,{t1:.3})\\,{x0:.1}+({x1:.1}-{x0:.1})*(t-{t0:.3})/{dt:.3}\\,{rest})",
                    t1 = t1,
                    x0 = x0,
                    x1 = x1,
                    t0 = t0,
                    dt = t1 - t0,
                    rest = expr
                );
            }
            expr
        }
    }
}

/// Escape a path for use inside an ffmpeg filter option value.
fn ff_escape_path(p: &Path) -> String {
    ff_escape_str(&p.to_string_lossy())
}
fn ff_escape_str(s: &str) -> String {
    s.replace('\\', "/").replace(':', "\\:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_keyframe_is_constant() {
        let e = crop_x_expr(&[CropKey { t_ms: 0, cx: 0.5 }], 3414);
        // 0.5*3414 - 540 = 1167
        assert_eq!(e, "1167.0");
    }

    #[test]
    fn keyframes_clamp_to_frame_edges() {
        let e = crop_x_expr(&[CropKey { t_ms: 0, cx: 0.02 }], 3414);
        assert_eq!(e, "0.0");
        let e = crop_x_expr(&[CropKey { t_ms: 0, cx: 0.99 }], 3414);
        assert_eq!(e, format!("{:.1}", (3414 - 1080) as f64));
    }

    #[test]
    fn multi_keyframe_builds_piecewise_expression() {
        let e = crop_x_expr(
            &[
                CropKey { t_ms: 0, cx: 0.4 },
                CropKey { t_ms: 2000, cx: 0.5 },
                CropKey { t_ms: 4000, cx: 0.45 },
            ],
            3414,
        );
        assert!(e.starts_with("if(lt(t\\,2.000)"));
        assert!(e.contains("if(lt(t\\,4.000)"));
    }

    #[test]
    fn escapes_colons_in_paths() {
        assert_eq!(ff_escape_str("C:/x/y"), "C\\:/x/y");
    }
}
