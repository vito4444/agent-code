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

`paper-empty.png` is the shell with no session open, and its input is the point of it. The screen
went through two wrong answers first. It said "No session selected.", which is accurate and useless —
it names a state without saying what to do about it, in the largest empty area in the application.
Then the composer was hidden here, on the grounds that a Send button with nothing to send to is an
affordance for something impossible, which treated "the button cannot work" as the only option when
the better answer was to make it work. Typing here opens a session and sends the message as its first
turn, so one intention takes one act instead of three — find the sidebar, open a session, and only
then say what you wanted, the first two of which are about our object model rather than the work.

The same screen with a session open, which is the state that shows the
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

## Everything here is regenerated by a script

`./scripts/screenshots.sh` produces this whole directory. It exists because the set went stale twice:
a screenshot of a product the code no longer produces is worse than no screenshot, because it is
evidence for something untrue, and remembering which files a change invalidated does not survive a
change that touches the sidebar. Run it after anything that alters the interface.

It clicks its way through the interface rather than posing it, and it finds what to click by looking
at the rendered window — the navigation rows by where their text is, a task card's two links by
splitting a row of accent-coloured pixels into clusters. Every hard-coded offset it used to have
pointed one row off as soon as a screen was added.

Writing it turned up two things worth keeping. `xdotool getwindowgeometry` reports the window *frame*
while `import -window` captures the *client area*, and on this window manager those differ by the
28-pixel title bar — so every click derived from a captured coordinate landed one row low, which is
the source of a run of "the click did nothing" confusion earlier in this project. And configuring
three agents made an orchestrated run fail, because routing treated every configured agent as a
candidate worker and sent tasks to two that were set up for interactive chat. That one is a real
defect the script found by doing something no test did.

## Reading a change

`review-task.png` is the review surface answering what one task did: a single file, because a task's
diff is taken from its own starting commit and that task's dependencies produced the other two. The
candidate's own diff — every task's work in one place — is not pictured, because it sits below the
fold behind the merge gate and three attempts at reaching it reliably produced screenshots of the
wrong screen. The claim the pair illustrated is asserted with actual file counts in
`scripts/e2e-orchestration.sh` instead, which is stronger evidence than a picture.

The surface exists partly because a reference to it already did. The inline diffs in a transcript are
deliberately small and their truncation note said "open the review surface", and there was no review
surface — a dead reference is worse than the truncation it apologises for, because it tells the reader
a way to see the whole thing exists. The note now states the fact and stops; the two places that know
which commits to compare, a task card and the merge gate, reach the surface from there.

## Attaching a file

`paper-attachment.png` is one prompt with two things attached, and the strip under the quote line
says what became of each: `crates/wkbd-proto/src/normalize.rs — contents sent — 12 kB` beside
`crates/wkbd-sec — path only, a directory`.

That second half is the part worth having. The protocol makes every agent accept text and resource
links, and makes embedded contents, images and audio conditional on capabilities declared at
`initialize`. So the same mention becomes a different block for a different agent: contents for one,
a path for another. An agent handed a path has to go and open it, which it may lack the tools or the
inclination to do — and when it does not, the answer is about a file nobody read, which from outside
is indistinguishable from the model ignoring the request. Zed, which co-designed the protocol, makes
the same fork and does not surface it. Naming the fallback is cheap and it is the difference between
a confusing answer and an explained one.

The composer says the same thing one step earlier, while the user can still decide to paste the
relevant part instead. Both predictions come from the capabilities that session negotiated, so an
agent that declares nothing shows "path only" on everything and never a control that does nothing.

Mentions are plain `@path` text in an ordinary `<textarea>`, not inline chips. Chips need
`contenteditable`, and `contenteditable` with an input method editor duplicates characters and drops
candidates. A prompt is where people write prose in their own language, so the box has to be the one
IMEs are actually tested against.

Which turned up an older bug in the same box: pressing Enter to confirm a Chinese candidate sent the
half-composed message, because the keydown handler never checked `isComposing`.

`paper-attachment.png` is also scrolled up, which is why the "Jump to latest" pill is in it.

## What a turn cost

Under each finished turn: `5.8s`, and the tokens it added when the agent reported usage on both
sides of it. The reference for this is a task panel reading `9s · 1.1k tokens`.

Tokens are a subtraction, not a reading. An agent reports the total context in use, so attributing
that to one turn means differencing the value before and after — which is why the first turn of a
session shows only a duration. There was no reading before it, and the alternative is to report the
running total as though the turn caused all of it. The context ring in the composer already shows
occupancy; this line is about one turn.

`Your usage limits — 73% — resets in 2 hr 21 min`, from the same reference, is not here and cannot
be. The protocol carries context occupancy and an optional cost, and has no concept of a quota or a
reset time. Whatever we drew there would be invented.

## Three corrections this round

**`file-boundary.png` was a picture of the wrong conversation, and had been for as long as it
existed.** The script opened a session over the API and captured immediately, with a comment saying
the newest session is selected automatically. It is not, and it should not be — the interface
auto-selects only when nothing is selected, because taking the view away from somebody mid-read
would be worse. Everything the paragraph above says about that screenshot is true of the current
one; it was not true of the file it described. The script now finds the newest sidebar row and
clicks it.

