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
# Not concurrent with the other scripts here. Every one of them kills daemons by name, so two
# running at once take each other's processes down — which surfaces as a section that connects for
# its first few assertions and then reports every remaining one against an empty string.
# Usage: ./scripts/screenshots.sh [output-dir]
#
# WKBD_ONLY=paper-transcript,run-view publishes just those, which exists because the full set takes
# five and a half minutes and a review loop that costs that much per look is a loop nobody runs
# twice. Everything still happens — the runs, the clicks, the navigation, and every capture — because
# a screenshot taken from a state the script did not reach is the kind of evidence this script exists
# to stop producing. What the filter skips is only the copy into the published directory.
#
# Every shot is taken, and taken into a scratch directory, because some of them are inputs: the
# navigation rows are found by measuring `run-in-progress.png` and `sidebar-grouping.png` is a crop
# of it. The first version of this filter skipped the write instead, and the next step then failed
# trying to read a file that no longer existed.

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
ONLY="${WKBD_ONLY:-}"
# Where every capture lands. Publishing is a separate step, so a filtered run still leaves the
# intermediate images the later measurements read.
SHOTS=""
shot() {
    DISPLAY="$DISPLAY_NUM" import -window "$WID" "$SHOTS/$1.png" || return 1
    publish "$1"
}

publish() {
    if [ -n "$ONLY" ] && ! printf '%s' ",$ONLY," | grep -q ",$1,"; then
        return 0
    fi
    cp "$SHOTS/$1.png" "$OUT/$1.png" && echo "  $1.png"
}

# The first clickable row on a list screen, found rather than assumed.
#
# The run list and the proposal queue both put one bordered row below a heading, and both were reached
# by a hard-coded offset until two captures came out byte-identical: the coordinate was measured before
# the title-bar correction, so the click landed in dead space and the "after" screenshot was the
# "before" one. Two files with the same hash is what caught it, which is a poor substitute for not
# guessing in the first place.
# The newest session in the sidebar, found rather than assumed.
#
# This exists because the assumption was wrong and the screenshots were lying about it. The
# script used to open a session over the API and capture immediately, with a comment saying the
# newest session is selected automatically. It is not: the interface only auto-selects when
# nothing is selected yet, which is correct — taking the view away from somebody reading a
# transcript because a run started a worker elsewhere would be worse. So `file-boundary.png`
# was a picture of the previous conversation for as long as that comment was there.
#
# Sessions are the only rows in the sidebar between the filter box and the nav footer, so the
# bottom-most band of text in that column is the newest one.
last_session_y() {
    DISPLAY="$DISPLAY_NUM" import -window "$WID" "$WORK/side.png"
    python3 - "$WORK/side.png" <<'PYSIDE'
import subprocess, sys, re

TOP, BOTTOM, LEFT, WIDTH = 95, 520, 16, 180
out = subprocess.run(
    ['convert', sys.argv[1], '-crop', f'{WIDTH}x{BOTTOM - TOP}+{LEFT}+{TOP}', 'txt:-'],
    capture_output=True, text=True).stdout

rows = {}
for line in out.splitlines()[1:]:
    m = re.match(r'(\d+),(\d+): \((\d+),(\d+),(\d+)', line)
    if not m:
        continue
    _, y, r, g, b = (int(v) for v in m.groups())
    # Text, not the paper background and not the muted grey of a project heading.
    if r < 110 and g < 110 and b < 110:
        rows[y] = rows.get(y, 0) + 1

bands, current = [], []
for y in sorted(rows):
    if current and y - current[-1] > 4:
        bands.append(current)
        current = []
    current.append(y)
if current:
    bands.append(current)

print(TOP + sum(bands[-1]) // len(bands[-1]) if bands else 0)
PYSIDE
}

first_row_y() {
    DISPLAY="$DISPLAY_NUM" import -window "$WID" "$WORK/probe.png"
    python3 - "$WORK/probe.png" <<'PYROW'
import subprocess, sys, re

out = subprocess.run(['convert', sys.argv[1], '-crop', '800x500+240+100', 'txt:-'],
                     capture_output=True, text=True).stdout
rows = {}
for line in out.splitlines()[1:]:
    m = re.match(r'(\d+),(\d+): \((\d+),(\d+),(\d+)', line)
    if not m:
        continue
    x, y, r, g, b = (int(v) for v in m.groups())
    # The border colour, #ddd8cd, which on these screens draws the row outlines.
    if abs(r - 221) < 12 and abs(g - 216) < 12 and abs(b - 205) < 12:
        rows.setdefault(y, 0)
        rows[y] += 1

# A row outline is a long horizontal run. The first one is the top edge of the first row; clicking a
# little below it lands inside.
edges = sorted(y for y, n in rows.items() if n > 500)
print(100 + edges[0] + 20 if edges else 0)
PYROW
}


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
SHOTS="$WORK/shots"
mkdir -p "$SHOTS"

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

# ---------------------------------------------------------------- one conversation, three captures
#
# All three from one session, which is the point rather than an economy. The earlier version opened
# a second session for the attachment capture and selected it by clicking the last sidebar row —
# and both rows read "Rich Agent", so the moment the list ordered them the other way the capture
# was of the wrong conversation and nothing said so. One session cannot be ambiguous.
#
# Permissions are answered because the interesting captures are finished turns. The unanswered case
# has its own assertions in the slice check.
S1=$(api /api/sessions -X POST -H 'content-type: application/json' \
    -d "{\"agent_id\":\"rich\",\"project_root\":\"$ROOT\"}" | jq -r .id)

# Answers the permission this session's turn is waiting on, whichever turn that is.
answer_permission() {
    sleep 4
    local req
    req=$(python3 "$ROOT/scripts/read-events.py" "$PORT" 0 1.5 2> /dev/null \
        | jq -r --arg s "$S1" \
          '[.[]|select(.session_id==$s and .payload.event=="permission_requested")]|last|.payload.request_id // "1"')
    api /api/sessions/"$S1"/permission -X POST -H 'content-type: application/json' \
        -d "{\"request_id\":\"$req\",\"option_id\":\"allow-once\"}" > /dev/null
    sleep 6
}

# First turn: the one with attachments, so its prompt is at the top of the record where the strip
# can be photographed. A file and a directory, so the strip has to show both outcomes — one the
# agent was handed, one it was only pointed at.
api /api/sessions/"$S1"/prompt -X POST -H 'content-type: application/json' \
    -d '{"text":"where is the one-live-segment invariant enforced?",
         "mentions":["crates/wkbd-proto/src/normalize.rs","crates/wkbd-sec"]}' > /dev/null
