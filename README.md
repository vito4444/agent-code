# Workbench

A local desktop workbench for running several AI coding agents. Each agent gets a real chat
surface built on structured protocol events, an orchestrator turns one sentence into a task graph
and runs it in real isolation, memory persists across sessions, and the system proposes its own
improvements without applying them unasked.

Nothing leaves the machine except what the agents themselves send to their own model providers.
There is no server component and no account.

## Status

The vertical slice works: daemon, one agent over the Agent Client Protocol, a turn with several
thought segments, a diff, an inline permission prompt, an answer, all replayed to a client from an
append-only event log.

The desktop shell is not written yet — the interface is served over HTTP and used in a browser.
See [docs/HANDOFF.md](docs/HANDOFF.md) for what is done, what is not, and every claim that has not
been verified.

## Running it

Needs Rust 1.85 or later, Node 22 or later, pnpm, and git 2.38 or later (for
`git merge-tree --write-tree`).

```bash
cargo build -p wkbd-core -p fake-acp-agent
cd ui && pnpm install && pnpm build && cd ..

./target/debug/wkbd-core \
  --state-dir /tmp/wkbd \
  --listen 127.0.0.1:8787 \
  --ui-dir ui/dist \
  --agent 'fake=Fake Agent=./target/debug/fake-acp-agent --profile rich --live-config true'
```

Open <http://127.0.0.1:8787>.

`fake-acp-agent` is a test double, not a model. To use a real agent, point `--agent` at any
Agent Client Protocol implementation:

```bash
--agent 'claude=Claude Code=npx @agentclientprotocol/claude-agent-acp'
--agent 'codex=Codex=codex acp'
```

Measure what a real agent actually does before trusting a capability:

```bash
./target/debug/acp-probe --agent 'claude=npx @agentclientprotocol/claude-agent-acp' \
                         --prompt 'read the README and tell me what this does'
```

Interface development against the running daemon:

```bash
cd ui && pnpm dev     # proxies /api, WebSocket included
```

## Checks

```bash
cargo test --workspace       # 281 tests
cd ui && pnpm vitest run         # 122 tests
./scripts/m0-mergetree.sh    # 15 assertions about git's own behaviour
./scripts/e2e-smoke.sh       # 51 assertions, the conversation slice
./scripts/e2e-orchestration.sh # 52 assertions, one run start to finish
```

Run `e2e-smoke.sh` before believing anything works. Every defect listed in section 5 of the
handover was found there and by nothing else — all of them in the handover between components,
none reachable from a crate's own tests.

`m0-mergetree.sh` measures git rather than our code. Re-run it after a git upgrade: it pins two
counter-intuitive behaviours the orchestrator depends on.

## Layout

| Crate | Responsibility |
| --- | --- |
| `wkbd-proto` | Protocol-adjacent types, the internal event model, turn segmentation |
| `wkbd-store` | SQLite: one writer and many readers, migrations, blobs, boot state |
| `wkbd-vcs` | git: merge prediction, worktrees, ownership, immutable paths |
| `wkbd-sec` | Path guard, git environment, process reaping, permissions, text sanitizing |
| `wkbd-agent` | Protocol client, process pool, session factory, capability probe |
| `wkbd-memory` | User rules, bi-temporal facts, retrieval, prelude construction |
| `wkbd-orch` | Task graph, durable execution, scheduling, acceptance |
| `wkbd-evolve` | Playbook, proposals, contextual bandit routing, distillation |
| `wkbd-core` | The daemon: HTTP, WebSocket, the turn runner |
| `fake-acp-agent` | A scriptable agent used as a test double |
| `acp-probe` | Measures what an agent actually does |
| `ui/` | React interface, served by the daemon |

Only `wkbd-proto` depends on the protocol crate, so "what breaks when the protocol changes" is
answerable by reading one crate.

## Documents

- [docs/HANDOFF.md](docs/HANDOFF.md) — the decisions worth knowing, what is not done, and every
  unverified claim. Start here.
- [docs/M0-FINDINGS.md](docs/M0-FINDINGS.md) — measurements taken on a real machine, with the
  commands that took them.
- [docs/UI-SPEC.md](docs/UI-SPEC.md) — tokens, components, and the behaviour constants that carry
  the interface.

## A few decisions, briefly

**Structured events, not scraped terminals.** The chat surface is written once and works for every
agent because it consumes protocol events. Screen-scraping approaches end up with a hardcoded list
of spinner characters and a debounce to guess whether the agent is still working.

**The model appears twice in an orchestrated run.** Drafting the task graph, and replanning when a
deterministic predicate says the graph was wrong. Ordering, dispatch, isolation, dependency
integration, ownership checking, acceptance and merging are ordinary code with a durable
checkpoint after each step, so a run that went wrong can be read and replayed exactly rather than
re-sampled.

**Absent beats wrong.** If an agent does not report its context usage, the indicator is not drawn
at all — not zero, not "unknown", not an estimate. If an agent declares no models, there is no
model menu. A control that appears to work and does nothing is worse than no control.

**Nothing in memory is deleted.** A corrected fact is closed on both its time axes and points at
its replacement, so "what did we believe last Wednesday" has an answer and "we were wrong" stays
distinguishable from "we never knew".

**User rules are not memory.** Separated in three independent places, because a preference a model
can rewrite is a setting the program has quietly stopped honouring while the settings screen still
shows it as enabled.

**Approval binds to content.** A remembered permission decision is keyed on a hash of what the
operation actually is. One changed byte asks again.

## License

Apache-2.0.

## Look and feel

The default theme is warm and paper-coloured, with a serif for the parts of a transcript that are read
as prose — the answer and the reasoning — and the sans-serif and monospace kept for paths, counts and
diffs. It lives entirely in `ui/src/tokens.css`; Primer's blue-grey palette is still there as
`[data-theme='primer']`. The reasoning behind each choice, and the three details that took more than
one attempt, are in [`docs/reference/shell/`](docs/reference/shell/) next to screenshots.

## The desktop shell

```bash
cd ui && pnpm build          # the daemon serves this; without it there is nothing to open
cd desktop && cargo build
./target/debug/wkbd-desktop -- --agent "rich=Rich=/path/to/agent --profile rich"
```

Everything after `--` goes to the daemon untouched, so the shell never needs to grow a duplicate of
a daemon flag — a duplicate would be the copy that falls behind, and a silently dropped flag looks
like a daemon ignoring its configuration.

It needs `libwebkit2gtk-4.1-dev` and `libgtk-3-dev` on Linux. Screenshots of it running, and the
three mistakes made getting there, are in [`docs/reference/shell/`](docs/reference/shell/).

The shell is thin on purpose: it starts the daemon, waits for it to answer, and points a webview at
it. Nothing in that process is testable without a display, so anything that moves in there stops
being covered. If the daemon will not start, the window still opens and shows the log path —
an application that cannot open cannot be used to find out why it cannot open.
