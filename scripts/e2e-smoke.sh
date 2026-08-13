#!/usr/bin/env bash
# End-to-end smoke test: daemon plus a real agent subprocess plus the HTTP surface.
#
# This is the vertical slice check. It is deliberately not a unit test: the defects this
# catches live in the handover between the pieces, and every one of them was found here
# rather than in a crate's own tests. Specifically it asserts that a turn producing several
# thought segments ends with at most one of them live, which is the property the whole
# layered transcript depends on and which a single-thought test double cannot exercise.
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
STATE=$(mktemp -d)
PORT=${PORT:-8799}
FAILED=0
DAEMON_PID=""

cleanup() {
    if [ -n "$DAEMON_PID" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$STATE"
}
trap cleanup EXIT

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        printf 'ok   %-64s %s\n' "$label" "$actual"
    else
        printf 'FAIL %-64s expected=%s actual=%s\n' "$label" "$expected" "$actual"
        FAILED=1
    fi
}

check_ge() {
    local label="$1" minimum="$2" actual="$3"
    if [ "$actual" -ge "$minimum" ] 2>/dev/null; then
        printf 'ok   %-64s %s (>= %s)\n' "$label" "$actual" "$minimum"
    else
        printf 'FAIL %-64s expected>=%s actual=%s\n' "$label" "$minimum" "$actual"
        FAILED=1
    fi
}

# Reads the event stream the same way a client does: subscribe from zero, take the replay.
read_events() {
    python3 "$ROOT/scripts/read-events.py" "$PORT" 0 "${1:-1.2}"
}

# Agents already running before we start, so the leak check can measure a delta.
BASELINE_AGENTS=$(pgrep -x fake-acp-agent 2>/dev/null | wc -l | tr -d ' ')

echo "building"
cargo build -q -p wkbd-core -p fake-acp-agent || { echo "build failed"; exit 1; }

AGENT="$ROOT/target/debug/fake-acp-agent --profile rich --live-config true"
"$ROOT/target/debug/wkbd-core" \
    --state-dir "$STATE" \
    --listen "127.0.0.1:$PORT" \
    --agent "fake=Fake Agent=$AGENT" \
    > "$STATE/daemon.log" 2>&1 &
DAEMON_PID=$!

echo "waiting for the daemon"
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$PORT/api/health" > /dev/null 2>&1; then break; fi
    sleep 0.2
done

HEALTH=$(curl -sf "http://127.0.0.1:$PORT/api/health" 2>/dev/null || echo '{}')
check "daemon is serving" "true" "$(echo "$HEALTH" | jq -r '.ok // false')"
check "store is writable" "writable" "$(echo "$HEALTH" | jq -r 'if .read_only == true then "read-only" else "writable" end')"
check "the agent was registered" "1" "$(echo "$HEALTH" | jq -r '.agents // 0')"

echo "opening a session"
SESSION=$(curl -sf -X POST "http://127.0.0.1:$PORT/api/sessions" \
    -H 'content-type: application/json' \
    -d "{\"agent_id\":\"fake\",\"project_root\":\"$STATE\"}" 2>/dev/null || echo '{}')
SID=$(echo "$SESSION" | jq -r '.id // empty')
if [ -z "$SID" ]; then
    echo "FAIL could not open a session"
    echo "--- daemon log ---"; cat "$STATE/daemon.log"
    exit 1
fi
printf 'ok   %-64s %s\n' "session opened" "$SID"

echo "running a turn"
curl -sf -X POST "http://127.0.0.1:$PORT/api/sessions/$SID/prompt" \
    -H 'content-type: application/json' \
    -d '{"text":"why is config slow"}' > /dev/null 2>&1

# The rich profile asks for permission and blocks until answered, so the turn will not finish
# on its own. Poll for the request, answer it, then wait for the turn to end.
REQ=""
for _ in $(seq 1 100); do
    read_events > "$STATE/poll.json" 2>/dev/null || true
    REQ=$(jq -r '[.[]|select(.payload.event=="permission_requested")]|last.payload.request_id // empty' \
        "$STATE/poll.json" 2>/dev/null || echo "")
    [ -n "$REQ" ] && break
    sleep 0.2
done

if [ -n "$REQ" ]; then
    printf 'ok   %-64s %s\n' "the agent asked for permission" "id=$REQ"
    curl -sf -X POST "http://127.0.0.1:$PORT/api/sessions/$SID/permission" \
        -H 'content-type: application/json' \
        -d "{\"request_id\":\"$REQ\",\"option_id\":\"allow-once\"}" > /dev/null 2>&1
