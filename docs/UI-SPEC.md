# Interface specification

The goal is that this reads as a member of the same family as GitHub Copilot's chat surface,
not as a copy of it. Numbers below come from two published sources rather than from measuring a
screenshot:

- **Spacing, control sizes, radii, font stacks**: GitHub Primer primitives.
- **Type scale**: the chat scale in `microsoft/vscode`, which is relative (em against one base)
  rather than absolute.

That combination is deliberate. Primer's absolute spacing keeps a dense interface consistent; a
relative type scale means one variable rescales all text, which a desktop application needs and
a website does not.

Colour tokens are named after VS Code's `chat.*` theme keys and given Primer values, so a future
theme is a matter of redefining variables rather than editing components.

Provenance and its limits are recorded in [M0-FINDINGS.md](M0-FINDINGS.md) section 5.

---


> **The default theme is no longer the one measured here.** Every colour below is Primer's, and the
> measurements were taken against it; that palette is still available as `[data-theme='primer']`. The
> default is now a warm, paper-coloured theme with a serif for running prose, described in
> `docs/reference/shell/README.md` with screenshots. The spacing scale, control sizes, radii and type
> scale are unchanged and still Primer's and VS Code's, so everything in this document about *size*
> and *rhythm* still holds. What changed is hue, and where a serif is used.


## 1. Tokens

Defined in [`ui/src/tokens.css`](../ui/src/tokens.css).

### Type scale

Relative to `--chat-font-size-base` (13px). Resolved pixel sizes in comments match VS Code's own
annotations.

| Token | Value | At 13px base |
| --- | --- | --- |
| `--chat-font-size-xs` | 0.846em | 11px |
| `--chat-font-size-s` | 0.923em | 12px |
| `--chat-font-size-m` | 1em | 13px |
| `--chat-font-size-l` | 1.077em | 14px |
| `--chat-font-size-xl` | 1.231em | 16px |
| `--chat-font-size-xxl` | 1.538em | 20px |

Line heights: 1.25 tight, 1.5 normal, 1.625 relaxed.

### Spacing

Primer's base scale: 2, 4, 6, 8, 12, 16, 20, 24, 32, 40 px. Note 2 and 6 exist; the scale is not
multiples of four. Container gaps: 8 condensed, 16 normal, 24 spacious.

### Controls

Primer control sizes. Toolbar controls use small (28px) or medium (32px).

| Size | Height | Padding block | Padding inline |
| --- | --- | --- | --- |
| small | 28px | 4px | 8 / 12 / 16 |
| medium | 32px | 6px | 8 / 12 / 16 |

Radii 3 / 6 / 12 / 9999px. Borders 1px default, 2px thick. Focus outline 2px, offset −2px.

### Fonts

```
--font-sans: 'Mona Sans VF', -apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans',
             Helvetica, Arial, sans-serif, 'Apple Color Emoji', 'Segoe UI Emoji';
--font-mono: ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas,
             'Liberation Mono', monospace;
```

Code blocks use 0.8125rem (13px) at 1.5, independent of surrounding text size.

### Colour

Named after VS Code chat theme keys, valued from Primer. Light theme:

| Token | Light value |
| --- | --- |
| `--chat-request-bubble-background` | `#f6f8fa` |
| `--chat-request-border` | `#d1d9e0` |
| `--chat-lines-added-foreground` | `#1a7f37` |
| `--chat-lines-removed-foreground` | `#d1242f` |
| `--chat-thinking-shimmer` | `#818b98` |
| `--chat-checkpoint-separator` | `#d1d9e0` |
| `--inline-chat-diff-inserted` | `#dafbe1` |
| `--inline-chat-diff-removed` | `#ffebe9` |

Dark values are in the same file under `[data-theme='dark']`.

The conversation column is capped at 950px rather than filling the window: the transcript is
prose plus code, and long measure hurts both.

---

## 2. Components

### 2.1 Turn

Three bands: how it thought, what it did, what it concluded.

The user's message is a `<blockquote>` with a 3px accent rule and a tinted background, not a
bubble. This is an instruction followed by a record of work, not a conversation between peers,
and bubbles cost the horizontal space the diffs need.

Vertical rhythm carries the separation: 8px inside a band, 16px between the reasoning and the
answer, 24px between turns with a rule in `--chat-checkpoint-separator`.

### 2.2 Thought block

Collapsible. **The expansion rule is the single most load-bearing line in the interface:**

```
expanded = userOverride ?? (segment.state === 'live')
```

Two properties this has and the obvious implementations do not.

**It keys off the segment, not the turn.** A real agent thinks several times per turn, between
tool calls. Treating "the turn has not ended" as "this thought is in progress" expands every
thought in the turn at once and pushes the answer off screen. The core guarantees at most one
segment is live at a time, so following `state` follows the newest and nothing else.