answer_permission
raise
# Scrolled back to the prompt, which a finished turn has pushed off the top. That also puts the way
# back on screen, which is the other half of following a conversation and only exists in this state.
DISPLAY="$DISPLAY_NUM" xdotool mousemove "$((WX + 600))" "$((WY + 300))"
DISPLAY="$DISPLAY_NUM" xdotool click --repeat 25 --delay 30 4
sleep 1.5
shot paper-attachment

# The picker, typed into the same box. A live control, so the only way to know it renders where
# the caret is, is to put a caret there.
#
# The composer is anchored to the bottom of the window, so it is clicked relative to that edge
# rather than found: measuring it the way the row finder measures lists would mean picking the
# box's own border out of the footer's.
WH=$(DISPLAY="$DISPLAY_NUM" xwininfo -id "$WID" | awk '/Height:/ {print $2}')
WW=$(DISPLAY="$DISPLAY_NUM" xwininfo -id "$WID" | awk '/Width:/ {print $2}')
click $((WW / 2)) $((WH - 120)) 1
DISPLAY="$DISPLAY_NUM" xdotool type --delay 60 'compare @norm'
sleep 2
raise
shot mention-picker
DISPLAY="$DISPLAY_NUM" xdotool key --clearmodifiers ctrl+a
DISPLAY="$DISPLAY_NUM" xdotool key --clearmodifiers BackSpace

# A second turn, so the finished-conversation capture shows the record following its newest
# content rather than sitting where the last screenshot left it.
api /api/sessions/"$S1"/prompt -X POST -H 'content-type: application/json' \
    -d '{"text":"refactor the loader and run the tests"}' > /dev/null
answer_permission
raise
# Back to the live edge first. The previous capture scrolled up, and the record correctly stayed
# where it was put rather than following the new turn — which is the behaviour, and which left
# this capture showing the turn before the one it is a capture of.
DISPLAY="$DISPLAY_NUM" xdotool mousemove "$((WX + 600))" "$((WY + 300))"
DISPLAY="$DISPLAY_NUM" xdotool click --repeat 40 --delay 20 5
sleep 1.5
shot paper-transcript

