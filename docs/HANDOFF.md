# Handover

**This document may be out of date. The code is the only authority.** Where they disagree, believe
the code and fix this file.

Everything below is either something the code does, or something measured on a machine and
recorded with the command that measured it. Claims that were not verified are marked as such, and
there is a list of them at the end.

---

## 1. What this is

A local desktop workbench for running several AI coding agents. Four pillars:

1. **A real chat surface per agent**, built on structured protocol events rather than scraped
   terminal output, so it is written once and works for every agent.
2. **An orchestrator** that turns one sentence into a task graph, runs it in real isolation,
   accepts results by a machine-checkable criterion, and merges them. The model appears at two
   points only: drafting the graph, and replanning when a deterministic predicate says the graph
   was wrong.
3. **Cross-session memory** with two time axes, corrected by superseding rather than deleting.
4. **Self-improvement** as itemized notes with counters, merged by ordinary code, gated by an
   approval bound to content.

## 2. Shape

```
crates/
  wkbd-proto      ACP-adjacent types, the internal event model, turn segmentation
  wkbd-store      SQLite: one writer, many readers, migrations, blobs, boot state
  wkbd-vcs        git: merge prediction, worktrees, ownership, immutable paths
  wkbd-sec        path guard, git environment, process reaping, permissions, text sanitizing
  wkbd-agent      ACP client, process pool, session factory, capability probe
  wkbd-memory     rules, bi-temporal facts, retrieval, prelude construction
  wkbd-orch       task graph, durable execution, scheduling, acceptance
  wkbd-evolve     playbook, proposals, contextual bandit routing, distillation
  wkbd-core       the daemon: HTTP, WebSocket, the turn runner
  fake-acp-agent  a scriptable agent, used as a test double
  acp-probe       measures what an agent actually does
ui/               React interface, served by the daemon
scripts/          reproducible measurements and the end-to-end slice check
docs/
```

Only `wkbd-proto` depends on the protocol crate. That keeps "what breaks when the protocol
changes" answerable by reading one crate.

## 3. Running it

```bash
cargo build -p wkbd-core -p fake-acp-agent
cd ui && pnpm install && pnpm build && cd ..

./target/debug/wkbd-core \
  --state-dir /tmp/wkbd \
  --listen 127.0.0.1:8787 \
  --ui-dir ui/dist \
  --agent 'fake=Fake Agent=./target/debug/fake-acp-agent --profile rich --live-config true'
```

Then open <http://127.0.0.1:8787>.

Interface development against the real daemon:

```bash
cd ui && pnpm dev     # proxies /api to 127.0.0.1:8787, WebSocket included
```

### Checks

```bash
cargo test --workspace              # 299 tests
cd ui && pnpm vitest run            # 153 tests
./scripts/m0-mergetree.sh           # 15 assertions about git's own behaviour
./scripts/e2e-smoke.sh              # 55 assertions, the conversation slice
./scripts/e2e-orchestration.sh      # 70 assertions, one run start to finish
```

The two end-to-end checks are the ones to run before believing anything works. Every defect in
section 5 was found by them and by nothing else — including four that every unit test in the
repository passes through without noticing.

The conversation check exercises three agents, not one. The first declares everything the protocol allows; the second
declares none of it — no `messageId`, no config options, no usage — from a separate daemon and a
cold start. That second pass is the one most users will actually be on, so the assertions include
that segmentation still splits correctly without message ids, that every boundary is marked as
inferred, and that neither config options nor usage are invented.

The third really tries to escape its workspace, four ways: an absolute path outside the roots, a
symlink inside pointing out, a symlink as an intermediate component, and a sibling directory whose
name begins with the root's name. The guard has its own tests; this is a different claim — that the
daemon wired it into the protocol path — and "the capability was declared but the check was
skipped" looks exactly like success from outside. Alongside the refusal reasons it asserts that the
attempted write outside the workspace is absent from disk afterwards. Bypassing the guard in
`fs_bridge` turns five of those red, and the failure reads `the write outside the workspace did not
land: IT LANDED` rather than an assertion diff.

There is also a hard-crash pass: `SIGKILL` the daemon, which skips every graceful path, then start
a fresh one and check that nothing survived. It exists because the boot-side sweep reads a process
registry, and until recently nothing wrote one — a cleanup layer present in the code and absent in
effect.

The orchestration check runs one goal to a merged commit against a repository it creates. Two of its
assertions carry most of the weight, and both are written so that the interesting failure is a
real-world outcome rather than a mismatched string.

The first is that a dependency edge transports work. The dependent task's acceptance script passes
only when its own file *and* both dependencies' files are present in its worktree, so an edge that
merely summarised the dependencies into a prompt fails at acceptance. Making every task start from
the base commit instead of from its dependencies' turns that red with `the dependent task's
acceptance passed: false` — the acceptance check finds the missing work, which is the property being
claimed.

The second is that nothing merges without a person. Making a verified run merge itself turns two
assertions red, one of which is `the project's branch has not moved`, compared against the commit
recorded before the run started.

