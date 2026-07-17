//! Caption generation — two house styles, rendered by libass via ffmpeg:
//!
//! - **Impact** (default): kinetic stacked lockups. Each spoken phrase becomes
//!   a tight, ragged stack of words at different sizes — connective words
//!   small, the key word HUGE in caps — popping in mid-frame, with the
//!   currently spoken word tinted. The short-form-native look.
//! - **Clean**: the original restrained PRD §11.3 treatment — 3–7 word groups
//!   in the lower safe area, one accent color on the active word.

use crate::domain::Word;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaptionStyle {
    Impact,
    Clean,
}

impl CaptionStyle {
    pub fn from_str(s: &str) -> CaptionStyle {
        match s.trim().to_lowercase().as_str() {
            "clean" | "minimal" => CaptionStyle::Clean,
            _ => CaptionStyle::Impact,
        }
    }
    /// Strict parse for API input: unknown names are an error, not a default.
    pub fn parse_strict(s: &str) -> Option<CaptionStyle> {
        match s.trim().to_lowercase().as_str() {
            "impact" => Some(CaptionStyle::Impact),
            "clean" => Some(CaptionStyle::Clean),
            _ => None,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            CaptionStyle::Impact => "impact",
            CaptionStyle::Clean => "clean",
        }
    }
}

/// Curated for caption legibility. Keep this list strict: every option is a
/// sturdy display, sans-serif, or highly readable serif face available on the
/// target desktop rather than a decorative/script font.
pub const CAPTION_FONTS: [&str; 6] = [
    "Inter",
    "Arial",
    "Helvetica Neue",
    "Avenir Next",
    "Verdana",
    "Georgia",
];

pub fn caption_font_name(input: &str) -> Option<&'static str> {
    CAPTION_FONTS
        .iter()
        .copied()
        .find(|font| font.eq_ignore_ascii_case(input.trim()))
}

/// Default accent as `#RRGGBB`, mirroring the ASS BGR constants below
/// (consistency is asserted by a unit test).
pub fn default_accent_hex(style: CaptionStyle) -> &'static str {
    match style {
        CaptionStyle::Impact => "#FFDD00",
        CaptionStyle::Clean => "#FFB224",
    }
}

/// The words fully inside a clip interval, for caption generation.
pub fn words_in_interval(words: &[Word], start_ms: u64, end_ms: u64) -> Vec<Word> {
    words
        .iter()
        .filter(|w| w.start_ms >= start_ms && w.end_ms <= end_ms)
        .cloned()
        .collect()
}

/// Accent (currently spoken word). ASS colors are &HBBGGRR.
const ACCENT_BGR: &str = "00DDFF"; // #FFDD00 vivid yellow
const CLEAN_ACCENT_BGR: &str = "24B2FF"; // #FFB224 warm amber
const WHITE_BGR: &str = "FFFFFF";

pub struct CaptionInput<'a> {
    /// Words fully inside the clip, with absolute source timestamps.
    pub words: &'a [Word],
    pub clip_start_ms: u64,
    pub clip_end_ms: u64,
    pub headline: &'a str,
    pub font: &'a str,
    /// Accent color in ASS BGR order (see `accent_bgr_for`).
    pub accent_bgr: String,
}

/// Resolve the accent color: a user-picked #RRGGBB wins, otherwise each style
/// has its default (vivid yellow for Impact, warm amber for Clean).
pub fn accent_bgr_for(style: CaptionStyle, user_hex: Option<&str>) -> String {
    user_hex
        .and_then(hex_to_ass_bgr)
        .unwrap_or_else(|| match style {
            CaptionStyle::Impact => ACCENT_BGR.to_string(),
            CaptionStyle::Clean => CLEAN_ACCENT_BGR.to_string(),
        })
}

/// `#RRGGBB` (hash optional) → ASS `BBGGRR` hex, or None if malformed.
pub fn hex_to_ass_bgr(hex: &str) -> Option<String> {
    let h = hex.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{}{}{}", &h[4..6], &h[2..4], &h[0..2]).to_uppercase())
}

pub fn build_ass(input: &CaptionInput, style: CaptionStyle) -> String {
    match style {
        CaptionStyle::Impact => build_impact(input),
        CaptionStyle::Clean => build_clean(input),
    }
}

