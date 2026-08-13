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

# Reads one number out of the daemon's database. Opened read-only: the daemon is still running, and a
# writable handle from a second process is how a test corrupts the thing it is measuring.
# As count_rows, for a query returning text.
count_text() {
    python3 - "$STATE" "$1" <<'SQLPY'
import sqlite3, glob, os, sys
for p in glob.glob(os.path.join(sys.argv[1], "*.db")):
    try:
        c = sqlite3.connect(f"file:{p}?mode=ro", uri=True)
        print(c.execute(sys.argv[2]).fetchone()[0])
        break
    except Exception:
        continue
else:
    print("")
SQLPY
}

count_rows() {
    python3 - "$STATE" "$1" <<'SQLPY'
import sqlite3, glob, os, sys
for p in glob.glob(os.path.join(sys.argv[1], "*.db")):
    try:
        c = sqlite3.connect(f"file:{p}?mode=ro", uri=True)
        print(c.execute(sys.argv[2]).fetchone()[0])
        break
    except Exception:
        continue
else:
    print(-1)
SQLPY
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
    [ -n "${DAEMON7_PID:-}" ] && kill "$DAEMON7_PID" 2> /dev/null
    [ -n "${DAEMON3_PID:-}" ] && kill "$DAEMON3_PID" 2> /dev/null
    [ -n "${DAEMON2_PID:-}" ] && kill "$DAEMON2_PID" 2> /dev/null
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
    --agent "worker-b=Worker B=$WORKER" \
    --agent-cost worker-b=4 \
    --worker-agent worker --worker-agent worker-b \
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
echo "--- what each task actually did"
# The gap this closes: a card could say a task passed and give no way to see what it did, while the
# transcript and the diff were both on disk. A task's diff is taken from its own starting commit, so
# for a dependent task it excludes everything its dependencies produced — the question at a task card
# is what *this* task did.
TD=$(curl -sf "http://127.0.0.1:$PORT/api/runs/$RUN_ID/tasks/base-module/diff")
check "a task's diff is available" "true" "$([ -n "$TD" ] && echo true || echo false)"
check "it names the file that task wrote" "src/base.rs" \
    "$(echo "$TD" | jq -r '.files[0].path')"
check "a file the task created has no earlier version" "null" \
    "$(echo "$TD" | jq -r '.files[0].old_text')"
# The load-bearing one. feature-a starts from a merge of its dependencies, so its own diff must not
# contain their files — a diff against the run base would show all three and answer a different
# question.
FD=$(curl -sf "http://127.0.0.1:$PORT/api/runs/$RUN_ID/tasks/feature-a/diff")
check "a dependent task's diff excludes its dependencies' work" "src/a.rs" \
    "$(echo "$FD" | jq -r '[.files[].path] | join(",")')"
# And the whole candidate, which is the diff the merge decision is about.
CD=$(curl -sf "http://127.0.0.1:$PORT/api/runs/$RUN_ID/candidate/diff")
check "the candidate diff carries every task's work" "3" \
    "$(echo "$CD" | jq -r '.files | length')"
check "nothing was left out of it" "0" "$(echo "$CD" | jq -r '.truncated')"

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
echo "--- routing recorded a choice and learned from the outcome"
# The first of the three learning loops. It is a cost control rather than a quality improvement, and
# the only thing it is fed is the acceptance check — the strong results for learning from experience
# rest on having a verifiable signal, and where there is none the reported benefit collapses to
# roughly nothing.
check_ge "a routing decision was recorded per task" 3 \
    "$(count_rows "SELECT count(*) FROM routing_decisions")"
# A decision that is never rewarded is a row nothing can learn from, which is the state this loop was
# in before: complete, tested, and never fed.
check "every decision was rewarded" "0" \
    "$(count_rows "SELECT count(*) FROM routing_decisions WHERE reward IS NULL")"
# Binary, from acceptance. Every task in this run passed, so every reward is a 1 — an invented
# gradient would show up here as something else.
check "the reward is the acceptance result and nothing else" "0" \
    "$(count_rows "SELECT count(*) FROM routing_decisions WHERE reward NOT IN (0.0, 1.0)")"
check_ge "the arms carry state a later run can start from" 1 \
    "$(count_rows "SELECT count(*) FROM routing_arms")"
# Which agent ran a task is in the log. A choice nobody can see is a choice nobody can question, and
# this one is made by a model of past outcomes rather than by the user.
# Nominated, not inferred. Treating every configured agent as a candidate sent tasks to agents set
# up for interactive chat, and the run failed for a reason visible only in the sidebar.
check "only nominated agents were routed to" "true" \
    "$(jq -r "$RUN | map(select(.event==\"task_state_changed\" and .status==\"dispatched\")) | all(.detail | test(\"worker\"))" "$E")"
check "the log says which agent each task went to" "true" \
    "$(jq -r "$RUN | map(select(.event==\"task_state_changed\" and .status==\"dispatched\")) | all(.detail != null)" "$E")"


echo
echo "--- what the run taught the system"
# Facts are written directly: they are inferred statements with a confidence and a provenance, and
# everything that reads them treats them as evidence rather than as instruction.
check_ge "facts were extracted without anyone filling in a form" 1 "$(count_rows "SELECT count(*) FROM facts")"
# Every fact records which run produced it. A fact with no provenance cannot be re-examined when it
# turns out to be wrong, and that is the first question anyone asks about a memory that misled them.
check "every extracted fact names the run it came from" "0" "$(count_rows "SELECT count(*) FROM facts WHERE source_run IS NULL")"
# The vocabulary boundary between rules and memory. The extractor must not be able to write a fact
# that claims the user said it: such a fact would outrank real rules at injection time and would be
# immune to the retirement that applies to everything inferred.
check "nothing extracted claims the user said it" "0" "$(count_rows "SELECT count(*) FROM facts WHERE source_trust = 'user'")"

echo
echo "--- the system cannot change its own instructions without being asked"
PROPOSALS=$(curl -sf "http://127.0.0.1:$PORT/api/proposals")
check "the approval queue is reachable" "true" "$([ -n "$PROPOSALS" ] && echo true || echo false)"
# This run had no failures and no ownership violations, so there is nothing worth proposing. A queue
# that fills after every run stops being read, and an approval gate nobody reads is not a gate.
check "a clean run proposes nothing" "0" "$(echo "$PROPOSALS" | jq -r '.proposals | length')"
# The list deliberately carries no bodies: a list is skim-read, and the body is the part that has to
# be read carefully with the invisible characters already stripped.
check "the list carries no proposal bodies" "0" "$(echo "$PROPOSALS" | jq -r '[.proposals[]|select(has("body"))] | length')"
# Approving something that is not there is refused rather than quietly succeeding.
GHOST=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    "http://127.0.0.1:$PORT/api/proposals/does-not-exist/approve" \
    -H 'content-type: application/json' -d '{"content_hash":"whatever"}')
check "approving a proposal that does not exist is refused" "409" "$GHOST"

echo
echo "--- a run with something to learn from raises a proposal, and the proposal waits"
# The rail only exists if something can actually reach the queue. A clean run proposing nothing shows
# the queue is not noisy; it does not show the queue works. This run has a task whose assertion names
# a test that will never pass, which is exactly the kind of thing worth writing down.
FAIL_STATE="$WORK/fail"
PORT2=$((PORT + 1))
FREPO="$WORK/failing"
mkdir -p "$FREPO/tests"
(
    cd "$FREPO" && git init -q . \
        && git config user.email f@wkbd.invalid && git config user.name f
    printf '#!/bin/sh\necho "test result: ok. done"\n' > tests/check.sh
    chmod +x tests/check.sh
    echo x > README.md
    git add -A && git commit -q -m initial
)
cat > "$WORK/failplan.json" <<'FAILPLAN'
{"goal":"attempt something that cannot be accepted","tasks":[
  {"id":"impossible","title":"make a test pass that does not exist",
   "body":"Try.\n\nWRITE src/x.rs <<<// nothing\n>>>",
   "declared_paths":["src/x.rs"],"depends_on":[],
   "verify":{"cmd":"sh tests/check.sh","must_pass":["unit::never_exists"],
             "immutable_paths":["tests/**"]}}]}
FAILPLAN

"$ROOT/target/debug/wkbd-core" \
    --state-dir "$FAIL_STATE" --listen "127.0.0.1:$PORT2" \
    --agent "worker=Worker=$WORKER" --worker-agent worker \
    --fixed-plan "$WORK/failplan.json" > "$WORK/daemon2.log" 2>&1 &
DAEMON2_PID=$!
for _ in $(seq 1 80); do
    curl -sf "http://127.0.0.1:$PORT2/api/health" > /dev/null 2>&1 && break
    sleep 0.25
done
FRID=$(curl -sf -X POST "http://127.0.0.1:$PORT2/api/runs" -H 'content-type: application/json' \
    -d "{\"goal\":\"attempt the impossible\",\"project_root\":\"$FREPO\"}" | jq -r '.id // empty')
for _ in $(seq 1 60); do
    N=$(curl -sf "http://127.0.0.1:$PORT2/api/proposals" | jq -r '.proposals | length')
    [ "${N:-0}" -ge 1 ] && break
    sleep 0.5
done

PQ=$(curl -sf "http://127.0.0.1:$PORT2/api/proposals")
check_ge "a run with a failure raises a proposal" 1 "$(echo "$PQ" | jq -r '.proposals | length')"
PID_=$(echo "$PQ" | jq -r '.proposals[0].id')
REVIEW=$(curl -sf "http://127.0.0.1:$PORT2/api/proposals/$PID_")
# Evidence, not an assertion from nowhere. A reviewer has to be able to see which run produced this.
check "the proposal names the run it came from" "$FRID" \
    "$(echo "$REVIEW" | jq -r '.evidence.supporting_runs[0]')"
check_ge "the proposal carries checkable signals" 1 \
    "$(echo "$REVIEW" | jq -r '.evidence.verified_signals | length')"
check "the body is offered for review" "true" \
    "$(echo "$REVIEW" | jq -r '(.body_for_human | length) > 0')"

# The whole rail: nothing is in effect until a person agrees. Approving against a hash the reviewer
# did not see has to be refused, or the check is decorative.
WRONG=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    "http://127.0.0.1:$PORT2/api/proposals/$PID_/approve" \
    -H 'content-type: application/json' -d '{"content_hash":"0000000000000000"}')
check "approving against content the reviewer did not see is refused" "409" "$WRONG"
check "the refused proposal is still pending" "1" \
    "$(curl -sf "http://127.0.0.1:$PORT2/api/proposals" | jq -r '.proposals | length')"

HASH=$(echo "$REVIEW" | jq -r '.content_hash')
OK_CODE=$(curl -s -o "$WORK/approved.json" -w '%{http_code}' -X POST \
    "http://127.0.0.1:$PORT2/api/proposals/$PID_/approve" \
    -H 'content-type: application/json' -d "{\"content_hash\":\"$HASH\"}")
check "approving against the content that was shown succeeds" "200" "$OK_CODE"
check "the queue is empty afterwards" "0" \
    "$(curl -sf "http://127.0.0.1:$PORT2/api/proposals" | jq -r '.proposals | length')"
# Approving has to *do* something. Recording the approval and never running it is what happened
# first, and from outside it is indistinguishable from the loop working: the queue empties, the row
# says approved, and nothing changed. That is worse than not having the queue, because it looks
# closed.
check "approving applied it rather than only recording it" "applied" \
    "$(STATE="$FAIL_STATE" count_text "SELECT status FROM proposals LIMIT 1")"
check "the payload actually landed in the playbook" "1" \
    "$(STATE="$FAIL_STATE" count_rows "SELECT count(*) FROM playbook")"

kill "$DAEMON2_PID" 2>/dev/null || true
wait "$DAEMON2_PID" 2>/dev/null || true

echo
echo "--- a task that left its declared paths sends the graph back to the planner"
# The model's second entry point, and the one that was reported but never taken. A declaration is a
# scheduling input rather than a note — overlapping declarations are what force two tasks to run in
# sequence — so a task that took paths it did not declare has invalidated the reasoning the whole
# schedule was built on. Widening that one task's declaration and carrying on would leave it running
# beside whatever now shares those paths, which is why the answer is a new graph.
echo
REPLAN_STATE="$WORK/replan"
PORT4=$((PORT + 3))
RREPO="$WORK/replanrepo"
mkdir -p "$RREPO/src" "$RREPO/tests"
(
    cd "$RREPO" && git init -q . \
        && git config user.email r@wkbd.invalid && git config user.name r
    printf '#!/bin/sh\nok(){ echo "test $1 ... ok"; }\n[ -f src/a.rs ] && [ -f src/b.rs ] && ok unit::both\necho "test result: ok. done"\n' > tests/check.sh
    chmod +x tests/check.sh
    echo x > README.md
    git add -A && git commit -q -m initial
)
# The first graph says the task touches src/a.rs. The agent writes src/a.rs *and* src/b.rs, so the
# ownership check fails. The second declares both, and the run finishes.
cat > "$WORK/replanplan.json" <<'REPLANPLAN'
[
 {"goal":"add a and b","tasks":[
  {"id":"pair","title":"add both files",
   "body":"Write them.\n\nWRITE src/a.rs <<<// a\n>>>\n\nWRITE src/b.rs <<<// b\n>>>",
   "declared_paths":["src/a.rs"],"depends_on":[],
   "verify":{"cmd":"sh tests/check.sh","must_pass":["unit::both"],
             "immutable_paths":["tests/**"]}}]},
 {"goal":"add a and b","tasks":[
  {"id":"pair","title":"add both files",
   "body":"Write them.\n\nWRITE src/a.rs <<<// a\n>>>\n\nWRITE src/b.rs <<<// b\n>>>",
   "declared_paths":["src/a.rs","src/b.rs"],"depends_on":[],
   "verify":{"cmd":"sh tests/check.sh","must_pass":["unit::both"],
             "immutable_paths":["tests/**"]}}]}
]
REPLANPLAN

"$ROOT/target/debug/wkbd-core" \
    --state-dir "$REPLAN_STATE" --listen "127.0.0.1:$PORT4" \
    --agent "worker=Worker=$WORKER" --worker-agent worker \
    --fixed-plan "$WORK/replanplan.json" > "$WORK/daemon4.log" 2>&1 &
DAEMON7_PID=$!
for _ in $(seq 1 80); do
    curl -sf "http://127.0.0.1:$PORT4/api/health" > /dev/null 2>&1 && break
    sleep 0.25
done
RRID=$(curl -sf -X POST "http://127.0.0.1:$PORT4/api/runs" -H 'content-type: application/json' \
    -d "{\"goal\":\"add a and b\",\"project_root\":\"$RREPO\"}" | jq -r '.id // empty')
for _ in $(seq 1 120); do
    curl -sf "http://127.0.0.1:$PORT4/api/runs/$RRID" > "$WORK/replan.json" 2>/dev/null || true
    S=$(jq -r '[.events[].payload.run|select(.event=="awaiting_merge" or .event=="finished" or .event=="merge_rejected")]|last|.event // empty' "$WORK/replan.json" 2>/dev/null)
    [ -n "$S" ] && break
    sleep 0.5
done
RE="$WORK/replan.json"
RRUN='[.events[]|select(.payload.event=="run")|.payload.run]'

check_ge "the violation was reported" 1 \
    "$(jq -r "$RRUN | map(select(.event==\"replanning\")) | length" "$RE")"
check "the trigger names the file the task took without declaring it" "true" \
    "$(jq -r "$RRUN | map(select(.event==\"replanning\")) | .[0].trigger | test(\"b.rs\")" "$RE")"
# The point. Reporting it and stopping is what happened before; a second `planned` event is the
# graph actually coming back from the planner.
check_ge "the graph was drafted twice" 2 \
    "$(jq -r "$RRUN | map(select(.event==\"planned\")) | length" "$RE")"
check "the second graph declares the path the first one missed" "true" \
    "$(jq -r "$RRUN | map(select(.event==\"planned\")) | last | .tasks[0].declared_paths | index(\"src/b.rs\") != null" "$RE")"
# And the run finishes rather than dying on the violation.
check "the run reached a candidate on the second graph" "awaiting_merge" \
    "$(jq -r "$RRUN | map(select(.event==\"awaiting_merge\" or .event==\"finished\" or .event==\"merge_rejected\")) | last | .event" "$RE")"
check "the task passed once its declaration matched what it did" "true" \
    "$(jq -r "$RRUN | map(select(.event==\"task_verified\")) | last | .passed" "$RE")"

kill "$DAEMON7_PID" 2>/dev/null || true
wait "$DAEMON7_PID" 2>/dev/null || true


echo
echo "--- cancelling a run stops the work, not just the status column"
# A button labelled "cancel" that leaves agents running reports something untrue, so this asserts
# both halves: the run says it is cancelled, and it says what it interrupted. Three tasks with a
# delay, so there is something in flight when the click lands.
CANCEL_STATE="$WORK/cancel"
PORT3=$((PORT + 2))
CREPO="$WORK/cancelrepo"
mkdir -p "$CREPO/src" "$CREPO/tests"
(
    cd "$CREPO" && git init -q . \
        && git config user.email c@wkbd.invalid && git config user.name c
    printf '#!/bin/sh\necho "test result: ok. done"\n' > tests/check.sh
    chmod +x tests/check.sh
    echo x > README.md
    git add -A && git commit -q -m initial
)
cat > "$WORK/cancelplan.json" <<'CANCELPLAN'
{"goal":"three tasks, two of them concurrent","tasks":[
 {"id":"t1","title":"one","body":"WRITE src/1.rs <<<a\n>>>","declared_paths":["src/1.rs"],
  "depends_on":[],"verify":{"cmd":"sh tests/check.sh","must_pass":["unit::x"],
                            "immutable_paths":["tests/**"]}},
 {"id":"t2","title":"two","body":"WRITE src/2.rs <<<b\n>>>","declared_paths":["src/2.rs"],
  "depends_on":[],"verify":{"cmd":"sh tests/check.sh","must_pass":["unit::x"],
                            "immutable_paths":["tests/**"]}},
 {"id":"t3","title":"three","body":"WRITE src/3.rs <<<c\n>>>","declared_paths":["src/3.rs"],
  "depends_on":["t1","t2"],"verify":{"cmd":"sh tests/check.sh","must_pass":["unit::x"],
                                     "immutable_paths":["tests/**"]}}]}
CANCELPLAN

"$ROOT/target/debug/wkbd-core" \
    --state-dir "$CANCEL_STATE" --listen "127.0.0.1:$PORT3" \
    --agent "worker=Worker=$ROOT/target/debug/fake-acp-agent --profile worker --delay-ms 4000" \
    --worker-agent worker --fixed-plan "$WORK/cancelplan.json" > "$WORK/daemon3.log" 2>&1 &
DAEMON3_PID=$!
for _ in $(seq 1 80); do
    curl -sf "http://127.0.0.1:$PORT3/api/health" > /dev/null 2>&1 && break
    sleep 0.25
done
CRID=$(curl -sf -X POST "http://127.0.0.1:$PORT3/api/runs" -H 'content-type: application/json' \
    -d "{\"goal\":\"three tasks\",\"project_root\":\"$CREPO\"}" | jq -r '.id // empty')

# Cancelled only once tasks are really dispatched. Cancelling before anything started would pass
# these assertions without exercising the interruption at all.
for _ in $(seq 1 60); do
    DISPATCHED=$(curl -sf "http://127.0.0.1:$PORT3/api/runs/$CRID" \
        | jq -r '[.events[].payload.run|select(.event=="task_state_changed" and .status=="dispatched")]|length')
    [ "${DISPATCHED:-0}" -ge 1 ] && break
    sleep 0.3
done
check_ge "tasks were in flight before the cancel" 1 "${DISPATCHED:-0}"
CANCEL_CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    "http://127.0.0.1:$PORT3/api/runs/$CRID/cancel")
check "cancel is accepted" "202" "$CANCEL_CODE"
sleep 6

CE=$(curl -sf "http://127.0.0.1:$PORT3/api/runs/$CRID")
check "the run reports itself cancelled" "cancelled" \
    "$(echo "$CE" | jq -r '[.events[].payload.run|select(.event=="finished")]|last|.status')"
check "the cancellation says what it interrupted" "true" \
    "$(echo "$CE" | jq -r '[.events[].payload.run|select(.event=="finished")]|last|.detail|test("interrupted")')"
check "the recorded status is cancelled" "cancelled" \
    "$(curl -sf "http://127.0.0.1:$PORT3/api/runs" | jq -r --arg i "$CRID" '.runs[]|select(.id==$i)|.status')"
# Both of the next two follow from the interruption rather than from the ordering checks, and the
# labels say so because a mutation run proved it: neutralising every `is_cancelled` check leaves both
# of them green. Interrupting the turns makes the tasks in flight fail, so a task depending on them is
# blocked and the queue is empty. Worth asserting — those are the outcomes a user sees — but not
# evidence that the pre-dispatch and wave-boundary checks work.
#
# Those checks are deliberately not covered. A wave is dispatched in a tight loop, so the per-task
# check almost never wins the race, and observing the wave-boundary check needs a cancel that lands
# after wave 1 succeeds and before wave 2 starts. A test built on winning that race would be flaky in
# both directions, and a flaky test guarding a cancellation path is worse than an uncovered one.
check "a task depending on interrupted work does not start" "0" \
    "$(echo "$CE" | jq -r '[.events[].payload.run|select(.task_id=="t3" and .event=="task_workspace_ready")]|length')"
check "no merge candidate is offered" "0" \
    "$(echo "$CE" | jq -r '[.events[].payload.run|select(.event=="awaiting_merge")]|length')"
# Cancelling is not destructive. A stop button that deleted work would be a destructive operation
# wearing a harmless label.
check "work that finished keeps its branch" "true" \
    "$(git -C "$CREPO" branch --list 'wkbd/*' | grep -q . && echo true || echo false)"

kill "$DAEMON3_PID" 2>/dev/null || true
wait "$DAEMON3_PID" 2>/dev/null || true

# ---------------------------------------------------------------- distillation
#
# The third learning loop, end to end: the same shape of work succeeding on three separate
# occasions becomes a procedure waiting for approval. Three runs rather than one with three
# tasks, because the gate counts occasions and sibling tasks from one planner are one.
#
# Nothing is merged in between. Learning happens when a run reaches its candidate — whether a
# person accepts it says nothing about which acceptance checks passed.
echo
echo "distillation"
DIST_STATE=$(mktemp -d)
DPORT=$((PORT + 5))
DREPO="$WORK/distrepo"
mkdir -p "$DREPO/src" "$DREPO/tests"
cat > "$DREPO/tests/check.sh" <<'CHECK'
#!/bin/sh
[ -f src/mod.rs ] && echo "test unit::x ... ok"
echo "test result: ok. done"
CHECK
(
    cd "$DREPO" && git init -q . && git config user.email t@e && git config user.name t
    git add -A && git commit -q -m initial
)
# One graph per planner call, and there are four runs below. The fixed planner hands out its
# list in order and refuses when it runs dry, which is the behaviour that makes it a test double
# rather than a stub: a planner that silently repeated itself would hide a run that asked twice.
python3 - "$WORK/distplan.json" <<'DISTPLAN'
import json, sys
plan = {"goal": "one small change", "tasks": [{
    "id": "only", "title": "write a module",
    "body": "WRITE src/mod.rs <<<pub fn f() {}\n>>>",
    "declared_paths": ["src/mod.rs"], "depends_on": [],
    "verify": {"cmd": "sh tests/check.sh", "must_pass": ["unit::x"],
               "immutable_paths": ["tests/**"]}}]}
open(sys.argv[1], "w").write(json.dumps([plan] * 4))
DISTPLAN

"$ROOT/target/debug/wkbd-core" \
    --state-dir "$DIST_STATE" --listen "127.0.0.1:$DPORT" \
    --agent "worker=Worker=$ROOT/target/debug/fake-acp-agent --profile worker" \
    --worker-agent worker --fixed-plan "$WORK/distplan.json" > "$WORK/daemon-distil.log" 2>&1 &
DDAEMON_PID=$!
for _ in $(seq 1 80); do
    curl -sf "http://127.0.0.1:$DPORT/api/health" > /dev/null 2>&1 && break
    sleep 0.25
done
if ! curl -sf "http://127.0.0.1:$DPORT/api/health" > /dev/null 2>&1; then
    echo "FAIL the distillation daemon did not start"
    tail -20 "$WORK/daemon-distil.log" 2>/dev/null
    FAILED=1
fi

proposals_of_kind() {
    curl -sf "http://127.0.0.1:$DPORT/api/proposals" \
        | jq -r --arg k "$1" '[.proposals[]|select(.kind==$k)]|length'
}

for n in 1 2 3; do
    DRID=$(curl -sf -X POST "http://127.0.0.1:$DPORT/api/runs" -H 'content-type: application/json' \
        -d "{\"goal\":\"small change $n\",\"project_root\":\"$DREPO\"}" | jq -r '.id // empty')
    for _ in $(seq 1 80); do
        DSTATUS=$(curl -sf "http://127.0.0.1:$DPORT/api/runs" \
            | jq -r --arg id "$DRID" '.runs[]|select(.id==$id)|.status')
        [ "$DSTATUS" = "awaiting_merge" ] && break
        case "$DSTATUS" in failed|cancelled) break ;; esac
        sleep 0.3
    done
    check "run $n reached a candidate" "awaiting_merge" "${DSTATUS:-none}"

    # Learning is spawned rather than awaited, so the run reports its result without waiting on it.
    sleep 2
    if [ "$n" -lt 3 ]; then
        # The gate is three occasions, so two must not be enough. Asserted rather than assumed:
        # a distiller that fires on the first run would pass the final check below just as well.
        check "nothing is distilled after $n run(s)" "0" "$(proposals_of_kind workflow)"
    fi
done

check "the third occasion distils a procedure" "1" "$(proposals_of_kind workflow)"

DPID=$(curl -sf "http://127.0.0.1:$DPORT/api/proposals" \
    | jq -r '[.proposals[]|select(.kind=="workflow")]|first|.id')
DREVIEW=$(curl -sf "http://127.0.0.1:$DPORT/api/proposals/$DPID")
check "it cites all three runs" "3" \
    "$(echo "$DREVIEW" | jq -r '.evidence.supporting_runs|length')"
check "the evidence is the acceptance command, not a claim" "true" \
    "$(echo "$DREVIEW" | jq -r '[.evidence.verified_signals[]|select(test("check.sh"))]|length >= 3')"
# The step it recorded is the one the worker actually took, through the client's file methods.
check "the procedure names what the worker did" "true" \
    "$(echo "$DREVIEW" | jq -r '.body_for_human|test("write \\*.rs")')"

# Running again must not queue it a second time. A queue that regrows after every run stops
# being read, and this is the loop most able to regrow it.
DRID=$(curl -sf -X POST "http://127.0.0.1:$DPORT/api/runs" -H 'content-type: application/json' \
    -d "{\"goal\":\"small change 4\",\"project_root\":\"$DREPO\"}" | jq -r '.id // empty')
for _ in $(seq 1 80); do
    DSTATUS=$(curl -sf "http://127.0.0.1:$DPORT/api/runs" \
        | jq -r --arg id "$DRID" '.runs[]|select(.id==$id)|.status')
    [ "$DSTATUS" = "awaiting_merge" ] && break
    sleep 0.3
done
sleep 2
check "a fourth run does not queue it again" "1" "$(proposals_of_kind workflow)"

# And it is still a proposal: nothing reached the playbook without a person.
check "it is waiting, not in effect" "true" \
    "$(curl -sf "http://127.0.0.1:$DPORT/api/proposals" | jq -r --arg i "$DPID" \
        '[.proposals[]|select(.id==$i)]|length == 1')"

kill "$DDAEMON_PID" 2>/dev/null || true
wait "$DDAEMON_PID" 2>/dev/null || true
rm -rf "$DIST_STATE"

echo
if [ "$FAILED" -eq 0 ]; then
    echo "orchestration works end to end"
else
    echo "ORCHESTRATION BROKEN"
    echo "--- daemon log tail"
    tail -40 "$WORK/daemon.log"
fi
exit "$FAILED"