The first graph the check feeds in is deliberately invalid — one task connected to nothing in a graph
that has edges — so the redraft path runs too. A replan loop that is never exercised is a replan loop
that does not work.

There is also a cancellation pass, because a button labelled "cancel" that leaves agents running
reports something untrue. It cancels a run only once tasks are really dispatched, and asserts that the
run says it is cancelled *and* says what it interrupted. Two of its assertions are labelled as
following from the interruption rather than from the ordering checks, and that is not modesty: a
mutation run neutralising every `is_cancelled` check left both green. Those checks are deliberately
uncovered — a wave is dispatched in a tight loop so the per-task check rarely wins the race, and
observing the wave-boundary check needs a cancel landing after wave 1 succeeds and before wave 2
starts. A test built on winning that race would be flaky in both directions, and a flaky test guarding
a cancellation path is worse than an uncovered one.

It then runs a second, deliberately failing goal, because the safety rail around self-modification is
only real if something can reach it. A clean run proposing nothing shows the queue is not noisy; it
does not show the queue works. So the second run raises a proposal, and the assertions are that the
proposal names the run that produced it, that approving it against a hash the reviewer was not shown
is refused and leaves it pending, and that approving against the hash that *was* shown succeeds.
Removing the hash comparison turns the first two red — an approval that does not say what it approved
would otherwise be accepted.

## 4. The decisions worth knowing

### 4.1 The protocol supports runtime model switching. It just does not call it that.

There is no `session/set_model`. There is `session/set_config_option`, and
`SessionConfigOptionCategory` has the constants `model`, `model_config` and `thought_level`. An
agent declares its options in the `session/new` response; the client renders whatever it was
given and calls `set_config_option` to change one.

Verified against `schema/v1/schema.json` at the time of writing. Every appearance of "model" in
that file is either prose or one of those two category constants.

Consequences, in the code:

- The process pool key is `(agent id, fingerprint of every launch-time setting)`. Options the
  agent can change at runtime are excluded from the fingerprint, so an agent that supports
  switching keeps one process while an agent that does not gets one per configuration. Keying by
  agent alone works until the day a model selector is added, at which point changing the model
  restarts the one process every session is sharing.
- Whether an option is live cannot be read off the wire. There is no field for it. It comes from
  the capability probe, and until the probe has run the answer is "no", because assuming
  otherwise produces a control that appears to work and changes nothing.
- The interface hardcodes no model name.

### 4.2 There is no mid-turn injection, so there is no steer button

`session/prompt` cannot be interrupted except by cancelling, which discards in-flight work and
surfaces unfinished tool calls as cancelled. The proposal to add injection is open, unlabelled and
has no owner.

So the send control offers queueing and stopping. Queueing is honest — the message never leaves
this process — and the interface says when it will be delivered. A steer button appears only if an
agent declares support, which none does.

The only authoritative end of a turn is the `session/prompt` response returning. Nothing infers it
from the stream going quiet: "the model paused between tool calls" and "the turn is over" look
identical from outside.

### 4.3 The context ring is absent rather than estimated

`usage_update` is optional, and the specification says an agent that cannot give a meaningful
window size should send nothing rather than a null — the protocol declines to define an unknown
state. Most agents send nothing.

A client cannot compute it either: we cannot see the agent's system prompt, its loaded rule files,
its tool schemas, or whether it just compacted. So the choice is the agent's number or nothing,
and the code takes nothing.

### 4.4 One segment is live at a time, by construction

A real agent thinks several times per turn, between tool calls. `Normalizer` closes the open
segment before opening another, so at most one is ever live, and the interface can then be
"expand iff live" with no further logic.

Grouping is by `messageId` where the agent sends one. In v1 that field is optional, so the
fallback is that any interleaving update of a different kind closes the open segment. A turn
segmented that way is flagged, and the interface says the boundary was inferred.

`max_concurrent_live` in `wkbd-proto` asserts the invariant over a payload stream. It is used by
the unit tests, by the capability probe against real agents, and by the slice check.

### 4.5 Declared file ownership cannot decide parallelism

Measured, not reasoned. Two branches whose changed paths do not intersect at all still fail to
merge when one splits a directory in two and the other adds a file to the original:

```
$ git merge-tree --write-tree $SPLIT $ADD
exit_code=1
CONFLICT (directory rename split): Unclear where to rename dirA to; ...
```

Exit code 1 and the conflicted-files section is empty — there is no file to point at. So declared
paths order work and nothing else; `merge-tree` decides whether results combine. Ownership is
still checked afterwards, because a task that wandered outside its declaration has invalidated
the reasoning the schedule was built on.