fn relative_words(input: &CaptionInput) -> (Vec<Word>, u64) {
    let rel: Vec<Word> = input
        .words
        .iter()
        .map(|w| Word {
            text: w.text.clone(),
            start_ms: w.start_ms.saturating_sub(input.clip_start_ms),
            end_ms: w.end_ms.saturating_sub(input.clip_start_ms),
            p: w.p,
        })
        .collect();
    let clip_len = input.clip_end_ms.saturating_sub(input.clip_start_ms);
    (rel, clip_len)
}

// ===========================================================================
// IMPACT STYLE — stacked lockups: small connectives, one HUGE emphasis word
// ===========================================================================

/// Sizes for the two tiers. The emphasis word is fit-clamped to the frame.
const SMALL_FS: f32 = 70.0;
const EMPH_FS: f32 = 150.0;
const EMPH_FS_FLOOR: f32 = 92.0;
/// Uppercase Inter ExtraBold ≈ 0.62 em/char; lowercase ≈ 0.55.
const CHAR_EM_UPPER: f32 = 0.62;
const CHAR_EM_LOWER: f32 = 0.55;
const MAX_LINE_W: f32 = 940.0;
/// Vertical center of the lockup and its allowed band.
const BLOCK_ANCHOR_Y: f32 = 1270.0;
const BLOCK_TOP_MIN: f32 = 920.0;
const BLOCK_BOTTOM_MAX: f32 = 1640.0;
/// Line pitch relative to font size — snug but collision-free.
const LINE_PITCH: f32 = 1.04;

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "so", "of", "to", "in", "on", "at", "is", "are", "was",
    "were", "be", "been", "it", "its", "it's", "that", "that's", "this", "these", "if", "you",
    "your", "you're", "we", "we're", "i", "i'm", "he", "she", "they", "them", "their", "there",
    "there's", "like", "just", "really", "very", "what", "what's", "when", "how", "why", "would",
    "could", "can", "can't", "will", "won't", "because", "about", "for", "with", "as", "do", "did",
    "does", "don't", "have", "has", "had", "not", "no", "yes", "my", "me", "us", "our", "than",
    "then", "get", "got", "go", "going", "gonna", "all", "any", "some", "one", "out", "up", "down",
    "now", "well",
];

fn is_stopword(w: &str) -> bool {
    let clean: String = w
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .collect();
    STOPWORDS.contains(&clean.to_lowercase().as_str())
}

fn alnum_len(w: &str) -> usize {
    w.chars().filter(|c| c.is_alphanumeric()).count()
}

/// The word that gets blasted huge: the last substantial content word, then
/// any content word, then the longest word.
pub fn pick_emphasis(words: &[Word]) -> usize {
    let mut pick: Option<usize> = None;
    for (i, w) in words.iter().enumerate() {
        if alnum_len(&w.text) >= 5 && !is_stopword(&w.text) {
            pick = Some(i);
        }
    }
    if pick.is_none() {
        for (i, w) in words.iter().enumerate() {
            if alnum_len(&w.text) >= 3 && !is_stopword(&w.text) {
                pick = Some(i);
            }
        }
    }
    pick.unwrap_or_else(|| {
        words
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| alnum_len(&w.text))
            .map(|(i, _)| i)
            .unwrap_or(0)
    })
}

#[derive(Debug, PartialEq)]
pub struct LockupLine {
    /// Indexes into the page's words.
    pub word_idx: Vec<usize>,
    pub emphasis: bool,
    pub fs: f32,
    pub x: f32,
    pub y: f32,
}

