//! Filesystem JSON project store — no database, per PRD §13/§14.
//!
//! Layout:
//! ```text
//! <data_dir>/projects/<project-id>/
//!   project.json
//!   transcript.json
//!   candidates-raw.json      (selector proposals, pre-validation)
//!   candidates.json          (validated SelectionReport)
//!   render-manifest.json
//!   source.mp4
//!   audio.wav                (temporary; deleted after transcription)
//!   clips/
//! ```

use crate::domain::*;
use crate::util::atomic_write_json;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(data_dir: &Path) -> Store {
        Store {
            root: data_dir.join("projects"),
        }
    }

    pub fn project_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }
    pub fn project_json(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("project.json")
    }
    pub fn source_path(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("source.mp4")
    }
    pub fn audio_path(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("audio.wav")
    }
    pub fn transcript_path(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("transcript.json")
    }
    pub fn raw_candidates_path(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("candidates-raw.json")
    }
    pub fn candidates_path(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("candidates.json")
    }
    pub fn manifest_path(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("render-manifest.json")
    }
    pub fn clips_dir(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("clips")
    }
    /// Uncaptioned framed intermediates, kept so captions can be restyled
    /// without re-doing the expensive framing render.
    pub fn base_dir(&self, id: &str) -> PathBuf {
        self.clips_dir(id).join("base")
    }
    pub fn base_clip_path(&self, id: &str, clip_id: &str) -> PathBuf {
        self.base_dir(id).join(format!("{clip_id}.mp4"))
    }
    pub fn frames_dir(&self, id: &str) -> PathBuf {
        self.project_dir(id).join("frames")
    }

    pub fn exists(&self, id: &str) -> bool {
        self.project_json(id).is_file()
    }

    pub async fn create_dirs(&self, id: &str) -> Result<()> {
        tokio::fs::create_dir_all(self.base_dir(id)).await?;
        Ok(())
    }

    pub async fn save_project(&self, p: &Project) -> Result<()> {
        atomic_write_json(&self.project_json(&p.id), p).await
    }

    pub async fn load_project(&self, id: &str) -> Result<Project> {
        let bytes = tokio::fs::read(self.project_json(id))
            .await
            .with_context(|| format!("project {} not found", id))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn save_transcript(&self, id: &str, t: &Transcript) -> Result<()> {
        atomic_write_json(&self.transcript_path(id), t).await
    }
    pub async fn load_transcript(&self, id: &str) -> Result<Transcript> {
        let bytes = tokio::fs::read(self.transcript_path(id)).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn save_raw_candidates(&self, id: &str, c: &Vec<Candidate>) -> Result<()> {
        atomic_write_json(&self.raw_candidates_path(id), c).await
    }
    pub async fn load_raw_candidates(&self, id: &str) -> Result<Vec<Candidate>> {
        let bytes = tokio::fs::read(self.raw_candidates_path(id)).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn save_selection(&self, id: &str, r: &SelectionReport) -> Result<()> {
        atomic_write_json(&self.candidates_path(id), r).await
    }
    pub async fn load_selection(&self, id: &str) -> Result<SelectionReport> {
        let bytes = tokio::fs::read(self.candidates_path(id)).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn save_manifest(&self, id: &str, m: &RenderManifest) -> Result<()> {
        atomic_write_json(&self.manifest_path(id), m).await
    }
    pub async fn load_manifest(&self, id: &str) -> Result<RenderManifest> {
        let bytes = tokio::fs::read(self.manifest_path(id)).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRD §16.3: project-state recovery must be covered by tests.
    #[tokio::test]
    async fn project_roundtrip_survives_reload() {
        let tmp = std::env::temp_dir().join(format!("cf-test-{}", crate::util::short_id()));
        let store = Store::new(&tmp);
        let id = "testproj01".to_string();
        store.create_dirs(&id).await.unwrap();

        let mut p = Project::new(id.clone(), store.source_path(&id));
        p.status = JobState::Transcribing;
        p.stage_mut("transcribing").progress = Some(0.42);
        p.stage_mut("transcribing").detail = Some("12:00 of 28:00".into());
        store.save_project(&p).await.unwrap();

        let mut loaded = store.load_project(&id).await.unwrap();
        assert_eq!(loaded.status, JobState::Transcribing);
        assert_eq!(loaded.stages.len(), STAGES.len());
        assert_eq!(loaded.stage_mut("transcribing").progress, Some(0.42));

        tokio::fs::remove_dir_all(&tmp).await.ok();
    }

    #[tokio::test]
    async fn manifest_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("cf-test-{}", crate::util::short_id()));
        let store = Store::new(&tmp);
        let id = "testproj02".to_string();
        store.create_dirs(&id).await.unwrap();

        let m = RenderManifest {
            clips: vec![ClipRecord {
                id: "c1".into(),
                rank: 1,
                headline: "A test".into(),
                filename: "01-a-test.mp4".into(),
                start_ms: 1000,
                end_ms: 31000,
                duration_ms: 30000,
                selection_reason: "why".into(),
                scores: Scores::default(),
                layout: LayoutPlan::BlurPad,
                status: ClipStatus::Ready,
                error: None,
                low_confidence: false,
                caption_style: Some("impact".into()),
                accent_color: Some("#FFDD00".into()),
            }],
            output_dir: Some("/tmp/out".into()),
        };
        store.save_manifest(&id, &m).await.unwrap();
        let loaded = store.load_manifest(&id).await.unwrap();
        assert_eq!(loaded.clips.len(), 1);
        assert_eq!(loaded.clips[0].layout, LayoutPlan::BlurPad);
        assert_eq!(loaded.clips[0].status, ClipStatus::Ready);

        tokio::fs::remove_dir_all(&tmp).await.ok();
    }
}