Reproduce with `./scripts/m0-mergetree.sh`.

### 4.6 A dependency edge carries the work, not a description of it

A dependent task's worktree starts from a real merge commit of its dependencies' results, produced
by `merge-tree` and `commit-tree` without touching any working tree. Its files already contain
what the dependencies produced.

The alternative, which the well-known frameworks do, is to put a summary of the dependency's
output into the dependent's prompt. That edge transports a description of the work.

### 4.7 git worktrees are not isolation

All measured on this machine:

- `git worktree add` **runs `post-checkout`** from the shared hooks directory. Creating worktrees
  is our most frequent privileged operation.
- `.git/config` is **shared**. One agent setting `core.pager` — documented as "meant to be
  interpreted by the shell" — compromises every other worktree and the main checkout.
- `refs/stash` is **shared**. Agents must not use `git stash`.
- A branch checked out in one worktree **cannot** be checked out in another. That refusal is
  useful: it is git enforcing the rule that a branch in use elsewhere must not be rewritten.

Hence: `.git` is outside the agents' write boundary, every git invocation passes
`-c core.hooksPath=/dev/null`, and the environment for a git child is constructed from a
whitelist rather than inherited and trimmed. The last one matters because
`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n` are *command scope*, which git treats as protected
configuration — whoever controls a child's environment controls settings git trusts more than
anything in the repository.

### 4.8 Path boundaries are decided in the kernel

`fs/read_text_file` and `fs/write_text_file` have the client perform real disk I/O on the agent's
behalf, with an absolute path, and the specification requires the client to create a file that
does not exist. It defines no path boundary at all.

`wkbd-sec::path_guard` uses `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
RESOLVE_NO_MAGICLINKS` where available, so the decision happens in the kernel with no window
between check and use, and falls back to walking from a root descriptor with `O_NOFOLLOW` per
component elsewhere. **A resolution failure always refuses.** Never a fallback to comparing
strings: that same defect has shipped in four separate products, and the shape is always
"resolution failed, so fall back to a prefix check".

### 4.9 Approval binds to content, not to a name

A remembered permission decision is keyed on `(tool kind, SHA-256 of the canonical decision
bytes)`. One changed byte in a command, an argument or a path invalidates it.

Binding approval to a name is the defect behind a real vulnerability where an approved entry could
be swapped for a different payload without re-prompting. Regression test:
`one_changed_byte_after_approval_stops_the_apply`.

Anything the system writes for its own future consumption is a proposal that does nothing until
approved, and the approval records the hash that was shown to the human. At apply time the hash is
recomputed and compared. Body text is stripped of invisible characters before display, with the
strippings reported, because hidden Unicode is invisible in review interfaces — so the human and
the agent would otherwise be reading different content.

### 4.10 Nothing in memory is deleted

Facts carry event time (`valid_at` / `invalid_at`) and system time (`created_ms` / `expired_ms`).
A correction closes the old fact on both axes and points it at its replacement. `believed_at`
answers what we thought at any past moment. Deleting would make "we were wrong" and "we never
knew" indistinguishable, and those call for different responses.

The model decides only which of a supplied candidate list a new fact contradicts. All interval
arithmetic is code. The candidate list is narrowed to the same subject and predicate *before* the
model sees it, so a model that claims everything conflicts cannot retire an unrelated fact, and an
invented index retires nothing. Both are tested.

### 4.11 User rules are not memory, in three places

1. The extractor's output type has no variant for a user rule. It cannot name one.
2. `facts.kind` has a CHECK constraint, and only `rules.rs` writes `'user_rule'`.
3. Every consolidation, supersession and decay query filters `kind = 'inferred'`.

One layer would do if nobody edited this again. The failure being prevented is specific: a
preference a model can rewrite, or that decay can retire, is a setting the program has stopped
honouring while the settings screen still shows it as enabled.

In the prelude, rules come first, under their own heading, verbatim, with no confidence
annotation, followed by memories under a heading that says which wins on conflict. A rule is an
instruction; a memory is evidence. Flattened into one list a model weighs them the same way, and
an instruction loses to a longer pile of evidence.

### 4.12 One place opens sessions

`SessionFactory::open` is the only way to get a `SessionHandle`, and `SessionPurpose` enumerates
every route: new chat, resume, relaunch to apply a launch-only setting, orchestrator worker,
replay. `every_session_purpose_receives_the_prelude` iterates all of them.

This is not tidiness. Rules are injected here, and "the rules mysteriously do not apply in this
one situation" is what happens when a new entry point is added and the injection is not.

### 4.13 Acceptance needs assertions, and the tests are not writable

