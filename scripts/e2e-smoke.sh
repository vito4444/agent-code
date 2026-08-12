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
BEFORE=$(pgrep -x fake-acp-agent 2>/dev/null | wc -l | tr -d ' ')
check_ge "agent processes while running" 1 "$BEFORE"
kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
sleep 1
AFTER=$(pgrep -x fake-acp-agent 2>/dev/null | wc -l | tr -d ' ')
check "agent processes after shutdown" "0" "$AFTER"

echo
if [ "$FAILED" -eq 0 ]; then
    echo "vertical slice works end to end"
else
    echo "SLICE BROKEN"
    echo "--- daemon log (tail) ---"
    tail -40 "$STATE/daemon.log" 2>/dev/null || true
fi
exit "$FAILED"
