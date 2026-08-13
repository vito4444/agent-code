import { useState } from 'react';
import { createRun } from '../../lib/api';
import { runBlocker } from '../../lib/store';
import type { RunState } from '../../lib/store';
import type { RunSummary } from '../../lib/types';
import { RunStatusBadge } from './RunView';

/**
 * Every run this machine has recorded.
 *
 * A row carries the sentence the run started from, where it ran, and what it is waiting on.
 * The last of those is the reason the list exists: with several runs going, the only question
 * a list can usefully answer is which of them needs a person, and a row that showed only a
 * status would make the reader open each one to find out.
 */
export function RunList({
  runs,
  activeRunId,
  onSelect,
}: {
  runs: RunState[];
  activeRunId: string | null;
  onSelect: (runId: string) => void;
}) {
  if (runs.length === 0) {
    return (
      <p className="runs-empty" data-testid="runs-empty">
        No runs yet. A run turns one sentence into a task graph and works on the tasks in
        parallel, in isolated checkouts.
      </p>
    );
  }

  return (
    <ul className="run-list" data-testid="run-list">
      {[...runs]
        // Newest first, by the daemon's clock. Runs with no known time sort last rather than first:
        // an unknown date is not evidence of being recent.
        .sort((a, b) => (b.createdMs ?? -1) - (a.createdMs ?? -1))
        .map((run) => {
        const blocker = runBlocker(run);
        const tasks = Object.values(run.tasks);
        const done = tasks.filter((t) => t.status === 'completed').length;

        return (
          <li key={run.id}>
            <button
              type="button"
              className="run-row"
              data-active={run.id === activeRunId}
              data-testid={`run-row-${run.id}`}
              onClick={() => onSelect(run.id)}
            >
              <span className="run-row-goal">{run.goal || run.id}</span>
              <span className="run-row-meta">
                <RunStatusBadge status={run.status} />
                {tasks.length > 0 && (
                  <span className="run-row-progress">
                    {done}/{tasks.length} tasks complete
                  </span>
                )}
                <span className="run-row-root">{run.project_root}</span>
              </span>
              {blocker && blocker.kind === 'awaiting_merge' && (
                <span className="run-row-waiting">waiting for you to merge</span>
              )}
            </button>
          </li>
        );
      })}
    </ul>
  );
}

/**
 * Starting a run.
 *
 * One sentence and a directory, because those are the only two things the orchestrator needs
 * that it cannot work out: the graph, the ordering and the acceptance criteria are all derived
 * downstream. Asking for anything more here would be asking the user to do the planner's job
 * before the planner has seen the repository.
 */
export function StartRun({ onStarted }: { onStarted: (run: RunSummary) => void }) {
  const [goal, setGoal] = useState('');
  const [projectRoot, setProjectRoot] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);

  const ready = goal.trim().length > 0 && projectRoot.trim().length > 0;

  const start = async () => {
    if (!ready) return;
    setPending(true);
    setError(null);
    try {
      // The daemon answers with an id and nothing else, because at this point there is nothing
      // else true about the run. The rest of the row is what we just sent.
      const { id } = await createRun(goal.trim(), projectRoot.trim());
      onStarted({
        id,
        goal: goal.trim(),
        project_root: projectRoot.trim(),
        status: 'planning',
        // This client's clock, and only until the daemon's `started` event arrives with the real one.
        // Seeding it keeps a just-started run at the top of the list instead of at the bottom, where
        // an unknown date would put it.
        created_ms: Date.now(),
      });
      setGoal('');
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setPending(false);
    }
  };

  return (
    <div className="run-start">
      <textarea
        className="run-start-goal"
        value={goal}
        rows={2}
        placeholder="What should this run achieve? One sentence."
        data-testid="run-goal-input"
        onChange={(e) => setGoal(e.target.value)}
      />
      <div className="run-start-actions">
        <label>
          <span className="run-start-label">Project</span>
          <input
            type="text"
            value={projectRoot}
            placeholder="/path/to/repository"
            data-testid="run-root-input"
            onChange={(e) => setProjectRoot(e.target.value)}
          />
        </label>
        <button
          type="button"
          disabled={!ready || pending}
          onClick={start}
          data-testid="run-start"
        >
          Start run
        </button>
      </div>
      {error && <p className="run-error">{error}</p>}
    </div>
  );
}
