import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { SessionList, asWorker } from './SessionList';
import type { SessionSummary } from '../../lib/types';

function session(over: Partial<SessionSummary>): SessionSummary {
  return {
    id: 's1',
    agent_id: 'a',
    agent_display_name: 'Rich Agent',
    project_root: '/workspace',
    title: null,
    ...over,
  };
}

const noop = () => {};

describe('recognising a worker worktree', () => {
  it('reads the run and the task out of the path', () => {
    expect(asWorker('/home/me/.local/state/wkbd/worktrees/abc-123/feature-a')).toEqual({
      runId: 'abc-123',
      task: 'feature-a',
    });
  });

  /** The state directory is configurable, so the prefix cannot be hard-coded. */
  it('works wherever the state directory is', () => {
    expect(asWorker('/srv/data/worktrees/r1/t1')).toEqual({ runId: 'r1', task: 't1' });
  });

  it('leaves an ordinary project alone', () => {
    expect(asWorker('/workspace')).toBeNull();
    expect(asWorker('/home/me/worktrees')).toBeNull();
  });
});

describe('the session list', () => {
  /**
   * The defect this replaced. Grouping by directory is what a worker literally is — each has its own
   * worktree — and it produced one group per worker, each headed by a path ending in a run's uuid:
   * three headings of noise around one row each, and rows all reading "Worker".
   */
  it('puts one run\u2019s workers in one group, labelled by task', () => {
    const workers = ['base-module', 'docs', 'feature-a'].map((task, i) =>
      session({
        id: `w${i}`,
        agent_display_name: 'Worker',
        project_root: `/state/worktrees/run-abcdef12/${task}`,
      }),
    );
    render(
      <SessionList
        sessions={workers}
        agents={[]}
        activeId={null}
        onSelect={noop}
        onNew={noop}
      />,
    );
    const groups = document.querySelectorAll('.sidebar-group');
    expect(groups).toHaveLength(1);
    expect(groups[0].querySelectorAll('li')).toHaveLength(3);
    for (const task of ['base-module', 'docs', 'feature-a']) {
      expect(screen.getByText(task)).toBeTruthy();
    }
  });

  it('heads that group with the run rather than with a path', () => {
    render(
      <SessionList
        sessions={[
          session({ agent_display_name: 'Worker', project_root: '/state/worktrees/run-abcdef12/t' }),
        ]}
        agents={[]}
        activeId={null}
        onSelect={noop}
        onNew={noop}
      />,
    );
    const heading = document.querySelector('.sidebar-group-name');
    expect(heading?.textContent).toContain('Run ');
    expect(heading?.textContent).not.toContain('worktrees');
  });

  it('still groups ordinary sessions by their project', () => {
    render(
      <SessionList
        sessions={[
          session({ id: 'a', project_root: '/one' }),
          session({ id: 'b', project_root: '/one', agent_display_name: 'Other' }),
          session({ id: 'c', project_root: '/two' }),
        ]}
        agents={[]}
        activeId={null}
        onSelect={noop}
        onNew={noop}
      />,
    );
    expect(document.querySelectorAll('.sidebar-group')).toHaveLength(2);
  });

  /**
   * An empty menu says a choice exists and is broken, which is worse than no menu. Nothing is drawn
   * at all when the daemon was started without agents.
   */
  it('says why rather than drawing an empty agent picker', async () => {
    const user = userEvent.setup();
    render(
      <SessionList sessions={[]} agents={[]} activeId={null} onSelect={noop} onNew={noop} />,
    );
    await user.click(screen.getByTestId('new-session'));
    expect(screen.getByTestId('no-agents')).toBeTruthy();
    expect(screen.queryByTestId('new-session-agent')).toBeNull();
  });

  it('will not open a session without a directory', async () => {
    const user = userEvent.setup();
    const onNew = vi.fn();
    render(
      <SessionList
        sessions={[]}
        agents={[{ id: 'a', display_name: 'A', live_config_ids: [] }]}
        activeId={null}
        onSelect={noop}
        onNew={onNew}
      />,
    );
    await user.click(screen.getByTestId('new-session'));
    const start = screen.getByTestId('new-session-start') as HTMLButtonElement;
    expect(start.disabled).toBe(true);
    await user.click(start);
    expect(onNew).not.toHaveBeenCalled();
  });
});