A `VerifySpec` has `must_pass` and `must_still_pass`. A spec with a command and no assertions is
rejected at validation: it can only report that it ran. Every existing tool in this space stores
acceptance as prose, which is why they all stop at human review.

Anti-tamper, cheapest first: hash and lock the immutable paths, restore them from the snapshot
immediately before running, take the command from the task graph rather than the working tree, and
reproduce the result from the start commit plus the recorded patch. The verification cache key
includes the patch hash, because a harness keyed only on a run identifier will replay a previous
verdict for a different patch.

One leak worth naming: repository history is readable from inside a worktree and refs are shared
across worktrees. If an answer exists in a branch or a reflog, an agent can find it without
touching a test file.

### 4.13b Proposal terminal states are distinguishable

`applied` and `voided` are separate states. Both previously retired to `superseded`, which
folded a security-relevant event into a housekeeping one: a proposal whose bytes changed after
approval is the exact shape of a known vulnerability, and it has to be visible as itself rather
than as "something newer came along". Migration 5 recreates the table to widen the constraint,
which is the only way to do it in SQLite, and a test asserts every column survives the copy.

### 4.14 The playbook has no rewrite operation

Deltas are `Add`, `Update`, `Deprecate`. There is no variant for replacing the document, because
letting a model rewrite accumulated context has a measured failure mode: it compresses to a short
summary and accuracy drops below the no-adaptation baseline. Merging is deterministic, non-model
code. Bullets carry helpful and harmful counters and a TTL — pruning only by harmful count has no
time dimension, so a note that was right about a codebase that has since changed never ages out.

Bullets whose source is outside the project never reach the injection path.

### 4.15 Routing is a cost control, not a quality improvement

LinUCB rather than Thompson sampling: a deterministic score interacts more predictably with a
budget penalty. Geometric forgetting at γ=0.997 handles suppliers silently changing a model
behind an API. Staleness inflates variance for arms nobody picks, with a ceiling — without one the
exploration bonus grows without bound and eventually overwhelms any finite cost penalty, driving
the system toward whatever is most expensive and most stale. The forgetting rate and the
exploration coefficient have to be calibrated together, because forgetting pushes the scatter
matrix toward singular and that inflates the confidence bonus.

The feature vector is stored at decision time, so a reward that arrives days later — was the pull
request merged, was it reverted — can still update the model.

The published gains for routing are systematically overstated; a benchmark across 400k instances
found most methods collapsing to similar performance and deployed commercial routers failing to
beat a simple baseline. So the objective is the cheapest arm that meets a quality target, not the
best arm.

### 4.16 Sequence numbers come from the database

`events.seq` is `INTEGER PRIMARY KEY AUTOINCREMENT`, allocated inside the write transaction. An
in-memory counter restarts at zero after a crash and the resulting primary-key collision leaves
the application unable to open at all. `AUTOINCREMENT` rather than a bare rowid, because a bare
rowid reuses the ids of deleted rows and a reused id in an append-only log means a reconnecting
client silently misses or duplicates events.

It guarantees monotonicity, **not contiguity**. A rolled-back insert leaves a permanent hole, so
consumers resume from the returned high-water mark and never treat a gap as loss.

### 4.17 The startup path cannot make the application unopenable

- A failed migration leaves the store read-only and reports why, rather than refusing to start.
  A workbench that will not open cannot be used to rescue the work inside it.
- `VACUUM INTO` writes a backup before any schema change.
- The boot counter lives in a separate file, not in the database. A counter inside the thing that
  is corrupt gets erased by the reset meant to rescue it. Three consecutive launches that never
  reach serving stop restoring session state; five also stop launching agents.
- Orphan sweeping matches process id **and** executable name. Matching the id alone eventually
  kills whatever unrelated process inherited that number.

---

## 5. Defects the slice check found, and nothing else did

Recorded because each is a class rather than an incident.

**A permission request deadlocked by construction.** The request event was persisted only after
the answer arrived, but a client learns there is something to answer *from that event*. Client
waited for the event; daemon waited for the client. Now: register the waiter, announce the
request, and wait on a separate task. The unit tests missed it because the harness answered from
the options list without needing the event.

**Responses overtook notifications.** A response goes straight to whoever awaits it; a
notification crosses the pool channel and a dispatcher. The shorter path won, and the turn ended
before the final answer and last tool result were processed — intermittently, depending on
scheduling. Now each inbound line carries its read position and the connection reports how far it
has dispatched, so the turn waits on a known quantity rather than a timer. **This appeared three
times** — in the tests, in the capability probe, and in the daemon — which is why the turn loop
now exists in exactly one place.

**The daemon ignored SIGTERM.** It listened only for interrupt, so every process manager skipped
all cleanup and agent processes outlived the workbench. Also: killing without reaping leaves a
zombie that still answers to the agent's name, so a leak check reports a leak that is real enough
to matter.

