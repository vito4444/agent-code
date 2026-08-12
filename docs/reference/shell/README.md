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
- **The composer footer says `(restarts)` next to the model and thinking-level selectors.** Changing
  either can only take effect when the agent process starts, so changing it mid-conversation means a
  new session carrying a summary of this one. The control says so rather than looking like an
  in-place switch that changes nothing.

`tauri-linux-empty.png` is the same shell with no session open, which is the state that shows the
autonomy selector sitting above the input box rather than in the row with the per-moment controls.

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