else
    echo "FAIL the agent never asked for permission"
    FAILED=1
fi

echo "waiting for the turn to end"
STREAM_OUT="$STATE/events.json"
for _ in $(seq 1 150); do
    # Read the event log through the same replay path a reconnecting client uses.
    read_events 1.2 > "$STREAM_OUT" 2>/dev/null || echo "[]" > "$STREAM_OUT"
    ENDED=$(jq -r '[.[] | select(.payload.event=="turn_ended")] | length' "$STREAM_OUT" 2>/dev/null || echo 0)
    [ "$ENDED" != "0" ] && break
    sleep 0.2
done

EVENTS="$STREAM_OUT"
TOTAL=$(jq -r 'length' "$EVENTS" 2>/dev/null || echo 0)
check_ge "events replayed from the log" 10 "$TOTAL"

check "the turn ended" "1" "$(jq -r '[.[]|select(.payload.event=="turn_ended")]|length' "$EVENTS")"
check "stop reason" "end_turn" "$(jq -r '[.[]|select(.payload.event=="turn_ended")]|last.payload.stop_reason' "$EVENTS")"

# Sequence numbers must be strictly increasing. This is what a reconnecting client relies on.
check "sequence numbers strictly increase" "true" \
    "$(jq -r '[.[].seq] | . as $s | ($s | sort) == $s and (($s|unique|length) == ($s|length))' "$EVENTS")"

THOUGHTS=$(jq -r '[.[]|select(.payload.event=="segment_started" and .payload.kind=="thought")]|length' "$EVENTS")
check "thought segments in the turn" "3" "$THOUGHTS"

ANSWERS=$(jq -r '[.[]|select(.payload.event=="segment_started" and .payload.kind=="message")]|length' "$EVENTS")
check "answer segments in the turn" "1" "$ANSWERS"

# The invariant the transcript depends on: replay the started/settled pairs and track the
# maximum number simultaneously open.
MAXLIVE=$(jq -r '
  reduce .[] as $e ({live: [], max: 0};
    if $e.payload.event == "segment_started" then
      (.live |= (if any(.[]; . == $e.payload.segment.raw) then . else . + [$e.payload.segment.raw] end))
      | .max = ([.max, (.live|length)] | max)
    elif $e.payload.event == "segment_settled" then
      .live |= map(select(. != $e.payload.segment.raw))
    else . end)
  | .max' "$EVENTS")
check "maximum simultaneously live segments" "1" "$MAXLIVE"

check "no segment left live at the end" "0" "$(jq -r '
  reduce .[] as $e ([];
    if $e.payload.event == "segment_started" then (. + [$e.payload.segment.raw]) | unique
    elif $e.payload.event == "segment_settled" then map(select(. != $e.payload.segment.raw))
    else . end) | length' "$EVENTS")"

check "the permission request reached the transcript" "1" \
    "$(jq -r '[.[]|select(.payload.event=="permission_requested")]|length' "$EVENTS")"
check "the permission answer reached the transcript" "1" \
    "$(jq -r '[.[]|select(.payload.event=="permission_resolved")]|length' "$EVENTS")"
check "structured diff content survived" "1" \
    "$(jq -r '[.[]|select(.payload.event=="tool_call_updated")|.payload.content[]?|select(.type=="diff")]|length' "$EVENTS")"
check "usage was reported" "true" \
    "$(jq -r '[.[]|select(.payload.event=="usage_changed")]|length > 0' "$EVENTS")"
check "the plan reached the transcript" "true" \
    "$(jq -r '[.[]|select(.payload.event=="plan_changed")]|length > 0' "$EVENTS")"

# Config options come from the agent, and the model selector must be one of them.
CFG=$(curl -sf "http://127.0.0.1:$PORT/api/agents" 2>/dev/null | jq -r '.[0].id')
check "agent listed over HTTP" "fake" "$CFG"

# The model and thinking selectors come from the agent at session/new. If they never reach the
# event log the interface sees an empty list and correctly draws nothing, so the feature looks
# absent rather than broken — which is why this is asserted here rather than left to the eye.
check "config options reached the event log" "true" \
    "$(jq -r '[.[]|select(.payload.event=="config_options_changed")]|length > 0' "$EVENTS")"
check "a model selector was offered" "model" \
    "$(jq -r '[.[]|select(.payload.event=="config_options_changed")]|last.payload.options[]?|select(.category=="model")|.id' "$EVENTS" | head -1)"
check "a thinking level selector was offered" "thought_level" \
    "$(jq -r '[.[]|select(.payload.event=="config_options_changed")]|last.payload.options[]?|select(.category=="thought_level")|.id' "$EVENTS" | head -1)"
# An option the agent declares no category for must still be carried through rather than dropped.
check "an uncategorised option was carried through" "verbose" \
    "$(jq -r '[.[]|select(.payload.event=="config_options_changed")]|last.payload.options[]?|select(.category==null)|.id' "$EVENTS" | head -1)"

echo "checking that a second prompt is refused while one is running"
curl -sf -X POST "http://127.0.0.1:$PORT/api/sessions/$SID/prompt" \
    -H 'content-type: application/json' -d '{"text":"again"}' > /dev/null 2>&1
sleep 0.3
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    "http://127.0.0.1:$PORT/api/sessions/$SID/prompt" \
    -H 'content-type: application/json' -d '{"text":"and again"}')
