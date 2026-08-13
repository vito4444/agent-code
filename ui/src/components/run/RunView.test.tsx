import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { RunList, StartRun } from './RunList';
import { RunView } from './RunView';
import { applyRunOne, emptyRun } from '../../lib/store';
import type { RunState } from '../../lib/store';
import type { RunEvent, TaskSummary } from '../../lib/types';

vi.mock('../../lib/api', () => ({
  mergeRun: vi.fn().mockResolvedValue({ commit: 'cccc3333dddd4444' }),
  abandonRun: vi.fn().mockResolvedValue(undefined),
  cancelRun: vi.fn().mockResolvedValue(undefined),
  createRun: vi.fn().mockResolvedValue({ id: 'r2' }),
}));

import { abandonRun, createRun, mergeRun } from '../../lib/api';

function fold(events: RunEvent[]): RunState {
  return events.reduce(applyRunOne, emptyRun('r1'));
}

function task(id: string, overrides: Partial<TaskSummary> = {}): TaskSummary {
  return {
    id,
    title: `Task ${id}`,
    depends_on: [],
    declared_paths: [`src/${id}.rs`],
    verify_cmd: 'cargo test',
    must_pass: [`${id}::works`],
    ...overrides,
  };
}

const started: RunEvent = {
  event: 'started',
  run_id: 'r1',
  goal: 'add a login endpoint',
  project_root: '/repo',
  base_commit: '9f1c2d3e4a5b6c7d8e9f0a1b',
};

/** Two tasks that can run at once, then one that needs both. */
const planned: RunEvent = {
  event: 'planned',
  tasks: [task('a'), task('b'), task('c', { depends_on: ['a', 'b'] })],
  waves: [['a', 'b'], ['c']],
  attempt: 1,
};

beforeEach(() => {
  vi.clearAllMocks();
});

describe('the task graph', () => {
  it('puts one wave in one container, so concurrency is readable as position', () => {
    render(<RunView run={fold([started, planned])} />);

    const waves = screen.getAllByTestId(/^wave-\d+$/);
    expect(waves).toHaveLength(2);
    // Two tasks side by side in the first wave means those two run at the same time; the
    // third is in its own container because it cannot start until they are done.
    expect(waves[0].querySelectorAll('.task-card')).toHaveLength(2);
    expect(waves[1].querySelectorAll('.task-card')).toHaveLength(1);
    expect(within(waves[0]).getByTestId('task-a')).toBeTruthy();
    expect(within(waves[1]).getByTestId('task-c')).toBeTruthy();
  });

  it('shows what a task committed to before it ran', () => {
    render(<RunView run={fold([started, planned])} />);
    const card = screen.getByTestId('task-a');
    expect(card.textContent).toContain('src/a.rs');
    expect(card.textContent).toContain('cargo test');
    expect(within(screen.getByTestId('task-a-must-pass')).getByText('a::works')).toBeTruthy();
  });

  it('states the dependency edge as the commits the task actually starts from', () => {
    render(
      <RunView
        run={fold([
          started,
          planned,
          {
            event: 'task_workspace_ready',
            task_id: 'c',
            branch: 'wkbd/r1/c',
            start_commit: 'ffff0000eeee1111',
            from_dependencies: ['aaaa1111bbbb2222', 'bbbb2222cccc3333'],
          },
        ])}
      />,
    );
    const line = screen.getByTestId('task-c-workspace');
    expect(line.textContent).toContain('wkbd/r1/c');
    expect(line.textContent).toContain('aaaa111');
    expect(line.textContent).toContain('bbbb222');
  });

  it('shows a placeholder instead of a graph while the run is still being planned', () => {
    render(<RunView run={fold([started])} />);

    expect(screen.queryByTestId('run-waves')).toBeNull();
    expect(screen.getByTestId('run-no-graph').textContent).toContain('No task graph yet');
  });
});

