import { describe, expect, it } from 'vitest';
import { ruleScopeFor } from './App';
import type { SessionSummary } from './lib/types';

/**
 * Which path the interface calls "this project".
 *
 * A worker's `project_root` is a git worktree the run created and will delete. Offering it as a
 * rule's scope offers to write a rule against a directory that stops existing — the rule is saved,
 * the screen shows it as enabled, and nothing reads it again. Worse than not offering it, which is
 * the same argument the rules screen makes for keeping rules and inferred memories apart.
 *
 * The daemon reports both fields, so the whole decision is which one is read. It is a named
 * function in `App.tsx` so that this can test the decision rather than a copy of it.
 */
function worker(): SessionSummary {
  return {
    id: 's-1',
    agent_id: 'worker',
    agent_display_name: 'Worker',
    project_root:
      '/home/me/.local/state/wkbd/worktrees/025caf75-4e7c-4500-87dd-16a996d357d1/base-module',
    memory_scope: '/home/me/projects/demo',
    title: null,
    prompt_capabilities: { image: false, audio: false, embedded_context: false },
  };
}

describe('the project a rule attaches to', () => {
  it('is the project, not the worktree a worker happens to run in', () => {
    const s = worker();
    expect(ruleScopeFor(s)).toBe('/home/me/projects/demo');
    expect(ruleScopeFor(s)).not.toContain('worktrees');
  });

  it('is the same directory for a conversation somebody started', () => {
    const s: SessionSummary = { ...worker(), project_root: '/w', memory_scope: '/w' };
    expect(ruleScopeFor(s)).toBe(s.project_root);
  });

  it('is nothing when no session is open, so only a global rule can be written', () => {
    expect(ruleScopeFor(undefined)).toBeNull();
  });
});
