import { describe, expect, it } from 'vitest';
import { workerSessionFor } from './App';
import type { SessionSummary } from './lib/types';

function session(id: string, projectRoot: string): SessionSummary {
  return {
    id,
    agent_id: 'w',
    agent_display_name: 'Worker',
    project_root: projectRoot,
    title: null,
  };
}

const RUN = '788ed2ae-608e-475f-934b-ea63f1e0d860';
const OTHER = '11111111-2222-3333-4444-555555555555';

const sessions = [
  session('s-base', `/home/me/.local/state/wkbd/worktrees/${RUN}/base-module`),
  session('s-docs', `/home/me/.local/state/wkbd/worktrees/${RUN}/docs`),
  session('s-other', `/home/me/.local/state/wkbd/worktrees/${OTHER}/base-module`),
  session('s-plain', '/workspace'),
];

describe('finding the session a task ran in', () => {
  /**
   * What the run view's "open the transcript" link resolves. Matched on the worktree path, which
   * already contains the run and the task, so nothing extra has to be recorded to make the link.
   */
  it('finds the right task in the right run', () => {
    expect(workerSessionFor(sessions, RUN, 'base-module')).toBe('s-base');
    expect(workerSessionFor(sessions, RUN, 'docs')).toBe('s-docs');
  });

  /** The same task name exists under another run. Taking the first match would open the wrong one. */
  it('does not cross runs to find a task with the same name', () => {
    expect(workerSessionFor(sessions, OTHER, 'base-module')).toBe('s-other');
  });

  it('is null for a task whose session has gone', () => {
    expect(workerSessionFor(sessions, RUN, 'feature-a')).toBeNull();
  });

  /**
   * The weaker question, which decides whether the link is offered at all. A link that leads nowhere
   * is worse than no link: it says the record is there and then proves it is not.
   */
  it('reports whether any of a run\u2019s work is still open', () => {
    expect(workerSessionFor(sessions, RUN)).not.toBeNull();
    expect(workerSessionFor(sessions, 'a-run-with-nothing-left')).toBeNull();
  });

  it('ignores ordinary sessions entirely', () => {
    expect(workerSessionFor([session('s', '/workspace')], RUN, 'base-module')).toBeNull();
  });
});
