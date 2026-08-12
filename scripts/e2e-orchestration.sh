#!/usr/bin/env bash
#
# One sentence to a merged commit, against a real git repository.
#
# What this is for: every part of the orchestrator has unit tests, and they pass whether or not the
# parts are connected to each other. This connects them, and the assertions are about the things
# that only go wrong at the seams.
#
# The two that matter most:
#
#   - A dependency edge has to transport work, not a description of it. The acceptance check for the
#     dependent task passes only if the dependency's file is present in its worktree, so an edge that
#     merely put a summary in a prompt fails here rather than passing with a nice-looking log.
#   - Nothing merges without a person. A run that verified everything stops at a candidate, and the
#     assertion is that the project's branch has not moved.
#
# The first graph deliberately contains a task the validator will reject, so the redraft path runs
# too. A replan loop that is never exercised is a replan loop that does not work.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

FAILED=0
PORT="${PORT:-8791}"

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        printf 'ok   %-62s %s\n' "$label" "$actual"
    else
        printf 'FAIL %-62s expected=%s actual=%s\n' "$label" "$expected" "$actual"
        FAILED=1
    fi
}

check_ge() {
    local label="$1" want="$2" actual="$3"
    if [ "${actual:-0}" -ge "$want" ] 2>/dev/null; then
        printf 'ok   %-62s %s (>= %s)\n' "$label" "$actual" "$want"
    else
        printf 'FAIL %-62s expected>=%s actual=%s\n' "$label" "$want" "$actual"
        FAILED=1
    fi
}

for tool in jq curl git python3; do
    command -v "$tool" > /dev/null || { echo "need $tool"; exit 1; }
done

echo "building"
cargo build -q -p wkbd-core -p fake-acp-agent 2>&1 | tail -5

WORK=$(mktemp -d)
REPO="$WORK/project"
STATE="$WORK/state"
cleanup() {
    [ -n "${DAEMON_PID:-}" ] && kill "$DAEMON_PID" 2> /dev/null
    [ -n "${DAEMON_PID:-}" ] && wait "$DAEMON_PID" 2> /dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

# ----------------------------------------------------------------- the repository
mkdir -p "$REPO/src" "$REPO/tests"
cd "$REPO"
git init -q .
git config user.email "e2e@wkbd.invalid"
git config user.name "e2e"

# The acceptance check. Written as a shell script emitting the format the parser reads, because the
# point is to exercise the orchestrator's acceptance path, not a language toolchain.
#
# The middle assertion is the load-bearing one: it passes only when a task's own file *and* both of
# its dependencies' files are present, so it fails if a dependency edge did not carry the work.
cat > tests/check.sh <<'CHECK'
#!/bin/sh
ok() { echo "test $1 ... ok"; }
[ -f src/base.rs ] && ok unit::base_exists
[ -f NOTES.md ] && ok unit::notes_exist
if [ -f src/a.rs ] && [ -f src/base.rs ] && [ -f NOTES.md ]; then
    ok unit::a_sees_its_dependencies
fi
echo "test result: ok. done"
CHECK
chmod +x tests/check.sh
echo "# project" > README.md
git add -A
git commit -q -m "initial"
BASE_BEFORE=$(git rev-parse HEAD)
cd "$ROOT"

# ----------------------------------------------------------------- the graphs
#
# Two of them. The first has `docs` connected to nothing in a graph that has edges, which the
# validator rejects as an orphan — usually a sign the planner forgot to connect it. The second
# connects it, and does so by making the third task depend on *both* roots, so its starting point is
# a real merge of two commits rather than a fast-forward.
cat > "$WORK/plan.json" <<'PLAN'
[
  {
    "goal": "add a base module, some notes, and a feature that uses both",
    "tasks": [
      {
        "id": "base-module",
        "title": "add the base module",
        "body": "Create the base module.\n\nWRITE src/base.rs <<<pub fn base() -> u32 { 1 }\n>>>",
        "declared_paths": ["src/base.rs"],
        "depends_on": [],
        "verify": {
          "cmd": "sh tests/check.sh",
          "must_pass": ["unit::base_exists"],
          "immutable_paths": ["tests/**"]
        }
      },
      {
        "id": "docs",
        "title": "write the notes",
        "body": "Write the notes file.\n\nWRITE NOTES.md <<<# Notes\n>>>",
        "declared_paths": ["NOTES.md"],
        "depends_on": [],
        "verify": {
          "cmd": "sh tests/check.sh",
          "must_pass": ["unit::notes_exist"],
          "immutable_paths": ["tests/**"]
        }
      },
      {
        "id": "feature-a",
        "title": "add the feature that uses the base module",
        "body": "Add the feature.\n\nWRITE src/a.rs <<<pub fn a() -> u32 { super::base::base() + 1 }\n>>>",
        "declared_paths": ["src/a.rs"],
        "depends_on": ["base-module"],
        "verify": {
          "cmd": "sh tests/check.sh",
          "must_pass": ["unit::a_sees_its_dependencies"],
          "immutable_paths": ["tests/**"]
        }
      }
    ]
  },
  {
    "goal": "add a base module, some notes, and a feature that uses both",
    "tasks": [
      {
        "id": "base-module",
        "title": "add the base module",
        "body": "Create the base module.\n\nWRITE src/base.rs <<<pub fn base() -> u32 { 1 }\n>>>",
        "declared_paths": ["src/base.rs"],
        "depends_on": [],
        "verify": {
          "cmd": "sh tests/check.sh",
          "must_pass": ["unit::base_exists"],
          "immutable_paths": ["tests/**"]
        }
      },
      {
        "id": "docs",
        "title": "write the notes",
        "body": "Write the notes file.\n\nWRITE NOTES.md <<<# Notes\n>>>",
        "declared_paths": ["NOTES.md"],
        "depends_on": [],
        "verify": {
          "cmd": "sh tests/check.sh",
          "must_pass": ["unit::notes_exist"],
          "immutable_paths": ["tests/**"]
        }
      },
      {
        "id": "feature-a",
        "title": "add the feature that uses the base module",
        "body": "Add the feature.\n\nWRITE src/a.rs <<<pub fn a() -> u32 { super::base::base() + 1 }\n>>>",
        "declared_paths": ["src/a.rs"],
        "depends_on": ["base-module", "docs"],
        "verify": {
          "cmd": "sh tests/check.sh",
          "must_pass": ["unit::a_sees_its_dependencies"],
          "immutable_paths": ["tests/**"]
        }
      }
    ]
  }
]
PLAN

# ----------------------------------------------------------------- the run
WORKER="$ROOT/target/debug/fake-acp-agent --profile worker"
"$ROOT/target/debug/wkbd-core" \
    --state-dir "$STATE" --listen "127.0.0.1:$PORT" \
    --agent "worker=Worker=$WORKER" \
    --worker-agent worker \
    --fixed-plan "$WORK/plan.json" \
    > "$WORK/daemon.log" 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 80); do
    curl -sf "http://127.0.0.1:$PORT/api/health" > /dev/null 2>&1 && break
    sleep 0.25
