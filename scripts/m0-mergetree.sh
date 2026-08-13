#!/usr/bin/env bash
# Reproduces the git experiments recorded in docs/M0-FINDINGS.md sections 1 and 2.
#
# These are not unit tests: they measure git's own behaviour, which is why they live here
# rather than in a crate. Re-run this after a git upgrade. The behaviours it pins are
# load-bearing for wkbd-vcs and wkbd-sec, and two of them (the --stdin status inversion
# and directory-rename-split conflicts with disjoint paths) are counter-intuitive enough
# that a silent change would be easy to miss.
set -u

ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
FAILED=0

check() {
    local label="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        printf 'ok   %-58s %s\n' "$label" "$actual"
    else
        printf 'FAIL %-58s expected=%s actual=%s\n' "$label" "$expected" "$actual"
        FAILED=1
    fi
}

echo "git version: $(git --version)"
echo

# ---------------------------------------------------------------- fixture
cd "$ROOT"
git init -q repo && cd repo
git config user.email t@example.invalid && git config user.name t

printf 'line1\nline2\nline3\n' > a.txt
printf 'shared\n' > shared.txt
git add -A && git commit -qm base
BASE=$(git rev-parse HEAD)

git checkout -q -b X
printf 'line1-X\nline2\nline3\n' > a.txt && git commit -qam X
X=$(git rev-parse HEAD)

git checkout -q "$BASE" && git checkout -q -b Y
printf 'shared-Y\n' > shared.txt && git commit -qam Y
Y=$(git rev-parse HEAD)

git checkout -q "$BASE" && git checkout -q -b Z
printf 'line1-Z\nline2\nline3\n' > a.txt && git commit -qam Z
Z=$(git rev-parse HEAD)

# ---------------------------------------------------------------- 1.1 exit codes
git merge-tree --write-tree "$X" "$Y" >/dev/null 2>&1
check "clean merge exit code" 0 $?

git merge-tree --write-tree "$X" "$Z" >/dev/null 2>&1
check "conflicting merge exit code" 1 $?

# ---------------------------------------------------------------- 1.2 --stdin inversion
# The output is NUL-separated and bash command substitution silently drops NUL bytes, so
# the response has to go through a file. One pair per invocation keeps the parse to
# "first field of the first record", which is the only part being asserted.
stdin_status() {
    printf '%s %s\n' "$1" "$2" | git merge-tree --stdin > "$ROOT/stdin.bin"
    tr '\0' '\n' < "$ROOT/stdin.bin" | head -1
}
printf '%s %s\n%s %s\n' "$X" "$Y" "$X" "$Z" | git merge-tree --stdin > /dev/null
check "--stdin overall exit code (even with a conflict)" 0 $?
check "--stdin status for the CLEAN pair (1 means clean)" 1 "$(stdin_status "$X" "$Y")"
check "--stdin status for the CONFLICTING pair (0 means conflict)" 0 "$(stdin_status "$X" "$Z")"

# ---------------------------------------------------------------- 1.3 disjoint yet conflicting
cd "$ROOT" && git init -q r2 && cd r2
git config user.email t@example.invalid && git config user.name t
mkdir -p dirA && printf 'c1\n' > dirA/f1.txt && printf 'c2\n' > dirA/f2.txt
git add -A && git commit -qm base
B2=$(git rev-parse HEAD)

git checkout -q -b P && mkdir -p dirB dirC
git mv dirA/f1.txt dirB/f1.txt && git mv dirA/f2.txt dirC/f2.txt
git commit -qm split && P=$(git rev-parse HEAD)

git checkout -q "$B2" && git checkout -q -b Q
printf 'new\n' > dirA/f3.txt && git add -A && git commit -qm addnew
Q=$(git rev-parse HEAD)

OVERLAP=$(comm -12 \
    <(git diff --name-only "$B2" "$P" | sort) \
    <(git diff --name-only "$B2" "$Q" | sort) | wc -l)
check "P and Q touch disjoint paths" 0 "$OVERLAP"

git merge-tree --write-tree "$P" "$Q" >/dev/null 2>&1
check "disjoint paths STILL conflict (directory rename split)" 1 $?

# ---------------------------------------------------------------- 1.4 full mechanism
cd "$ROOT/repo"
NEWTREE=$(git merge-tree --write-tree "$X" "$Y")
NEWCOMMIT=$(git commit-tree "$NEWTREE" -m integration -p "$X" -p "$Y")
git worktree add -q -b task-B "$ROOT/wt-taskB" "$NEWCOMMIT"
check "dependency edge carries X's output" "line1-X" "$(head -1 "$ROOT/wt-taskB/a.txt")"
check "dependency edge carries Y's output" "shared-Y" "$(cat "$ROOT/wt-taskB/shared.txt")"

# ---------------------------------------------------------------- 2.1 post-checkout runs
mkdir -p .git/hooks
printf '#!/bin/sh\necho fired >> %s/hook.log\n' "$ROOT" > .git/hooks/post-checkout
chmod +x .git/hooks/post-checkout
rm -f "$ROOT/hook.log"
git worktree add -q -b hooktest "$ROOT/wt-hook" "$BASE"
check "worktree add executes post-checkout" "fired" "$(cat "$ROOT/hook.log" 2>/dev/null)"

# ...and that -c core.hooksPath=/dev/null is an effective defence
rm -f "$ROOT/hook.log"
git -c core.hooksPath=/dev/null worktree add -q -b hooktest2 "$ROOT/wt-hook2" "$BASE"
check "core.hooksPath=/dev/null suppresses it" "" "$(cat "$ROOT/hook.log" 2>/dev/null)"

# ---------------------------------------------------------------- 2.2 config is shared
(cd "$ROOT/wt-taskB" && git config --local core.pager 'INJECTED')
check "config written in one worktree is visible in main" "INJECTED" "$(git config --get core.pager)"
check "...and in a third worktree" "INJECTED" "$(cd "$ROOT/wt-hook" && git config --get core.pager)"
git config --unset core.pager

# ---------------------------------------------------------------- 2.3 stash is shared
(cd "$ROOT/wt-taskB" && echo dirty >> a.txt && git stash push -q -m from-taskB)
check "refs/stash is shared across worktrees" "1" "$(git stash list | wc -l)"
git stash drop -q 2>/dev/null || true

# ---------------------------------------------------------------- 2.4 branch reuse refused
if git worktree add -q "$ROOT/wt-dup" task-B 2>/dev/null; then
    check "checking out an already-checked-out branch is refused" "refused" "allowed"
else
    check "checking out an already-checked-out branch is refused" "refused" "refused"
fi

echo
if [ "$FAILED" -eq 0 ]; then
    echo "all git behaviour assumptions hold"
else
    echo "SOME ASSUMPTIONS NO LONGER HOLD -- wkbd-vcs and wkbd-sec need review"
fi
exit "$FAILED"
