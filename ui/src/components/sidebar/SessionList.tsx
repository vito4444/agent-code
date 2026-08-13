import { useMemo, useState } from 'react';
import type { AgentSummary, SessionSummary } from '../../lib/types';

/**
 * The session list, grouped by project.
 *
 * Grouped rather than flat because the identifying part of a session is which project it is in, and a
 * flat list of eight sessions across three projects makes the reader do that grouping in their head
 * every time they look. It also fixes something the flat version got actively wrong: orchestrated
 * workers are all rooted under one run's worktree directory, so a flat list showed a column of rows
 * that were textually identical.
 *
 * The filter is over both the project path and the agent name, and it is present at three sessions
 * rather than at thirty. A search box that appears once a list is long is a search box nobody has a
 * habit of using, and the moment it would help most is the moment it is newest.
 *
 * Orchestrated workers are grouped by their run rather than by their directory. Grouping them by
 * directory is technically what they are — each has its own worktree — and it produces one group per
 * worker, each headed by a path ending in a run's uuid, which is three headings of noise around one
 * row each. What a reader wants there is the run and the task, and both are already in the path.
 */
interface Row {
  session: SessionSummary;
  primary: string;
  secondary: string | null;
  mono: boolean;
}

/**
 * Recognises a worker's worktree, which the orchestrator creates at
 * `<state>/worktrees/<run id>/<task id>`.
 *
 * Matched on the two path segments after `worktrees` rather than on the whole prefix, because the
 * state directory is configurable and hard-coding where it usually is would make this quietly stop
 * working for anyone who moved it — and the symptom would be a sidebar full of uuids, which is what
 * this exists to prevent.
 */
export function asWorker(projectRoot: string): { runId: string; task: string } | null {
  const parts = projectRoot.split('/').filter(Boolean);
  const at = parts.lastIndexOf('worktrees');
  if (at === -1 || parts.length < at + 3) return null;
  return { runId: parts[at + 1], task: parts[at + 2] };
}

export function SessionList({
  sessions,
  agents,
  activeId,
  onSelect,
  onNew,
}: {
  sessions: SessionSummary[];
  agents: AgentSummary[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onNew: (agentId: string, projectRoot: string) => void;
}) {
  const [filter, setFilter] = useState('');
  const [starting, setStarting] = useState(false);

  const groups = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    const kept = needle
      ? sessions.filter(
          (s) =>
            s.project_root.toLowerCase().includes(needle) ||
            s.agent_display_name.toLowerCase().includes(needle),
        )
      : sessions;

    const byGroup = new Map<string, { label: string; rows: Row[] }>();
    for (const s of kept) {
      const worker = asWorker(s.project_root);
      const key = worker ? `run:${worker.runId}` : s.project_root;
      const label = worker ? `Run ${worker.runId.slice(0, 8)}` : s.project_root;
      const entry = byGroup.get(key) ?? { label, rows: [] };
      entry.rows.push({
        session: s,
        // The task, for a worker. Otherwise the agent, which is what distinguishes two sessions in
        // the same directory.
        primary: worker ? worker.task : s.agent_display_name,
        secondary: worker ? s.agent_display_name : s.title,
        mono: worker !== null,
      });
      byGroup.set(key, entry);
    }
    // Insertion order, which is the order the daemon returned them in — creation order. Sorting
    // alphabetically would move a group under the reader's cursor whenever a new one appeared.
    return [...byGroup.entries()];
  }, [sessions, filter]);

  return (
    <div className="sidebar-panel">
      <div className="sidebar-actions">
        <button
          type="button"
          className="sidebar-new"
          onClick={() => setStarting((v) => !v)}
          data-testid="new-session"
          aria-expanded={starting}
        >
          New session
        </button>
      </div>

      {starting && (
        <NewSession
          agents={agents}
          onCancel={() => setStarting(false)}
          onStart={(agentId, root) => {
            setStarting(false);
            onNew(agentId, root);
          }}
        />
      )}

      {sessions.length > 2 && (
        <input
          className="sidebar-filter"
          type="search"
          value={filter}
          placeholder="Filter sessions"
          onChange={(e) => setFilter(e.target.value)}
          data-testid="session-filter"
        />
      )}

      {groups.length === 0 && (
        <p className="sidebar-empty">
          {sessions.length === 0 ? 'No sessions yet.' : 'Nothing matches that.'}
        </p>
      )}

      {groups.map(([key, group]) => (
        <section className="sidebar-group" key={key} data-testid={`group-${key}`}>
          {/* Stated once for the group rather than under every row. Truncated from the left, because
              the end of a path is the part that tells two of them apart. */}
          <h2 className="sidebar-group-name" title={group.label}>
            <bdi>{group.label}</bdi>
          </h2>
          <ul className="sidebar-sessions">
            {group.rows.map((row) => (
              <li key={row.session.id}>
                <button
                  type="button"
                  data-active={row.session.id === activeId}
                  onClick={() => onSelect(row.session.id)}
                  title={row.session.project_root}
                >
                  <span className={row.mono ? 'session-agent mono' : 'session-agent'}>
                    {row.primary}
                  </span>
                  {row.secondary && <span className="session-title">{row.secondary}</span>}
                </button>
              </li>
            ))}
          </ul>
        </section>
      ))}
    </div>
  );
}

/**
 * Picking an agent and a directory.
 *
 * Both are required and neither is guessed. A default project root would be this process's working
 * directory, which is wherever the shell happened to be launched from — a session silently rooted
 * there is a session whose file boundary is somewhere the user did not choose.
 */
function NewSession({
  agents,
  onStart,
  onCancel,
}: {
  agents: AgentSummary[];
  onStart: (agentId: string, projectRoot: string) => void;
  onCancel: () => void;
}) {
  const [agentId, setAgentId] = useState(agents[0]?.id ?? '');
  const [root, setRoot] = useState('');
  const ready = agentId !== '' && root.trim() !== '';

  if (agents.length === 0) {
    return (
      <p className="sidebar-empty" data-testid="no-agents">
        No agents are configured. Start the daemon with <code>--agent</code>, and the picker appears
        once one is declared — an empty menu says a choice exists and is broken, which is worse than
        no menu.
      </p>
    );
  }

  return (
    <form
      className="new-session"
      onSubmit={(e) => {
        e.preventDefault();
        if (ready) onStart(agentId, root.trim());
      }}
    >
      <select
        value={agentId}
        onChange={(e) => setAgentId(e.target.value)}
        aria-label="Agent"
        data-testid="new-session-agent"
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
        placeholder="/path/to/project"
        onChange={(e) => setRoot(e.target.value)}
        aria-label="Project directory"
        data-testid="new-session-root"
      />
      <div className="new-session-actions">
        <button type="submit" disabled={!ready} data-testid="new-session-start">
          Open
        </button>
        <button type="button" className="quiet" onClick={onCancel}>
          Cancel
        </button>
      </div>
    </form>
  );
}