**A user override is permanent for that segment.** There is deliberately no reset: each segment
owns its own override, so a new turn starts fresh because its segments are new, and a block the
reader deliberately opened stays open. Re-collapsing it is the specific behaviour that makes a
transcript feel like it is fighting back.

Body is capped at 260px with internal scrolling rather than growing without bound: an expanding
block pushes everything below it down on every token, and that layout shift while reading is the
complaint that made two other products change this behaviour.

A segment whose boundary we inferred (because the agent sent no `messageId`) is badged
`inferred`, so the interface does not imply the agent drew that boundary.

### 2.3 Tool call card

Collapsed by default, with two exceptions that matter more than the default:

- **A failed call stays open.** The output of a non-zero exit is the thing the reader needs.
- **A call carrying a diff stays open.** A file change is a result, not a detail.

Content is dispatched by type, never concatenated: a diff renders as a diff with line numbers and
red/green backgrounds, terminal output renders monospaced on a dark ground. This is the entire
reason for taking structured events from a protocol instead of scraping a terminal.

The collapsed header carries the tool kind as a small uppercase chip, the title, the status, and
`+N −M` when there is a diff.

### 2.4 Diff view

Hand-rolled line diff with a longest-common-subsequence core, trimmed head and tail, capped at 60
lines with a link to the full review surface. Deliberately not an editor instance: this appears
inline and potentially many times in one transcript.

### 2.5 Permission card

Rendered inline in the transcript at the point the agent asked, so the reader can see what it was
doing when it asked. Copilot uses a modal for this; a modal loses the context, and an approval
given without context is the approval most likely to be wrong.

One convention worth borrowing, visible in
[`copilot-tool-approval-dialog.webp`](reference/copilot/copilot-tool-approval-dialog.webp): the
remembering scope is chosen *at the moment of approval* — "Allow in this Session", "Allow in this
Workspace", "Always Allow" — rather than configured beforehand. Ours takes the options the agent
offers, which today means the protocol's four kinds, and applies its own scope policy on top; the
scope is therefore implied by which option was chosen rather than picked separately. Making it
explicit is worth doing once there is more than one plausible scope for a given decision.

Once answered it collapses to the chosen option. A decision that came from a remembered choice
is badged `remembered`, with a tooltip stating that the decision is bound to the exact content of
the operation so a changed command or path asks again.

### 2.6 Answer

The highest-contrast text in the turn. Four things carry the separation at once, because a review
against a real transcript found size and colour alone were not enough:

- 16px against 11px reasoning text
- full `--fg-default` against `--fg-muted`
- a rule above
- 16px of space above and below

Measured in the running interface: `fontSize=16.003px`, `color=rgb(31, 35, 40)`,
`borderTop=1.01587px solid`, `marginTop=16px`, `paddingTop=16px`.

While streaming it carries a blinking caret, so "still writing" is visible without having to
notice that the text stopped growing.

### 2.7 Composer

Top to bottom:

1. **Autonomy row**, above the input. Answers "under what conditions do my messages go out" — a
   standing policy.
2. **Queued messages**, if any.
3. **Text area.**
4. **Footer row**, inside the input's border.

The autonomy setting is kept out of the footer deliberately. The footer is right-now state;
autonomy is a policy that persists across turns. On one line they look like the same kind of
thing and the row becomes cramped.

#### Footer slots

Order is kept in one constant,
[`footerOrder.ts`](../ui/src/components/composer/footerOrder.ts).

It was originally derived from source and release notes, which give the CSS classes but not the
order. Screenshots since obtained (archived in
[`docs/reference/copilot/`](reference/copilot/README.md)) settle it:

```
Copilot, left:   [+ add context] [@ attach] [Agent ▼ mode]
Copilot, right:  [model name ▼] [High] [200K] [settings] [send]
```

Mode on the left, model on the right, send rightmost, and the thinking level and the context
size sit immediately beside the model name rather than anywhere else. Ours follows the same
conventions — an identity on the left, model and thinking level adjacent, context beside them,
send last — so the order below stands as written rather than being a guess.

```
agent · model ⌄ · thought level ⌄ · other config · context ring · send
```

**Every slot is independently omittable, and the rest must not move.** On an agent that declares
no options and reports no usage, three of these disappear.

- **Model and thinking level** are generated entirely from what the agent declared, filtered by
  the `model` and `thought_level` categories. No model name is hardcoded anywhere. An agent that
  declares nothing gets no control: an empty menu says the choice exists and is broken, which is
  worse than no menu.
- Options with an unknown or absent category collapse into an `N more` overflow, so a
  vendor-private category still reaches the user without being given a position it did not earn.
