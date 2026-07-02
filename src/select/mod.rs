//! Editorial selection engine (PRD §9): builds transcript windows, asks the
//! configured provider for candidate intervals, and merges the results.
//!
//! Providers:
//! - `openai`    — the PRD's primary provider (user's key)
//! - `anthropic` — optional alternative provider
//! - `offline`   — deterministic heuristic; also the automatic fallback when
//!                 no key is configured, clearly labeled in the UI.

pub mod anthropic;
pub mod heuristic;
pub mod openai;

use crate::domain::{fmt_ms, Candidate, Scores, SourceInfo, Transcript};
use crate::settings::{AiSettings, PROVIDER_ANTHROPIC, PROVIDER_OFFLINE, PROVIDER_OPENAI};
use anyhow::{anyhow, Result};

/// Planning targets from PRD §9.1.
pub fn plan_counts(source_duration_ms: u64) -> (usize, usize) {
    let minutes = source_duration_ms as f64 / 60_000.0;
    let target = ((minutes / 10.0).round() as usize).max(1);
    let proposals = ((target as f64 * 1.5).ceil() as usize).max(3);
    (target, proposals)
}

/// Local ranking is cheap, so keep every strong, distinct moment the
/// validator can reasonably use instead of applying the AI proposal quota.
pub fn local_proposal_limit(source_duration_ms: u64) -> usize {
    (source_duration_ms.div_ceil(30_000) as usize).clamp(6, 30)
}

pub struct SelectionOutcome {
    pub candidates: Vec<Candidate>,
    pub selector: String,
}

pub async fn propose(
    settings: &AiSettings,
    transcript: &Transcript,
    source: &SourceInfo,
) -> Result<SelectionOutcome> {
    let (target, proposals) = plan_counts(source.duration_ms);

    let provider = if settings.connected() { settings.provider.as_str() } else { PROVIDER_OFFLINE };

    match provider {
        PROVIDER_OFFLINE => Ok(SelectionOutcome {
            candidates: heuristic::propose(
                transcript,
                source.duration_ms,
                local_proposal_limit(source.duration_ms),
            ),
            selector: "local ranking".into(),
        }),
        PROVIDER_OPENAI | PROVIDER_ANTHROPIC => {
            let key = settings
                .api_key
                .clone()
                .ok_or_else(|| anyhow!("AI key missing. Open AI connection and add your key."))?;
            let model = settings.effective_model();
            let windows = build_windows(transcript, source.duration_ms);
            let mut all: Vec<Candidate> = Vec::new();
            let per_window = ((proposals as f64) / (windows.len() as f64)).ceil() as usize;

            for win in &windows {
                let user_prompt = window_prompt(win, source, target, per_window.max(2));
                let raw = match provider {
                    PROVIDER_ANTHROPIC => {
                        anthropic::complete(&key, &model, SYSTEM_PROMPT, &user_prompt).await?
                    }
                    _ => openai::complete(&key, &model, SYSTEM_PROMPT, &user_prompt).await?,
                };
                let mut cands = parse_candidates(&raw)?;
                all.append(&mut cands);
            }
            if windows.len() > 1 {
                all = dedupe_similar(all);
            }
            Ok(SelectionOutcome {
                candidates: all,
                selector: format!("{} · {}", provider, model),
            })
        }
        other => Err(anyhow!("Unknown AI provider `{}`.", other)),
    }
}

/// Test connectivity for the configured provider (PRD §14.1 `/api/settings/ai/test`).
pub async fn test_connection(settings: &AiSettings) -> Result<String> {
    match settings.provider.as_str() {
        PROVIDER_OFFLINE => Ok("Local ranking is ready — no API key needed.".into()),
        PROVIDER_OPENAI => {
            let key = settings.api_key.as_deref().filter(|k| !k.trim().is_empty())
                .ok_or_else(|| anyhow!("Enter an OpenAI API key first."))?;
            openai::test(key).await?;
            Ok(format!("OpenAI connection verified. Using model {}.", settings.effective_model()))
        }
        PROVIDER_ANTHROPIC => {
            let key = settings.api_key.as_deref().filter(|k| !k.trim().is_empty())
                .ok_or_else(|| anyhow!("Enter an Anthropic API key first."))?;
            anthropic::test(key).await?;
            Ok(format!("Anthropic connection verified. Using model {}.", settings.effective_model()))
        }
        other => Err(anyhow!("Unknown provider `{}`.", other)),
    }
}

// ---------------------------------------------------------------------------
// Windowing (PRD §9.1: overlapping windows for long transcripts)
// ---------------------------------------------------------------------------

pub struct Window {
    pub start_ms: u64,
    pub end_ms: u64,
    pub lines: String,
}