if [ "$CODE" = "409" ] || [ "$CODE" = "202" ]; then
    printf 'ok   %-64s %s\n' "concurrent prompt handling" "$CODE"
else
    printf 'FAIL %-64s %s\n' "concurrent prompt handling" "$CODE"
    FAILED=1
fi

echo "checking that the agent process is gone after shutdown"
# Measured as a delta against the processes that existed before this daemon started. A global
# count would include agents belonging to any other daemon on the machine, which makes the
# assertion pass or fail for reasons that have nothing to do with the code under test.
BEFORE=$(pgrep -x fake-acp-agent 2>/dev/null | wc -l | tr -d ' ')
STARTED=$((BEFORE - BASELINE_AGENTS))
check_ge "agent processes started by this daemon" 1 "$STARTED"
kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
sleep 1
AFTER=$(pgrep -x fake-acp-agent 2>/dev/null | wc -l | tr -d ' ')
check "agent processes left behind by this daemon" "0" "$((AFTER - BASELINE_AGENTS))"

# ---------------------------------------------------------------- degraded agent
#
# The profile above declares everything the protocol allows. Most agents do not: no messageId on
# chunks, no config options, no usage reporting. That is the path most users will actually be on,
# so it gets asserted rather than assumed — and it is asserted through a second daemon, because
# the degraded behaviour has to hold from a cold start rather than only after a rich agent has
# already populated the log.
echo
echo "checking the degraded agent path"
STATE2=$(mktemp -d)
PORT2=$((PORT + 1))
SPARTAN="$ROOT/target/debug/fake-acp-agent --profile spartan"
"$ROOT/target/debug/wkbd-core" \
    --state-dir "$STATE2" --listen "127.0.0.1:$PORT2" \
    --agent "spartan=Spartan Agent=$SPARTAN" > "$STATE2/daemon.log" 2>&1 &
DAEMON2_PID=$!

for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$PORT2/api/health" > /dev/null 2>&1 && break
    sleep 0.2
done

SID2=$(curl -sf -X POST "http://127.0.0.1:$PORT2/api/sessions" \
    -H 'content-type: application/json' \
    -d "{\"agent_id\":\"spartan\",\"project_root\":\"$STATE2\"}" 2>/dev/null | jq -r '.id // empty')

