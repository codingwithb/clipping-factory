//! Offline heuristic selector.
//!
//! Runs when no AI key is configured (and in tests). It is deliberately
//! conservative: it looks for sentence windows that open with a hook, develop
//! one idea, and close on a resolution cue, then scores them honestly on the
//! same 1–5 rubric so the deterministic validator applies identical rules.
//! Quotes are exact transcript substrings, so faithfulness is guaranteed.

use crate::domain::{Candidate, Scores, Sentence, Transcript};
use crate::select::overlap_ms;

const HOOK_STARTS: &[&str] = &[
    "what", "why", "how", "here's", "heres", "the biggest", "the problem", "the thing",
    "most people", "nobody", "everyone", "everybody", "if you", "let me tell", "the truth",
    "people think", "you know what", "the mistake", "one thing", "my favorite", "the best",
    "the worst", "i learned", "i realized", "the secret", "stop", "never", "always",
];

const CONTRAST_CUES: &[&str] = &[
    "but ", "actually", "the truth is", "turns out", "instead", "wrong", "mistake",
    "nobody talks", "don't realize", "dont realize", "counterintuitive", "surprised",
    "the opposite", "not what you think", "myth", "lie",
];

const PAYOFF_CUES: &[&str] = &[
    "so ", "that's why", "thats why", "which means", "the lesson", "at the end of the day",
    "that is what", "and that's", "and thats", "the point is", "that changed", "ever since",
    "now i", "the answer", "it works because", "that's how", "thats how",
];

const REFERENCE_CUES: &[&str] = &[
    "as i said", "like i said", "as i mentioned", "like i mentioned", "we talked about",
    "going back to", "earlier i", "mentioned earlier", "said earlier", "as we discussed",
];

const PRONOUN_OPENERS: &[&str] = &["that", "this", "it", "he", "she", "they", "which", "those", "and", "so"];

const FILLER_WORDS: &[&str] = &["um", "uh", "like", "you know", "kind of", "sort of"];

const ABSOLUTE_CUES: &[&str] = &[
    "everyone", "everybody", "nobody", "no one", "never", "always", "all of us",
];

const MIN_MS: u64 = 20_000;
const MAX_MS: u64 = 90_000;