**The transcript scrolled, but nothing ever scrolled it.** Every capture of a turn longer than the
window showed the prompt and the first two tool calls, with the answer below the fold — the one
thing a reader is there for. It survived this long because the captures were of short transcripts.
It follows the newest content now, and lets go the moment the reader scrolls up: a turn streams for
a minute, and being yanked to the bottom on every chunk while trying to read a diff is the worse of
the two bugs.

**The fake agent reported the same usage figures on every turn**, so the difference across any turn
was zero — indistinguishable from an agent that reports nothing, and enough to hide whether the
per-turn token line worked at all.

## The protocol log, and what it was hiding

`protocol-log.png` is the last screen to get a capture, and taking one is how the rest of this
section happened: a screen nobody has looked at is where a defect sits undisturbed.

Three of them were sitting in it, all introduced by attachments.

**The buffer bounded the number of frames and nothing bounded their size.** Which was the whole
story until a prompt could carry an embedded resource — 256 kB in one frame, five thousand frames
kept, a gigabyte held to show somebody the shape of a JSON message. Frames are clipped to 4 kB,
which keeps the method, the parameters and the front of any payload.

**Four kilobytes is the right amount to hold and the wrong amount to show.** It is about forty
lines, so after the first fix one prompt still filled the viewport and buried every frame around
it. A frame shows its first 420 characters and opens on request; the rest is already there.

**It refetched the whole log every second**, whether or not anything had changed, on the screen
somebody opens when an agent is already misbehaving. Frames carry a monotonic sequence — not the
buffer index, which points at a different frame after the ring drops a thousand entries — and the
poll asks for what comes after the newest it has.

What the screen is worth having for is visible in the capture: the handshake, with
`promptCapabilities` in the agent's reply, and the prompt below it carrying `"type":"resource"`.
That is the negotiation and its consequence, four lines apart.

## Read off the captures

Nine things in this round were found by looking rather than by testing, which is why tests had not
found them.

- **The plan panel claimed to be working next to a finished turn.** An entry left `in_progress`
  when a turn ends is not work in progress — the agent said it was doing that step and stopped
  saying anything, which is what a refusal, a token limit or a cancellation looks like from here.
  It reads `stopped here` now, which is both honest and more useful.
- **A resolved permission rendered as its own button's label.** "Allow once" under a title reads
  like a control that does nothing when pressed. The kind carries the verb, so the outcome can be
  past tense.
- **Playbook bodies carried their own bullet**, which the queue doubled with a list marker and
  injection doubled with its own dash — so the text an agent reads began `- - Task ...`.
- **A workflow's steps were separate change entries**, so each got the queue's bullet *and* its own
  number: `• 1. write *.rs`. A workflow is one change.
- **Three identical evidence lines.** A distilled procedure cites the same acceptance command once
  per occasion, and three copies of one sentence is longer than one and says less: the reader has
  to compare them to find out they are the same, and the number — the actual evidence — was never
  stated. It reads `×3`.
- **File rows were absolute while attachments were relative**, so one file read two ways looked
  like two files. Paths inside the workspace are shortened, which has a second effect worth more
  than the consistency: an absolute path in that column now means exactly one thing. Compare the
  refusals in `file-boundary.png` — `/etc/passwd` against `escape-link`, which is inside the
  workspace and points out of it.
- **The mention picker's highlight used `--bg-muted`**, which on the paper theme is *lighter* than
  the picker's own background by five parts in 255. The row Enter would have chosen was unmarked.
  Hover and keyboard focus are different marks now, because Enter acts on one of them.
- **A routing decision was erased by the next status change.** Which agent ran a task was written
  into the dispatch state change's detail, and a detail is replaced by the next one — so the choice
  survived for the seconds the task spent dispatched and was gone by "completed", which is the
  state anybody reading a finished run is looking at. `run-view.png` carries `Ran on` on every card.
- **Approve was a filled button and Reject was not.** A filled accent button is the one a reader
  clicks without reading, and what waits in that queue is the system asking to change its own
  future instructions. Both carry the same weight now; only their colour says which is which.

And one that was the interface being right: `paper-transcript.png` came out showing the turn
*before* the one it is a capture of, because the previous capture had scrolled up and the record
correctly stayed where it was put rather than following the new turn. The script scrolls back now.

## Two things from the references that were not adopted

**The user's message is still a quote line, not a right-aligned bubble.** Both references bubble it,
and the earlier instruction for this project was explicit and reasoned: this is not two people
talking, it is an instruction with an execution trace under it. Overturning that on the strength of a
screenshot seemed like the wrong way round, and it is a one-line change if the reference wins.

**The model picker is still a flat list.** The reference groups models by capability with a sentence
about each tier. An agent declares a list of model ids over the protocol and nothing else — no tier,
no description, no price — so grouping them would mean inventing the tiers, and a confident-looking
grouping that came from nowhere is worse than an honest flat list.

**No suggested prompts on the empty screen.** One reference fills its empty state with starter
chips. The useful version of that is specific to the project in front of you, which nothing here can
infer, and the generic version — "fix a bug", "write tests" — is furniture that takes up the space
where the input should be. The empty screen has an input in it instead.