# ---------------------------------------------------------------- the file boundary
S2=$(api /api/sessions -X POST -H 'content-type: application/json' \
    -d "{\"agent_id\":\"probe\",\"project_root\":\"$FS\"}" | jq -r .id)
api /api/sessions/"$S2"/prompt -X POST -H 'content-type: application/json' \
    -d '{"text":"probe the boundary"}' > /dev/null
sleep 8
raise
click 100 "$(last_session_y)" 2
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
    python3 - "$SHOTS/run-in-progress.png" "$1" <<'PY'
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
order = ['runs', 'proposals', 'rules', 'protocol', 'settings']
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
PROTOCOL_Y=$(nav_y protocol)
SETTINGS_Y=$(nav_y settings)
# The sidebar with a run in flight: workers grouped under the run and named by task, which is what
# the grouping exists for and what the first version of it got wrong.
# A crop rather than a capture: the sidebar is 200 px of a 1024 px window, and at that size the
# grouping this illustrates is unreadable. The full-window shot it comes from is scratch.
convert "$SHOTS/run-in-progress.png" -crop 210x560+0+0 -resize 200% "$SHOTS/sidebar-grouping.png"
publish sidebar-grouping
rm -f "$OUT/run-in-progress.png"
echo "  sidebar-grouping.png"
if [ "${RUNS_Y:-0}" -lt 100 ]; then
    echo "could not locate the sidebar navigation; refusing to click blindly"
    exit 1
fi
echo "  nav: runs=$RUNS_Y proposals=$PROPOSALS_Y rules=$RULES_Y protocol=$PROTOCOL_Y settings=$SETTINGS_Y"

click 30 "$RUNS_Y"
shot run-list
# The run row is found by its status badge rather than by its border. A border scan finds the Project
# field above it — which is what happened, and the evidence was a screenshot of the run list with that
# field focused. The badge colour appears nowhere else on the screen.
RUN_ROW=$(DISPLAY="$DISPLAY_NUM" import -window "$WID" "$WORK/runs.png" && python3 - "$WORK/runs.png" <<'PYRUN'
import subprocess, sys, re
out = subprocess.run(['convert', sys.argv[1], 'txt:-'], capture_output=True, text=True).stdout
rows = {}
for line in out.splitlines()[1:]:
    m = re.match(r'(\d+),(\d+): \((\d+),(\d+),(\d+)', line)
    if not m:
        continue
    x, y, r, g, b = (int(v) for v in m.groups())
    # The attention foreground, #8a6320, which on this screen only a run's status badge uses.
    if abs(r - 138) < 25 and abs(g - 99) < 25 and abs(b - 32) < 30:
        rows.setdefault(y, 0)
        rows[y] += 1
hits = sorted(y for y, n in rows.items() if n > 8)
print(hits[0] if hits else 0)
PYRUN
)
[ "${RUN_ROW:-0}" -gt 0 ] || { echo "could not find the run row"; exit 1; }
echo "  run row at y=$RUN_ROW"
click 600 "$RUN_ROW" 3
shot run-view

