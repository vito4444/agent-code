#!/usr/bin/env bash
#
# Regenerates every screenshot in docs/reference/shell/.
#
# This exists because the set went stale twice. A screenshot of a product the code no longer produces
# is worse than no screenshot, because it is evidence for something untrue — and the discipline of
# remembering which ones a change invalidated does not survive contact with a change that touches the
# sidebar. So it is a command instead: run it after anything that alters the interface, and the whole
# set is current or the script failed.
#
# It needs an X display and a WebKitGTK build of the shell. On a machine with neither, the browser
# path serves the same interface — but these are shell screenshots on purpose, because they are
# evidence about the whole stack rather than about the interface in isolation.
#
# Usage: ./scripts/screenshots.sh [output-dir]

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/docs/reference/shell}"
DISPLAY_NUM="${WKBD_DISPLAY:-:1}"
PORT="${WKBD_PORT:-8787}"

for tool in git jq curl python3 xdotool xwininfo import convert; do
    command -v "$tool" > /dev/null || { echo "need $tool"; exit 1; }
done
DISPLAY="$DISPLAY_NUM" xdpyinfo > /dev/null 2>&1 || {
    echo "no X display at $DISPLAY_NUM"
    exit 1
}
[ -x "$ROOT/desktop/target/debug/wkbd-desktop" ] || {
    echo "build the shell first: cd desktop && cargo build"
    exit 1
}