- An option the agent can only read at launch is badged `restarts`, with a tooltip explaining
  that choosing a different value opens a new session. There is no in-place model switch to
  offer for such an agent, so the label offers what actually happens.

#### Send control

Idle: one `Send` button.

Busy: **two actions, not three.** Copilot offers three — `Stop and Send`, `Add to Queue`, and
`Steer with Message` — and this is the one place the family resemblance is deliberately broken.

- `Add to queue` — held until the turn ends. Nothing is interrupted.
- `Stop and send` — cancels the turn, discarding work in flight, then sends.

There is deliberately no `Steer` that interrupts at the next convenient moment. Copilot can offer
it because Copilot's agent is Copilot's own; over this protocol there is no way to inject a message
into a running turn, and the proposal for one is unmerged and unowned. A steer control here could
therefore only be a queue with a different label or a cancel pretending to be gentler. It appears
only when the connected agent declares support, which today no agent does.

Queued messages say when they will be delivered ("will be sent when this turn ends") and can be
withdrawn. That part is honest: the message never left this process.

### 2.8 Context ring

A 20px ring plus a percentage, inside a pill border, in the footer — on the path the eye takes
back to the input.

**It renders nothing at all when the agent has not reported usage.** Not zero, not "unknown", not
an estimate. The protocol's usage notification is optional and its own specification says an
agent that cannot give a meaningful window size should send nothing rather than a null. And as a
client we cannot compute it: we do not know the agent's system prompt, which rule files it
loaded, how large its tool schemas are, or whether it has just compacted. A locally estimated
figure would be a different quantity wearing the same label.

Hover gives the token counts, the cumulative cost when the agent reports one, and the sentence
"Reported by the agent. This client does not estimate context usage."

Level thresholds: normal below 75%, warning at 75%, critical at 90%.

Copilot shows this as a plain `200K` label beside the model name — the window size rather than the
proportion used. A ring is a deliberate difference: the number a reader glances at between two
messages is how full the window is, not how large it is, and a proportion is only honest when the
agent reports both halves. Hence a ring when there are two halves, and nothing when there are not.

### 2.9 User rules screen

Two scopes: every project on this machine, or this project only.

The important property is behind the screen rather than on it: a user rule and an inferred memory
are different kinds of record, separated in three places (see
[`wkbd-memory/src/rules.rs`](../crates/wkbd-memory/src/rules.rs)). This screen only ever shows
rules, and says where each applies.

Each rule shows how many session entry points have been observed applying it. Recorded rather
than asserted: rules silently not applying in one situation — a resumed session, an orchestrator
worker, a session restarted to change a model — is the classic failure.

### 2.10 Protocol log

Raw frames in both directions, exactly as they crossed the wire, built on day one rather than
added when something breaks. When an agent's behaviour and the interface disagree there is no
other way to find out which is wrong. It is also the only place an unparseable line is visible:
those are skipped so a chatty agent cannot end a session, and skipped-and-invisible would be
indistinguishable from never-sent.

Direction is a filled badge rather than a bare glyph: `▶` on accent blue for outbound, `◀` on
success green for inbound. Measured: 18×18px, 13px glyph, `rgb(9,105,218)` and `rgb(26,127,55)`.

---

## 3. Behaviour constants

| Behaviour | Value | Why |
| --- | --- | --- |
| Thought expanded | `override ?? live` | Several thoughts per turn; only the newest may open |
| Thought body cap | 260px, scrolls | Unbounded growth pushes content down on every token |
| Tool call default | collapsed | Successive calls are visual noise |
| Tool call exception | failed, or has a diff | Both are results, not details |
| Answer size | 16px vs 11px reasoning | Size and colour alone were not distinguishable |
| Context ring absent | when no `usage_update` | We cannot compute it and will not invent it |
| Send while busy | queue or stop, never steer | The protocol cannot inject mid-turn |
| Event publishing | one animation frame | Per-token renders push commit past 50ms |

---

## 4. Known gaps

- **The inline diff treatment is still unverified against a screenshot.** The documentation
  describes the in-editor overlay but does not illustrate it, so what is specified here comes from
  theme tokens and release-note text. Everything else in this file is either measured in the
  running interface or cited to a published source.
- **No full-screen review surface yet.** The inline diff links to one that does not exist.
- **No terminal emulator.** `terminal/*` is refused by the client, so embedded terminal content
  reports that rather than showing output. The renderer defaults to canvas rather than WebGL on
  Linux, so the risky path is opt-in when it does arrive.
- **No virtualized transcript.** Collapsing by default keeps the node count down at the lengths
  reachable today. Virtualization has to be designed together with the sticky prompt header and
  end-anchored scrolling, not bolted on.
