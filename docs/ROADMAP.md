# Roadmap

How this project improves without regressing: guardrails → measurement →
thin vertical slices. One PR = one user-visible change or one refactor,
never both. Tests land with the change.

## Done

- [x] **CI guardrails** — `cargo fmt --check`, `clippy -D warnings`, `cargo test`
      on every push/PR. The workflow ships as `docs/ci-workflow.yml` (API tokens
      can't write `.github/workflows/`); activate it with the one-liner in that
      file's header.
- [x] **Two-pass rendering** — framed, uncaptioned base intermediates are kept
      per clip; captions burn in a fast second pass.
- [x] **Post-render caption restyling** — style + accent color are chosen on
      each finished clip, not up front. Restyles re-burn from the base in
      seconds (`POST /api/projects/{id}/clips/{clipId}/restyle`).
- [x] **Steadier face tracking** — median outlier rejection, true per-frame
      means, and a pan-speed clamp so the crop never whips.
- [x] **Async hardening** — no blocking fs/process calls on the runtime,
      single-quote-safe FFmpeg filter escaping, stuck-`rendering` recovery
      after hard interruptions, `Cache-Control: no-store` on clip serving.
- [x] **Eval harness scaffold** — `evals/` golden-set workflow and rubric.

## Next (ranked by impact on the core loop: drop MP4 → great clips, fast)

1. **Golden-set evals with real episodes** — assemble 5–10 diverse sources
   (interview, solo, panel, noisy audio, accents), score with `evals/rubric.csv`,
   and record baselines. Every selection-ranking change gets measured against
   them before merging. This is the quality ratchet.
2. **Selection quality iteration** — tune the local ranker against eval scores
   (hook strength, payoff detection, dedup thresholds). The validator already
   gates slop; the ranker decides what reaches it.
3. **Live caption preview** — overlay word-timed captions on the base render in
   the browser (HTML/CSS) so style/color changes preview instantly before the
   burn. The restyle API stays the source of truth for output files.
4. **Speaker-aware framing for two-person podcasts** — active-speaker detection
   (audio energy + face position) so two-face sources can face-crop instead of
   always falling back to blur-pad.
5. **Packaging** — Tauri desktop shell around the existing binary (the $5
   product). The axum surface is already the app's API.
6. **Hosted "Pro" selection relay** — optional paid tier: managed API key
   behind a rate-limited relay, using the existing `src/select/` provider seam.
   Local ranking stays free forever; BYO-key stays free forever.

## Working agreements

- Search before building. Test before shipping. Ship the complete thing.
- Media pipelines regress silently — never merge a ranking/caption/framing
  change without running the golden set.
- Zero clips is a valid output; never lower the validator bar to inflate counts.