pub fn propose(t: &Transcript, source_duration_ms: u64, proposal_count: usize) -> Vec<Candidate> {
    let sentences = &t.sentences;
    if sentences.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(f32, Candidate)> = Vec::new();

    for start_idx in 0..sentences.len() {
        let opener = &sentences[start_idx];
        let opener_lower = opener.text.to_lowercase();

        // Grow the window sentence by sentence; consider every end point that
        // lands in the 20–90s range and pick the best close.
        let mut best_end: Option<(f32, usize)> = None;
        for end_idx in start_idx..sentences.len() {
            let dur = sentences[end_idx].end_ms.saturating_sub(opener.start_ms);
            if dur < MIN_MS {
                continue;
            }
            if dur > MAX_MS {
                break;
            }
            let closer = &sentences[end_idx];
            let closer_lower = closer.text.to_lowercase();
            let mut end_score = 0.0f32;
            if closer.text.trim_end().ends_with(['.', '?', '!', '…']) {
                end_score += 1.0;
            }
            if PAYOFF_CUES.iter().any(|c| closer_lower.contains(c)) {
                end_score += 1.4;
            }
            // Reward a pause after the closing sentence (natural resolution).
            if let Some(next) = sentences.get(end_idx + 1) {
                if next.start_ms.saturating_sub(closer.end_ms) >= 700 {
                    end_score += 0.8;
                }
            } else {
                end_score += 0.5;
            }
            // Mild preference for the 30–70s sweet spot.
            let dur_s = dur as f32 / 1000.0;
            end_score += 1.0 - ((dur_s - 45.0).abs() / 45.0).min(1.0) * 0.6;

            if best_end.map(|(s, _)| end_score > s).unwrap_or(true) {
                best_end = Some((end_score, end_idx));
            }
        }
        let Some((end_score, end_idx)) = best_end else { continue };
        let closer = &sentences[end_idx];
        let window_text: String = sentences[start_idx..=end_idx]
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let window_lower = window_text.to_lowercase();
        let window = &sentences[start_idx..=end_idx];

        // --- Feature detection -> honest rubric scores -------------------
        let first_word = opener_lower.split_whitespace().next().unwrap_or("");
        let vague_open = is_vague_opener(&opener_lower);
        let pronoun_open = PRONOUN_OPENERS.contains(&first_word) || vague_open;
        let hook = HOOK_STARTS.iter().any(|h| opener_lower.starts_with(h))
            || opener.text.contains('?');
        let contrast = CONTRAST_CUES.iter().any(|c| window_lower.contains(c));
        let payoff_cue = PAYOFF_CUES.iter().any(|c| {
            sentences[end_idx.saturating_sub(1)..=end_idx]
                .iter()
                .any(|s| s.text.to_lowercase().contains(c))
        });
        let reference = REFERENCE_CUES.iter().any(|c| window_lower.contains(c));
        let has_number = window_text.chars().any(|c| c.is_ascii_digit());
        let word_count = window_text.split_whitespace().count().max(1);
        let filler_count = FILLER_WORDS
            .iter()
            .map(|f| window_lower.matches(f).count())
            .sum::<usize>();
        let filler_rate = filler_count as f32 / word_count as f32;
        let question_open = opener.text.contains('?')
            || ["what", "why", "how", "who", "when"].contains(&first_word);
        let repeated_claim = has_repeated_claim(window);
        let exchange = has_reaction_exchange(window);
        let absolute_claim = contains_absolute_claim(&window_lower);

        let self_contained: u8 = match (pronoun_open, reference) {
            (false, false) => if hook { 5 } else { 4 },
            (true, false) => 3,
            (_, true) => 2,
        };
        let opening_strength: u8 = if vague_open {
            3
        } else if hook && question_open || absolute_claim {
            5
        } else if hook {
            4
        } else {
            3
        };
        let specificity: u8 =
            if repeated_claim || has_number && contrast { 5 } else if absolute_claim || has_number || contrast { 4 } else { 3 };
        let tension: u8 = if exchange || contrast && question_open { 5 } else if contrast || question_open { 4 } else { 3 };
        let payoff: u8 = if repeated_claim || payoff_cue && end_score >= 2.5 { 5 } else if exchange || payoff_cue || end_score >= 2.2 { 4 } else { 3 };
        let clarity: u8 = if filler_rate > 0.09 { 3 } else if filler_rate > 0.05 { 4 } else { 5 };
        let context_dependency: u8 = if reference { 4 } else if pronoun_open { 3 } else { 1 };
        let slop_risk: u8 = 1; // continuous faithful excerpt, no effects

        let scores = Scores {
            self_contained,
            opening_strength,
            specificity,
            tension_or_novelty: tension,
            payoff,
            clarity,
            context_dependency,
            slop_risk,
        };

        let composite = self_contained as f32 * 2.0
            + payoff as f32 * 1.6
            + opening_strength as f32 * 1.4
            + clarity as f32 * 1.2
            + tension as f32 * 1.0
            + specificity as f32 * 0.8
            - context_dependency as f32 * 1.5
            + end_score
            + if repeated_claim { 4.0 } else { 0.0 }
            + if exchange { 3.0 } else { 0.0 }
            + if absolute_claim { 1.5 } else { 0.0 }
            - if vague_open { 4.0 } else { 0.0 };

        let headline = make_headline(best_headline_sentence(window));
        let opening_quote = quote_head(&opener.text, 12);
        let closing_quote = quote_tail(&closer.text, 12);
        let selection_reason = make_reason(hook, question_open, contrast, payoff_cue, has_number);

        scored.push((
            composite,
            Candidate {
                start_ms: opener.start_ms,
                end_ms: closer.end_ms,
                headline,
                opening_quote,
                closing_quote,
                selection_reason,
                scores,
            },
        ));
    }

    // Rank, then keep a diverse, non-overlapping set spread across the source.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut kept: Vec<Candidate> = Vec::new();
    for (_, cand) in scored {
        if kept.len() >= proposal_count {
            break;
        }
        let overlaps = kept.iter().any(|k| {
            let inter = overlap_ms(k.start_ms, k.end_ms, cand.start_ms, cand.end_ms) as f64;
            inter / ((cand.end_ms - cand.start_ms).max(1) as f64) > 0.25
        });
        if overlaps {
            continue;
        }
        // Positional diversity: don't let one hot region eat every slot.
        let third = (source_duration_ms / 3).max(1);
        let region = (cand.start_ms / third).min(2);
        let region_count = kept.iter().filter(|k| (k.start_ms / third).min(2) == region).count();
        if region_count >= (proposal_count / 2).max(2) {
            continue;
        }
        kept.push(cand);
    }
    kept
}