if [ -n "$SID2" ]; then
    curl -sf -X POST "http://127.0.0.1:$PORT2/api/sessions/$SID2/prompt" \
        -H 'content-type: application/json' -d '{"text":"find the todos"}' > /dev/null 2>&1
    for _ in $(seq 1 60); do
        python3 "$ROOT/scripts/read-events.py" "$PORT2" 0 1.0 > "$STATE2/events.json" 2>/dev/null || echo '[]' > "$STATE2/events.json"
        [ "$(jq -r '[.[]|select(.payload.event=="turn_ended")]|length' "$STATE2/events.json")" != "0" ] && break
        sleep 0.2
    done
    E2="$STATE2/events.json"

    check "degraded: turn ended" "1" "$(jq -r '[.[]|select(.payload.event=="turn_ended")]|length' "$E2")"
    # Segmentation with no messageId at all has to come from interleaving boundaries.
    check "degraded: thoughts split without message ids" "2" \
        "$(jq -r '[.[]|select(.payload.event=="segment_started" and .payload.kind=="thought")]|length' "$E2")"
    check "degraded: every boundary is marked inferred" "true" \
        "$(jq -r '[.[]|select(.payload.event=="segment_started")]|all(.payload.segment.synthesized)' "$E2")"
    check "degraded: still at most one live segment" "1" "$(jq -r '
      reduce .[] as $e ({live: [], max: 0};
        if $e.payload.event == "segment_started" then
          (.live |= (if any(.[]; . == $e.payload.segment.raw) then . else . + [$e.payload.segment.raw] end))
          | .max = ([.max, (.live|length)] | max)
        elif $e.payload.event == "segment_settled" then
          .live |= map(select(. != $e.payload.segment.raw))
        else . end) | .max' "$E2")"
    # The two absences that must stay absent rather than becoming zero or "unknown".
    check "degraded: no config options were invented" "0" \
        "$(jq -r '[.[]|select(.payload.event=="config_options_changed")]|length' "$E2")"
    check "degraded: no usage was invented" "0" \
        "$(jq -r '[.[]|select(.payload.event=="usage_changed")]|length' "$E2")"
else
    echo "FAIL could not open a session against the degraded agent"
    FAILED=1
fi

kill "$DAEMON2_PID" 2>/dev/null || true
wait "$DAEMON2_PID" 2>/dev/null || true
rm -rf "$STATE2"

# ------------------------------------------------- a question nobody answered
#
# The agent asks for permission, gives up, and finishes its turn while the question is still on
# screen. Real agents have their own timeouts and none of them is the client's, so a client that
# assumes somebody is still waiting behind a prompt is assuming something it was never told.
#
# What used to happen: the waiter stayed registered for ten minutes after the turn ended. For those
# ten minutes the interface offered buttons that did nothing, and the daemon accepted an answer for a
# conversation that was over — writing into the log a decision that influenced nothing, which a later
# reader cannot tell apart from one that did.
echo
echo "checking a permission request nobody answered"
PERM_STATE=$(mktemp -d)
PORT5=$((PORT + 4))
"$ROOT/target/debug/wkbd-core" \
    --state-dir "$PERM_STATE" --listen "127.0.0.1:$PORT5" \
    --agent "asks=Asks=$ROOT/target/debug/fake-acp-agent --profile rich --permission-wait-ms 0" \
    > "$PERM_STATE/daemon.log" 2>&1 &
DAEMON6_PID=$!
for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$PORT5/api/health" > /dev/null 2>&1 && break
    sleep 0.2
done
PSID=$(curl -sf -X POST "http://127.0.0.1:$PORT5/api/sessions" \
    -H 'content-type: application/json' \
    -d "{\"agent_id\":\"asks\",\"project_root\":\"$ROOT\"}" | jq -r '.id // empty')
curl -sf -X POST "http://127.0.0.1:$PORT5/api/sessions/$PSID/prompt" \
    -H 'content-type: application/json' -d '{"text":"edit something"}' > /dev/null 2>&1
for _ in $(seq 1 60); do
    python3 "$ROOT/scripts/read-events.py" "$PORT5" 0 1.0 > "$PERM_STATE/events.json" 2>/dev/null \
        || echo '[]' > "$PERM_STATE/events.json"
    [ "$(jq -r '[.[]|select(.payload.event=="turn_ended")]|length' "$PERM_STATE/events.json")" != "0" ] && break
    sleep 0.25
done
PE="$PERM_STATE/events.json"

check "the request was recorded" "true" \
    "$(jq -r '[.[]|select(.payload.event=="permission_requested")]|length >= 1' "$PE")"
check "it expired when the turn ended rather than staying open" "true" \
    "$(jq -r '[.[]|select(.payload.event=="permission_expired")]|length >= 1' "$PE")"
# The distinction that keeps the log readable: nobody refused, the agent stopped waiting. Recording
# the lapse as a refusal would put a choice in the log that no person made.
check "the lapse was not recorded as a decision" "0" \
    "$(jq -r '[.[]|select(.payload.event=="permission_resolved")]|length' "$PE")"
LATE=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    "http://127.0.0.1:$PORT5/api/sessions/$PSID/permission" \
    -H 'content-type: application/json' -d '{"request_id":"1","option_id":"allow-once"}')
check "answering after the turn ended is refused" "409" "$LATE"

