# Eval harness — the quality ratchet

Clipping Factory's quality lives in *output judgment*: did it pick moments
worth posting? Are captions word-accurate? Did framing hold the speaker?
None of that shows up in unit tests. This harness turns "make it better"
into a measurement.

## Golden set

Put 5–10 real episodes in `evals/sources/` (gitignored — media never gets
committed). Cover the failure modes that matter:

| Slot | Source type | What it stresses |
|---|---|---|
| 1 | Two-person interview, clean audio | the happy path |
| 2 | Solo monologue | ranking without conversational turns |
| 3 | Panel (3+ voices) | framing fallback, crosstalk transcription |
| 4 | Noisy / room-tone recording | transcription confidence, caption accuracy |
| 5 | Accented or fast speech | word timestamps under pressure |
| 6+ | Your actual content | the distribution you really ship |

## Running

```bash
# Terminal 1: the studio
cargo run --release

# Terminal 2: run every source through the pipeline
bash evals/run.sh            # uploads each MP4, polls to completion,
                               # collects manifests into evals/results/<run>/
```

Each run directory ends up with, per source: `view.json` (final project view:
accepted clips, rejected candidates with named reasons, selector used) and a
copy of `rubric.csv` to fill in.

## Scoring

Watch every produced clip. Score 1–5 per row in `rubric.csv`:

- **hook** — do the first 3 seconds earn a stop-scroll?
- **standalone** — fully understandable with zero episode context?
- **payoff** — does it land somewhere, or just trail off?
- **caption_accuracy** — words correct and on-beat with speech?
- **framing** — speaker held steady, no drift, no crop-through-face?
- **would_post** — the only score that really matters (yes=5 / no=1)

Also record: clips produced, clips you'd actually post, and any rejected
candidate that should have been accepted (validator too strict) or accepted
clip that should have been rejected (validator too loose).

## The rule

Before merging any change to `src/select/`, `src/validate.rs`, `src/frame.rs`,
or `src/captions.rs`: run the golden set, compare scores to the previous run,
and put the delta in the PR description. No score, no merge.