**Config options never reached the interface.** They were held in memory on the session handle and
never written to the event log, so the interface saw an empty list and correctly drew no model
selector. The feature looked absent rather than broken, which is exactly the kind of gap that
survives review. Now asserted in the slice check.

**The event batcher threw `Illegal invocation`.** `requestAnimationFrame` was passed as a bare
default parameter, losing its binding to `window`.

### Found by looking at screenshots of the running interface

Three, and all three were invisible to every test in the repository at the time.

**Files touched for an agent were rendered nowhere.** The events were recorded, the audit list held
them, and the transcript showed nothing. For an agent that edits through the protocol's file
methods — the path the daemon encourages, because it is the only one that is bounded and logged —
that means a turn containing a thought, an answer, and no sign that a file changed. Refusals were
invisible too, which is the worse half: the boundary was enforced and the person watching was told
nothing. The end-to-end assertion that reads "refusals are recorded, not swallowed" was accurate
about the log and said nothing about the interface, and it has been renamed to say so.

The fix needed three layers, and finding that out took two rounds: adding the item to the Rust view
builder changed nothing on screen, because the interface folds events with its own reducer and the
Rust builder is a mirror. Rendering it changed nothing either, because nothing produced the item.
The layer that decides what is on screen is `store.ts`, and it is the layer that had no test.

**Two counts for one change, side by side.** A tool card header read `+5 −3` directly above a diff
reading `+4 −2`. The header approximated with a common-prefix heuristic while the body ran a real
line diff, so a line shared at the *end* of both versions — a closing brace — was counted as an
addition and a deletion at once by one and as context by the other. They now share one function.
The first test written for this could not fail: it asserted the card's text contained `+4`, and the
card contains the header *and* the body, so the correct number was always present somewhere in it.
It now compares the two elements to each other.

**Three identical rows for three different pieces of work.** Orchestrated workers are rooted at
`worktrees/<run>/<task>`, and the sidebar truncated the path from the right — which is where the
distinguishing part is. All three read `/home/me/.local/state/wkbd/worktre…`.

### Found by comparing against other clients

**The v2 plan update was parsed as an empty plan.** ACP v1 flattens a plan's `entries` onto the
update; v2 removed that variant and replaced it with `plan_update`, which nests them under a `plan`
object carrying a `planId`. This read only the v1 position while accepting both discriminators, so a
v2 agent's plan matched, produced no entries, and reported no unknown update — the plan arrived
silently blank, which is the worst of the three possible failures. Both shapes are read now, plans are
keyed by id because v2 lets an agent keep several at once, and an unrecognised priority or status is
kept verbatim rather than mapped onto a default: the spec reserves plain names for future versions and
requires custom ones to begin with `_`, so an unfamiliar value means a newer agent, and calling it
`pending` would state something the agent did not.

**The plan was never rendered.** It had been arriving, being stored, and being dropped since the first
day. An agent that reports a plan is saying the shape of the work before it does it, which is the one
thing that makes a long turn readable while it is still running. It sits above the composer rather
than above the transcript, for the same reason the context ring is in the composer: progress is what
somebody glances at between two messages, so it belongs on the path their eye takes back to the input.

### Found by writing the script that regenerates the screenshots

**Routing treated every configured agent as a candidate worker.** Configuring three agents — one for
orchestrated work and two for interactive chat — made a run fail, because the bandit sent tasks to two
agents nobody had nominated for the job. An agent being present is not an agent volunteering.
`--worker-agent` is repeatable now, the nominated set is the routing arms, and a name that was never
configured is refused at startup rather than at the first worktree. Found by starting the daemon the
way the screenshot script needed to, which no test did.

**Clicks derived from a captured coordinate landed a title bar too low.** `xdotool
getwindowgeometry` reports the window *frame* and `import -window` captures the *client area*, and on
this window manager those differ by 28 pixels. Every attempt at driving the interface by measuring a
screenshot and clicking the measurement selected the row below the one measured, which is the source of
a run of "the click did nothing" confusion earlier in this project — including one thing reported as
unverified that was simply mis-clicked. `xwininfo` reports the client origin, which is what the capture
is relative to.

### Found by using it in the desktop rather than reading about it

**Approving a proposal recorded the approval and never ran it.** `apply` existed, was tested, and had
no caller. Clicking Approve emptied the queue and set the row to `approved`, and nothing happened —
which from outside is indistinguishable from the loop working, and is worse than not having the queue
at all, because it looks closed. Approval and application are one request now. This was found by
clicking the button and then reading the database, not by reading the code.

**Task branch names were not scoped to the run.** They were `wkbd/<task id>`, and task ids come from a
planner reading a goal, so the common ones — `docs`, `tests`, `base-module` — recur across runs. A
second run against the same repository died at its first worktree with a git error about an existing
branch: during setup, which is nowhere anybody would look for a planning problem. Branches carry the
run now. Found by running a second goal against a repository that had already had one, which no test
did.