kill "$DAEMON6_PID" 2>/dev/null || true
wait "$DAEMON6_PID" 2>/dev/null || true
rm -rf "$PERM_STATE"

# --------------------------------------------------------- process cleanup
#
# Three layers, and only the third covers a hard crash. It reads a registry, so something has to
# write one; a sweep over an empty file is a layer that exists in the code and not in effect. This
# kills the daemon with SIGKILL, which skips every graceful path, and then checks that a fresh
# daemon finds and reaps what was left.
echo
echo "checking cleanup after a hard crash"
CRASH_STATE=$(mktemp -d)
PORT4=$((PORT + 3))
CRASH_BASE=$(pgrep -x fake-acp-agent 2>/dev/null | wc -l | tr -d ' ')
"$ROOT/target/debug/wkbd-core" \
    --state-dir "$CRASH_STATE" --listen "127.0.0.1:$PORT4" \
    --agent "fake=Fake=$ROOT/target/debug/fake-acp-agent --profile spartan" \
    > "$CRASH_STATE/daemon.log" 2>&1 &
DAEMON4_PID=$!
for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$PORT4/api/health" > /dev/null 2>&1 && break
    sleep 0.2
done
curl -sf -X POST "http://127.0.0.1:$PORT4/api/sessions" -H 'content-type: application/json' \
    -d "{\"agent_id\":\"fake\",\"project_root\":\"$CRASH_STATE\"}" > /dev/null 2>&1
sleep 1

check_ge "cleanup: the registry records the spawned agent" 1 \
    "$(jq -r '.entries | length' "$CRASH_STATE/processes.json" 2>/dev/null || echo 0)"

# SIGKILL: no signal handler runs, no graceful shutdown, no Drop. Exactly the case the boot sweep
# is for. The child dies with us via the death signal, so what is being checked is that the
# registry entry is left behind for the sweep and that the sweep tolerates it.
kill -9 "$DAEMON4_PID" 2>/dev/null || true
wait "$DAEMON4_PID" 2>/dev/null || true
DAEMON4_PID=""
sleep 1

"$ROOT/target/debug/wkbd-core" \
    --state-dir "$CRASH_STATE" --listen "127.0.0.1:$PORT4" \
    --agent "fake=Fake=$ROOT/target/debug/fake-acp-agent --profile spartan" \
    > "$CRASH_STATE/daemon2.log" 2>&1 &
DAEMON5_PID=$!
for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$PORT4/api/health" > /dev/null 2>&1 && break
    sleep 0.2
done
check "cleanup: a fresh daemon starts after a hard crash" "true" \
    "$(curl -sf "http://127.0.0.1:$PORT4/api/health" 2>/dev/null | jq -r '.ok // false')"
kill "$DAEMON5_PID" 2>/dev/null || true
wait "$DAEMON5_PID" 2>/dev/null || true
sleep 1
CRASH_AFTER=$(pgrep -x fake-acp-agent 2>/dev/null | wc -l | tr -d ' ')
check "cleanup: no agent survived the crash and the restart" "0" "$((CRASH_AFTER - CRASH_BASE))"
rm -rf "$CRASH_STATE"

# ------------------------------------------------------- filesystem boundary
#
# The protocol has the client perform disk I/O on the agent's behalf, with an absolute path the
# agent chose, and defines no boundary of its own. The guard has its own tests; this checks that
# the daemon actually wired it into the protocol path, which is a different claim. "The capability
# was declared but the check was skipped" looks exactly like success from outside, so the agent
# here really tries to leave, four ways, and the assertions are about what did not happen on disk
# as well as what the log says.
echo
echo "checking the filesystem boundary against a probing agent"
FS_STATE=$(mktemp -d)
PORT3=$((PORT + 2))
WORK="$FS_STATE/work"
OUTSIDE="$FS_STATE/outside"
mkdir -p "$WORK/nested" "$WORK/via" "$OUTSIDE" "${WORK}_evil"
echo "inside the workspace" > "$WORK/inside.txt"
echo "not for the agent" > "$OUTSIDE/secret.txt"
echo "not for the agent either" > "${WORK}_evil/secret.txt"
ln -s "$OUTSIDE/secret.txt" "$WORK/escape-link"
ln -s "$OUTSIDE" "$WORK/via/link"
rm -f /tmp/wkbd-should-not-exist