done
if ! curl -sf "http://127.0.0.1:$PORT/api/health" > /dev/null 2>&1; then
    echo "FAIL the daemon did not start"
    tail -30 "$WORK/daemon.log"
    exit 1
fi

# A run against something that is not a repository is refused before the planner is paid for.
NOT_REPO=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/api/runs" \
    -H 'content-type: application/json' \
    -d "{\"goal\":\"x\",\"project_root\":\"$WORK\"}")
check "a run against a non-repository is refused" "400" "$NOT_REPO"

RUN_ID=$(curl -sf -X POST "http://127.0.0.1:$PORT/api/runs" \
    -H 'content-type: application/json' \
    -d "{\"goal\":\"add a base module, notes, and a feature using both\",\"project_root\":\"$REPO\"}" \
    | jq -r '.id // empty')
check "the run was accepted" "true" "$([ -n "$RUN_ID" ] && echo true || echo false)"
if [ -z "$RUN_ID" ]; then
    tail -30 "$WORK/daemon.log"
    exit 1
fi

echo "run $RUN_ID"
for _ in $(seq 1 240); do
    curl -sf "http://127.0.0.1:$PORT/api/runs/$RUN_ID" > "$WORK/run.json" 2>/dev/null || true
    STATUS=$(jq -r '[.events[]|select(.payload.event=="run")|.payload.run]
                    | map(select(.event=="awaiting_merge" or .event=="finished" or .event=="merge_rejected"))
                    | last | .event // empty' "$WORK/run.json" 2>/dev/null)
    [ -n "$STATUS" ] && break
    sleep 0.5
done

E="$WORK/run.json"
RUN='[.events[]|select(.payload.event=="run")|.payload.run]'

echo
echo "--- planning"
# The first graph is rejected by the validator, which is the only way the redraft path gets exercised.
check "the invalid graph was rejected rather than run" "1" \
    "$(jq -r "$RUN | map(select(.event==\"plan_rejected\")) | length" "$E")"
check "the rejection names the unconnected task" "true" \
    "$(jq -r "$RUN | map(select(.event==\"plan_rejected\")) | .[0].problems | tostring | test(\"docs\")" "$E")"
check "the second graph was accepted" "1" \
    "$(jq -r "$RUN | map(select(.event==\"planned\")) | length" "$E")"
check "the accepted graph has three tasks" "3" \
    "$(jq -r "$RUN | map(select(.event==\"planned\")) | .[0].tasks | length" "$E")"

echo
echo "--- parallelism comes from the edges, not a heuristic"
check "two waves" "2" \
    "$(jq -r "$RUN | map(select(.event==\"planned\")) | .[0].waves | length" "$E")"
check "the first wave holds both root tasks" "2" \
    "$(jq -r "$RUN | map(select(.event==\"planned\")) | .[0].waves[0] | length" "$E")"
check "the second wave holds the dependent task" "1" \
    "$(jq -r "$RUN | map(select(.event==\"planned\")) | .[0].waves[1] | length" "$E")"

echo
echo "--- isolation is real"
check "every task got its own worktree" "3" \
    "$(jq -r "$RUN | map(select(.event==\"task_workspace_ready\")) | length" "$E")"
check "the worktrees are on distinct branches" "3" \
    "$(jq -r "$RUN | map(select(.event==\"task_workspace_ready\")) | map(.branch) | unique | length" "$E")"

echo
echo "--- a dependency edge carries work, not a description of it"
# feature-a starts from a merge of both roots. If it started from the base commit instead, the
# assertion below would fail because its acceptance check needs both dependencies' files present.
check "the dependent task starts from both dependency commits" "2" \
    "$(jq -r "$RUN | map(select(.event==\"task_workspace_ready\" and .task_id==\"feature-a\")) | .[0].from_dependencies | length" "$E")"
check "the dependent task's start commit is not the base commit" "false" \
    "$(jq -r --arg base "$BASE_BEFORE" "$RUN | map(select(.event==\"task_workspace_ready\" and .task_id==\"feature-a\")) | .[0].start_commit == \$base" "$E")"
# The assertion that could only pass if the files really arrived.
check "the dependent task's acceptance passed" "true" \
    "$(jq -r "$RUN | map(select(.event==\"task_verified\" and .task_id==\"feature-a\")) | last | .passed" "$E")"
check "every task passed acceptance" "3" \
    "$(jq -r "$RUN | map(select(.event==\"task_verified\" and .passed==true)) | length" "$E")"

echo
echo "--- nothing merges without a person"
check "the run stopped at a candidate" "awaiting_merge" \
    "$(jq -r "$RUN | map(select(.event==\"awaiting_merge\" or .event==\"finished\" or .event==\"merge_rejected\")) | last | .event" "$E")"
check "the candidate contains all three tasks" "3" \
    "$(jq -r "$RUN | map(select(.event==\"awaiting_merge\")) | .[0].order | length" "$E")"
# The whole point of the gate.
check "the project's branch has not moved" "$BASE_BEFORE" "$(git -C "$REPO" rev-parse HEAD)"
check "the run's recorded status says it is waiting" "awaiting_merge" \
    "$(curl -sf "http://127.0.0.1:$PORT/api/runs" | jq -r --arg id "$RUN_ID" '.runs[]|select(.id==$id)|.status')"

echo
echo "--- and then a person merges"
MERGED=$(curl -sf -X POST "http://127.0.0.1:$PORT/api/runs/$RUN_ID/merge" | jq -r '.commit // empty')
check "the merge produced a commit" "true" "$([ -n "$MERGED" ] && echo true || echo false)"
check "the project's branch moved" "true" \
    "$([ "$(git -C "$REPO" rev-parse HEAD)" != "$BASE_BEFORE" ] && echo true || echo false)"
# Every task's work is present in one tree, which is the actual deliverable.
for f in src/base.rs NOTES.md src/a.rs; do
    check "the merged tree contains $f" "true" \
        "$([ -f "$REPO/$f" ] && echo true || echo false)"
done
check "the tests were not modified by any task" "true" \
    "$(git -C "$REPO" diff --quiet "$BASE_BEFORE" HEAD -- tests/ && echo true || echo false)"

echo
echo "--- the run is replayable"
# Every step is checkpointed, so the log is a complete account rather than a summary.
check_ge "checkpoints recorded" 10 \
    "$(python3 - "$STATE" <<'PY'
import sqlite3, sys, glob, os
paths = glob.glob(os.path.join(sys.argv[1], "*.sqlite3")) + glob.glob(os.path.join(sys.argv[1], "*.db"))
for p in paths:
    try:
        c = sqlite3.connect("file:%s?mode=ro" % p, uri=True)
        print(c.execute("SELECT count(*) FROM step_outputs").fetchone()[0])
        break
    except Exception:
        continue
else:
    print(0)
PY
)"
check_ge "run events recorded" 18 "$(jq -r "$RUN | length" "$E")"

echo
if [ "$FAILED" -eq 0 ]; then
    echo "orchestration works end to end"
else
    echo "ORCHESTRATION BROKEN"
    echo "--- daemon log tail"
    tail -40 "$WORK/daemon.log"
fi
exit "$FAILED"