**The review body was serialised JSON.** The stored body is the payload, so a reviewer had to dig one
sentence of English out of a line of `{"payload":"playbook","deltas":[...]}` — which is the shape of an
approval given to something nobody read. It renders as sentences now, with the exact bytes one
disclosure away, because the hash covers the bytes and not the rendering.

### Found by looking at screenshots a second time

**A permission request outlived the turn that asked it.** The waiter stayed registered for its own
ten-minute timeout after the turn ended, so the interface offered buttons for a question the agent
had stopped waiting behind, and the daemon accepted an answer for a conversation that was over —
writing into the log a decision that influenced nothing, which a later reader cannot tell apart from
one that did. Outstanding requests are now closed out when the turn ends, a late answer is refused
with 409, and the lapse is its own event rather than a refusal: nobody refused, the agent stopped
waiting, and recording a choice no person made is the entry hardest to question afterwards.

**Grouping the sidebar by project made it worse for the case it was added for.** An orchestrated
worker's project *is* its own worktree, so grouping by directory produced one group per worker, each
headed by a path ending in the run's uuid — three headings of noise around one row each, and three
rows all reading "Worker". Workers are grouped by their run now and labelled by their task.

**Six of the eleven archived screenshots showed a product the code no longer produced.** They were
taken before the theme changed. A screenshot that is out of date is worse than no screenshot, because
it is evidence for something untrue; they have all been retaken.

### Found by looking at the running shell

**The shell would adopt a daemon it did not start.** The port is fixed, so a stale daemon holding it
makes the new one fail to bind and exit — after which the readiness check, which only asked whether
*something* answered, passed against a process the shell cannot configure or restart. Every symptom is
indirect: agents that were configured are missing, a flag has no effect, the interface is a version
behind. `/api/health` reports its pid now and the shell compares it, and the failure page says which
of the two things went wrong rather than always blaming startup.

**The diagnostic page was mojibake.** Rendered from a `data:` URL with no charset, so the browser
decoded UTF-8 bytes as latin-1 and every em dash arrived as three characters of noise. A page that
looks corrupted is one the reader stops trusting halfway through.

**A session created anywhere else stayed invisible.** The session list was fetched once at startup,
which is right for the common case and wrong for every other one: the daemon serves more than one
client, and the orchestrator opens sessions of its own. Those workers' transcripts are the only record
of what a task actually did, which makes them the worst thing to hide. The interface now refetches
when the stream mentions a session it has not heard of.

### Found by connecting the orchestrator

Four more, and three of them needed two agent processes running at once — which is what the
orchestrator does and what no single-session test creates.

**Session ids were keyed globally.** They are assigned by the agent, and the protocol says nothing
about them being unique beyond one conversation; in practice an agent numbers them from one per
process, so two processes of the same agent both call their first session `session-1`. Keyed on the
id alone, the second registration silently replaced the first and every message from both processes
was delivered to whichever registered last. What that looked like from outside was one task's file
write being refused for leaving a workspace it had never approached. The map is now keyed on
`(process, session id)`.

**A turn waited on its own progress.** To decide it had received everything preceding its response,
a turn compared its own highest processed read position against the response's. On a process serving
several sessions the lines in between belong to somebody else and never arrive, so every turn paid
the full ten-second timeout. The connection now carries a delivery watermark, advanced *after* a
message reaches its destination inbox — before it, and a waiter would conclude it had a message
still in flight.

**Protecting the tests looked like changing them.** `lock_paths` assigned `0o444` and `unlock_paths`
assigned `0o644`, which permanently dropped the executable bit — from the file most likely to be
protected, the one that runs the tests. Git records that bit, so the chmod appeared in `git diff`,
and the ownership check then reported every task as having modified a file it never touched. Two of
our own mechanisms fighting, with the blame landing on a third party. Both functions now adjust bits
rather than assigning modes.

**A file could not be created in a new directory.** The write was refused because the parent did not
exist, and the protocol has no method for creating one — so an agent could not put a file anywhere
new, and what it would do instead is the write itself, outside anything we can check. Directories
are now created through the guard with `mkdirat` against the descriptor above, so no path is ever
resolved and then used by name. A test confirms a symlink out of the root is still refused and that
nothing appears outside it.

---

## 6. What is not done

### Blocked on this machine, not on the code

