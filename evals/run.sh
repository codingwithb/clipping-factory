#!/usr/bin/env bash
# Run every MP4 in evals/sources/ through a running Clipping Factory studio
# and collect the resulting project views for scoring.
#
# Usage:  bash evals/run.sh [host]        (default http://localhost:4571)
set -euo pipefail

HOST="${1:-http://localhost:4571}"
ROOT="$(cd "$(dirname "$0")" && pwd)"
SOURCES="$ROOT/sources"
RUN_DIR="$ROOT/results/$(date +%Y%m%d-%H%M%S)"

command -v jq >/dev/null || { echo "jq is required (brew install jq)"; exit 1; }
curl -sf "$HOST/api/setup" >/dev/null || { echo "Studio not reachable at $HOST — start it with: cargo run --release"; exit 1; }
shopt -s nullglob
mp4s=("$SOURCES"/*.mp4 "$SOURCES"/*.m4v)
[ ${#mp4s[@]} -gt 0 ] || { echo "No MP4s in $SOURCES — add golden-set episodes first (see evals/README.md)"; exit 1; }

mkdir -p "$RUN_DIR"
echo "Eval run → $RUN_DIR (${#mp4s[@]} source(s))"

for src in "${mp4s[@]}"; do
  name="$(basename "$src")"
  echo ""
  echo "── $name"
  id="$(curl -sf -X POST "$HOST/api/projects" -F "file=@$src" | jq -r '.project.id')"
  echo "   project $id — processing…"

  status="created"
  while :; do
    sleep 5
    view="$(curl -sf "$HOST/api/projects/$id")"
    status="$(jq -r '.project.status' <<<"$view")"
    case "$status" in
      complete|failed|cancelled) break ;;
      *) printf '   %s\r' "$status" ;;
    esac
  done

  out="$RUN_DIR/${name%.*}"
  mkdir -p "$out"
  jq . <<<"$view" > "$out/view.json"
  jq -r '"   → \(.project.status): \(.clips | map(select(.status=="ready")) | length) clip(s) ready, \(.rejected) rejected candidate(s), selector: \(.selector // "n/a")"' <<<"$view"
done

cp "$ROOT/rubric.csv" "$RUN_DIR/rubric.csv"
echo ""
echo "Done. Watch every clip, then score $RUN_DIR/rubric.csv (see evals/README.md)."
