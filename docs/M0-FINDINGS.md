# Milestone 0 findings

Everything in this file was produced by running commands on this machine, not by reading
documentation. Where a claim could only be established from documentation, it says so.

Environment: Ubuntu 24.04.4, kernel 6.12.94, git 2.43.0, Rust 1.97.1, Node 22.14.0.
Date: 2026-08-12.

---

## 1. `git merge-tree` semantics (m0-mergetree)

All eleven experiments were run against throwaway repositories under `/tmp/mt`. The
reproduction script is `scripts/m0-mergetree.sh`.

### 1.1 Exit codes are the conflict signal

```
$ git merge-tree --write-tree $X $Y      # disjoint edits
exit_code=0
a4c4594b43441f2490609f8ede859e851e00cdd2

$ git merge-tree --write-tree $X $Z      # both edit a.txt line 1
exit_code=1
16bfaa5142c493c197f01888dbb52f4e6c0b2756
100644 83db48f84ec878fbfb30b46d16630e944e34f205 1	a.txt
100644 27fb719d66abcd2457b29b32db07a387225e36d3 2	a.txt
100644 31a54bb5bc8a3ca1cf63ff4e8bf5a8575d7ff459 3	a.txt

Auto-merging a.txt
CONFLICT (content): Merge conflict in a.txt
```

Confirmed: `0` = clean, `1` = conflict. The tree OID is printed in both cases, so the
presence of a tree says nothing about success.

### 1.2 `--stdin` inverts the meaning of the status number

This is the trap flagged during research, and it is real. Feeding two pairs — the first
known-clean, the second known-conflicting:

```
$ printf "%s %s\n%s %s\n" $X $Y $X $Z | git merge-tree --stdin
overall exit_code=0
1<NUL>a4c4594b...        <- first pair, the CLEAN one, reports 1
0<NUL>16bfaa51...        <- second pair, the CONFLICTING one, reports 0
```

So in `--stdin` mode: **`1` means the merge was clean and `0` means it had conflicts**,
which is the opposite of the process exit code in single-pair mode. The overall process
exit code is `0` even though one of the merges conflicted.

Consequence for us: the VCS layer must never share a "did it conflict" helper between the
two modes. `wkbd-vcs` uses single-pair mode only, and the `--stdin` batch path is
deliberately not implemented until we have a measured need for it.

### 1.3 Disjoint file sets can still conflict

This disproves "task A and task B touch different files, therefore they can run in
parallel and merge cleanly", which was an assumption in the original brief.

Setup: branch `P` splits `dirA/` into `dirB/` and `dirC/`; branch `Q` adds a brand-new
file `dirA/f3.txt`.

```
### P changed:  dirB/f1.txt  dirC/f2.txt
### Q changed:  dirA/f3.txt
### intersection: (empty)

$ git merge-tree --write-tree $P $Q
exit_code=1
2f3f9f389c27f6bcf97898812bb6daa365927ce2

CONFLICT (directory rename split): Unclear where to rename dirA to; it was renamed to
multiple other directories, with no destination getting a majority of the files.
```

Exit code 1, and the "Conflicted file info" section is **empty** — there is no individual
conflicting file to point at. Any scheduler that decides parallelism by intersecting
declared path sets will call this pair safe and be wrong.

Consequence: path ownership is a scheduling heuristic only. `merge-tree` is the only
authority on whether two results combine. Implemented in
`wkbd-vcs::merge::predict_merge`.

### 1.4 The full dependency-edge mechanism works

```
$ NEWTREE=$(git merge-tree --write-tree $X $Y)          # rc=0
$ NEWCOMMIT=$(git commit-tree $NEWTREE -m "integration for task-B" -p $X -p $Y)
$ git worktree add -b task-B /tmp/mt/wt-taskB $NEWCOMMIT
$ cat /tmp/mt/wt-taskB/a.txt      -> line1-X     (from branch X)
$ cat /tmp/mt/wt-taskB/shared.txt -> shared-Y    (from branch Y)
```

Both dependencies' outputs are present in the dependent task's working tree, and no
existing working tree was touched at any point. This is the mechanism the orchestrator
uses so that a dependency edge actually transports data instead of only transporting a
sentence in a prompt.

---

## 2. `git worktree` is not isolation (m0-mergetree, experiments 6–11)

Four properties were measured, all of which constrain the design.

### 2.1 `git worktree add` executes `post-checkout`

```
$ cat .git/hooks/post-checkout
#!/bin/sh
echo "[HOOK EXECUTED] ..." >> /tmp/mt/hook_fired.log

$ git worktree add -b hooktest /tmp/mt/wt-hook $BASE
$ cat /tmp/mt/hook_fired.log
[HOOK EXECUTED] post-checkout pid=9798 cwd=/tmp/mt/wt-hook
```