- **The capability matrix is half measured.** `acp-probe` works and is verified against the test
  double, but there is no agent CLI and no model credential here, so only the `initialize` and
  `session/new` half was obtained. Everything about a real turn — whether `messageId` is present,
  how many thought segments a turn produces, whether usage is reported, whether
  `set_config_option` is honoured — needs credentials. Run:

  ```bash
  ./target/debug/acp-probe \
    --agent 'claude=npx @agentclientprotocol/claude-agent-acp' \
    --agent 'codex=codex acp' \
    --agent 'gemini=gemini --acp' \
    --agent 'opencode=opencode acp' \
    --agent 'cursor=cursor-agent acp' \
    --prompt 'read the README and tell me what this project does' \
    --json > docs/capability-matrix.json
  ```

  Until then every capability defaults to absent, which is the right default but means the
  degraded paths are the tested ones and the rich paths are not.

- **The Linux WebKitGTK stress spike was not run.** Part of the reasoning here was wrong and is
  corrected in `docs/reference/shell/README.md`: `webkit2gtk` *is* available, and the shell runs —
  there are screenshots. What has not been done is the part the spike was for. The reported failures
  are GPU and compositor specific, and this machine has no GPU and no Wayland session, so everything
  observed here ran under software rendering with compositing disabled. That is precisely the
  configuration in which the failure does not reproduce, so "it worked here" is not evidence that it
  works on a real desktop, and reporting it as such would be worse than reporting nothing. Still
  required before claiming the shell is usable on Linux generally. Mitigation already in place: the
  browser path is first class — the daemon serves the interface itself, so Chrome is a working escape
  route rather than a rewrite — and the terminal renderer defaults to canvas rather than WebGL.

### Not started

- ~~**The Tauri shell.**~~ Written and running. See `docs/reference/shell/` for screenshots taken on
  this machine and for two claims in this document that turned out to be wrong — including the one
  that said it could not be built here.
- ~~**A full-screen review surface.**~~ Built, and it closes two things at once: a task card could
  say a task passed and give no way to see what it did, and the inline diffs pointed at a review
  surface that did not exist. A task's diff is taken from its own starting commit, so a dependent
  task's excludes whatever its dependencies produced — the question at a task card is what *that*
  task did. The merge gate reaches the same surface for the whole candidate, above the buttons rather
  than beside them, because the gate's argument is that acceptance is not the same as the change being
  wanted and the only way to decide the second is to read it.
- **Terminal support.** `terminal/*` is refused. Embedded terminal content reports that rather
  than showing output.
- **Attachments come from the project tree only.** Typing `@` completes files and directories under
  the session's project root, and each resolves against what the agent declared it can be handed —
  contents where `embeddedContext` was advertised, an image or audio block where those were, a
  resource link otherwise, with the reason recorded when it degrades. What is *not* there is
  anything from outside that tree: pasting a screenshot off the clipboard, dragging a file in from
  the desktop, or fetching a URL. All three mean bytes arriving that no path guard has ruled on,
  which needs a decision about where they land before it needs an interface. Zed's mention system
  also covers symbols and diagnostics; both need a language server, and there is not one here.
- **No quota display.** A reference interface shows "usage limits 73%, resets in 2 hr 21 min". The
  protocol carries context occupancy and an optional cost figure, and has no concept of a quota or a
  reset. The per-turn line shows elapsed time and, when the agent reported usage on both sides of a
  turn, the difference. The first turn of a session therefore shows only a duration.
- **Windows.** Interfaces are in place (`wkbd-sec::reaper` has the Job Object slot) and return
  unimplemented.
- **A virtualized transcript.** Collapsing by default keeps the node count manageable at
  reachable lengths. Virtualization has to be designed together with the sticky prompt header.
  End-anchored scrolling is done — `ui/src/lib/stickToBottom.ts` — and a virtualized list has to
  cooperate with it rather than replace it: it follows growth through a `MutationObserver`, which
  a windowing implementation would have to keep firing.
- **Proposals are identified by their content hash, and a decided one does not come back.** Which
  the distiller forced: it reads the whole history after every run, so what it found last week it
  finds again every week. Pending, approved, applied and rejected all block a new row; expired,
  voided and superseded do not, because nobody weighed those on their merits. Whether a proposal
  lost to the daily budget can return is the budget policy's existing choice — `RejectByDefault`
  means what its comment says.
- ~~**The approval queue has no interface.**~~ It has one, and the loop is closed end to end: a run
  raises a proposal with its evidence, the queue shows it, approving it applies it, and the playbook
  gains the bullet. Verified in the desktop shell as well as in the suite.
- ~~**Routing preferences are not updated from runs.**~~ The loop turns: a task's agent is chosen by
  the bandit when more than one is configured, the choice is recorded in the run log, and the
  acceptance result is folded back in. What it cannot do yet is the *cost* half of its own objective:
  an agent is a command line and the daemon is told nothing about what running it costs, so unless
  somebody declares prices with `--agent-cost` every arm costs the same and the objective degenerates
  to "the arm most likely to pass". Stated rather than papered over, because inventing prices would
  produce a router confidently optimising something nobody measured.