WORK=$(mktemp -d)
SHELL_PID=""
cleanup() {
    [ -n "${SQUAT_PID:-}" ] && kill "$SQUAT_PID" 2> /dev/null
    [ -n "$SHELL_PID" ] && kill "$SHELL_PID" 2> /dev/null
    pkill -x wkbd-core 2> /dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

api() { curl -sf "http://127.0.0.1:$PORT$1" "${@:2}"; }

# Coordinates are in the captured image's own pixels, and the click has to be converted to them.
#
# `import -window` captures the client area, while `xdotool getwindowgeometry` reports the *frame* —
# which on this window manager sits 28 pixels higher because of the title bar. Clicking at the frame
# origin plus an image coordinate therefore lands a title bar's worth too low, and the symptom is
# selecting the row below the one you measured. `xwininfo` reports the client area's absolute position,
# which is the origin the capture is relative to, so that is what is used.
WID=""
find_window() {
    for _ in $(seq 1 60); do
        WID=$(DISPLAY="$DISPLAY_NUM" xdotool search --name '^Workbench$' | head -1)
        [ -n "$WID" ] && return 0
        sleep 0.5
    done
    return 1
}
raise() { DISPLAY="$DISPLAY_NUM" xdotool windowactivate --sync "$WID" 2> /dev/null; sleep 1; }
click() {
    DISPLAY="$DISPLAY_NUM" xdotool mousemove "$((WX + $1))" "$((WY + $2))"
    sleep 0.4
    DISPLAY="$DISPLAY_NUM" xdotool click 1
    sleep "${3:-2.5}"
}
shot() { DISPLAY="$DISPLAY_NUM" import -window "$WID" "$OUT/$1.png" && echo "  $1.png"; }

echo "building"
( cd "$ROOT/ui" && pnpm build > /dev/null 2>&1 )
cargo build -q -p wkbd-core -p fake-acp-agent 2>&1 | tail -3
rm -rf "$ROOT/desktop/target/debug/ui"
mkdir -p "$ROOT/desktop/target/debug/ui"
cp -r "$ROOT/ui/dist/"* "$ROOT/desktop/target/debug/ui/"
cp "$ROOT/target/debug/wkbd-core" "$ROOT/target/debug/fake-acp-agent" "$ROOT/desktop/target/debug/"

AG="$ROOT/desktop/target/debug/fake-acp-agent"

# A repository for a run to work on, and one for an agent to try to escape from.
REPO="$WORK/demo"
mkdir -p "$REPO/src" "$REPO/tests"
(
    cd "$REPO" && git init -q . \
        && git config user.email s@wkbd.invalid && git config user.name s
    cat > tests/check.sh <<'CHECK'
#!/bin/sh
ok() { echo "test $1 ... ok"; }
[ -f src/base.rs ] && ok unit::base_exists
[ -f NOTES.md ] && ok unit::notes_exist
if [ -f src/a.rs ] && [ -f src/base.rs ] && [ -f NOTES.md ]; then ok unit::a_sees_its_dependencies; fi
echo "test result: ok. done"
CHECK
    chmod +x tests/check.sh
    echo "# demo" > README.md
    git add -A && git commit -q -m initial
)

FS="$WORK/fs"
mkdir -p "$FS/nested" "$FS/via"
echo inside > "$FS/inside.txt"
ln -s /etc/hostname "$FS/escape-link"
ln -s /etc "$FS/via/link"

# A repository whose run fails, so a proposal is raised.
FAILREPO="$WORK/failing"
mkdir -p "$FAILREPO/src" "$FAILREPO/tests"
(
    cd "$FAILREPO" && git init -q . \
        && git config user.email f@wkbd.invalid && git config user.name f
    printf '#!/bin/sh\necho "test result: ok. done"\n' > tests/check.sh
    chmod +x tests/check.sh
    echo x > README.md
    git add -A && git commit -q -m initial
)

python3 - "$ROOT" "$WORK" <<'PY'
import sys
root, work = sys.argv[1], sys.argv[2]
s = open(f"{root}/scripts/e2e-orchestration.sh").read()
i = s.index("<<'PLAN'\n") + len("<<'PLAN'\n")
j = s.index("\nPLAN\n", i)
open(f"{work}/plan.json", "w").write(s[i:j])
open(f"{work}/failplan.json", "w").write("""
{"goal":"attempt something that cannot be accepted","tasks":[
 {"id":"impossible","title":"make a test pass that does not exist",
  "body":"Try.\\n\\nWRITE src/x.rs <<<// nothing\\n>>>",
  "declared_paths":["src/x.rs"],"depends_on":[],
  "verify":{"cmd":"sh tests/check.sh","must_pass":["unit::never_exists"],
            "immutable_paths":["tests/**"]}}]}
""")
PY

mkdir -p "$OUT"

# ---------------------------------------------------------------- the empty shell
#
# Started with no state so the first capture is the screen somebody actually sees first.
pkill -x wkbd-desktop 2> /dev/null
pkill -x wkbd-core 2> /dev/null
sleep 2
rm -rf "$HOME/.local/state/wkbd"

start_shell() {
    (
        cd "$ROOT/desktop"
        WEBKIT_DISABLE_COMPOSITING_MODE=1 WEBKIT_DISABLE_DMABUF_RENDERER=1 \
            LIBGL_ALWAYS_SOFTWARE=1 DISPLAY="$DISPLAY_NUM" \
            nohup ./target/debug/wkbd-desktop -- "$@" > "$WORK/shell.log" 2>&1 &
        echo $! > "$WORK/shell.pid"
    )
    sleep 24
    SHELL_PID=$(cat "$WORK/shell.pid")
    find_window || { echo "the shell window never appeared"; tail -20 "$WORK/shell.log"; exit 1; }
    local info
    info=$(DISPLAY="$DISPLAY_NUM" xwininfo -id "$WID")
    WX=$(echo "$info" | awk '/Absolute upper-left X/ {print $4}')
    WY=$(echo "$info" | awk '/Absolute upper-left Y/ {print $4}')
    [ -n "$WX" ] && [ -n "$WY" ] || { echo "could not read the client origin"; exit 1; }
    echo "  client origin: $WX,$WY"
    raise
}

echo "capturing"
start_shell \
    --agent "rich=Rich Agent=$AG --profile rich" \
    --agent "probe=Probing Agent=$AG --profile fsprobe" \
    --agent "worker=Worker=$AG --profile worker" \
    --worker-agent worker --fixed-plan "$WORK/plan.json"

shot paper-empty

# ---------------------------------------------------------------- a conversation
#
# The permission is answered, because the interesting capture is a finished turn. The unanswered case
# has its own assertions in the slice check.
S1=$(api /api/sessions -X POST -H 'content-type: application/json' \
    -d "{\"agent_id\":\"rich\",\"project_root\":\"$ROOT\"}" | jq -r .id)
api /api/sessions/"$S1"/prompt -X POST -H 'content-type: application/json' \
    -d '{"text":"refactor the loader and run the tests"}' > /dev/null
sleep 4
REQ=$(python3 "$ROOT/scripts/read-events.py" "$PORT" 0 1.5 2> /dev/null \
    | jq -r '[.[]|select(.payload.event=="permission_requested")]|last|.payload.request_id // "1"')
api /api/sessions/"$S1"/permission -X POST -H 'content-type: application/json' \
    -d "{\"request_id\":\"$REQ\",\"option_id\":\"allow-once\"}" > /dev/null
sleep 6
raise
shot paper-transcript

# ---------------------------------------------------------------- the file boundary
S2=$(api /api/sessions -X POST -H 'content-type: application/json' \
    -d "{\"agent_id\":\"probe\",\"project_root\":\"$FS\"}" | jq -r .id)
api /api/sessions/"$S2"/prompt -X POST -H 'content-type: application/json' \
    -d '{"text":"probe the boundary"}' > /dev/null
sleep 8
raise
# The newest session is selected automatically, so the probing agent's transcript is on screen.
shot file-boundary

# ---------------------------------------------------------------- a run
api /api/runs -X POST -H 'content-type: application/json' \
    -d "{\"goal\":\"add a base module, notes, and a feature that uses both\",\"project_root\":\"$REPO\"}" \
    > /dev/null
sleep 14
raise
shot run-in-progress

# Nav positions are read from the rendered window rather than assumed, because the footer grows an
# entry whenever a screen is added and every hard-coded offset then points one row off.
nav_y() {
    python3 - "$OUT/run-in-progress.png" "$1" <<'PY'
import subprocess, sys, re
png, label = sys.argv[1], sys.argv[2]
# The footer buttons are the only text in the left column below two thirds of the height. Their rows
# are found by looking for dark pixels in that strip and grouping them into bands.
out = subprocess.run(['convert', png, '-crop', '200x300+0+560', 'txt:-'],
                     capture_output=True, text=True).stdout
rows = {}
for line in out.splitlines()[1:]:
    m = re.match(r'(\d+),(\d+): \((\d+),(\d+),(\d+)', line)
    if not m:
        continue
    x, y, r, g, b = (int(v) for v in m.groups())
    if r < 120 and g < 120 and b < 120:
        rows.setdefault(y, 0)
        rows[y] += 1
bands, current = [], []
for y in sorted(rows):
    if current and y - current[-1] > 3:
        bands.append(current)
        current = []
    current.append(y)
if current:
    bands.append(current)
order = ['runs', 'proposals', 'rules', 'protocol']
try:
    band = bands[order.index(label)]
except (ValueError, IndexError):
    print(0)
else:
    print(560 + sum(band) // len(band))
PY
}

RUNS_Y=$(nav_y runs)
PROPOSALS_Y=$(nav_y proposals)
RULES_Y=$(nav_y rules)
# The sidebar with a run in flight: workers grouped under the run and named by task, which is what
# the grouping exists for and what the first version of it got wrong.
convert "$OUT/run-in-progress.png" -crop 210x560+0+0 -resize 200% "$OUT/sidebar-grouping.png"
rm -f "$OUT/run-in-progress.png"
echo "  sidebar-grouping.png"
if [ "${RUNS_Y:-0}" -lt 100 ]; then
    echo "could not locate the sidebar navigation; refusing to click blindly"
    exit 1
fi
echo "  nav: runs=$RUNS_Y proposals=$PROPOSALS_Y rules=$RULES_Y"

click 30 "$RUNS_Y"
shot run-list
# The run row sits below the start-a-run form.
click 600 200 3
shot run-view

# The two links on a task card are found by looking for them, not by counting rows. They share a row
# and a colour, so the row has to be split by x: taking the mean of the accent pixels lands between
# them and hits whichever is wider.
LINKS=$(python3 - "$OUT/run-view.png" <<'PYLINKS'
import subprocess, sys, re

out = subprocess.run(['convert', sys.argv[1], 'txt:-'], capture_output=True, text=True).stdout
rows = {}
for line in out.splitlines()[1:]:
    m = re.match(r'(\d+),(\d+): \((\d+),(\d+),(\d+)', line)
    if not m:
        continue
    x, y, r, g, b = (int(v) for v in m.groups())
    # The accent colour, which on this screen only the links and the merge button use.
    if 150 < r < 210 and 60 < g < 110 and 30 < b < 80:
        rows.setdefault(y, []).append(x)

# The first row wide enough to be an underline rather than stray antialiasing.
candidates = sorted((y, sorted(xs)) for y, xs in rows.items() if len(xs) > 40)
if not candidates:
    print("0 0 0")
    raise SystemExit

y, xs = candidates[0]
# Runs of adjacent x, which is what separates the two links sharing the row.
clusters, current = [], [xs[0]]
for x in xs[1:]:
    if x - current[-1] > 12:
        clusters.append(current)
        current = []
    current.append(x)
clusters.append(current)


def mid(c):
    return (c[0] + c[-1]) // 2


left = mid(clusters[0])
right = mid(clusters[-1]) if len(clusters) > 1 else 0
print(f"{left} {right} {y}")
PYLINKS
)
set -- $LINKS
LEFT_X=$1; RIGHT_X=$2; LINK_Y=$3
if [ "${LINK_Y:-0}" -gt 0 ]; then
    echo "  task links at y=$LINK_Y: diff=$LEFT_X transcript=$RIGHT_X"
    click "$LEFT_X" "$LINK_Y" 3
    shot review-task
    click 1211 29 2
    if [ "${RIGHT_X:-0}" -gt 0 ]; then
        click "$RIGHT_X" "$LINK_Y" 3
        shot worker-transcript
        click 30 "$RUNS_Y"
        click 600 200 3
    fi

    # The candidate's own diff is deliberately not captured here.
    #
    # It sits behind the merge gate, below the fold on a run with a rejected first draft, and three
    # attempts at getting there — a wheel event, a taller window, a scroll into view — each failed
    # silently in a way that produced a screenshot of the wrong screen. A capture this script cannot
    # regenerate is exactly the kind that goes stale, which is what this script exists to prevent.
    #
    # Nothing is lost. The claim the pair illustrated — that a task's diff excludes what its
    # dependencies produced, while the candidate contains every task's work — is asserted with actual
    # file counts in scripts/e2e-orchestration.sh, which is stronger evidence than a picture.
else
    echo "  (no task links found; skipping review-task.png)"
fi

# ---------------------------------------------------------------- the rules screen
click 30 "$RULES_Y"
shot user-rules

# ---------------------------------------------------------------- a proposal
#
# Its own daemon, because the proposal has to come from a run that failed and this one's succeeded.
kill "$SHELL_PID" 2> /dev/null
wait "$SHELL_PID" 2> /dev/null
pkill -x wkbd-core 2> /dev/null
sleep 3
rm -rf "$HOME/.local/state/wkbd"

start_shell \
    --agent "worker=Worker=$AG --profile worker" \
    --worker-agent worker --fixed-plan "$WORK/failplan.json"
api /api/runs -X POST -H 'content-type: application/json' \
    -d "{\"goal\":\"attempt the impossible\",\"project_root\":\"$FAILREPO\"}" > /dev/null
sleep 12
raise
click 30 "$PROPOSALS_Y"
shot proposal-queue
click 700 150 3
shot proposal-review

# ---------------------------------------------------------------- mid-thought
#
# The live state, which needs a slow agent and a capture taken while the turn is still running. Its own
# shell because the delay would make every other capture in this script wait for it.
kill "$SHELL_PID" 2> /dev/null
wait "$SHELL_PID" 2> /dev/null
pkill -x wkbd-core 2> /dev/null
sleep 3
rm -rf "$HOME/.local/state/wkbd"

start_shell --agent "slow=Slow Agent=$AG --profile rich --delay-ms 2500"
SS=$(api /api/sessions -X POST -H 'content-type: application/json' \
    -d "{\"agent_id\":\"slow\",\"project_root\":\"$ROOT\"}" | jq -r .id)
api /api/sessions/"$SS"/prompt -X POST -H 'content-type: application/json' \
    -d '{"text":"refactor the loader and run the tests"}' > /dev/null
# Timed to land inside the first thought, which is the only moment the live band exists: the newest
# segment is expanded, its node pulses, and the label reads "Thinking" rather than "Thought process".
sleep 4
raise
shot paper-thinking

# ---------------------------------------------------------------- the port already taken
#
# A daemon this shell did not start, holding the port. The readiness check used to ask only whether
# something answered, so it would adopt the stranger — and every symptom of that is indirect: agents
# that were configured are missing, a flag has no effect, the interface is a version behind.
kill "$SHELL_PID" 2> /dev/null
wait "$SHELL_PID" 2> /dev/null
pkill -x wkbd-core 2> /dev/null
sleep 3

"$ROOT/target/debug/wkbd-core" --state-dir "$WORK/squat" \
    --listen "127.0.0.1:$PORT" > "$WORK/squat.log" 2>&1 &
SQUAT_PID=$!
sleep 4
start_shell
shot port-taken
kill "$SQUAT_PID" 2> /dev/null


echo
echo "wrote $(ls "$OUT"/*.png | wc -l) screenshots to $OUT"