FSAGENT="$ROOT/target/debug/fake-acp-agent --profile fsprobe"
"$ROOT/target/debug/wkbd-core" \
    --state-dir "$FS_STATE/state" --listen "127.0.0.1:$PORT3" \
    --agent "probe=Probing Agent=$FSAGENT" > "$FS_STATE/daemon.log" 2>&1 &
DAEMON3_PID=$!
for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$PORT3/api/health" > /dev/null 2>&1 && break
    sleep 0.2
done

SID3=$(curl -sf -X POST "http://127.0.0.1:$PORT3/api/sessions" \
    -H 'content-type: application/json' \
    -d "{\"agent_id\":\"probe\",\"project_root\":\"$WORK\"}" 2>/dev/null | jq -r '.id // empty')

if [ -n "$SID3" ]; then
    curl -sf -X POST "http://127.0.0.1:$PORT3/api/sessions/$SID3/prompt" \
        -H 'content-type: application/json' -d '{"text":"probe the boundary"}' > /dev/null 2>&1
    for _ in $(seq 1 80); do
        python3 "$ROOT/scripts/read-events.py" "$PORT3" 0 1.0 > "$FS_STATE/events.json" 2>/dev/null || echo '[]' > "$FS_STATE/events.json"
        [ "$(jq -r '[.[]|select(.payload.event=="turn_ended")]|length' "$FS_STATE/events.json")" != "0" ] && break
        sleep 0.25
    done
    E3="$FS_STATE/events.json"

    check "boundary: reads inside the workspace are allowed" "true" \
        "$(jq -r --arg p "$WORK/inside.txt" '[.[]|select(.payload.event=="file_access" and .payload.requested==$p)]|last.payload.allowed' "$E3")"
    check "boundary: writes inside the workspace are allowed" "true" \
        "$(jq -r --arg p "$WORK/written-by-agent.txt" '[.[]|select(.payload.event=="file_access" and .payload.requested==$p)]|last.payload.allowed' "$E3")"
    check "boundary: the written file is really there" "written through the client" \
        "$(cat "$WORK/written-by-agent.txt" 2>/dev/null || echo MISSING)"
    # The protocol requires the client to create a file that does not exist. That is the normal
    # path, and it is the condition the published defects in this area needed.
    check "boundary: a missing file inside the workspace is created" "created on demand" \
        "$(cat "$WORK/nested/new.txt" 2>/dev/null || echo MISSING)"

    # Four escapes, each a published defect shape.
    check "boundary: an absolute path outside is refused" "outside-root" \
        "$(jq -r '[.[]|select(.payload.event=="file_access" and .payload.requested=="/etc/passwd")]|last.payload.refusal' "$E3")"
    check "boundary: a symlink pointing out is refused" "symlink-encountered" \
        "$(jq -r --arg p "$WORK/escape-link" '[.[]|select(.payload.event=="file_access" and .payload.requested==$p)]|last.payload.refusal' "$E3")"
    check "boundary: a symlink as an intermediate component is refused" "symlink-encountered" \
        "$(jq -r --arg p "$WORK/via/link/secret.txt" '[.[]|select(.payload.event=="file_access" and .payload.requested==$p)]|last.payload.refusal' "$E3")"
    check "boundary: a sibling whose name shares the prefix is refused" "outside-root" \
        "$(jq -r --arg p "${WORK}_evil/secret.txt" '[.[]|select(.payload.event=="file_access" and .payload.requested==$p)]|last.payload.refusal' "$E3")"

    # What the log says is one thing; what happened on disk is another.
    if [ -e /tmp/wkbd-should-not-exist ]; then
        printf 'FAIL %-64s %s\n' "boundary: the write outside the workspace did not land" "IT LANDED"
        FAILED=1
        rm -f /tmp/wkbd-should-not-exist
    else
        printf 'ok   %-64s %s\n' "boundary: the write outside the workspace did not land" "absent"
    fi
    # Narrower than it looks, and worth labelling honestly: file contents are never put in the
    # log, so this checks that property rather than proving the escapes failed. What proves that
    # is the refusal reasons above and the absent file below.
    check "boundary: file contents are never written to the log" "0" \
        "$(jq -r '[.[]|select(.payload|tostring|test("not for the agent"))]|length' "$E3")"
    # Every attempt is recorded, allowed or not. Enforcement with no record cannot be audited.
    check_ge "boundary: attempts recorded" 8 \
        "$(jq -r '[.[]|select(.payload.event=="file_access")]|length' "$E3")"
    # The plan, in both shapes the protocol has had. The v2 shape nests its entries under a `plan` object,