const WINDOW_MS: u64 = 12 * 60_000;
const OVERLAP_MS: u64 = 2 * 60_000;

pub fn build_windows(t: &Transcript, source_duration_ms: u64) -> Vec<Window> {
    let mut windows = Vec::new();
    let mut win_start: u64 = 0;
    loop {
        let win_end = (win_start + WINDOW_MS).min(source_duration_ms);
        let mut lines = String::new();
        for s in &t.sentences {
            if s.end_ms < win_start || s.start_ms > win_end {
                continue;
            }
            lines.push_str(&format!(
                "[{} --> {}] {}\n",
                ts_precise(s.start_ms),
                ts_precise(s.end_ms),
                s.text
            ));
        }
        if !lines.is_empty() {
            windows.push(Window { start_ms: win_start, end_ms: win_end, lines });
        }
        if win_end >= source_duration_ms {
            break;
        }
        win_start = win_end - OVERLAP_MS;
    }
    if windows.is_empty() {
        windows.push(Window { start_ms: 0, end_ms: source_duration_ms, lines: String::new() });
    }
    windows
}

fn ts_precise(ms: u64) -> String {
    format!("{}.{:03}", fmt_ms(ms), ms % 1000)
}

// ---------------------------------------------------------------------------
// Prompting & parsing
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT: &str = r#"You are the editorial selector inside Clipping Factory, a tool that turns one long podcast into a few faithful vertical clips. You choose which continuous moments of the source recording deserve to stand alone. You are a demanding editor: quality over quota.

HARD RULES
- Each candidate is ONE continuous interval of the source. You choose only start_ms and end_ms.
- Never rewrite, reorder, splice, or invent speech.
- opening_quote and closing_quote must be VERBATIM text from the transcript near the start and end of your interval.
- Clips normally run 20–90 seconds. Start on a natural sentence boundary; end after the idea resolves.
- The headline summarizes the excerpt in sentence case, under 90 characters, supported directly by what the speaker says. Never invent numbers, certainty, or conflict.

A PASSING CLIP MUST
- Make sense without the preceding conversation.
- Establish its subject within the first sentence or few seconds.
- Contain a specific insight, story, disagreement, reveal, joke, or useful explanation.
- Build toward a payoff or clear conclusion, and end cleanly.
- Preserve the speaker's actual meaning.
- Avoid unresolved references like "like I said earlier".
- Avoid sponsor reads, housekeeping, introductions, and generic agreement.

SCORING (1–5 integers)
self_contained, opening_strength, specificity, tension_or_novelty, payoff, clarity: 5 is best.
context_dependency, slop_risk: these are penalties — 1 is safest, 5 is worst.
Score honestly; weak moments should score low so the validator can reject them.

OUTPUT
Return ONLY a JSON object, no markdown fences, shaped exactly like:
{"candidates":[{"start_ms":1122000,"end_ms":1188000,"headline":"...","opening_quote":"...","closing_quote":"...","selection_reason":"...","scores":{"self_contained":5,"opening_strength":4,"specificity":4,"tension_or_novelty":4,"payoff":5,"clarity":5,"context_dependency":1,"slop_risk":1}}]}
Propose fewer candidates than asked rather than padding with weak ones. If nothing qualifies, return {"candidates":[]}."#;

fn window_prompt(win: &Window, source: &SourceInfo, target: usize, per_window: usize) -> String {
    format!(
        "Source: \"{}\" — total duration {} ({} ms). Planning target for the whole source: about {} clip(s); this is guidance, not a quota.\n\nTranscript window ({} → {}), one sentence per line as [start --> end] text:\n\n{}\n\nPropose up to {} strong candidates from THIS window only. Timestamps are absolute source milliseconds. Remember: return only the JSON object.",
        source.filename,
        fmt_ms(source.duration_ms),
        source.duration_ms,
        target,
        fmt_ms(win.start_ms),
        fmt_ms(win.end_ms),
        win.lines,
        per_window
    )
}

#[derive(serde::Deserialize)]
struct CandidatesWrapper {
    candidates: Vec<CandidateIn>,
}

#[derive(serde::Deserialize)]
struct CandidateIn {
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    #[serde(default)]
    headline: String,
    #[serde(default)]
    opening_quote: String,
    #[serde(default)]
    closing_quote: String,
    #[serde(default)]
    selection_reason: String,
    scores: Option<ScoresIn>,
}

#[derive(serde::Deserialize, Default)]
struct ScoresIn {
    #[serde(default)]
    self_contained: f64,
    #[serde(default)]
    opening_strength: f64,
    #[serde(default)]
    specificity: f64,
    #[serde(default)]
    tension_or_novelty: f64,
    #[serde(default)]
    payoff: f64,
    #[serde(default)]
    clarity: f64,
    #[serde(default)]
    context_dependency: f64,
    #[serde(default)]
    slop_risk: f64,
}

