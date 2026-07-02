//! Deterministic candidate validator (PRD §9.3).
//!
//! Every rule here is pure and unit-tested: score thresholds, duration bounds
//! (with the explicit exception path), timestamp bounds, word-boundary
//! snapping, verbatim quote matching, and >30% overlap suppression against
//! higher-ranked candidates. The validator is the final authority — the LLM
//! only proposes.

use crate::domain::*;

const MIN_MS: u64 = 20_000;
const MAX_MS: u64 = 90_000;
/// Exception envelope (PRD §9.3 "without an explicit validator exception"):
/// slightly-out-of-range candidates pass only when unusually strong.
const EXC_MIN_MS: u64 = 15_000;
const EXC_MAX_MS: u64 = 110_000;
const MAX_OVERLAP: f64 = 0.30;

pub fn validate(
    candidates: Vec<Candidate>,
    transcript: &Transcript,
    source_duration_ms: u64,
    selector: String,
) -> SelectionReport {
    let mut evaluated: Vec<Result<(Candidate, bool, f32), RejectedCandidate>> = Vec::new();

    for mut cand in candidates {
        let mut reasons: Vec<String> = Vec::new();

        // --- Timestamp bounds (PRD §15) ---------------------------------
        if cand.start_ms >= cand.end_ms {
            reasons.push("start time is not before end time".into());
        }
        if cand.end_ms > source_duration_ms + 500 {
            reasons.push(format!(
                "end timestamp {} is outside the source duration {}",
                fmt_ms(cand.end_ms),
                fmt_ms(source_duration_ms)
            ));
        }
        if !reasons.is_empty() {
            evaluated.push(Err(RejectedCandidate { candidate: cand, reasons }));
            continue;
        }

        // --- Snap to real word timestamps (PRD §9.1) ---------------------
        if let Some((s, e)) = snap_to_words(transcript, cand.start_ms, cand.end_ms) {
            cand.start_ms = s;
            cand.end_ms = e;
        } else {
            evaluated.push(Err(RejectedCandidate {
                candidate: cand,
                reasons: vec!["interval contains no transcribed words".into()],
            }));
            continue;
        }

        // --- Score thresholds (PRD §9.3) ---------------------------------
        let s = cand.scores;
        if s.self_contained < 4 {
            reasons.push(format!("self_contained {} is below 4", s.self_contained));
        }
        if s.payoff < 3 {
            reasons.push(format!("payoff {} is below 3", s.payoff));
        }
        if s.clarity < 4 {
            reasons.push(format!("clarity {} is below 4", s.clarity));
        }
        if s.context_dependency > 2 {
            reasons.push(format!("context_dependency {} is above 2", s.context_dependency));
        }
        if s.slop_risk > 2 {
            reasons.push(format!("slop_risk {} is above 2", s.slop_risk));
        }

        // --- Duration with explicit exception path ------------------------
        let dur = cand.end_ms - cand.start_ms;
        let mut duration_exception = false;
        if dur < MIN_MS || dur > MAX_MS {
            let exceptional = s.payoff >= 4 && s.self_contained >= 5 && s.clarity >= 4;
            if (EXC_MIN_MS..=EXC_MAX_MS).contains(&dur) && exceptional {
                duration_exception = true;
            } else {
                reasons.push(format!(
                    "duration {}s falls outside 20–90 seconds",
                    dur / 1000
                ));
            }
        }

        // --- Verbatim quote matching (PRD §9.3) ---------------------------
        let excerpt = excerpt_text(transcript, cand.start_ms, cand.end_ms);
        let excerpt_norm = normalize(&excerpt);
        for (label, quote, near_start) in [
            ("opening", &cand.opening_quote, true),
            ("closing", &cand.closing_quote, false),
        ] {
            let qn = normalize(quote);
            if qn.is_empty() {
                reasons.push(format!("{} quote is empty", label));
                continue;
            }
            match excerpt_norm.find(&qn) {
                None => reasons.push(format!(
                    "{} quote cannot be found in the transcribed excerpt",
                    label
                )),
                Some(pos) => {
                    let len = excerpt_norm.len().max(1);
                    let zone = (len as f64 * 0.5) as usize;
                    let ok = if near_start {
                        pos <= zone.max(160)
                    } else {
                        pos + qn.len() >= len.saturating_sub(zone.max(160))
                    };
                    if !ok {
                        reasons.push(format!(
                            "{} quote is not near the {} of the excerpt",
                            label,
                            if near_start { "start" } else { "end" }
                        ));
                    }
                }
            }
        }

        if reasons.is_empty() {
            let composite = composite_score(&s);
            evaluated.push(Ok((cand, duration_exception, composite)));
        } else {
            evaluated.push(Err(RejectedCandidate { candidate: cand, reasons }));
        }
    }

    // --- Rank survivors, then suppress >30% overlaps ----------------------
    let mut rejected: Vec<RejectedCandidate> = Vec::new();
    let mut passing: Vec<(Candidate, bool, f32)> = Vec::new();
    for item in evaluated {
        match item {
            Ok(v) => passing.push(v),
            Err(r) => rejected.push(r),
        }
    }
    passing.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut accepted: Vec<ValidatedCandidate> = Vec::new();
    for (cand, duration_exception, composite) in passing {
        let dur = (cand.end_ms - cand.start_ms).max(1) as f64;
        let too_much_overlap = accepted.iter().any(|a| {
            let inter = crate::select::overlap_ms(
                a.candidate.start_ms,
                a.candidate.end_ms,
                cand.start_ms,
                cand.end_ms,
            ) as f64;
            inter / dur > MAX_OVERLAP
        });
        if too_much_overlap {
            rejected.push(RejectedCandidate {
                candidate: cand,
                reasons: vec!["overlaps more than 30% with a higher-ranked clip".into()],
            });
            continue;
        }
        accepted.push(ValidatedCandidate {
            rank: accepted.len() + 1,
            candidate: cand,
            composite,
            duration_exception,
        });
    }

    SelectionReport { selector, accepted, rejected }
}

