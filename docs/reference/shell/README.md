# The desktop shell, actually running

Captured on this machine: Ubuntu 24.04, X11 on display `:1`, software rendering
(`LIBGL_ALWAYS_SOFTWARE=1`), WebKitGTK 2.52.3, GTK 3.24.41.

`paper-transcript.png` is the shell showing one finished turn. Worth looking at for four
things that are easy to get wrong and are visible here:

- **Three separate collapsed "Thought process" blocks in one turn.** A real agent thinks between
  tool calls, so a turn produces several reasoning segments. Expanding "everything in an unfinished
  turn" pushes the answer off screen; only the newest is expanded while the turn runs, and all of
  them collapse with a tick when it ends.
- **The prompt is a quote line, not a bubble.** This is not two people talking. It is an instruction
  with an execution trace under it.
- **The diff is a diff.** Line numbers, per-side colouring, a count in the card header. Not grey text.
  The header count and the diff's own count agree, which they did not until reading this screenshot
  caught them disagreeing: the header said `+5 −3` over a body saying `+4 −2`, because the header
  approximated with a common-prefix heuristic and counted the closing brace shared by both versions
  as an addition and a deletion at once. Two numbers for one thing, side by side.
- **The composer footer says `(restarts)` next to the model and thinking-level selectors.** Changing
  either can only take effect when the agent process starts, so changing it mid-conversation means a
  new session carrying a summary of this one. The control says so rather than looking like an
  in-place switch that changes nothing.

`file-boundary.png` is an agent trying to leave its workspace five ways, and it exists because the
first version of this interface showed none of this. Files read and written for an agent through the
protocol's file methods were recorded in the log and rendered nowhere, so an agent that edits that way
— the path the daemon encourages, since it is the only one that is bounded and logged — produced a
transcript containing a thought, an answer, and no sign that a file had changed. Refusals were
invisible too, which is worse: the boundary was enforced and the person watching was told nothing.

Now an allowed access is a quiet row with a byte count, because a turn can contain many reads and
giving each the weight of a tool call buries the two that changed something. A refusal is not quiet,
and it says why in words — "a symlink led outside the workspace" rather than `symlink-encountered`.
The stored value stays the identifier so logs and tests can match on it; the sentence is for the
person being told their agent was stopped.

`paper-empty.png` is the same shell with no session open, which is the state that shows the
autonomy selector sitting above the input box rather than in the row with the per-moment controls.

`run-view.png` is one orchestrated run, and it is the densest of these. Four things in it are the
whole argument for the orchestrator, and each is visible rather than asserted:

- **The plan the validator rejected is shown, with its reason in full.** The model's first graph left
  one task connected to nothing, and the run redrafted. Hiding the rejected attempt would leave a
  reader unable to tell a planner that got it right first time from one that took three tries.
- **Waves express parallelism, and the header says where it came from.** "2 in parallel from the base
  commit", then "1 in parallel, after wave 1". No heuristic decides what is safe to run together; the
  edges do.
- **A dependency edge is shown as commit folding**: `wkbd/feature-a starts at 0d99b9c, which folds in
  8112576, b35347d`. The dependent task's worktree already contains what its dependencies produced.
  This is the line that distinguishes an edge carrying work from an edge carrying a summary of work,
  and it is why the acceptance check for that task can require its dependencies' files.
- **The merge gate explains itself.** "Acceptance proved that the assertions each task named pass. It
  did not prove the change is the one you asked for, and a named test suite is the easiest thing in a
  run to satisfy the wrong way." A gate whose reason is invisible gets clicked through.

`run-list.png` shows a finished run as `AWAITING MERGE`, `3/3 tasks complete`, `waiting for you to
merge` — the status a run sits in indefinitely rather than a spinner that resolves itself.

`user-rules.png` states the rule/memory separation in the interface rather than only in the code:
"passed through verbatim, they are never rewritten, and nothing in the learning system can retire
them."

`sidebar-grouping.png` is the sidebar with a run in progress, and it is there because the first
version of this grouping was worse than no grouping. Sessions were grouped by directory, which is
literally what an orchestrated worker is — each has its own worktree — and that produced one group
per worker, each headed by a path ending in the run's uuid: three headings of noise around one row
each, and three rows all reading "Worker". Workers are grouped by their run now and labelled by their
task, which is what a reader wants and was already in the path.