describe('acceptance results', () => {
  const failing = fold([
    started,
    {
      event: 'planned',
      tasks: [task('a', { must_pass: ['auth::login', 'db::migrate'] })],
      waves: [['a']],
      attempt: 1,
    },
    {
      event: 'task_verified',
      task_id: 'a',
      passed: false,
      missing_pass: ['auth::login'],
      regressed: ['db::migrate'],
      detail: '1 failed, 1 regressed',
    },
    { event: 'task_state_changed', task_id: 'a', status: 'failed', detail: null },
  ]);

  it('names the assertions rather than reporting that something failed', () => {
    render(<RunView run={failing} />);

    const missing = screen.getByTestId('task-a-missing-pass');
    const regressed = screen.getByTestId('task-a-regressed');

    expect(within(missing).getByText('auth::login')).toBeTruthy();
    expect(within(regressed).getByText('db::migrate')).toBeTruthy();
  });

  it('keeps a regression apart from something that never started passing', () => {
    render(<RunView run={failing} />);

    // The two lists mean different things — one task is unfinished, the other broke work that
    // already worked — and a single "failed" list would erase that difference.
    expect(screen.getByTestId('task-a-missing-pass').textContent).not.toContain('db::migrate');
    expect(screen.getByTestId('task-a-regressed').textContent).not.toContain('auth::login');
    expect(screen.getByTestId('task-a-verdict').dataset.passed).toBe('false');
  });

  it('marks a passing task with a check and no assertion list', () => {
    render(
      <RunView
        run={fold([
          started,
          planned,
          {
            event: 'task_verified',
            task_id: 'a',
            passed: true,
            missing_pass: [],
            regressed: [],
            detail: null,
          },
        ])}
      />,
    );
    const verdict = screen.getByTestId('task-a-verdict');
    expect(verdict.dataset.passed).toBe('true');
    expect(verdict.textContent).toContain('acceptance passed');
    expect(screen.queryByTestId('task-a-missing-pass')).toBeNull();
  });
});

describe('the merge gate', () => {
  const waiting = fold([
    started,
    planned,
    {
      event: 'task_verified',
      task_id: 'a',
      passed: true,
      missing_pass: [],
      regressed: [],
      detail: null,
    },
    { event: 'task_state_changed', task_id: 'a', status: 'completed', detail: null },
    { event: 'awaiting_merge', commit: 'cccc3333dddd4444', order: ['a'], excluded: ['b'] },
  ]);

  it('offers the decision rather than reporting a merge that has not happened', () => {
    render(<RunView run={waiting} />);

    expect(screen.getByTestId('merge-run')).toBeTruthy();
    expect(screen.getByTestId('abandon-run')).toBeTruthy();
    // Everything in the candidate passed acceptance, and it is still not merged. The gate has
    // to say that in words, because a green board otherwise reads as "done".
    expect(screen.getByTestId('merge-gate').textContent).toContain('nothing is merged until you say so');
    expect(screen.getByTestId('merge-gate').textContent).not.toMatch(/already merged/i);
    expect(screen.getByTestId('run-status-awaiting_merge')).toBeTruthy();
  });

  it('shows the candidate, the order it was combined in, and what was left out', () => {
    render(<RunView run={waiting} />);

    expect(screen.getByTestId('merge-order').textContent).toContain('a');
    expect(within(screen.getByTestId('merge-excluded')).getByText('b')).toBeTruthy();
  });

  it('merges only when the button is pressed, and only that run', async () => {
    const user = userEvent.setup();
    render(<RunView run={waiting} />);

    expect(mergeRun).not.toHaveBeenCalled();
    await user.click(screen.getByTestId('merge-run'));
    await waitFor(() => expect(mergeRun).toHaveBeenCalledWith('r1'));
    expect(abandonRun).not.toHaveBeenCalled();
  });

  it('abandons through its own call', async () => {
    const user = userEvent.setup();
    render(<RunView run={waiting} />);

    await user.click(screen.getByTestId('abandon-run'));
    await waitFor(() => expect(abandonRun).toHaveBeenCalledWith('r1'));
    expect(mergeRun).not.toHaveBeenCalled();
  });
});

