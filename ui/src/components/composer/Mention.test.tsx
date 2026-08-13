import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { Composer } from './Composer';
import { activeMention, applyMention } from './MentionPicker';
import type { PathEntry } from '../../lib/types';

const FILES: PathEntry[] = [
  { path: 'src/main.rs', name: 'main.rs', is_dir: false },
  { path: 'src/lib.rs', name: 'lib.rs', is_dir: false },
];

function setup(over: Partial<React.ComponentProps<typeof Composer>> = {}) {
  const onSend = vi.fn();
  const searchPaths = vi.fn(async () => FILES);
  render(
    <Composer
      agentName="Test Agent"
      configOptions={[]}
      contextPercent={null}
      usage={null}
      busy={false}
      queue={[]}
      autonomy="ask_every_time"
      sessionId="s-1"
      promptCapabilities={{ image: false, audio: false, embedded_context: true }}
      searchPaths={searchPaths}
      onSend={onSend}
      onQueue={vi.fn()}
      onStopAndSend={vi.fn()}
      onCancel={vi.fn()}
      onConfigChange={vi.fn()}
      onAutonomyChange={vi.fn()}
      onDequeue={vi.fn()}
      {...over}
    />,
  );
  return { onSend, searchPaths };
}

describe('finding the mention under the caret', () => {
  it('reads the word after an @ that starts a word', () => {
    expect(activeMention('look at @src/ma', 15)).toEqual({ start: 8, query: 'src/ma' });
    expect(activeMention('@', 1)).toEqual({ start: 0, query: '' });
  });

  /** An address is not a mention, and opening a file list over one would be maddening. */
  it('ignores an @ in the middle of a word', () => {
    expect(activeMention('mail me@example.com', 19)).toBeNull();
  });

  it('ignores a mention the user has already finished', () => {
    expect(activeMention('@src/main.rs now explain it', 27)).toBeNull();
  });

  it('replaces only the mention being typed', () => {
    const text = 'look at @src/ma and tell me';
    const active = activeMention(text, 15)!;
    expect(applyMention(text, active, 'src/main.rs')).toBe('look at @src/main.rs and tell me');
  });
});

describe('attaching a file', () => {
  it('completes a path and sends it alongside the message', async () => {
    const user = userEvent.setup();
    const { onSend, searchPaths } = setup();

    await user.click(screen.getByTestId('composer-input'));
    await user.keyboard('read @main');

    await waitFor(() => expect(searchPaths).toHaveBeenCalled());
    await user.click(await screen.findByTestId('mention-option-src/main.rs'));

    expect((screen.getByTestId('composer-input') as HTMLTextAreaElement).value).toBe(
      'read @src/main.rs ',
    );
    expect(screen.getByTestId('attachments').textContent).toContain('src/main.rs');

    await user.click(screen.getByTestId('send'));
    expect(onSend).toHaveBeenCalledWith('read @src/main.rs', ['src/main.rs']);
  });

  /**
   * The composer's list and the text can disagree, and the text wins. Someone who deletes the
   * path out of their message has withdrawn the attachment, and sending it anyway would put a
   * file in front of the agent that the message never mentions.
   */
  it('drops an attachment whose path was deleted from the message', async () => {
    const user = userEvent.setup();
    const { onSend } = setup();

    await user.click(screen.getByTestId('composer-input'));
    await user.keyboard('read @main');
    await user.click(await screen.findByTestId('mention-option-src/main.rs'));
    await user.clear(screen.getByTestId('composer-input'));
    await user.type(screen.getByTestId('composer-input'), 'never mind');

    await user.click(screen.getByTestId('send'));
    expect(onSend).toHaveBeenCalledWith('never mind', []);
  });

  it('says what the agent will actually get', async () => {
    const user = userEvent.setup();
    setup({ promptCapabilities: { image: false, audio: false, embedded_context: false } });

    await user.click(screen.getByTestId('composer-input'));
    await user.keyboard('@main');
    await user.click(await screen.findByTestId('mention-option-src/main.rs'));

    expect(screen.getByTestId('attachments').textContent).toContain('path only');
  });

  it('offers nothing when there is no session to search', async () => {
    const user = userEvent.setup();
    setup({ sessionId: null, searchPaths: undefined });

    await user.click(screen.getByTestId('composer-input'));
    await user.keyboard('@main');

    expect(screen.queryByTestId('mention-picker')).toBeNull();
  });
});

describe('typing with an input method', () => {
  /**
   * The bug this exists for: pressing Enter to accept a Chinese candidate would send the
   * half-composed message. Every keydown during composition carries isComposing, and a
   * composer that does not check it is unusable in any language that needs an IME.
   */
  it('does not send when Enter confirms a composition', () => {
    const { onSend } = setup();
    const box = screen.getByTestId('composer-input');

    // What a browser does mid-composition: the box holds the in-progress text, and the
    // keydown for Enter is flagged as belonging to the candidate window. `fireEvent.change`
    // rather than assigning `.value`, which React's change tracker would swallow — the first
    // version of this test did that, left the component's text empty, and passed because
    // nothing could have been sent either way.
    fireEvent.change(box, { target: { value: '你好' } });
    fireEvent.keyDown(box, { key: 'Enter', isComposing: true });

    expect(onSend).not.toHaveBeenCalled();
  });

  it('still sends on a plain Enter', async () => {
    const user = userEvent.setup();
    const { onSend } = setup();

    await user.click(screen.getByTestId('composer-input'));
    await user.keyboard('hello{Enter}');

    expect(onSend).toHaveBeenCalledWith('hello', []);
  });
});