# The two links on a task card are found by looking for them, not by counting rows. They share a row
# and a colour, so the row has to be split by x: taking the mean of the accent pixels lands between
# them and hits whichever is wider.
LINKS=$(python3 - "$SHOTS/run-view.png" <<'PYLINKS'
import subprocess, sys, re

out = subprocess.run(['convert', sys.argv[1], 'txt:-'], capture_output=True, text=True).stdout
rows = {}
for line in out.splitlines()[1:]:
    m = re.match(r'(\d+),(\d+): \((\d+),(\d+),(\d+)', line)
    if not m:
        continue
    x, y, r, g, b = (int(v) for v in m.groups())
    # The main column only. The sidebar marks its selected row with a left border in the same
    # accent colour, and once the sidebar grew run groupings that border started landing on the
    # same row as a task card's links — where it became the leftmost cluster, so the click went
    # into the sidebar and two captures came out byte-identical.
    if x < 230:
        continue
    # The accent colour, which in this column only the links and the merge button use.
    if 150 < r < 210 and 60 < g < 110 and 30 < b < 80:
        rows.setdefault(y, []).append(x)

def cluster(xs):
    out, current = [], [xs[0]]
    for x in xs[1:]:
        if x - current[-1] > 12:
            out.append(current)
            current = []
        current.append(x)
    out.append(current)
    return out


# The row with *two* underlines, which is what a task card has and nothing else on this screen does.
# Picking the first wide row instead finds "All runs" at the top, which is also an accent link — the
# earlier version did exactly that and produced a screenshot of the run list.
candidates = []
for y, xs in rows.items():
    if len(xs) < 40:
        continue
    cs = cluster(sorted(xs))
    if len(cs) >= 2:
        candidates.append((y, cs))
candidates.sort()
if not candidates:
    print("0 0 0")
    raise SystemExit

y, clusters = candidates[0]


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
    DISPLAY="$DISPLAY_NUM" xdotool key Escape
    sleep 2
    if [ "${RIGHT_X:-0}" -gt 0 ]; then
        click "$RIGHT_X" "$LINK_Y" 3
        shot worker-transcript
        click 30 "$RUNS_Y"
        click 600 "$RUN_ROW" 3
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

# ---------------------------------------------------------------- the protocol log
#
# The last screen with no capture, which is the reason to take one: a screen nobody has looked at
# is where a defect sits undisturbed. It is also the one that answers "what did the agent actually
# say", so it is the screen a protocol argument gets settled on.
if [ "${PROTOCOL_Y:-0}" -gt 100 ]; then
    click 30 "$PROTOCOL_Y"
    shot protocol-log
else
    echo "  (could not locate the protocol log; skipping protocol-log.png)"
fi

# ---------------------------------------------------------------- settings
if [ "${SETTINGS_Y:-0}" -gt 100 ]; then
    click 30 "$SETTINGS_Y"
    shot settings
else
    echo "  (could not locate settings; skipping settings.png)"
fi

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
PROP_ROW=$(first_row_y)
[ "${PROP_ROW:-0}" -gt 0 ] || { echo "could not find the proposal row"; exit 1; }
click 700 "$PROP_ROW" 3
shot proposal-review

# ---------------------------------------------------------------- a distilled procedure
#
# Its own daemon and three runs, because the gate is three separate occasions: sibling tasks inside
# one run are one planner getting one decision right, and counting them as three would let a single
# run promote a coincidence into a procedure. Nothing here is merged in between — learning happens
# when a run reaches its candidate, and whether a person accepts it says nothing about which
# acceptance checks passed.
kill "$SHELL_PID" 2> /dev/null
wait "$SHELL_PID" 2> /dev/null
pkill -x wkbd-core 2> /dev/null
sleep 3
rm -rf "$HOME/.local/state/wkbd"

DISTREPO="$WORK/distrepo"
mkdir -p "$DISTREPO/src" "$DISTREPO/tests"
cat > "$DISTREPO/tests/check.sh" <<'CHECK'
#!/bin/sh
[ -f src/mod.rs ] && [ -f NOTES.md ] && echo "test unit::x ... ok"
echo "test result: ok. done"
CHECK
(
    cd "$DISTREPO" && git init -q . && git config user.email t@e && git config user.name t
    git add -A && git commit -q -m initial
) > /dev/null 2>&1
python3 - "$WORK/distplan.json" <<'DISTPLAN'
import json, sys
plan = {"goal": "add a helper and note it", "tasks": [{
    "id": "helper", "title": "add the helper and note it",
    "body": "WRITE src/mod.rs <<<pub fn f() {}\n>>>\nWRITE NOTES.md <<<added f\n>>>",
    "declared_paths": ["src/mod.rs", "NOTES.md"], "depends_on": [],
    "verify": {"cmd": "sh tests/check.sh", "must_pass": ["unit::x"],
               "immutable_paths": ["tests/**"]}}]}
open(sys.argv[1], "w").write(json.dumps([plan] * 3))
DISTPLAN

start_shell \
    --agent "worker=Worker=$AG --profile worker" \
    --worker-agent worker --fixed-plan "$WORK/distplan.json"
for n in 1 2 3; do
    api /api/runs -X POST -H 'content-type: application/json' \
        -d "{\"goal\":\"add a helper ($n)\",\"project_root\":\"$DISTREPO\"}" > /dev/null
    sleep 7
done
raise
click 30 "$PROPOSALS_Y"
sleep 2
DIST_ROW=$(first_row_y)
if [ "${DIST_ROW:-0}" -gt 0 ]; then
    click 700 "$DIST_ROW" 3
    shot distilled-workflow
else
    echo "  (no distilled proposal found; skipping distilled-workflow.png)"
fi

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
