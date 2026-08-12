# The desktop shell, actually running

Captured on this machine: Ubuntu 24.04, X11 on display `:1`, software rendering
(`LIBGL_ALWAYS_SOFTWARE=1`), WebKitGTK 2.52.3, GTK 3.24.41.

`tauri-linux-transcript.png` is the shell showing one finished turn. Worth looking at for four
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

`tauri-linux-empty.png` is the same shell with no session open, which is the state that shows the
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

`worker-transcript.png` is a worker session's own conversation, which is worth a look because it shows
exactly what a dispatched agent is told: the paths it may change, the assertions that will judge it,
and the paths that are read-only and will be restored if it changes them. A worker that does not know
which tests decide its fate optimises for looking finished.

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