fn clamp_score(v: f64) -> u8 {
    (v.round() as i64).clamp(1, 5) as u8
}

/// Parse a provider response into candidates. Malformed JSON is a named,
/// retryable error (PRD §15).
pub fn parse_candidates(raw: &str) -> Result<Vec<Candidate>> {
    let cleaned = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let start = cleaned.find('{');
    let end = cleaned.rfind('}');
    let json = match (start, end) {
        (Some(s), Some(e)) if e > s => &cleaned[s..=e],
        _ => return Err(anyhow!("The AI returned malformed JSON. Retry the stage.")),
    };
    let wrapper: CandidatesWrapper = serde_json::from_str(json)
        .map_err(|e| anyhow!("The AI returned malformed JSON ({}). Retry the stage.", e))?;

    let mut out = Vec::new();
    for c in wrapper.candidates {
        let (Some(start_ms), Some(end_ms)) = (c.start_ms, c.end_ms) else { continue };
        if start_ms < 0 || end_ms <= start_ms {
            continue;
        }
        let s = c.scores.unwrap_or_default();
        out.push(Candidate {
            start_ms: start_ms as u64,
            end_ms: end_ms as u64,
            headline: c.headline.trim().to_string(),
            opening_quote: c.opening_quote.trim().to_string(),
            closing_quote: c.closing_quote.trim().to_string(),
            selection_reason: c.selection_reason.trim().to_string(),
            scores: Scores {
                self_contained: clamp_score(s.self_contained),
                opening_strength: clamp_score(s.opening_strength),
                specificity: clamp_score(s.specificity),
                tension_or_novelty: clamp_score(s.tension_or_novelty),
                payoff: clamp_score(s.payoff),
                clarity: clamp_score(s.clarity),
                context_dependency: clamp_score(s.context_dependency),
                slop_risk: clamp_score(s.slop_risk),
            },
        });
    }
    Ok(out)
}

/// Merge near-duplicate candidates from overlapping windows: keep the higher
/// scoring of any pair whose intervals overlap more than 55%.
fn dedupe_similar(mut cands: Vec<Candidate>) -> Vec<Candidate> {
    let quality = |c: &Candidate| -> i32 {
        c.scores.self_contained as i32
            + c.scores.payoff as i32
            + c.scores.opening_strength as i32
            + c.scores.clarity as i32
            - c.scores.context_dependency as i32
            - c.scores.slop_risk as i32
    };
    cands.sort_by_key(|c| -quality(c));
    let mut kept: Vec<Candidate> = Vec::new();
    for c in cands {
        let dup = kept.iter().any(|k| {
            let inter = overlap_ms(k.start_ms, k.end_ms, c.start_ms, c.end_ms) as f64;
            let dur = (c.end_ms - c.start_ms).max(1) as f64;
            inter / dur > 0.55
        });
        if !dup {
            kept.push(c);
        }
    }
    kept
}

pub fn overlap_ms(a0: u64, a1: u64, b0: u64, b1: u64) -> u64 {
    let lo = a0.max(b0);
    let hi = a1.min(b1);
    hi.saturating_sub(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_counts_match_prd() {
        // 60-minute source → target 6, proposals 9.
        assert_eq!(plan_counts(60 * 60_000), (6, 9));
        // 3-minute source → target 1, proposals 3 (floor).
        assert_eq!(plan_counts(3 * 60_000), (1, 3));
        // 25 minutes → round(2.5) = 3 (round-half-up), proposals 5.
        let (t, p) = plan_counts(25 * 60_000);
        assert_eq!(t, 3);
        assert_eq!(p, 5);
    }

    #[test]
    fn local_ranking_keeps_many_more_candidates() {
        assert_eq!(local_proposal_limit(3 * 60_000), 6);
        assert_eq!(local_proposal_limit(366_805), 13);
        assert_eq!(local_proposal_limit(60 * 60_000), 30);
    }

    #[test]
    fn parses_fenced_json() {
        let raw = "```json\n{\"candidates\":[{\"start_ms\":1000,\"end_ms\":31000,\"headline\":\"H\",\"opening_quote\":\"a\",\"closing_quote\":\"b\",\"selection_reason\":\"r\",\"scores\":{\"self_contained\":5,\"opening_strength\":4,\"specificity\":4,\"tension_or_novelty\":4,\"payoff\":5,\"clarity\":5,\"context_dependency\":1,\"slop_risk\":1}}]}\n```";
        let c = parse_candidates(raw).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].scores.self_contained, 5);
    }

    #[test]
    fn malformed_json_is_error() {
        assert!(parse_candidates("no json here").is_err());
    }
}
