//! Core domain types shared across the pipeline, storage, and API layers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Ordered pipeline stages, matching PRD §14.2.
pub const STAGES: &[&str] = &[
    "inspecting",
    "extracting_audio",
    "transcribing",
    "selecting_candidates",
    "validating_candidates",
    "analyzing_layout",
    "rendering",
];

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Created,
    Inspecting,
    ExtractingAudio,
    Transcribing,
    SelectingCandidates,
    ValidatingCandidates,
    AnalyzingLayout,
    Rendering,
    Complete,
    Cancelled,
    Failed,
}

impl JobState {
    pub fn from_stage(stage: &str) -> JobState {
        match stage {
            "inspecting" => JobState::Inspecting,
            "extracting_audio" => JobState::ExtractingAudio,
            "transcribing" => JobState::Transcribing,
            "selecting_candidates" => JobState::SelectingCandidates,
            "validating_candidates" => JobState::ValidatingCandidates,
            "analyzing_layout" => JobState::AnalyzingLayout,
            "rendering" => JobState::Rendering,
            _ => JobState::Created,
        }
    }

    /// True while a pipeline run is (or should be) actively working.
    pub fn is_active(&self) -> bool {
        !matches!(
            self,
            JobState::Created | JobState::Complete | JobState::Cancelled | JobState::Failed
        )
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StageRecord {
    pub name: String,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// 0.0 – 1.0 when a meaningful percentage exists.
    pub progress: Option<f32>,
    /// Human-readable description of the current operation.
    pub detail: Option<String>,
    pub error: Option<String>,
}

impl StageRecord {
    pub fn new(name: &str) -> Self {
        StageRecord {
            name: name.to_string(),
            started_at: None,
            completed_at: None,
            progress: None,
            detail: None,
            error: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SourceInfo {
    pub filename: String,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub video_codec: String,
    pub audio_codec: String,
    pub size_bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Project {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub status: JobState,
    pub source: Option<SourceInfo>,
    pub source_path: PathBuf,
    pub stages: Vec<StageRecord>,
    /// Top-level error message when status == Failed.
    pub error: Option<String>,
    /// Which selector produced candidates: "openai" | "anthropic" | "offline heuristic".
    pub selector: Option<String>,
    /// Non-fatal warning surfaced in the UI (e.g. low transcription confidence).
    pub warning: Option<String>,
    /// Caption style for this project: "impact" (default) or "clean".
    #[serde(default)]
    pub caption_style: Option<String>,
    /// Accent color for the active caption word, as #RRGGBB.
    #[serde(default)]
    pub accent_color: Option<String>,
    /// Output composition selected before upload.
    #[serde(default)]
    pub framing_mode: FramingMode,
}

impl Project {
    pub fn new(id: String, source_path: PathBuf) -> Self {
        Project {
            id,
            created_at: Utc::now(),
            status: JobState::Created,
            source: None,
            source_path,
            stages: STAGES.iter().map(|s| StageRecord::new(s)).collect(),
            error: None,
            selector: None,
            warning: None,
            caption_style: None,
            accent_color: None,
            framing_mode: FramingMode::default(),
        }
    }

    pub fn stage_mut(&mut self, name: &str) -> &mut StageRecord {
        let idx = self
            .stages
            .iter()
            .position(|s| s.name == name)
            .expect("unknown stage name");
        &mut self.stages[idx]
    }
}

// ---------------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Word {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    /// Mean token probability (0–1) for this word.
    pub p: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Sentence {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    /// Inclusive start / exclusive end indexes into `Transcript::words`.
    pub word_start: usize,
    pub word_end: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Transcript {
    pub language: String,
    pub words: Vec<Word>,
    pub sentences: Vec<Sentence>,
    pub avg_confidence: f32,
}

// ---------------------------------------------------------------------------
// Candidates (editorial selection)
// ---------------------------------------------------------------------------

/// 1–5 rubric scores per PRD §9.2. For `context_dependency` and `slop_risk`,
/// 1 is safest and 5 is worst.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct Scores {
    pub self_contained: u8,
    pub opening_strength: u8,
    pub specificity: u8,
    pub tension_or_novelty: u8,
    pub payoff: u8,
    pub clarity: u8,
    pub context_dependency: u8,
    pub slop_risk: u8,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Candidate {
    pub start_ms: u64,
    pub end_ms: u64,
    pub headline: String,
    pub opening_quote: String,
    pub closing_quote: String,
    pub selection_reason: String,
    pub scores: Scores,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ValidatedCandidate {
    pub candidate: Candidate,
    pub rank: usize,
    pub composite: f32,
    pub duration_exception: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RejectedCandidate {
    pub candidate: Candidate,
    pub reasons: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SelectionReport {
    pub selector: String,
    pub accepted: Vec<ValidatedCandidate>,
    pub rejected: Vec<RejectedCandidate>,
}

// ---------------------------------------------------------------------------
// Layout & rendering
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FramingMode {
    /// Fill the 9:16 canvas, following a face when one can be tracked.
    #[default]
    Fill,
    /// Preserve the full source over a blurred background.
    Background,
}

impl FramingMode {
    pub fn apply(self, analyzed: LayoutPlan) -> LayoutPlan {
        match (self, analyzed) {
            (FramingMode::Fill, tracked @ LayoutPlan::FaceCrop { .. }) => tracked,
            (FramingMode::Fill, LayoutPlan::BlurPad) => LayoutPlan::FaceCrop {
                keyframes: vec![CropKey { t_ms: 0, cx: 0.5 }],
            },
            (FramingMode::Background, _) => LayoutPlan::BlurPad,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LayoutPlan {
    /// Smoothed vertical crop that follows one persistent face.
    FaceCrop { keyframes: Vec<CropKey> },
    /// Uncropped source centered over a blurred, darkened background.
    BlurPad,
}

impl LayoutPlan {
    pub fn label(&self) -> &'static str {
        match self {
            LayoutPlan::FaceCrop { .. } => "face_crop",
            LayoutPlan::BlurPad => "blur_pad",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CropKey {
    /// Milliseconds relative to clip start.
    pub t_ms: u64,
    /// Normalized horizontal face center in the source frame (0–1).
    pub cx: f32,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ClipStatus {
    Pending,
    Rendering,
    Ready,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClipRecord {
    pub id: String,
    pub rank: usize,
    pub headline: String,
    pub filename: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub duration_ms: u64,
    pub selection_reason: String,
    pub scores: Scores,
    pub layout: LayoutPlan,
    pub status: ClipStatus,
    pub error: Option<String>,
    /// True when transcription confidence inside this interval was low (PRD §10).
    pub low_confidence: bool,
    /// Caption style burned into the current render: "impact" or "clean".
    /// `None` on manifests written before post-render restyling existed.
    #[serde(default)]
    pub caption_style: Option<String>,
    /// Accent color burned into the current render, as `#RRGGBB`.
    #[serde(default)]
    pub accent_color: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RenderManifest {
    pub clips: Vec<ClipRecord>,
    /// Final user-facing output directory once at least one clip copied there.
    pub output_dir: Option<String>,
}

/// Format a millisecond offset as `MM:SS` (or `H:MM:SS` above one hour).
pub fn fmt_ms(ms: u64) -> String {
    let total_s = ms / 1000;
    let (h, m, s) = (total_s / 3600, (total_s % 3600) / 60, total_s % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_framing_keeps_face_tracking_when_available() {
        let tracked = LayoutPlan::FaceCrop {
            keyframes: vec![CropKey { t_ms: 0, cx: 0.42 }],
        };
        assert_eq!(FramingMode::Fill.apply(tracked.clone()), tracked);
    }

    #[test]
    fn fill_framing_uses_center_crop_when_tracking_is_unavailable() {
        assert_eq!(
            FramingMode::Fill.apply(LayoutPlan::BlurPad),
            LayoutPlan::FaceCrop {
                keyframes: vec![CropKey { t_ms: 0, cx: 0.5 }],
            }
        );
    }

    #[test]
    fn background_framing_always_preserves_the_full_source() {
        let tracked = LayoutPlan::FaceCrop {
            keyframes: vec![CropKey { t_ms: 0, cx: 0.42 }],
        };
        assert_eq!(FramingMode::Background.apply(tracked), LayoutPlan::BlurPad);
    }

    /// Manifests written before per-clip caption styling must still load.
    #[test]
    fn old_manifest_without_caption_fields_deserializes() {
        let old = r#"{
            "clips": [{
                "id": "c1", "rank": 1, "headline": "A test",
                "filename": "01-a-test.mp4",
                "start_ms": 1000, "end_ms": 31000, "duration_ms": 30000,
                "selection_reason": "why",
                "scores": {"self_contained":5,"opening_strength":4,"specificity":4,
                            "tension_or_novelty":4,"payoff":4,"clarity":5,
                            "context_dependency":1,"slop_risk":1},
                "layout": {"mode": "blur_pad"},
                "status": "ready", "error": null, "low_confidence": false
            }],
            "output_dir": null
        }"#;
        let m: RenderManifest = serde_json::from_str(old).expect("old manifest must load");
        assert_eq!(m.clips.len(), 1);
        assert_eq!(m.clips[0].caption_style, None);
        assert_eq!(m.clips[0].accent_color, None);
    }
}
