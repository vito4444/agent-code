# Copilot interface reference

Screenshots taken from VS Code's own documentation and release notes, archived here because
`/tmp` does not survive and because the interface specification cites them.

| File | Shows | Source |
| --- | --- | --- |
| `copilot-agent-mode-selector.webp` | The mode selector inside the input box | v1.99 release notes |
| `copilot-chat-input-running-state.webp` | The send control while a request is running | Chat overview |
| `copilot-chat-keep-undo-buttons.webp` | The changed-files list with Keep and Undo | Chat overview |
| `copilot-agents-window-full-interface.webp` | The whole Agents window | Agents window docs |
| `copilot-tool-approval-dialog.webp` | Tool approval, including the remembering scopes | v1.99 release notes |
| `copilot-thinking-chain-of-thought.webp` | Reasoning rendered as stacked collapsible phases | v1.105 release notes |

**These are of mixed vintage.** The model name visible in the input box is a 2025-era model, so
some are two years old and the surface has been reorganized since — there are now two surfaces
(a Chat view and an Agents window) rather than one panel. They are evidence about layout
conventions, not about the current build. Where a claim depends on the current version, the
specification cites source or release notes rather than these files.

## What they settled

The one thing that could not be established from source was the left-to-right order inside the
input box, because CSS gives the classes and not the order. Observed:

```
left:   [+ add context] [@ attach] [Agent ▼ mode]
right:  [model name ▼] [High] [200K] [settings] [send]
```

So: mode on the left, model on the right, send rightmost, and the thinking level (`High`) and
context size (`200K`) sit immediately beside the model name rather than anywhere else.

Above the input, not inside it: the changed-files list, collapsed to `1 file changed +67 -0`,
with **Keep** in accent blue and **Undo** in grey on its right.

While a request is running the send control becomes three options — `Stop and Send`,
`Add to Queue` (Alt+Enter), `Steer with Message` (Enter).

Reasoning is a vertical stack of collapsible phases with names like "Planning Next.js
exploration", each with a green check and a `Read • filename` line when done, under a
`Used 1 reference` header.

Tool approval offers `Continue` with a dropdown whose options are "Allow in this Session",
"Allow in this Workspace" and "Always Allow" — that is, the remembering scope is chosen at the
moment of approval rather than configured beforehand.

## Not found

- No screenshot of the in-editor inline diff overlay. The documentation describes it but does
  not illustrate it, so the inline-diff treatment in our specification remains derived from
  theme tokens and release-note text.
- No separate Copilot Edits page; the URL now redirects to the chat overview.