Creating a worktree is our most frequent privileged operation, and it runs whatever is in
the shared hooks directory. This is why `.git` must be outside the agents' write boundary
and why every git invocation we make passes `-c core.hooksPath=/dev/null`.

### 2.2 `.git/config` is shared across all worktrees

```
$ cd wt-taskB && git config --local core.pager 'THIS-IS-INJECTED'
$ cd repo    && git config --get core.pager   -> THIS-IS-INJECTED
$ cd wt-hook && git config --get core.pager   -> THIS-IS-INJECTED
$ git rev-parse --git-common-dir              -> /tmp/mt/repo/.git
```

One agent writing one config key compromises every other agent and the main checkout.
`core.pager` is documented as "meant to be interpreted by the shell", so this is arbitrary
code execution against the user, triggered the next time anything paginates.

### 2.3 `refs/stash` is shared

```
$ cd wt-taskB && git stash push -m "from-taskB"
$ cd repo    && git stash list -> stash@{0}: On task-B: from-taskB
$ cd wt-hook && git stash list -> stash@{0}: On task-B: from-taskB
```

Agents must be prevented from using `git stash`; concurrent use corrupts other agents'
work. Enforced by `wkbd-sec::git_env` denylist.

### 2.4 The hooks directory is shared, and branch checkout is refused

```
$ cd wt-taskB && git rev-parse --git-path hooks -> /tmp/mt/repo/.git/hooks
$ git worktree add /tmp/mt/wt-dup task-B
fatal: 'task-B' is already used by worktree at '/tmp/mt/wt-taskB'
```

The refusal is useful: it is git enforcing, on our behalf, the rule that a branch checked
out in one worktree must not be rewritten from another.

`git worktree list --porcelain` output format confirmed as the stable parsing target.

---

## 3. ACP capability matrix (m0-acp-matrix)

**Status: partially blocked. Do not read the missing rows as "the agent does not do this".**

The probe tool is built (`crates/acp-probe`) and verified against the reference agent in
`crates/fake-acp-agent`. What it can establish without credentials is the `initialize`
half of the matrix: protocol version, agent capabilities, auth methods. What requires
credentials is everything that needs a real prompt turn — whether `messageId` is present,
how many thought segments a turn actually produces, whether `configOptions` are offered,
whether `usage_update` is ever sent.

This machine has no agent CLI installed and no model API credentials
(`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` are all absent; the only
credentials present belong to the Cursor harness itself). The blocked rows can be filled
by running `acp-probe` on a developer machine that has the CLIs and keys; the command is
in `docs/HANDOFF.md`.

Because the matrix is incomplete, the code treats every capability as absent until
observed. That is the correct default regardless — but it means the UI's degraded paths
are currently the *tested* paths and the rich paths are the untested ones.

---

## 4. Linux WebKitGTK stress spike (m0-linux-spike)

**Status: cannot be performed on this machine, and a weaker substitute would be
misleading.**

The risk being tested is GPU- and compositor-specific: WebGL-heavy views degrading or
crashing under WebKitGTK, particularly on NVIDIA and Wayland. This machine has no GPU, no
Wayland session, and no `webkit2gtk` installed (`pkg-config --modversion webkit2gtk-4.1`
fails; `ldconfig -p | grep webkit` returns nothing). Only `Xvfb` software rendering on
`DISPLAY=:1` is available.

Running the spike under Xvfb would exercise the software rasterizer, which is precisely
the configuration that does *not* reproduce the failure mode. A green result would be
worthless and actively misleading, so it was not run.

Two things were done instead:

1. The browser path is a first-class target rather than a fallback bolted on later. The
   daemon serves the UI over HTTP + WebSocket, and the UI is developed and tested against
   Chrome. If WebKitGTK proves unusable on a given Linux setup, the product still works
   there with no code change.
2. The terminal renderer selection is explicit and defaults to `canvas` rather than
   `webgl` on Linux, so the risky path is opt-in. See `ui/src/lib/renderer.ts`.

The spike itself remains open and is listed in `docs/HANDOFF.md` as required before any
claim that the Tauri shell is usable on Linux.

---

## 5. Copilot screenshot archive (m0-screenshots)

**Status: not obtained. The interface spec is built on source rather than pixels.**

Fetching tooling on this machine strips images, so no screenshots were captured. The
numbers in `docs/UI-SPEC.md` therefore come from the MIT-licensed `microsoft/vscode`
source (`src/vs/workbench/contrib/chat/`) and from versioned release notes, which is a
stronger source for exact values than measuring a screenshot would be.

It is a weaker source for one specific thing: the left-to-right ordering and iconography
of the controls inside the chat input box. `docs/UI-SPEC.md` marks those items as
unverified, and the implementation keeps the footer row order in a single constant
(`ui/src/components/composer/footerOrder.ts`) so it can be corrected in one place once
screenshots exist.