fn is_vague_opener(text: &str) -> bool {
    let words = text.split_whitespace().count();
    words <= 9
        && (text.contains("that")
            || text.contains("this")
            || text.contains("those")
            || text.contains(" it ")
            || text.starts_with("it "))
}

fn normalized_claim(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn has_repeated_claim(sentences: &[Sentence]) -> bool {
    for (i, a) in sentences.iter().enumerate() {
        let a = normalized_claim(&a.text);
        for b in sentences.iter().skip(i + 1) {
            let b = normalized_claim(&b.text);
            let shorter = if a.len() <= b.len() { &a } else { &b };
            let longer = if a.len() <= b.len() { &b } else { &a };
            if shorter.split_whitespace().count() >= 3 && longer.contains(shorter) {
                return true;
            }
        }
    }
    false
}

fn has_reaction_exchange(sentences: &[Sentence]) -> bool {
    sentences.windows(2).any(|pair| {
        pair[0].text.contains('?')
            && pair[1].text.split_whitespace().count() <= 12
            && pair[1].start_ms.saturating_sub(pair[0].end_ms) <= 1_500
    })
}

fn contains_absolute_claim(text: &str) -> bool {
    let normalized = format!(" {} ", normalized_claim(text));
    ABSOLUTE_CUES
        .iter()
        .any(|cue| normalized.contains(&format!(" {} ", cue)))
}

fn repeats_elsewhere(sentence: &Sentence, sentences: &[Sentence]) -> bool {
    let claim = normalized_claim(&sentence.text);
    claim.split_whitespace().count() >= 3
        && sentences
            .iter()
            .any(|other| !std::ptr::eq(sentence, other) && normalized_claim(&other.text).contains(&claim))
}

fn best_headline_sentence(sentences: &[Sentence]) -> &Sentence {
    sentences
        .iter()
        .take(5)
        .max_by_key(|s| {
            let lower = s.text.to_lowercase();
            let words = s.text.split_whitespace().count();
            let repeated = repeats_elsewhere(s, sentences) as i32;
            let absolute = contains_absolute_claim(&lower) as i32;
            let question = s.text.contains('?') as i32;
            let concise = (3..=16).contains(&words) as i32;
            repeated * 5 + absolute * 3 + question * 2 + concise
                - is_vague_opener(&lower) as i32 * 3
        })
        .unwrap_or(&sentences[0])
}

fn make_headline(opener: &Sentence) -> String {
    let mut text = opener.text.trim().to_string();
    // Strip weak leading connectives for a cleaner headline.
    for lead in ["so ", "and ", "but ", "um ", "uh ", "well ", "yeah ", "okay ", "ok "] {
        let lower = text.to_lowercase();
        if lower.starts_with(lead) {
            text = text[lead.len()..].trim_start().to_string();
        }
    }
    let mut headline = text.trim_end_matches(['.', ',']).to_string();
    if headline.len() > 90 {
        let cut = headline[..90].rfind(' ').unwrap_or(87);
        headline = format!("{}…", headline[..cut].trim_end());
    }
    // Sentence case.
    let mut chars = headline.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => headline,
    }
}

fn quote_head(text: &str, words: usize) -> String {
    text.split_whitespace().take(words).collect::<Vec<_>>().join(" ")
}

fn quote_tail(text: &str, words: usize) -> String {
    let all: Vec<&str> = text.split_whitespace().collect();
    let start = all.len().saturating_sub(words);
    all[start..].join(" ")
}

