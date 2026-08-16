#!/usr/bin/env bash
# Resume the B5-4 acceptance gate once the provider quota resets
# (the last run hit GoUsageLimitError — 5h usage limit, resets ~2.5h after
# the run). All code is landed and verified; only the final acceptance
# verdict is pending.
#
# Usage: bash scripts/resume-b54-acceptance.sh   (from the repo root)
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
RUN=docs/auto/runs/land-b5-4-runtime-fail-closed-loop-outbox-status-09dc902b
touch "$RUN"/artifacts/*/*.md "$RUN"/artifacts/*/*/*.md 2>/dev/null || true
tmux kill-session -t b54-resume 2>/dev/null || true
tmux new-session -d -s b54-resume \
  "cd $PWD && python3 /home/u1/ai-batch-runner/pi-batch.py --pipeline $RUN/pipeline.yaml --reuse --log-file logs/b54-resume.log 2>&1 | tee /tmp/b54-resume.out"
echo "launched: tail -f /tmp/b54-resume.out"
