import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, describe, expect, it } from 'vitest';
import { Settings, applyTheme, readTheme } from './Settings';

afterEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute('data-theme');
});

describe('choosing a theme', () => {
  /**
   * Three themes existed in the stylesheet with no way to choose one, which is the same shape of defect
   * as a mechanism with no caller: the work was done and nobody could reach it.
   */
  it('applies the chosen theme to the document', async () => {
    const user = userEvent.setup();
    render(<Settings agents={[]} />);
    await user.click(screen.getByTestId('theme-dark'));
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark');
  });

  /**
   * The default theme is the stylesheet's own `:root` block, so it is the absence of the attribute
   * rather than a value. Writing one would need a fourth block that duplicated it.
   */
  it('sets no attribute for the default theme', async () => {
    const user = userEvent.setup();
    render(<Settings agents={[]} />);
    await user.click(screen.getByTestId('theme-dark'));
    await user.click(screen.getByTestId('theme-paper'));
    expect(document.documentElement.hasAttribute('data-theme')).toBe(false);
  });

  it('remembers the choice', async () => {
    const user = userEvent.setup();
    render(<Settings agents={[]} />);
    await user.click(screen.getByTestId('theme-primer'));
    expect(readTheme()).toBe('primer');
  });

  it('falls back to the default when nothing was saved or the value is junk', () => {
    expect(readTheme()).toBe('paper');
    localStorage.setItem('wkbd.theme', 'chartreuse');
    expect(readTheme()).toBe('paper');
  });

  /** Failing to remember a preference is not a reason to refuse to apply it. */
  it('still applies a theme when storage is denied', () => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = () => {
      throw new Error('denied');
    };
    try {
      applyTheme('dark');
      expect(document.documentElement.getAttribute('data-theme')).toBe('dark');
    } finally {
      Storage.prototype.setItem = original;
    }
  });
});

describe('what settings does not offer', () => {
  /**
   * An agent is a command line the daemon was given. A provider or credential editor here would be a
   * page of controls that change nothing, and an empty provider list would say a choice exists and is
   * broken.
   */
  it('lists agents rather than offering to edit them', () => {
    render(
      <Settings
        agents={[{ id: 'rich', display_name: 'Rich Agent', live_config_ids: [] }]}
      />,
    );
    const list = screen.getByTestId('settings-agents');
    expect(list.textContent ?? '').toContain('rich');
    expect(list.textContent ?? '').toMatch(/starts a new session/i);
    expect(screen.queryByRole('textbox')).toBeNull();
  });

  /** Which is the honest answer to "why is the sidebar empty". */
  it('says when none are configured', () => {
    render(<Settings agents={[]} />);
    expect(screen.getByTestId('settings-no-agents')).toBeTruthy();
  });

  /**
   * What an agent can change without restarting decides whether a selector switches in place or opens
   * a new session, and it is absent for almost every agent. Saying so beats an empty list.
   */
  it('says what an agent can change in place when it can', () => {
    render(
      <Settings
        agents={[{ id: 'a', display_name: 'A', live_config_ids: ['thinking'] }]}
      />,
    );
    expect(screen.getByTestId('settings-agents').textContent ?? '').toContain(
      'can change thinking in place',
    );
  });
});
