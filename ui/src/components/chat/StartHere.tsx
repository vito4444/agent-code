import { useState } from 'react';
import type { AgentSummary } from '../../lib/types';

/**
 * The screen before there is a conversation, and the way to start one.
 *
 * This went through two wrong answers before this one. First it said "No session selected.", which
 * is accurate and useless: it names a state without saying what to do about it, in the largest empty
 * area in the application. Then the session composer was hidden here, on the grounds that a Send
 * button with nothing to send to is an affordance for something impossible — which treated "the
 * button cannot work" as the only option available, when the better answer was to make it work.
 *
 * So the input is the first thing on the screen and it does the whole job: it opens a session and
 * sends the message as its first turn. The alternative asks somebody to find the sidebar, open a
 * session, and only then say what they wanted — three steps to express one intention, and the first
 * two are about our object model rather than about their work.
 */
export function StartHere({
  agents,
  onStart,
  onRuns,
}: {
  agents: AgentSummary[];
  /** Opens a session with this agent in this directory, and sends `text` as the first turn. */
  onStart: (agentId: string, projectRoot: string, text: string) => void;
  onRuns: () => void;
}) {
  const [text, setText] = useState('');
  const [agentId, setAgentId] = useState('');
  const [root, setRoot] = useState('');
  const chosenAgent = agentId || agents[0]?.id || '';
  const ready = text.trim() !== '' && chosenAgent !== '' && root.trim() !== '';

  const submit = () => {
    if (!ready) return;
    onStart(chosenAgent, root.trim(), text.trim());
    setText('');
  };

  return (
    <div className="start-here" data-testid="start-here">
      <h1>Workbench</h1>
      <p>
        Say what you want done. The reasoning, every tool call and every file touched will appear
        here as it happens.
      </p>

      {agents.length === 0 ? (
        /* No picker at all rather than an empty one. An empty menu says a choice exists and is
           broken, which is worse than saying there is nothing to choose from and why. */
        <p className="start-here-blocked" data-testid="start-here-no-agents">
          No agents are configured. Start the daemon with <code>--agent</code> and this becomes an
          input.
        </p>
      ) : (
        <form
          className="start-composer"
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <textarea
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              // Enter sends, shift+enter breaks the line. The same binding as the session composer,
              // because this is the same act and a different binding here would be a trap for
              // exactly the person who has already learned the other one.
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                submit();
              }
            }}
            placeholder="Describe what you want done…"
            rows={3}
            data-testid="start-prompt"
          />
          <div className="start-composer-foot">
            <select
              value={chosenAgent}
              onChange={(e) => setAgentId(e.target.value)}
              aria-label="Agent"
              data-testid="start-agent"
            >
              {agents.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.display_name}
                </option>
              ))}
            </select>
            <input
              type="text"
              value={root}
              onChange={(e) => setRoot(e.target.value)}
              placeholder="/path/to/project"
              aria-label="Project directory"
              data-testid="start-root"
            />
            {/* No default directory. The obvious one is this process's working directory, which is
                wherever the shell happened to be launched from — and it is also the root the file
                boundary is built from, so a session silently rooted there is a session whose
                boundary somebody else chose. */}
            <button type="submit" disabled={!ready} data-testid="start-send">
              Start
            </button>
          </div>
        </form>
      )}

      <div className="start-here-cards">
        <article>
          <h2>One agent, one conversation</h2>
          <p>
            Streaming reasoning you can collapse, tool calls with the diff inside them, and every
            file read or written on the agent&rsquo;s behalf — including the ones it was refused.
          </p>
        </article>
        <article>
          <h2>A goal, planned and split</h2>
          <p>
            One sentence becomes a task graph. Each task gets its own worktree, its own acceptance
            check, and nothing merges until you say so.
          </p>
          <button type="button" onClick={onRuns} data-testid="start-here-runs">
            Go to runs
          </button>
        </article>
      </div>
    </div>
  );
}