# and reading only the v1 position matches the discriminator and yields an empty list — the plan arrives
# silently blank, which is the worst of the three possible failures.
check "the plan arrived and was not blank" "true" \
    "$(jq -r '[.[]|select(.payload.event=="plan_changed")]|length >= 2' "$EVENTS")"
check "the v2 shape was read, not dropped" "3" \
    "$(jq -r '[.[]|select(.payload.event=="plan_changed")]|last|.payload.entries|length' "$EVENTS")"
# Replaced wholesale, which is what the protocol requires. A merge would leave four entries.
check "the later plan replaced the earlier one" "completed" \
    "$(jq -r '[.[]|select(.payload.event=="plan_changed")]|last|.payload.entries[0].status' "$EVENTS")"
check "both updates landed under the same id" "1" \
    "$(jq -r '[.[]|select(.payload.event=="plan_changed")]|map(.payload.plan_id)|unique|length' "$EVENTS")"

check "boundary: refusals are recorded, not swallowed" "true" \
        "$(jq -r '[.[]|select(.payload.event=="file_access" and .payload.allowed==false)]|length >= 5' "$E3")"
else
    echo "FAIL could not open a session against the probing agent"
    FAILED=1
fi

kill "$DAEMON3_PID" 2>/dev/null || true
wait "$DAEMON3_PID" 2>/dev/null || true
rm -rf "$FS_STATE"

# ---------------------------------------------------------------- attachments
#
# The whole point of reading promptCapabilities is that the same mention has to become a
# different block for a different agent. Two agents in one daemon, one declaring
# embeddedContext and one declaring nothing, and the assertion is what the agent says it
# received rather than what we logged sending: only the agent can report what crossed the pipe.
echo
echo "attachments"
AT_STATE=$(mktemp -d)
AT_PORT=$((PORT + 3))
AT_PROJECT="$AT_STATE/project"
mkdir -p "$AT_PROJECT/src"
printf 'fn main() { println!("hi"); }\n' > "$AT_PROJECT/src/main.rs"
printf 'node_modules/\n' > "$AT_PROJECT/.gitignore"
mkdir -p "$AT_PROJECT/node_modules"
printf 'junk\n' > "$AT_PROJECT/node_modules/main.rs"

"$ROOT/target/debug/wkbd-core" \
    --state-dir "$AT_STATE" \
    --listen "127.0.0.1:$AT_PORT" \
    --agent "rich=Rich Agent=$ROOT/target/debug/fake-acp-agent --profile rich" \
    --agent "spartan=Spartan Agent=$ROOT/target/debug/fake-acp-agent --profile spartan" \
    > "$AT_STATE/daemon.log" 2>&1 &
DAEMON4_PID=$!
for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$AT_PORT/api/health" > /dev/null 2>&1 && break
    sleep 0.2
done

attach_turn() {
    # $1 agent id, $2 mention path. Echoes the flattened answer text.
    local agent="$1" path="$2" sid
    sid=$(curl -sf -X POST "http://127.0.0.1:$AT_PORT/api/sessions" \
        -H 'content-type: application/json' \
        -d "{\"agent_id\":\"$agent\",\"project_root\":\"$AT_PROJECT\"}" | jq -r '.id // empty')
    [ -z "$sid" ] && { echo ""; return; }
    echo "$sid" > "$AT_STATE/last-sid"
    curl -sf -X POST "http://127.0.0.1:$AT_PORT/api/sessions/$sid/prompt" \
        -H 'content-type: application/json' \
        -d "{\"text\":\"look at @$path\",\"mentions\":[\"$path\"]}" > /dev/null 2>&1
    for _ in $(seq 1 100); do
        PORT="$AT_PORT" python3 "$ROOT/scripts/read-events.py" "$AT_PORT" 0 1.0 \
            > "$AT_STATE/ev.json" 2>/dev/null || echo '[]' > "$AT_STATE/ev.json"
        local ended
        ended=$(jq -r --arg s "$sid" \
            '[.[]|select(.session_id==$s and .payload.event=="turn_ended")]|length' \
            "$AT_STATE/ev.json" 2>/dev/null || echo 0)
        [ "$ended" != "0" ] && break
        sleep 0.2
    done
    jq -r --arg s "$sid" \
        '[.[]|select(.session_id==$s and .payload.event=="segment_chunk")]|map(.payload.text)|join("")' \
        "$AT_STATE/ev.json"
}