/// Lay out a page as a 1–3 line lockup: pre-words small, emphasis word huge,
/// post-words small — with mild alternating offsets and safe-band clamping.
pub fn layout_lockup(words: &[Word], page_no: usize) -> Vec<LockupLine> {
    let e = pick_emphasis(words);
    let mut lines: Vec<LockupLine> = Vec::new();

    let small_line = |idxs: Vec<usize>| -> Option<(Vec<usize>, f32)> {
        if idxs.is_empty() {
            return None;
        }
        let chars: usize =
            idxs.iter().map(|&i| words[i].text.len()).sum::<usize>() + idxs.len().saturating_sub(1);
        let mut fs = SMALL_FS;
        if chars as f32 * CHAR_EM_LOWER * fs > MAX_LINE_W {
            fs = (MAX_LINE_W / (chars as f32 * CHAR_EM_LOWER)).max(46.0);
        }
        Some((idxs, fs))
    };

    if let Some((idxs, fs)) = small_line((0..e).collect()) {
        lines.push(LockupLine {
            word_idx: idxs,
            emphasis: false,
            fs,
            x: 0.0,
            y: 0.0,
        });
    }
    {
        let chars = words[e].text.len();
        let mut fs = EMPH_FS;
        if chars as f32 * CHAR_EM_UPPER * fs > MAX_LINE_W {
            fs = (MAX_LINE_W / (chars as f32 * CHAR_EM_UPPER)).max(EMPH_FS_FLOOR);
        }
        lines.push(LockupLine {
            word_idx: vec![e],
            emphasis: true,
            fs,
            x: 0.0,
            y: 0.0,
        });
    }
    if let Some((idxs, fs)) = small_line(((e + 1)..words.len()).collect()) {
        lines.push(LockupLine {
            word_idx: idxs,
            emphasis: false,
            fs,
            x: 0.0,
            y: 0.0,
        });
    }

    // Vertical stack centered on the anchor, clamped to the safe band.
    let total_h: f32 = lines.iter().map(|l| l.fs * LINE_PITCH).sum();
    let mut top = BLOCK_ANCHOR_Y - total_h / 2.0;
    if top < BLOCK_TOP_MIN {
        top = BLOCK_TOP_MIN;
    }
    if top + total_h > BLOCK_BOTTOM_MAX {
        top = BLOCK_BOTTOM_MAX - total_h;
    }
    let mut cursor = top;
    for (li, line) in lines.iter_mut().enumerate() {
        let lh = line.fs * LINE_PITCH;
        line.y = cursor + lh / 2.0;
        cursor += lh;
        line.x = if line.emphasis {
            540.0
        } else {
            // Mild raggedness that always stays on-canvas.
            let chars: usize = line
                .word_idx
                .iter()
                .map(|&i| words[i].text.len())
                .sum::<usize>()
                + line.word_idx.len().saturating_sub(1);
            let w = chars as f32 * CHAR_EM_LOWER * line.fs;
            let max_dx = ((1080.0 - w) / 2.0 - 50.0).max(0.0);
            let dx: f32 = if (li + page_no).is_multiple_of(2) {
                -34.0
            } else {
                34.0
            };
            540.0 + dx.clamp(-max_dx, max_dx)
        };
    }
    lines
}

fn build_impact(input: &CaptionInput) -> String {
    let (rel, clip_len) = relative_words(input);
    let mut ass = String::new();
    ass.push_str(&impact_header(input.font));

    let pages = paginate_impact(&rel);
    for (page_no, page) in pages.iter().enumerate() {
        if page.is_empty() {
            continue;
        }
        // Hard ceiling: never outlive the next page's first word.
        let next_start = pages
            .get(page_no + 1)
            .and_then(|p| p.first())
            .map(|w| w.start_ms)
            .unwrap_or(u64::MAX);
        let last = page.last().unwrap();
        let page_end = (last.end_ms + 200)
            .min(next_start)
            .min(clip_len.max(last.end_ms));

        let lines = layout_lockup(page, page_no);

        for (k, word) in page.iter().enumerate() {
            let start = word.start_ms;
            let end = if k + 1 < page.len() {
                page[k + 1].start_ms
            } else {
                page_end
            };
            if end <= start {
                continue;
            }
            for line in &lines {
                let pop = if k == 0 {
                    let from = if line.emphasis { 85 } else { 90 };
                    format!("\\fscx{f}\\fscy{f}\\t(0,110,\\fscx100\\fscy100)", f = from)
                } else {
                    String::new()
                };
                let mut text = format!(
                    "{{\\an5\\pos({:.0},{:.0})\\fs{:.0}\\blur0.6{}}}",
                    line.x, line.y, line.fs, pop
                );
                for (j, &wi) in line.word_idx.iter().enumerate() {
                    if j > 0 {
                        text.push(' ');
                    }
                    let raw = escape(&page[wi].text);
                    let shown = if line.emphasis {
                        raw.to_uppercase()
                    } else {
                        raw.to_lowercase()
                    };
                    if wi == k {
                        text.push_str(&format!(
                            "{{\\c&H{}&}}{}{{\\c&H{}&}}",
                            input.accent_bgr, shown, WHITE_BGR
                        ));
                    } else {
                        text.push_str(&shown);
                    }
                }
                ass.push_str(&format!(
                    "Dialogue: 0,{},{},Impact,,0,0,0,,{}\n",
                    ass_time(start),
                    ass_time(end),
                    text
                ));
            }
        }
    }
    ass
}