pub fn composite_score(s: &Scores) -> f32 {
    s.self_contained as f32 * 2.0
        + s.payoff as f32 * 1.6
        + s.opening_strength as f32 * 1.4
        + s.clarity as f32 * 1.2
        + s.tension_or_novelty as f32 * 1.0
        + s.specificity as f32 * 0.8
        - s.context_dependency as f32 * 1.5
        - s.slop_risk as f32 * 2.0
}

/// Snap a proposed interval to real word timestamps: the start of the word
/// containing (or nearest after) `start`, and the end of the word containing
/// (or nearest before) `end`.
pub fn snap_to_words(t: &Transcript, start: u64, end: u64) -> Option<(u64, u64)> {
    let words = &t.words;
    if words.is_empty() {
        return None;
    }
    let first = words
        .iter()
        .find(|w| w.end_ms > start)
        .map(|w| w.start_ms)?;
    let last = words
        .iter()
        .rev()
        .find(|w| w.start_ms < end)
        .map(|w| w.end_ms)?;
    if first >= last {
        return None;
    }
    Some((first, last))
}

pub fn excerpt_text(t: &Transcript, start: u64, end: u64) -> String {
    t.words
        .iter()
        .filter(|w| w.start_ms >= start && w.end_ms <= end)
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Lowercase, alphanumeric + single spaces — tolerant matching for
/// punctuation/casing differences while preserving wording.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Average word confidence inside an interval — used to surface the PRD §10
/// low-confidence warning on affected clips.
pub fn interval_confidence(t: &Transcript, start: u64, end: u64) -> f32 {
    let mut sum = 0.0f32;
    let mut n = 0usize;
    for w in &t.words {
        if w.start_ms >= start && w.end_ms <= end {
            sum += w.p;
            n += 1;
        }
    }
    if n == 0 {
        1.0
    } else {
        sum / n as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(n_words: usize, word_ms: u64) -> Transcript {
        let mut words = Vec::new();
        for i in 0..n_words {
            let t0 = i as u64 * word_ms;
            words.push(Word {
                text: format!("word{}", i),
                start_ms: t0,
                end_ms: t0 + word_ms - 50,
                p: 0.9,
            });
        }
        let sentences = crate::transcribe::build_sentences(&words);
        Transcript { language: "en".into(), words, sentences, avg_confidence: 0.9 }
    }

    fn good_scores() -> Scores {
        Scores {
            self_contained: 5,
            opening_strength: 4,
            specificity: 4,
            tension_or_novelty: 4,
            payoff: 5,
            clarity: 5,
            context_dependency: 1,
            slop_risk: 1,
        }
    }

    fn cand(t: &Transcript, start: u64, end: u64, scores: Scores) -> Candidate {
        Candidate {
            start_ms: start,
            end_ms: end,
            headline: "A test headline".into(),
            opening_quote: excerpt_head(t, start, end, 5),
            closing_quote: excerpt_tail(t, start, end, 5),
            selection_reason: "test".into(),
            scores,
        }
    }

    fn excerpt_head(t: &Transcript, s: u64, e: u64, n: usize) -> String {
        excerpt_text(t, s, e).split_whitespace().take(n).collect::<Vec<_>>().join(" ")
    }
    fn excerpt_tail(t: &Transcript, s: u64, e: u64, n: usize) -> String {
        let text = excerpt_text(t, s, e);
        let v: Vec<&str> = text.split_whitespace().collect();
        v[v.len().saturating_sub(n)..].join(" ")
    }

    const SRC: u64 = 600_000;

    #[test]
    fn accepts_a_good_candidate() {
        let t = transcript(1500, 400); // 600s of words
        let c = cand(&t, 10_000, 50_000, good_scores());
        let r = validate(vec![c], &t, SRC, "test".into());
        assert_eq!(r.accepted.len(), 1);
        assert_eq!(r.rejected.len(), 0);
        assert_eq!(r.accepted[0].rank, 1);
    }

    #[test]
    fn rejects_each_score_threshold() {
        let t = transcript(1500, 400);
        for (field, value) in [
            ("self_contained", 3u8),
            ("payoff", 2),
            ("clarity", 3),
            ("context_dependency", 3),
            ("slop_risk", 3),
        ] {
            let mut s = good_scores();
            match field {
                "self_contained" => s.self_contained = value,
                "payoff" => s.payoff = value,
                "clarity" => s.clarity = value,
                "context_dependency" => s.context_dependency = value,
                _ => s.slop_risk = value,
            }
            let r = validate(vec![cand(&t, 10_000, 50_000, s)], &t, SRC, "test".into());
            assert_eq!(r.accepted.len(), 0, "{} should reject", field);
            assert!(
                r.rejected[0].reasons[0].contains(field),
                "reason should name {}: {:?}",
                field,
                r.rejected[0].reasons
            );
        }
    }

    #[test]
    fn rejects_out_of_range_duration() {
        let t = transcript(1500, 400);
        // 10s — too short even for the exception.
        let r = validate(vec![cand(&t, 10_000, 20_000, good_scores())], &t, SRC, "t".into());
        assert_eq!(r.accepted.len(), 0);
        // 150s — too long even for the exception.
        let r = validate(vec![cand(&t, 10_000, 160_000, good_scores())], &t, SRC, "t".into());
        assert_eq!(r.accepted.len(), 0);
    }

    #[test]
    fn duration_exception_requires_exceptional_scores() {
        let t = transcript(1500, 400);
        // 17s, exceptional scores → accepted with the exception flag.
        let r = validate(vec![cand(&t, 10_000, 27_000, good_scores())], &t, SRC, "t".into());
        assert_eq!(r.accepted.len(), 1);
        assert!(r.accepted[0].duration_exception);
        // 17s, mediocre payoff → rejected.
        let mut s = good_scores();
        s.payoff = 3;
        let r = validate(vec![cand(&t, 10_000, 27_000, s)], &t, SRC, "t".into());
        assert_eq!(r.accepted.len(), 0);
    }

    #[test]
    fn rejects_timestamps_outside_source() {
        let t = transcript(1500, 400);
        let c = cand(&t, 590_000, 640_000, good_scores());
        let r = validate(vec![c], &t, SRC, "t".into());
        assert_eq!(r.accepted.len(), 0);
        assert!(r.rejected[0].reasons[0].contains("outside the source"));
    }

    #[test]
    fn rejects_inverted_interval() {
        let t = transcript(1500, 400);
        let c = cand(&t, 50_000, 50_000, good_scores());
        let r = validate(vec![c], &t, SRC, "t".into());
        assert_eq!(r.accepted.len(), 0);
    }

    #[test]
    fn snaps_to_word_boundaries() {
        let t = transcript(1500, 400);
        // Propose an interval starting mid-word: word at 10_000..10_350.
        let c = cand(&t, 10_133, 50_177, good_scores());
        let r = validate(vec![c], &t, SRC, "t".into());
        assert_eq!(r.accepted.len(), 1);
        let a = &r.accepted[0].candidate;
        assert_eq!(a.start_ms % 400, 0, "start snapped to a word start");
        assert_eq!((a.end_ms + 50) % 400, 0, "end snapped to a word end");
    }

    #[test]
    fn rejects_unfindable_quotes() {
        let t = transcript(1500, 400);
        let mut c = cand(&t, 10_000, 50_000, good_scores());
        c.opening_quote = "words that were never spoken".into();
        let r = validate(vec![c], &t, SRC, "t".into());
        assert_eq!(r.accepted.len(), 0);
        assert!(r.rejected[0].reasons[0].contains("opening quote"));
    }

    #[test]
    fn suppresses_overlap_above_30_percent() {
        let t = transcript(1500, 400);
        let strong = cand(&t, 10_000, 70_000, good_scores());
        let mut weaker_scores = good_scores();
        weaker_scores.opening_strength = 3; // lower composite
        // 40s candidate overlapping 30s with `strong` → 75% overlap → rejected.
        let overlapping = cand(&t, 40_000, 80_000, weaker_scores);
        // Distant candidate survives.
        let distant = cand(&t, 200_000, 250_000, weaker_scores);
        let r = validate(vec![strong, overlapping, distant], &t, SRC, "t".into());
        assert_eq!(r.accepted.len(), 2);
        assert_eq!(r.rejected.len(), 1);
        assert!(r.rejected[0].reasons[0].contains("overlaps"));
        // Ranks are 1..n in composite order.
        assert_eq!(r.accepted[0].rank, 1);
        assert_eq!(r.accepted[1].rank, 2);
    }

    #[test]
    fn overlap_at_25_percent_is_allowed() {
        let t = transcript(1500, 400);
        let a = cand(&t, 10_000, 70_000, good_scores()); // 60s
        let mut s2 = good_scores();
        s2.tension_or_novelty = 3;
        // 60s candidate sharing 10s with `a` → 16% overlap → allowed.
        let b = cand(&t, 60_000, 120_000, s2);
        let r = validate(vec![a, b], &t, SRC, "t".into());
        assert_eq!(r.accepted.len(), 2);
    }

    #[test]
    fn normalize_is_punctuation_and_case_tolerant() {
        assert_eq!(normalize("Hello,   WORLD!"), "hello world");
        assert_eq!(normalize("don't-stop"), "don t stop");
    }

    #[test]
    fn zero_candidates_yields_clean_empty_report() {
        let t = transcript(100, 400);
        let r = validate(vec![], &t, SRC, "t".into());
        assert!(r.accepted.is_empty());
        assert!(r.rejected.is_empty());
    }
}
