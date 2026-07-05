//! Clip rendering (PRD §11–12), in two passes:
//!
//! 1. [`render_base_clip`] — one continuous source interval → framed
//!    1080×1920 H.264/AAC MP4 **without captions**. This is the expensive
//!    pass (decode, scale, blur or face-tracked crop, encode). The base is
//!    kept on disk so caption styling can change later without re-doing it.
//! 2. [`burn_captions`] — base MP4 + generated ASS → final captioned MP4.
//!    Fast: the video is re-encoded at output size with only the subtitle
//!    filter, and the audio stream is copied bit-for-bit.
//!
//! Two layouts (house style, §11.2/11.3):
//! - BlurPad:  source centered over a blurred, darkened copy of itself.
//! - FaceCrop: smoothed vertical crop following one persistent face,
//!   expressed as a piecewise-linear x(t) crop expression.

use crate::config::Config;
use crate::domain::{CropKey, LayoutPlan, SourceInfo};
use crate::util::run_streaming;
use anyhow::{anyhow, Result};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const OUT_W: u32 = 1080;
const OUT_H: u32 = 1920;

/// Render the framed, uncaptioned base clip from the source video.
/// (The argument list mirrors the render inputs one-to-one on purpose.)
#[allow(clippy::too_many_arguments)]
pub async fn render_base_clip<F>(
    cfg: &Config,
    src: &Path,
    source: &SourceInfo,
    layout: &LayoutPlan,
    start_ms: u64,
    end_ms: u64,
    out_path: &Path,
    cancel: &CancellationToken,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(f32),
{
    let dur_s = (end_ms.saturating_sub(start_ms)) as f64 / 1000.0;
    let graph = build_graph(source, layout, None);

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
    ];
    args.extend(video_encode_args());
    args.extend([
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "160k".into(),
        "-ar".into(),
        "48000".into(),
        "-movflags".into(),
        "+faststart".into(),
    ]);
    // Preserve source frame rate when practical, otherwise 30 fps (PRD §11.1).
    if !(20.0..=60.0).contains(&source.fps) {
        args.push("-r".into());
        args.push("30".into());
    }
    args.push("-progress".into());
    args.push("pipe:1".into());
    args.push(out_path.to_string_lossy().into_owned());

    run_ffmpeg_with_progress(cfg, &args, dur_s, cancel, &mut on_progress)
        .await
        .map_err(|e| {
            if e.to_string().contains("cancelled") {
                e
            } else {
                anyhow!("Render failed. {}", e)
            }
        })?;
    ensure_nontrivial(out_path, "Render produced an empty file. Retry this clip.").await
}

/// Burn ASS captions onto an already-framed base clip. The video is
/// re-encoded (subtitle filter only); the audio stream is copied.
pub async fn burn_captions<F>(
    cfg: &Config,
    base: &Path,
    ass_path: &Path,
    out_path: &Path,
    dur_ms: u64,
    cancel: &CancellationToken,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(f32),
{
    let dur_s = dur_ms as f64 / 1000.0;
    let subs = subtitles_filter(cfg.fonts_dir.as_deref(), ass_path);
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        base.to_string_lossy().into_owned(),
        "-vf".into(),
        subs,
    ];
    args.extend(video_encode_args());
    args.extend([
        "-c:a".into(),
        "copy".into(),
        "-movflags".into(),
        "+faststart".into(),
        "-progress".into(),
        "pipe:1".into(),
        out_path.to_string_lossy().into_owned(),
    ]);

    run_ffmpeg_with_progress(cfg, &args, dur_s, cancel, &mut on_progress)
        .await
        .map_err(|e| {
            if e.to_string().contains("cancelled") {
                e
            } else {
                anyhow!("Caption burn failed. {}", e)
            }
        })?;
    ensure_nontrivial(
        out_path,
        "Caption burn produced an empty file. Retry this clip.",
    )
    .await
}