Every screenshot here was retaken after the theme changed. Six of them had not been, and a
screenshot that shows a product the code no longer produces is worse than no screenshot, because it
is evidence for something untrue.

## Two corrections to earlier claims

The handoff document said the shell could not be built here because `webkit2gtk` was unavailable.
That was wrong. The package is in the Ubuntu repositories; the earlier check had been run before
`apt-get update`, so the index simply did not list it yet.

The shell's first version pointed the webview at the daemon with `window.eval` during setup and
showed a white window. A script sent at that point races the webview's own initialisation and is
lost with no error anywhere. The window's URL is declared in `tauri.conf.json` instead, so the
navigation is part of creating the webview and cannot race.

And the daemon did not serve the interface at all unless `--ui-dir` was passed, so a shell pointed at
it got 404 on every path outside `/api/*`. It now finds the built interface beside the binary or in
`ui/dist`, and says so at startup — or says it found none, which is the line that would have made
the white window take minutes instead of an hour.

## The paper theme

`paper-transcript.png`, `paper-thinking.png` and `paper-empty.png` are the interface after it was
re-themed from reference screenshots the user supplied. The change is in `tokens.css` and nowhere
else, which was the point of putting the colours there in the first place — the claim that a theme
could be swapped by redefining variables is now load-bearing rather than aspirational.

What changed and why, rather than what colour things are:

- **A cream ground instead of white.** Pure white at full brightness is the specific thing that makes
  a long session tiring, because the page is then the brightest object in the room and the text has to
  compete with it. Dropping the page below the text keeps the contrast between them and removes the
  glare, which is the opposite trade from dimming the text.
- **A serif for running prose only** — the answer and the reasoning. A transcript is mostly read, and
  a reading task wants what a book has. Paths, counts, rows, labels and diffs keep the sans-serif and
  the monospace, because a serif is worse at 11px and worse in a table. The split follows what the
  text is *for*.
- **Warm neutrals and a coral accent.** A cream ground under blue-grey text reads as a colour
  mistake, and a blue link on cream is the one element that looks pasted on.
- **Diff colours desaturated well below the usual.** A full-strength green block on cream is the
  loudest thing on the page, and a diff is reference material rather than an alert.
- **Reasoning drawn as a timeline**: a node where the thinking started, a rule beside the text, and a
  closing node that says it finished. A paragraph indented under a heading reads as part of the
  answer, and this band is specifically *not* the answer. `paper-thinking.png` catches the live state,
  where the node pulses and the label reads "Thinking".

Three details in there cost more than one attempt, all of them the same mistake — trusting a glyph to
land where it looks like it should:

1. The timeline nodes were `●` characters. A glyph's position comes from the font's metrics, and this
   one landed where a superscript goes and read as a typo in the middle of the sentence. They are CSS
   circles now.
2. The disclosure arrow was `▸`, which reads as a bullet, then `⌄`, which rendered below the baseline
   in the fallback serif and read as a comma. It is a rotated border pair now.
3. The label while thinking used the theme's "shimmer" colour, which made the only thing currently
   happening the quietest text on screen. It uses the ordinary muted colour and keeps the pulse.

`port-taken.png` is the shell refusing to adopt a daemon it did not start. The port is fixed, so a
stale daemon holding it makes the new one fail to bind and exit — after which "something answers on
8787" is true and points at a process this shell cannot configure or restart. Every symptom of
adopting it is indirect: agents that were configured are missing, a flag has no effect, the interface
is a version behind. The daemon now reports its pid on `/api/health` and the shell compares it.

## Two things from the references that were not adopted

**The user's message is still a quote line, not a right-aligned bubble.** Both references bubble it,
and the earlier instruction for this project was explicit and reasoned: this is not two people
talking, it is an instruction with an execution trace under it. Overturning that on the strength of a
screenshot seemed like the wrong way round, and it is a one-line change if the reference wins.

**The model picker is still a flat list.** The reference groups models by capability with a sentence
about each tier. An agent declares a list of model ids over the protocol and nothing else — no tier,
no description, no price — so grouping them would mean inventing the tiers, and a confident-looking
grouping that came from nowhere is worse than an honest flat list.