- ~~**Distillation does not run.**~~ It runs, and the third loop turns: after every run, every
  finished task in the project is reduced to a normalised step sequence, and a sequence that
  succeeded on three separate occasions with an external signal each time becomes a workflow
  proposal. Three limits are deliberate and worth knowing before changing them.

  **The occasion is the run, not the task.** An orchestrated run splits one goal into tasks that
  resemble each other by construction, so three siblings that all passed are one planner getting
  one decision right. `RunSummary::occasion` exists for this and the gate counts distinct values
  of it.

  **The step signature is coarse on purpose, and coarse enough to be wrong.** An agent's tool call
  titles are prose it wrote, so grouping on them makes every task unique — which looks like a
  distiller that never fires rather than like a bad signature. A step is its kind plus a file
  extension or a program name, and consecutive duplicates collapse. Two tasks that did different
  things in the same shape will group. That is what the approval queue is for; the proposal cites
  its runs.

  **`crates/wkbd-core/src/trace.rs` reads the event log, not the `tasks` table.** The table looks
  like the obvious source and is empty: nothing has ever written a row to it. `tasks`, `task_deps`
  and `verifications` are all dead schema — the durable workflow checkpoints into `step_outputs`.
  Worth deleting or filling, but not silently left as a trap for the next person to reach for.
- **A worker's permission requests are answered automatically.** An orchestrated worker has nobody
  watching it: waiting for a person stalls every parallel run on its first tool call, and the wait
  times out into a refusal, so "ask" and "refuse everything" are the same policy. They are allowed
  and recorded instead. What constrains a worker is therefore the worktree, the path guard rooted at
  it, and the acceptance check — not the prompt. Treating the prompt as a boundary for an unattended
  session would be believing a check nobody performs.
- ~~**Replanning is reported but not performed.**~~ An ownership violation now sends the graph back
  to the planner with the violation as a problem, and the run continues on the new graph, keeping the
  commits of tasks that already passed. Re-deriving the whole schedule rather than widening the one
  task's declaration is the point: overlapping declarations are what force two tasks to run in
  sequence, so a task that took paths it did not declare may belong in a different wave than the one
  it ran in.
- **`known_tests` is always empty.** Enumerating a repository's tests means running its build, so
  the validator degrades to accepting any identifier rather than skipping validation. A task can
  therefore name an assertion that does not exist, and it will fail at acceptance instead of at
  validation — later and more expensively than it should.

## 7. Unverified claims

Everything here is either untested or rests on a source rather than a measurement.

- Screenshots of Copilot's interface were obtained and are archived in
  [`docs/reference/copilot/`](reference/copilot/README.md). They settle the footer slot order,
  which was the one thing source could not give. They are of mixed vintage — the model name in
  the input box is two years old and the surface has been reorganized since — so they are
  evidence about layout conventions, not about the current build.
- **The inline diff treatment remains unverified.** The documentation describes the in-editor
  overlay without illustrating it, so that part of [UI-SPEC.md](UI-SPEC.md) comes from theme
  tokens and release-note text rather than from a picture.
- Every performance figure quoted in a comment comes from someone else's measurement. None was
  reproduced here.
- The claim that only-read tests reduce test tampering to near zero comes from a cited benchmark,
  not from our own measurement.
- `wkbd-sec`'s Windows paths are unimplemented and untested. They typecheck for the
  `x86_64-pc-windows-msvc` target and nothing more.
- **`wkbd-sec`'s boot-side orphan sweep does nothing on macOS.** `ProcessIdentity` cannot read
  an executable name or a start time there, so `matches()` always returns false and no
  recorded process is ever killed. That is deliberately the fail-safe direction — not killing
  beats killing a stranger's process that inherited the id — but it means the third layer of
  process cleanup is currently absent on macOS and needs `proc_pidinfo`.
- The macOS and Windows code paths in `wkbd-sec` have only been typechecked. In particular the
  descriptor-walking path guard fallback, which is the only backend on macOS, has never
  executed: `openat2` exists on this kernel, so every test took the fast path plus an
  explicitly forced walk, and the "probe says unavailable, so choose walk" branch is untaken.
- The `--stdin` mode of `git merge-tree` was measured but is not used; the inverted status
  semantics are recorded in [M0-FINDINGS.md](M0-FINDINGS.md) in case someone reaches for it.
- Load has not been characterized. The store's write path is tested for correctness under
  concurrency, not for throughput at a realistic event rate with ten agents.

## 8. If you change one thing, know this

**Do not make the turn loop exist in more than one place.** The ordering between a prompt
response and the notifications that precede it has been got wrong three times, in three separate
implementations of the same loop, and each time it presented as an intermittently truncated
transcript rather than as an error.
