use anyhow::{bail, Context, Result};
use std::path::Path;
use tokio::process::Command;

pub const ACCENT_PALETTE: [&str; 6] = [
    "#FFDD00", "#7CFF4F", "#FF4F4F", "#4FB5FF", "#C77DFF", "#FF9F1C",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccentMode {
    #[default]
    Manual,
    Random,
    Optimized,
}

impl AccentMode {
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_lowercase().as_str() {
            "manual" => Some(Self::Manual),
            "random" => Some(Self::Random),
            "optimized" => Some(Self::Optimized),
            _ => None,
        }
    }
}

pub fn random_accent() -> &'static str {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    ACCENT_PALETTE[bytes[0] as usize % ACCENT_PALETTE.len()]
}

/// Pick a vivid preset that contrasts with the video's average luminance and
/// sits far from its average color. The fixed palette keeps every result
/// legible with the caption styles' black outline.
pub fn optimized_accent(pixels: &[[u8; 3]]) -> &'static str {
    if pixels.is_empty() {
        return ACCENT_PALETTE[0];
    }
    let count = pixels.len() as f64;
    let mean = pixels.iter().fold([0.0; 3], |mut sum, pixel| {
        for i in 0..3 {
            sum[i] += pixel[i] as f64 / count;
        }
        sum
    });
    let source_luma = relative_luminance(mean);

    let mut best = 0;
    let mut best_score = f64::MIN;
    for (i, hex) in ACCENT_PALETTE.iter().enumerate() {
        let rgb = rgb_from_hex(hex);
        let candidate = [rgb[0] as f64, rgb[1] as f64, rgb[2] as f64];
        let distance = mean
            .iter()
            .zip(candidate)
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f64>()
            .sqrt()
            / 441.7;
        let candidate_luma = relative_luminance(candidate);
        let contrast =
            (source_luma.max(candidate_luma) + 0.05) / (source_luma.min(candidate_luma) + 0.05);
        let score = distance * 1.4 + contrast.min(7.0) / 7.0;
        if score > best_score {
            best = i;
            best_score = score;
        }
    }
    ACCENT_PALETTE[best]
}

fn rgb_from_hex(hex: &str) -> [u8; 3] {
    let hex = hex.trim_start_matches('#');
    [
        u8::from_str_radix(&hex[0..2], 16).unwrap_or_default(),
        u8::from_str_radix(&hex[2..4], 16).unwrap_or_default(),
        u8::from_str_radix(&hex[4..6], 16).unwrap_or_default(),
    ]
}

fn relative_luminance(rgb: [f64; 3]) -> f64 {
    let channel = |v: f64| {
        let v = v / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

pub async fn optimized_accent_for_video(
    ffmpeg: &str,
    ffprobe: &str,
    source: &Path,
) -> Result<&'static str> {
    let probe = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            &source.to_string_lossy(),
        ])
        .output()
        .await
        .context("could not inspect video duration")?;
    if !probe.status.success() {
        bail!("ffprobe could not read video duration");
    }
    let duration: f64 = String::from_utf8_lossy(&probe.stdout)
        .trim()
        .parse()
        .context("video duration was invalid")?;

    let mut pixels = Vec::with_capacity(16 * 16 * 4);
    for fraction in [0.1, 0.35, 0.65, 0.9] {
        let timestamp = (duration * fraction).max(0.0).to_string();
        let output = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-ss",
                &timestamp,
                "-i",
                &source.to_string_lossy(),
                "-vf",
                "scale=16:16",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "pipe:1",
            ])
            .output()
            .await
            .context("could not sample video colors")?;
        if !output.status.success() {
            bail!("ffmpeg could not sample video colors");
        }
        pixels.extend(
            output
                .stdout
                .chunks_exact(3)
                .map(|rgb| [rgb[0], rgb[1], rgb[2]]),
        );
    }
    Ok(optimized_accent(&pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optimized_accent_contrasts_with_dark_blue_video() {
        let pixels = vec![[18, 32, 75]; 64];
        assert_eq!(optimized_accent(&pixels), "#FFDD00");
    }

    #[test]
    fn optimized_accent_avoids_matching_a_warm_video() {
        let pixels = vec![[210, 92, 45]; 64];
        assert_eq!(optimized_accent(&pixels), "#4FB5FF");
    }

    #[test]
    fn random_accent_is_always_from_the_readable_palette() {
        for _ in 0..32 {
            assert!(ACCENT_PALETTE.contains(&random_accent()));
        }
    }

    #[test]
    fn accent_modes_are_strict() {
        assert_eq!(AccentMode::parse("random"), Some(AccentMode::Random));
        assert_eq!(AccentMode::parse("optimized"), Some(AccentMode::Optimized));
        assert_eq!(AccentMode::parse("surprise me"), None);
    }
}
