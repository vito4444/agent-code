import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { StartHere } from './StartHere';

const AGENTS = [
  { id: 'rich', display_name: 'Rich Agent', live_config_ids: [] },
  { id: 'other', display_name: 'Other Agent', live_config_ids: [] },
];

describe('starting from an empty workbench', () => {
  /**
   * The point of the screen. The alternative asks somebody to find the sidebar, open a session, and
   * only then say what they wanted — three steps to express one intention, and the first two are
   * about our object model rather than about their work.
   */
  it('opens a session and sends the first turn in one act', async () => {
    const user = userEvent.setup();
    const onStart = vi.fn();
    render(<StartHere agents={AGENTS} onStart={onStart} onRuns={() => {}} />);

    await user.type(screen.getByTestId('start-prompt'), 'refactor the loader');
    await user.type(screen.getByTestId('start-root'), '/repo');
    await user.click(screen.getByTestId('start-send'));

    expect(onStart).toHaveBeenCalledWith('rich', '/repo', 'refactor the loader');
  });

  it('uses the agent that was picked rather than the first one', async () => {
    const user = userEvent.setup();
    const onStart = vi.fn();
    render(<StartHere agents={AGENTS} onStart={onStart} onRuns={() => {}} />);

    await user.selectOptions(screen.getByTestId('start-agent'), 'other');
    await user.type(screen.getByTestId('start-prompt'), 'go');
    await user.type(screen.getByTestId('start-root'), '/repo');
    await user.click(screen.getByTestId('start-send'));

    expect(onStart).toHaveBeenCalledWith('other', '/repo', 'go');
  });

  /**
   * No default directory. The obvious one is the process's working directory, which is wherever the
   * shell was launched from — and it is the root the file boundary is built from, so a session
   * silently rooted there is a session whose boundary somebody else chose.
   */
  it('will not start without a directory', async () => {
    const user = userEvent.setup();
    const onStart = vi.fn();
    render(<StartHere agents={AGENTS} onStart={onStart} onRuns={() => {}} />);

    await user.type(screen.getByTestId('start-prompt'), 'go');
    expect((screen.getByTestId('start-send') as HTMLButtonElement).disabled).toBe(true);
    await user.click(screen.getByTestId('start-send'));
    expect(onStart).not.toHaveBeenCalled();
  });

  it('will not start without something to say', async () => {
    const user = userEvent.setup();
    const onStart = vi.fn();
    render(<StartHere agents={AGENTS} onStart={onStart} onRuns={() => {}} />);

    await user.type(screen.getByTestId('start-root'), '/repo');
    await user.click(screen.getByTestId('start-send'));
    expect(onStart).not.toHaveBeenCalled();
  });

  /** The same binding as the session composer. A different one here traps whoever learned the other. */
  it('sends on enter and breaks the line on shift+enter', async () => {
    const user = userEvent.setup();
    const onStart = vi.fn();
    render(<StartHere agents={AGENTS} onStart={onStart} onRuns={() => {}} />);

    await user.type(screen.getByTestId('start-root'), '/repo');
    await user.click(screen.getByTestId('start-prompt'));
    await user.keyboard('first{Shift>}{Enter}{/Shift}second');
    expect(onStart).not.toHaveBeenCalled();

    await user.keyboard('{Enter}');
    expect(onStart).toHaveBeenCalledWith('rich', '/repo', 'first\nsecond');
  });

  /** An empty menu says a choice exists and is broken, which is worse than saying there is none. */
  it('draws no input at all when no agent is configured', () => {
    render(<StartHere agents={[]} onStart={() => {}} onRuns={() => {}} />);
    expect(screen.queryByTestId('start-prompt')).toBeNull();
    expect(screen.getByTestId('start-here-no-agents').textContent ?? '').toMatch(/--agent/);
  });
});
