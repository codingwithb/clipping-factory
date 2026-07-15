# Swipe Review theater

Swipe Review is a focused, keyboard-first pass over the clips Clipping Factory already rendered.

## Try it

1. Finish a project with at least one ready clip.
2. Click **Review clips** in the results header, or press `R` while the results screen is visible.
3. Use `←` / `→` to move, `Space` to play or pause, and `1` / `2` / `3` to mark **Keep**, **Maybe**, or **Skip**.
4. A decision advances to the next clip until the final clip. `Esc` closes the theater.
5. The normal results list shows the saved decision. Skipped clips are dimmed, not deleted.

Review decisions are convenience state stored in browser `localStorage`, keyed by the clip-serving pathname. They stay on the local browser, survive refresh, and are not sent to an AI provider or treated as authoritative project state.

## Scope

This experiment does not change selection, ranking, rendering, files, or the source conversation. It does not delete skipped clips or train a preference model. The goal is narrower: make the human judgment pass over multiple generated clips dramatically faster.

The theater reads the ready video cards already rendered by the existing UI. If no compatible ready card exists, the Review button stays hidden and the original results experience remains unchanged.