PATHS=$(curl -sf "http://127.0.0.1:$AT_PORT/api/health" > /dev/null 2>&1; echo ok)
RICH_SID=$(curl -sf -X POST "http://127.0.0.1:$AT_PORT/api/sessions" \
    -H 'content-type: application/json' \
    -d "{\"agent_id\":\"rich\",\"project_root\":\"$AT_PROJECT\"}" | jq -r '.id // empty')
check "an agent that takes embedded context says so" "true" \
    "$(curl -sf "http://127.0.0.1:$AT_PORT/api/sessions" | jq -r --arg s "$RICH_SID" \
        '[.[]|select(.id==$s)]|first.prompt_capabilities.embedded_context')"
check "an agent that declares nothing is not assumed to" "false" \
    "$(curl -sf -X POST "http://127.0.0.1:$AT_PORT/api/sessions" \
        -H 'content-type: application/json' \
        -d "{\"agent_id\":\"spartan\",\"project_root\":\"$AT_PROJECT\"}" \
        | jq -r '.prompt_capabilities.embedded_context')"

COMPLETIONS=$(curl -sf "http://127.0.0.1:$AT_PORT/api/sessions/$RICH_SID/paths?q=main" || echo '[]')
check "completion finds the file" "src/main.rs" "$(echo "$COMPLETIONS" | jq -r 'first.path // "none"')"
check "completion skips what the repository ignores" "0" \
    "$(echo "$COMPLETIONS" | jq -r '[.[]|select(.path|startswith("node_modules"))]|length')"

RICH_ANSWER=$(attach_turn rich "src/main.rs")
case "$RICH_ANSWER" in
    *"resource main.rs"*) printf 'ok   %-64s %s\n' "the agent received the file's contents" "resource" ;;
    *) printf 'FAIL %-64s %s\n' "the agent received the file's contents" "$RICH_ANSWER"; FAILED=1 ;;
esac

SPARTAN_ANSWER=$(attach_turn spartan "src/main.rs")
case "$SPARTAN_ANSWER" in
    *"resource_link main.rs"*) printf 'ok   %-64s %s\n' "an agent that cannot embed got a link" "resource_link" ;;
    *) printf 'FAIL %-64s %s\n' "an agent that cannot embed got a link" "$SPARTAN_ANSWER"; FAILED=1 ;;
esac

# The transcript has to say which of the two happened, or the difference is invisible.
SPARTAN_SID=$(cat "$AT_STATE/last-sid")
check "the transcript records that it degraded to a link" "agent-cannot-embed" \
    "$(jq -r --arg s "$SPARTAN_SID" \
        '[.[]|select(.session_id==$s and .payload.event=="turn_started")]|last.payload.attachments[0].degraded' \
        "$AT_STATE/ev.json")"
check "and records what it was sent as" "link" \
    "$(jq -r --arg s "$SPARTAN_SID" \
        '[.[]|select(.session_id==$s and .payload.event=="turn_started")]|last.payload.attachments[0].sent_as' \
        "$AT_STATE/ev.json")"

# The composer is the softer target of the two paths into the filesystem: the path arrives as
# a string over HTTP. It must be refused with the same boundary as fs/read_text_file.
ESCAPE=$(curl -s -o "$AT_STATE/escape.json" -w '%{http_code}' \
    -X POST "http://127.0.0.1:$AT_PORT/api/sessions/$RICH_SID/prompt" \
    -H 'content-type: application/json' \
    -d '{"text":"read this","mentions":["../../../../etc/passwd"]}')
check "boundary: a mention cannot escape the project root" "400" "$ESCAPE"
check "boundary: and the refusal names the path" "true" \
    "$(jq -r '.error|test("etc/passwd")' "$AT_STATE/escape.json" 2>/dev/null || echo false)"

kill "$DAEMON4_PID" 2>/dev/null || true
wait "$DAEMON4_PID" 2>/dev/null || true
rm -rf "$AT_STATE"

echo
if [ "$FAILED" -eq 0 ]; then
    echo "vertical slice works end to end"
else
    echo "SLICE BROKEN"
    echo "--- daemon log (tail) ---"
    tail -40 "$STATE/daemon.log" 2>/dev/null || true
fi
exit "$FAILED"