async fn run_ffmpeg_with_progress<F>(
    cfg: &Config,
    args: &[String],
    dur_s: f64,
    cancel: &CancellationToken,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(f32),
{
    let dur_us = dur_s * 1_000_000.0;
    run_streaming(&cfg.ffmpeg, args, cancel, |is_err, line| {
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
}

/// Shared x264 output settings for both passes (PRD §11.1).
fn video_encode_args() -> [String; 8] {
    [
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-crf".into(),
        "19".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
    ]
}

/// The output must exist and be non-trivial.
async fn ensure_nontrivial(path: &Path, msg: &str) -> Result<()> {
    let size = tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if size < 10_000 {
        return Err(anyhow!("{}", msg));
    }
    Ok(())
}

/// The `ass=` subtitle filter clause, with optional bundled-fonts directory.
pub fn subtitles_filter(fonts_dir: Option<&Path>, ass_path: &Path) -> String {
    let ass = ff_escape_str(&ass_path.to_string_lossy());
    let fonts = fonts_dir
        .map(|d| format!(":fontsdir='{}'", ff_escape_str(&d.to_string_lossy())))
        .unwrap_or_default();
    format!("ass='{}'{}", ass, fonts)
}

fn build_graph(source: &SourceInfo, layout: &LayoutPlan, subs: Option<&str>) -> String {
    // Trailing subtitle step when burning in one pass; empty for base renders.
    let subs_step = subs.map(|s| format!("{},", s)).unwrap_or_default();
    match layout {
        LayoutPlan::BlurPad => format!(
            "[0:v]split=2[bga][fga];\
             [bga]scale={w}:{h}:force_original_aspect_ratio=increase:force_divisible_by=2,\
             crop={w}:{h},gblur=sigma=26,eq=brightness=-0.14:saturation=0.8[bg];\
             [fga]scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2[fg];\
             [bg][fg]overlay=(W-w)/2:(H-h)/2,{subs}format=yuv420p[v]",
            w = OUT_W,
            h = OUT_H,
            subs = subs_step
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
                return build_graph(source, &LayoutPlan::BlurPad, subs);
            }
            let expr = crop_x_expr(keyframes, scaled_w);
            format!(
                "[0:v]scale=-2:{h}:force_divisible_by=2,\
                 crop={w}:{h}:x='{expr}':y=0,{subs}format=yuv420p[v]",
                w = OUT_W,
                h = OUT_H,
                expr = expr,
                subs = subs_step
            )
        }
    }
}

/// Piecewise-linear x(t) between keyframes, clamped to the scaled frame.
/// `t` in the crop filter is the output timestamp in seconds (0 at clip start).
pub fn crop_x_expr(keyframes: &[CropKey], scaled_w: u64) -> String {
    let max_x = (scaled_w - OUT_W as u64) as f64;
    let px = |cx: f32| -> f64 {
        ((cx as f64) * scaled_w as f64 - (OUT_W as f64) / 2.0).clamp(0.0, max_x)
    };

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

/// Escape a string for use inside a single-quoted ffmpeg filter option value.
/// Backslashes are normalized to `/` (paths), `:` separates filter options,
/// and a literal `'` must close the quote, emit an escaped quote, and reopen.
fn ff_escape_str(s: &str) -> String {
    s.replace('\\', "/")
        .replace(':', "\\:")
        .replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(w: u32, h: u32) -> SourceInfo {
        SourceInfo {
            filename: "s.mp4".into(),
            duration_ms: 60_000,
            width: w,
            height: h,
            fps: 30.0,
            video_codec: "h264".into(),
            audio_codec: "aac".into(),
            size_bytes: 1,
        }
    }

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
                CropKey {
                    t_ms: 2000,
                    cx: 0.5,
                },
                CropKey {
                    t_ms: 4000,
                    cx: 0.45,
                },
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

    #[test]
    fn escapes_single_quotes_for_filter_values() {
        // A quote inside a quoted value must close, escape, and reopen.
        assert_eq!(ff_escape_str("a'b"), "a'\\''b");
    }

    #[test]
    fn base_graph_has_no_subtitle_filter() {
        for layout in [
            LayoutPlan::BlurPad,
            LayoutPlan::FaceCrop {
                keyframes: vec![CropKey { t_ms: 0, cx: 0.5 }],
            },
        ] {
            let g = build_graph(&source(1920, 1080), &layout, None);
            assert!(
                !g.contains("ass="),
                "base graph must not burn captions: {g}"
            );
            assert!(g.ends_with("[v]"));
        }
    }

    #[test]
    fn captioned_graph_includes_subtitle_filter() {
        let subs = subtitles_filter(None, Path::new("/tmp/c.ass"));
        let g = build_graph(&source(1920, 1080), &LayoutPlan::BlurPad, Some(&subs));
        assert!(g.contains("ass='/tmp/c.ass'"));
    }

    #[test]
    fn subtitles_filter_escapes_fonts_dir() {
        let s = subtitles_filter(Some(Path::new("/a'b")), Path::new("/tmp/c.ass"));
        assert!(s.contains("fontsdir='/a'\\''b'"));
    }

    #[test]
    fn portrait_source_falls_back_to_blur_pad() {
        // 540×1280 scales to 810×1920 — narrower than the 1080 crop window.
        let g = build_graph(
            &source(540, 1280),
            &LayoutPlan::FaceCrop {
                keyframes: vec![CropKey { t_ms: 0, cx: 0.5 }],
            },
            None,
        );
        assert!(
            g.contains("gblur"),
            "portrait must fall back to blur-pad: {g}"
        );
    }
}