fn make_reason(hook: bool, question: bool, contrast: bool, payoff: bool, number: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if question {
        parts.push("opens on a direct question");
    } else if hook {
        parts.push("opens by naming its subject immediately");
    } else {
        parts.push("opens on a complete thought");
    }
    if contrast {
        parts.push("sets up a tension or misconception");
    }
    if number {
        parts.push("grounds the point in specifics");
    }
    if payoff {
        parts.push("lands on a stated takeaway before it ends");
    } else {
        parts.push("resolves at a natural sentence boundary");
    }
    let mut reason = parts.join(", ");
    reason = format!(
        "The excerpt {}. Selected by the offline heuristic; connect an AI provider for editorial-grade selection.",
        reason
    );
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Word;
    use crate::transcribe::build_sentences;

    fn transcript_from(script: &[(&str, u64)]) -> Transcript {
        // (sentence text, gap_ms before it); words spaced ~360ms.
        let mut words: Vec<Word> = Vec::new();
        let mut t = 0u64;
        for (text, gap) in script {
            t += gap;
            for token in text.split_whitespace() {
                words.push(Word { text: token.into(), start_ms: t, end_ms: t + 300, p: 0.92 });
                t += 360;
            }
        }
        let sentences = build_sentences(&words);
        Transcript { language: "en".into(), words, sentences, avg_confidence: 0.92 }
    }

    #[test]
    fn finds_a_hooked_window_in_plausible_speech() {
        // ~40 words per sentence-group ≈ 14s each; three groups ≈ 43s total.
        let long = "Most people completely misunderstand what discipline actually is and I want to explain the real mechanics behind it because once you see it you cannot unsee it at all.";
        let mid = "The mistake is thinking discipline is about motivation when really it is about designing your environment so the default action is the right one every single day without fail.";
        let close = "So the lesson is simple: stop negotiating with yourself every morning and build the system once. That's why the habit finally sticks.";
        let t = transcript_from(&[(long, 0), (mid, 400), (close, 400)]);
        let cands = propose(&t, t.words.last().unwrap().end_ms + 500, 3);
        assert!(!cands.is_empty(), "expected at least one candidate");
        let c = &cands[0];
        assert!(c.end_ms - c.start_ms >= MIN_MS);
        assert!(c.end_ms - c.start_ms <= MAX_MS);
        assert!(c.scores.self_contained >= 4);
        assert!(!c.headline.is_empty() && c.headline.len() <= 92);
    }

    #[test]
    fn empty_transcript_yields_nothing() {
        let t = Transcript {
            language: "en".into(),
            words: vec![],
            sentences: vec![],
            avg_confidence: 0.0,
        };
        assert!(propose(&t, 60_000, 3).is_empty());
    }

    #[test]
    fn repeated_claim_and_reaction_surface_as_top_candidate() {
        let t = transcript_from(&[
            ("I believe the solution to making everybody happy is to give them what they want.", 0),
            ("Let's get them all rich.", 200),
            ("Let's get them all fit and healthy, and then let's get them all happy.", 200),
            ("Are those things even possible?", 200),
            ("Can everyone be rich?", 100),
            ("Everyone can be rich.", 100),
            ("Here's my thought exercise for you.", 200),
            ("Everyone can be rich.", 200),
            ("Everything I have created about making money is free because charging would ruin the point.", 200),
            ("Yes, everybody can be rich, and the reason is that knowledge and productive tools can spread.", 200),
        ]);
        let duration = t.words.last().unwrap().end_ms + 500;
        let cands = propose(&t, duration, 3);
        assert!(!cands.is_empty());
        let headline = cands[0].headline.to_lowercase();
        assert!(
            headline.contains("everyone") && headline.contains("rich"),
            "unexpected top candidate: {}",
            cands[0].headline
        );
    }

    #[test]
    fn short_demonstrative_question_is_not_self_contained() {
        assert!(is_vague_opener("how would that work?"));
        assert!(is_vague_opener("are those things possible?"));
        assert!(!is_vague_opener("can everyone be rich?"));
    }
}