fn impact_header(font: &str) -> String {
    let face = if font == "Inter" {
        "Inter ExtraBold".to_string()
    } else {
        font.to_string()
    };
    format!(
        "[Script Info]\n\
         Title: Clipping Factory captions (impact)\n\
         ScriptType: v4.00+\n\
         PlayResX: 1080\n\
         PlayResY: 1920\n\
         WrapStyle: 2\n\
         ScaledBorderAndShadow: yes\n\
         \n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
         Style: Impact,{face},84,&H00FFFFFF,&H00FFFFFF,&H00000000,&H9C000000,-1,0,0,0,100,100,1,0,1,3.2,3.6,5,60,60,60,1\n\
         \n\
         [Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
        face = face
    )
}

/// Character budget for a page (keeps the small tier comfortably wide).
const PAGE_CHAR_BUDGET: usize = 20;

/// Impact pages: 1–5 words with a look-ahead break so no page overflows.
pub fn paginate_impact(words: &[Word]) -> Vec<Vec<Word>> {
    let mut pages: Vec<Vec<Word>> = Vec::new();
    let mut page: Vec<Word> = Vec::new();
    let mut chars = 0usize;

    for (i, w) in words.iter().enumerate() {
        let w_len = w.text.len();
        if !page.is_empty() && chars + 1 + w_len > PAGE_CHAR_BUDGET {
            pages.push(std::mem::take(&mut page));
            chars = 0;
        }
        chars += if page.is_empty() { w_len } else { w_len + 1 };
        page.push(w.clone());

        let terminal = w
            .text
            .trim_end_matches(['"', '\'', ')', ']'])
            .ends_with(['.', '?', '!', '…', ',']);
        let gap = words
            .get(i + 1)
            .map(|n| n.start_ms.saturating_sub(w.end_ms))
            .unwrap_or(u64::MAX);

        let full = page.len() >= 5;
        let punct = terminal && page.len() >= 2;
        let pause = gap >= 600;

        if full || punct || pause {
            pages.push(std::mem::take(&mut page));
            chars = 0;
        }
    }
    if !page.is_empty() {
        pages.push(page);
    }
    pages
}

// ===========================================================================
// CLEAN STYLE — the original restrained treatment
// ===========================================================================

const MAX_WORDS_PER_PAGE: usize = 7;
const MIN_WORDS_BEFORE_PUNCT_BREAK: usize = 3;
const MAX_CHARS_PER_PAGE: usize = 30;
const PAGE_GAP_MS: u64 = 700;

fn build_clean(input: &CaptionInput) -> String {
    let (rel, clip_len) = relative_words(input);

    let mut ass = String::new();
    ass.push_str(&clean_header(input.font));

    // Headline: only when it adds context beyond the opening caption.
    if show_headline(input.headline, &rel) {
        ass.push_str(&format!(
            "Dialogue: 0,{},{},Headline,,0,0,0,,{}\n",
            ass_time(0),
            ass_time(3500.min(clip_len)),
            escape(input.headline)
        ));
    }

    let pages = paginate(&rel);
    for (page_no, page) in pages.iter().enumerate() {
        let next_start = pages
            .get(page_no + 1)
            .and_then(|p| p.first())
            .map(|w| w.start_ms)
            .unwrap_or(u64::MAX);
        let page_end = page
            .last()
            .map(|w| (w.end_ms + 160).min(next_start).min(clip_len.max(w.end_ms)))
            .unwrap_or(0);
        for (i, word) in page.iter().enumerate() {
            let start = word.start_ms;
            let end = page.get(i + 1).map(|n| n.start_ms).unwrap_or(page_end);
            if end <= start {
                continue;
            }
            let mut line = String::new();
            for (j, w) in page.iter().enumerate() {
                if j > 0 {
                    line.push(' ');
                }
                if j == i {
                    line.push_str(&format!(
                        "{{\\c&H{}&}}{}{{\\c&H{}&}}",
                        input.accent_bgr,
                        escape(&w.text),
                        WHITE_BGR
                    ));
                } else {
                    line.push_str(&escape(&w.text));
                }
            }
            ass.push_str(&format!(
                "Dialogue: 0,{},{},Caption,,0,0,0,,{}\n",
                ass_time(start),
                ass_time(end),
                line
            ));
        }
    }
    ass
}

fn clean_header(font: &str) -> String {
    format!(
        "[Script Info]\n\
         Title: Clipping Factory captions (clean)\n\
         ScriptType: v4.00+\n\
         PlayResX: 1080\n\
         PlayResY: 1920\n\
         WrapStyle: 2\n\
         ScaledBorderAndShadow: yes\n\
         \n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
         Style: Caption,{font},66,&H00FFFFFF,&H00FFFFFF,&H00141414,&H7A000000,-1,0,0,0,100,100,0,0,1,3.4,1.2,2,90,90,400,1\n\
         Style: Headline,{font},42,&H00F2F2F2,&H00FFFFFF,&H00141414,&H7A000000,-1,0,0,0,100,100,0,0,1,2.6,1,8,110,110,110,1\n\
         \n\
         [Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
        font = font
    )
}

/// Clean-style pages: 3–7 word groups.
pub fn paginate(words: &[Word]) -> Vec<Vec<Word>> {
    let mut pages: Vec<Vec<Word>> = Vec::new();
    let mut page: Vec<Word> = Vec::new();
    let mut chars = 0usize;

    for (i, w) in words.iter().enumerate() {
        page.push(w.clone());
        chars += w.text.len() + 1;

        let terminal = w
            .text
            .trim_end_matches(['"', '\'', ')', ']'])
            .ends_with(['.', '?', '!', '…', ',']);
        let gap = words
            .get(i + 1)
            .map(|n| n.start_ms.saturating_sub(w.end_ms))
            .unwrap_or(u64::MAX);

        let full = page.len() >= MAX_WORDS_PER_PAGE;
        let wide = chars >= MAX_CHARS_PER_PAGE && page.len() >= MIN_WORDS_BEFORE_PUNCT_BREAK;
        let punct = terminal && page.len() >= MIN_WORDS_BEFORE_PUNCT_BREAK;
        let pause = gap >= PAGE_GAP_MS;

        if full || wide || punct || pause {
            pages.push(std::mem::take(&mut page));
            chars = 0;
        }
    }
    if !page.is_empty() {
        pages.push(page);
    }
    pages
}

/// Skip the headline overlay when it (nearly) duplicates the opening words.
fn show_headline(headline: &str, words: &[Word]) -> bool {
    if headline.trim().is_empty() {
        return false;
    }
    let norm = |s: &str| -> Vec<String> {
        s.split_whitespace()
            .map(|w| {
                w.chars()
                    .filter(|c| c.is_alphanumeric())
                    .flat_map(|c| c.to_lowercase())
                    .collect::<String>()
            })
            .filter(|w| !w.is_empty())
            .collect()
    };
    let h = norm(headline);
    if h.is_empty() {
        return false;
    }
    let opening: std::collections::HashSet<String> =
        words.iter().take(14).flat_map(|w| norm(&w.text)).collect();
    let contained = h.iter().filter(|w| opening.contains(*w)).count();
    (contained as f32 / h.len() as f32) < 0.7
}

// ===========================================================================
// Shared plumbing
// ===========================================================================

/// ASS timestamp: `H:MM:SS.CS` (centiseconds).
fn ass_time(ms: u64) -> String {
    let cs = (ms / 10) % 100;
    let s = (ms / 1000) % 60;
    let m = (ms / 60_000) % 60;
    let h = ms / 3_600_000;
    format!("{}:{:02}:{:02}.{:02}", h, m, s, cs)
}

/// ASS treats `{}` as override blocks and `\` as escapes — neutralize them.
fn escape(s: &str) -> String {
    s.replace('{', "(").replace('}', ")").replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, start: u64) -> Word {
        Word {
            text: text.into(),
            start_ms: start,
            end_ms: start + 280,
            p: 0.9,
        }
    }

    // ---- Clean style ----

    #[test]
    fn clean_pages_stay_within_3_to_7_words() {
        let words: Vec<Word> = "this is a longer test sentence that should split into several caption pages cleanly without any single page growing too large"
            .split_whitespace()
            .enumerate()
            .map(|(i, t)| w(t, i as u64 * 320))
            .collect();
        let pages = paginate(&words);
        assert!(pages.len() >= 2);
        for p in &pages {
            assert!(p.len() <= 7, "page too long: {}", p.len());
        }
        assert_eq!(pages.iter().map(|p| p.len()).sum::<usize>(), words.len());
    }

    #[test]
    fn pause_forces_page_break() {
        let words = vec![w("before", 0), w("pause", 350), w("after", 3000)];
        let pages = paginate(&words);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].len(), 2);
    }

    #[test]
    fn ass_time_formats_centiseconds() {
        assert_eq!(ass_time(0), "0:00:00.00");
        assert_eq!(ass_time(61_230), "0:01:01.23");
        assert_eq!(ass_time(3_600_000), "1:00:00.00");
    }

    #[test]
    fn headline_skipped_when_duplicating_opening() {
        let words: Vec<Word> = "most people misunderstand what discipline is"
            .split_whitespace()
            .enumerate()
            .map(|(i, t)| w(t, i as u64 * 300))
            .collect();
        assert!(!show_headline(
            "Most people misunderstand what discipline is",
            &words
        ));
        assert!(show_headline(
            "A totally different framing of the idea",
            &words
        ));
    }

    #[test]
    fn clean_karaoke_events_cover_every_word_once() {
        let words: Vec<Word> = (0..5).map(|i| w("word", i * 400)).collect();
        let input = CaptionInput {
            words: &words,
            clip_start_ms: 0,
            clip_end_ms: 3000,
            headline: "",
            font: "Inter",
            accent_bgr: accent_bgr_for(CaptionStyle::Clean, None),
        };
        let ass = build_ass(&input, CaptionStyle::Clean);
        let dialogue_lines = ass.lines().filter(|l| l.starts_with("Dialogue:")).count();
        assert_eq!(dialogue_lines, 5);
        assert!(ass.contains(CLEAN_ACCENT_BGR));
    }

    #[test]
    fn escapes_ass_control_characters() {
        assert_eq!(escape(r"a{b}c\d"), "a(b)c/d");
    }

    // ---- Impact style ----

    fn words_from(s: &str) -> Vec<Word> {
        s.split_whitespace()
            .enumerate()
            .map(|(i, t)| w(t, i as u64 * 330))
            .collect()
    }

    fn input<'a>(words: &'a [Word], end_ms: u64) -> CaptionInput<'a> {
        CaptionInput {
            words,
            clip_start_ms: 0,
            clip_end_ms: end_ms,
            headline: "",
            font: "Inter",
            accent_bgr: accent_bgr_for(CaptionStyle::Impact, None),
        }
    }

    /// Parse "Dialogue: 0,H:MM:SS.CS,H:MM:SS.CS,..." start/end back to ms.
    fn parse_events(ass: &str) -> Vec<(u64, u64)> {
        fn t(s: &str) -> u64 {
            let parts: Vec<&str> = s.split(':').collect();
            let (h, m, rest) = (parts[0], parts[1], parts[2]);
            let (sec, cs) = rest.split_once('.').unwrap();
            h.parse::<u64>().unwrap() * 3_600_000
                + m.parse::<u64>().unwrap() * 60_000
                + sec.parse::<u64>().unwrap() * 1000
                + cs.parse::<u64>().unwrap() * 10
        }
        ass.lines()
            .filter(|l| l.starts_with("Dialogue:"))
            .map(|l| {
                let f: Vec<&str> = l.splitn(4, ',').collect();
                (t(f[1]), t(f[2]))
            })
            .collect()
    }

    #[test]
    fn hex_conversion_is_bgr() {
        assert_eq!(hex_to_ass_bgr("#FFDD00").unwrap(), "00DDFF");
        assert_eq!(hex_to_ass_bgr("4fb5ff").unwrap(), "FFB54F");
        assert!(hex_to_ass_bgr("#nope").is_none());
        assert!(hex_to_ass_bgr("#FFF").is_none());
    }

    #[test]
    fn accent_defaults_per_style_and_user_wins() {
        assert_eq!(accent_bgr_for(CaptionStyle::Impact, None), "00DDFF");
        assert_eq!(accent_bgr_for(CaptionStyle::Clean, None), "24B2FF");
        assert_eq!(
            accent_bgr_for(CaptionStyle::Impact, Some("#7CFF4F")),
            "4FFF7C"
        );
        assert_eq!(
            accent_bgr_for(CaptionStyle::Impact, Some("garbage")),
            "00DDFF"
        );
    }

    #[test]
    fn emphasis_prefers_last_substantial_content_word() {
        let words = words_from("when silence feels like strength");
        assert_eq!(pick_emphasis(&words), 4);
        let words = words_from("if this would work with 300");
        assert_eq!(words[pick_emphasis(&words)].text, "300");
    }

    #[test]
    fn lockup_has_a_dominant_emphasis_line_in_the_safe_band() {
        let words = words_from("when silence feels like strength");
        let lines = layout_lockup(&words, 0);
        // Every word appears in exactly one line.
        let mut covered: Vec<usize> = lines.iter().flat_map(|l| l.word_idx.clone()).collect();
        covered.sort();
        assert_eq!(covered, vec![0, 1, 2, 3, 4]);
        let emph = lines
            .iter()
            .find(|l| l.emphasis)
            .expect("has emphasis line");
        for l in &lines {
            if !l.emphasis {
                assert!(
                    emph.fs > l.fs * 1.5,
                    "emphasis dominates: {} vs {}",
                    emph.fs,
                    l.fs
                );
            }
            assert!(l.y > BLOCK_TOP_MIN - 1.0 && l.y < BLOCK_BOTTOM_MAX + 1.0);
        }
        // Lines never collide: centers are at least the smaller half-pitch apart.
        for pair in lines.windows(2) {
            let min_gap = (pair[0].fs + pair[1].fs) / 2.0 * 0.9;
            assert!(pair[1].y - pair[0].y >= min_gap * 0.9, "lines too close");
        }
    }

    #[test]
    fn long_emphasis_words_clamp_to_frame() {
        let words = words_from("this is counterintuitive");
        let lines = layout_lockup(&words, 0);
        let emph = lines.iter().find(|l| l.emphasis).unwrap();
        let w = "counterintuitive".len() as f32 * CHAR_EM_UPPER * emph.fs;
        assert!(w <= MAX_LINE_W + 1.0, "emphasis width {} exceeds frame", w);
        assert!(
            emph.fs >= EMPH_FS_FLOOR - 26.0,
            "still reads big: {}",
            emph.fs
        );
    }

    #[test]
    fn impact_pages_stay_small_and_lose_no_words() {
        let words = words_from(
            "most people think reselling is about finding cheap stuff and flipping it for profit online every day",
        );
        let pages = paginate_impact(&words);
        for p in &pages {
            assert!(p.len() <= 5, "impact page too long: {}", p.len());
            if p.len() > 1 {
                let chars: usize = p.iter().map(|w| w.text.len()).sum::<usize>() + p.len() - 1;
                assert!(chars <= PAGE_CHAR_BUDGET, "page over budget: {}", chars);
            }
        }
        assert_eq!(pages.iter().map(|p| p.len()).sum::<usize>(), words.len());
    }

    /// Regression: no two caption windows may partially overlap. (Lines of the
    /// same lockup legitimately share an identical window.)
    #[test]
    fn impact_windows_never_partially_overlap_in_fast_speech() {
        let words: Vec<Word> =
            "this is very fast speech with no pauses at all between any words here honestly"
                .split_whitespace()
                .enumerate()
                .map(|(i, t)| Word {
                    text: t.into(),
                    start_ms: i as u64 * 180,
                    end_ms: (i as u64 + 1) * 180,
                    p: 0.9,
                })
                .collect();
        let ass = build_ass(&input(&words, 20_000), CaptionStyle::Impact);
        let mut windows = parse_events(&ass);
        windows.sort();
        windows.dedup();
        for pair in windows.windows(2) {
            assert!(
                pair[0].1 <= pair[1].0,
                "windows overlap: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn impact_uppercases_emphasis_and_tints_active_word() {
        let words = words_from("when silence feels like strength");
        let ass = build_ass(&input(&words, 4000), CaptionStyle::Impact);
        assert!(ass.contains("STRENGTH"), "emphasis uppercased");
        assert!(ass.contains("silence"), "small words lowercase");
        assert!(ass.contains(&accent_bgr_for(CaptionStyle::Impact, None)));
        let pops = ass.matches("\\t(0,").count();
        assert!(pops >= paginate_impact(&words).len(), "pop-in on each page");
    }

    #[test]
    fn custom_accent_flows_into_the_ass() {
        let words = words_from("when silence feels like strength");
        let mut inp = input(&words, 4000);
        inp.accent_bgr = accent_bgr_for(CaptionStyle::Impact, Some("#7CFF4F"));
        let ass = build_ass(&inp, CaptionStyle::Impact);
        assert!(ass.contains("4FFF7C"), "custom green accent present");
        assert!(!ass.contains("00DDFF"), "default yellow fully replaced");
    }

    #[test]
    fn style_parses_from_string() {
        assert_eq!(CaptionStyle::from_str("clean"), CaptionStyle::Clean);
        assert_eq!(CaptionStyle::from_str("impact"), CaptionStyle::Impact);
        assert_eq!(CaptionStyle::from_str("anything"), CaptionStyle::Impact);
    }

    #[test]
    fn curated_caption_fonts_parse_to_canonical_names() {
        assert_eq!(caption_font_name("inter"), Some("Inter"));
        assert_eq!(caption_font_name("Helvetica Neue"), Some("Helvetica Neue"));
        assert_eq!(caption_font_name("avenir next"), Some("Avenir Next"));
    }

    #[test]
    fn arbitrary_or_decorative_fonts_are_rejected() {
        assert_eq!(caption_font_name("Comic Sans MS"), None);
        assert_eq!(caption_font_name("Brush Script MT"), None);
        assert_eq!(caption_font_name(""), None);
    }

    #[test]
    fn selected_font_is_written_to_the_caption_style() {
        let words = words_from("readable captions");
        let mut caption_input = input(&words, 2_000);
        caption_input.font = "Georgia";
        let ass = build_ass(&caption_input, CaptionStyle::Impact);
        assert!(ass.contains("Style: Impact,Georgia,"));
    }
}

#[cfg(test)]
mod restyle_support_tests {
    use super::*;

    #[test]
    fn default_hex_matches_ass_bgr_constants() {
        assert_eq!(
            hex_to_ass_bgr(default_accent_hex(CaptionStyle::Impact)).as_deref(),
            Some(ACCENT_BGR)
        );
        assert_eq!(
            hex_to_ass_bgr(default_accent_hex(CaptionStyle::Clean)).as_deref(),
            Some(CLEAN_ACCENT_BGR)
        );
    }

    #[test]
    fn parse_strict_rejects_unknown_styles() {
        assert_eq!(
            CaptionStyle::parse_strict("impact"),
            Some(CaptionStyle::Impact)
        );
        assert_eq!(
            CaptionStyle::parse_strict(" Clean "),
            Some(CaptionStyle::Clean)
        );
        assert_eq!(CaptionStyle::parse_strict("comic-sans"), None);
        assert_eq!(CaptionStyle::parse_strict(""), None);
    }

    #[test]
    fn words_in_interval_keeps_only_fully_contained_words() {
        let w = |s: u64, e: u64| Word {
            text: "w".into(),
            start_ms: s,
            end_ms: e,
            p: 1.0,
        };
        let words = vec![w(900, 1100), w(1000, 1500), w(1500, 2000), w(1900, 2100)];
        let inside = words_in_interval(&words, 1000, 2000);
        assert_eq!(inside.len(), 2);
        assert_eq!(inside[0].start_ms, 1000);
        assert_eq!(inside[1].end_ms, 2000);
    }
}
