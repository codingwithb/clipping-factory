//! Local word-timestamp transcription via whisper.cpp (PRD §10).
//!
//! We run `whisper-cli` with `--max-len 1 --split-on-word` so every emitted
//! segment is a single word with exact offsets, then rebuild sentence-level
//! segments deterministically (punctuation + pause boundaries). Both raw words
//! and normalized sentences are stored, as the PRD requires.

use crate::config::Config;
use crate::domain::{Sentence, Transcript, Word};
use crate::util::run_streaming;
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub async fn transcribe<F>(
    cfg: &Config,
    wav: &Path,
    cancel: &CancellationToken,
    mut on_progress: F,
) -> Result<Transcript>
where
    F: FnMut(f32),
{
    let bin = cfg
        .whisper_bin
        .as_ref()
        .ok_or_else(|| anyhow!("whisper-cli not found. Install whisper.cpp (macOS: `brew install whisper-cpp`) or set CF_WHISPER_BIN."))?;
    let model = cfg
        .whisper_model
        .as_ref()
        .ok_or_else(|| anyhow!("Transcription model missing. Download ggml-base.en.bin (~148MB) into <data-dir>/models or set CF_WHISPER_MODEL."))?;

    let out_prefix = wav.with_extension("whisper");
    let args: Vec<String> = vec![
        "-m".into(),
        model.to_string_lossy().into_owned(),
        "-f".into(),
        wav.to_string_lossy().into_owned(),
        "-l".into(),
        "en".into(),
        "-t".into(),
        cfg.threads.to_string(),
        "--max-len".into(),
        "1".into(),
        "--split-on-word".into(),
        "--output-json-full".into(),
        "--output-file".into(),
        out_prefix.to_string_lossy().into_owned(),
        "--print-progress".into(),
    ];

    run_streaming(&bin.to_string_lossy(), &args, cancel, |_is_err, line| {
        // whisper.cpp prints `whisper_print_progress_callback: progress = 35%`
        if let Some(idx) = line.find("progress =") {
            let tail = &line[idx + 10..];
            if let Ok(pct) = tail.trim().trim_end_matches('%').parse::<f32>() {
                on_progress((pct / 100.0).clamp(0.0, 1.0));
            }
        }
    })
    .await
    .map_err(|e| {
        if e.to_string().contains("cancelled") {
            e
        } else {
            anyhow!("Transcription failed. {}", e)
        }
    })?;

    let json_path = out_prefix.with_extension("whisper.json");
    // whisper.cpp appends `.json` to the output prefix.
    let json_path = if json_path.is_file() {
        json_path
    } else {
        Path::new(&format!("{}.json", out_prefix.to_string_lossy())).to_path_buf()
    };
    let bytes = tokio::fs::read(&json_path)
        .await
        .with_context(|| format!("whisper output not found at {}", json_path.display()))?;
    let parsed: serde_json::Value = serde_json::from_slice(&bytes)?;
    tokio::fs::remove_file(&json_path).await.ok();

    let words = parse_words(&parsed);
    if words.is_empty() {
        return Err(anyhow!(
            "No speech was detected in this video. Clipping Factory needs clear spoken audio."
        ));
    }
    let sentences = build_sentences(&words);
    let avg_confidence = words.iter().map(|w| w.p).sum::<f32>() / (words.len().max(1) as f32);
    let language = parsed["result"]["language"]
        .as_str()
        .unwrap_or("en")
        .to_string();

    Ok(Transcript {
        language,
        words,
        sentences,
        avg_confidence,
    })
}

fn parse_words(v: &serde_json::Value) -> Vec<Word> {
    let mut words = Vec::new();
    let Some(segments) = v["transcription"].as_array() else {
        return words;
    };
    for seg in segments {
        let text = seg["text"].as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            continue;
        }
        // Skip non-speech annotations like [BLANK_AUDIO], (music), ♪ etc.
        if (text.starts_with('[') && text.ends_with(']'))
            || (text.starts_with('(') && text.ends_with(')'))
            || text.chars().all(|c| !c.is_alphanumeric())
        {
            continue;
        }
        let from = seg["offsets"]["from"].as_u64().unwrap_or(0);
        let to = seg["offsets"]["to"].as_u64().unwrap_or(from);
        // Mean probability over real tokens (skip specials like [_BEG_]).
        let mut p_sum = 0.0f64;
        let mut p_n = 0usize;
        if let Some(tokens) = seg["tokens"].as_array() {
            for tok in tokens {
                let tt = tok["text"].as_str().unwrap_or("");
                if tt.starts_with("[_") {
                    continue;
                }
                if let Some(p) = tok["p"].as_f64() {
                    p_sum += p;
                    p_n += 1;
                }
            }
        }
        let p = if p_n > 0 {
            (p_sum / p_n as f64) as f32
        } else {
            0.5
        };
        words.push(Word {
            text,
            start_ms: from,
            end_ms: to.max(from),
            p,
        });
    }
    words
}

/// Group words into sentence-like segments: break after terminal punctuation,
/// on long pauses, or when a segment grows unreasonably large.
pub fn build_sentences(words: &[Word]) -> Vec<Sentence> {
    let mut sentences = Vec::new();
    let mut start_idx = 0usize;
    let mut char_len = 0usize;

    for i in 0..words.len() {
        char_len += words[i].text.len() + 1;
        let terminal = words[i]
            .text
            .trim_end_matches(['"', '\'', ')', ']'])
            .ends_with(['.', '?', '!', '…']);
        let long_pause = words
            .get(i + 1)
            .map(|next| next.start_ms.saturating_sub(words[i].end_ms) >= 1000)
            .unwrap_or(false);
        let too_long = char_len >= 260;
        let last = i + 1 == words.len();

        if terminal || long_pause || too_long || last {
            let slice = &words[start_idx..=i];
            let text = slice
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            sentences.push(Sentence {
                text,
                start_ms: slice[0].start_ms,
                end_ms: slice[slice.len() - 1].end_ms,
                word_start: start_idx,
                word_end: i + 1,
            });
            start_idx = i + 1;
            char_len = 0;
        }
    }
    sentences
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, start: u64, end: u64) -> Word {
        Word {
            text: text.into(),
            start_ms: start,
            end_ms: end,
            p: 0.9,
        }
    }

    #[test]
    fn sentences_break_on_punctuation_and_pauses() {
        let words = vec![
            w("Hello", 0, 300),
            w("there.", 350, 700),
            w("Second", 900, 1200),
            w("idea", 1250, 1500),
            // 1.5s pause here
            w("after", 3000, 3300),
            w("pause", 3350, 3700),
        ];
        let s = build_sentences(&words);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].text, "Hello there.");
        assert_eq!(s[1].word_start, 2);
        assert_eq!(s[2].start_ms, 3000);
    }
}