describe('what the model contributed', () => {
  it('lists each replan with what triggered it', () => {
    render(
      <RunView
        run={fold([
          started,
          planned,
          { event: 'replanning', trigger: 'merge_conflict', task_id: 'b', attempt: 2 },
        ])}
      />,
    );

    const history = screen.getByTestId('replan-history');
    expect(within(history).getByText('merge_conflict')).toBeTruthy();
    expect(history.textContent).toContain('b');
  });

  it('shows why a drafted plan was refused', () => {
    render(
      <RunView
        run={fold([
          started,
          {
            event: 'plan_rejected',
            problems: ['task c declares no assertions', 'cycle: a -> b -> a'],
            attempt: 1,
          },
        ])}
      />,
    );

    const rejections = screen.getByTestId('plan-rejections');
    expect(within(rejections).getByText('task c declares no assertions')).toBeTruthy();
    expect(within(rejections).getByText('cycle: a -> b -> a')).toBeTruthy();
  });
});

describe('a candidate that would not combine', () => {
  it('shows what was dropped and what kept its work', () => {
    render(
      <RunView
        run={fold([
          started,
          planned,
          {
            event: 'merge_rejected',
            task_id: 'b',
            detail: 'src/auth.rs conflicts with a',
            merged: ['a', 'c'],
          },
        ])}
      />,
    );

    const rejected = screen.getByTestId('merge-rejected');
    expect(rejected.textContent).toContain('b');
    expect(rejected.textContent).toContain('src/auth.rs conflicts with a');

    const kept = screen.getByTestId('merge-rejection-0-kept');
    expect(within(kept).getByText('a')).toBeTruthy();
    expect(within(kept).getByText('c')).toBeTruthy();
  });
});

describe('the run list', () => {
  it('says which run wants a person', () => {
    const waiting = fold([
      started,
      planned,
      { event: 'awaiting_merge', commit: 'cccc3333', order: ['a'], excluded: [] },
    ]);
    render(<RunList runs={[waiting]} activeRunId={null} onSelect={vi.fn()} />);

    expect(screen.getByTestId('run-row-r1').textContent).toContain('waiting for you to merge');
  });

  it('starts a run from one sentence and a directory', async () => {
    const user = userEvent.setup();
    const onStarted = vi.fn();
    render(<StartRun onStarted={onStarted} />);

    await user.type(screen.getByTestId('run-goal-input'), 'add a login endpoint');
    await user.type(screen.getByTestId('run-root-input'), '/repo');
    await user.click(screen.getByTestId('run-start'));

    await waitFor(() => expect(createRun).toHaveBeenCalledWith('add a login endpoint', '/repo'));
    // The four fields this client knows are true, pinned exactly. The timestamp is asserted to be
    // present rather than to a value: it is this client's clock standing in until the daemon's
    // `started` event replaces it, and pinning a wall-clock reading would be pinning the test's own
    // execution time.
    expect(onStarted).toHaveBeenCalledWith(
      expect.objectContaining({
        id: 'r2',
        goal: 'add a login endpoint',
        project_root: '/repo',
        status: 'planning',
      }),
    );
    const seeded = onStarted.mock.calls[0][0] as { created_ms?: number };
    expect(typeof seeded.created_ms).toBe('number');
  });
});

describe('reaching what a task actually did', () => {
  /**
   * The gap this closes. A card could say a task passed and give no way to see what it did — the
   * files it wrote, the commands it ran, what it was refused — while the transcript was on disk and
   * already in the sidebar, one click away and unreachable from here.
   */
  it('offers the worker transcript, and names the task it belongs to', async () => {
    const user = userEvent.setup();
    const opened: string[] = [];
    render(
      <RunView
        run={fold([started, planned])}
        onOpenTranscript={(taskId) => opened.push(taskId)}
      />,
    );
    await user.click(screen.getByTestId('task-a-transcript'));
    expect(opened).toEqual(['a']);
  });

  /** A link that leads nowhere is worse than no link: it says the record is there, then proves it is not. */
  it('offers nothing when the session is gone', () => {
    render(<RunView run={fold([started, planned])} />);
    expect(screen.queryByTestId('task-a-transcript')).toBeNull();
  });
});
