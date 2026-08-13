import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { Turn } from './Turn';
import type { TurnItem, TurnView } from '../../lib/types';
import { emptyTurn } from '../../lib/store';

function turn(items: TurnItem[]): TurnView {
  return { ...emptyTurn(1, 'do the thing'), items, stop_reason: 'end_turn' };
}

function file(over: Partial<Extract<TurnItem, { type: 'file' }>>): TurnItem {
  return {
    type: 'file',
    op: 'write',
    requested: '/w/src/a.rs',
    resolved: '/w/src/a.rs',
    allowed: true,
    refusal: null,
    bytes: 42,
    ...over,
  };
}

describe('files the client touched for the agent', () => {
  /**
   * The case that motivated this. An agent editing through the protocol's file methods — the path
   * the daemon encourages, because it is the only one that is bounded and logged — produced a
   * transcript containing a thought and an answer and no sign that any file had changed.
   */
  it('shows a write, because for some agents that is the entire visible work', () => {
    render(<Turn turn={turn([file({})])} />);
    expect(screen.getByTestId('file-/w/src/a.rs').textContent ?? '').toContain('/w/src/a.rs');
    expect(screen.getByTestId('file-/w/src/a.rs').textContent ?? '').toMatch(/wrote/i);
  });

  it('distinguishes a read from a write', () => {
    render(<Turn turn={turn([file({ op: 'read', requested: '/w/b.rs', resolved: '/w/b.rs' })])} />);
    expect(screen.getByTestId('file-/w/b.rs').textContent ?? '').toMatch(/read/i);
  });

  /**
   * Enforcement the reader cannot see is enforcement they cannot audit. This is the one line in a
   * transcript that says the agent tried to leave its workspace.
   */
  it('shows a refusal and says why in words', () => {
    render(
      <Turn
        turn={turn([
          file({
            requested: '/etc/passwd',
            resolved: null,
            allowed: false,
            refusal: 'outside-root',
            bytes: null,
          }),
        ])}
      />,
    );
    const row = screen.getByTestId('file-refused-/etc/passwd');
    expect(row.textContent ?? '').toContain('/etc/passwd');
    expect(row.textContent ?? '').toMatch(/outside the workspace/i);
  });

  it('names a symlink escape as a symlink escape rather than as a generic failure', () => {
    render(
      <Turn
        turn={turn([
          file({ requested: '/w/link', allowed: false, refusal: 'symlink-encountered', resolved: null }),
        ])}
      />,
    );
    expect(screen.getByTestId('file-refused-/w/link').textContent ?? '').toMatch(/symlink/i);
  });

  /**
   * A refusal must not read like the ordinary rows above it. The class is what carries that, so the
   * assertion is on the class rather than on a colour: a colour assertion in jsdom tests the
   * stylesheet loader, not the design.
   */
  it('sets a refused row apart from the ordinary ones', () => {
    render(
      <Turn
        turn={turn([
          file({ requested: '/w/ok.rs' }),
          file({ requested: '/nope', allowed: false, refusal: 'outside-root', resolved: null }),
        ])}
      />,
    );
    expect(screen.getByTestId('file-/w/ok.rs').className).not.toMatch(/refused/);
    expect(screen.getByTestId('file-refused-/nope').className).toMatch(/refused/);
  });

  /**
   * A reason nobody has written a sentence for yet is still more useful than no reason, so an
   * unrecognised kind falls through verbatim rather than collapsing to "refused".
   */
  it('passes an unfamiliar refusal reason through rather than flattening it', () => {
    render(
      <Turn
        turn={turn([
          file({ requested: '/w/x', allowed: false, refusal: 'some-new-kind', resolved: null }),
        ])}
      />,
    );
    expect(screen.getByTestId('file-refused-/w/x').textContent ?? '').toContain('some-new-kind');
  });
});

describe('a permission that expired', () => {
  function permission(over: Partial<Extract<TurnItem, { type: 'permission' }>>): TurnItem {
    return {
      type: 'permission',
      request_id: 'p1',
      title: 'Edit src/a.rs',
      options: [{ option_id: 'allow-once', name: 'Allow once', kind: 'allow_once' }],
      resolved_with: null,
      auto: false,
      expired: false,
      ...over,
    };
  }

  it('offers buttons while the request is still open', () => {
    render(<Turn turn={turn([permission({})])} />);
    expect(screen.getByText('Allow once')).toBeTruthy();
  });

  /**
   * The agent stopped waiting when the turn ended, so a button here posts a decision into a
   * conversation that has already moved on — and the record then shows a choice that influenced
   * nothing, indistinguishable from one that did.
   */
  it('offers none once the turn ended without an answer', () => {
    render(<Turn turn={turn([permission({ expired: true })])} />);
    expect(screen.queryByText('Allow once')).toBeNull();
    expect(screen.getByTestId('permission-expired-p1').textContent ?? '').toMatch(
      /not answered/i,
    );
  });

  /** An answer that did land still shows the answer, expired or not. */
  it('shows the decision when one was made', () => {
    render(<Turn turn={turn([permission({ resolved_with: 'allow-once' })])} />);
    expect(screen.getByTestId('permission-p1').textContent ?? '').toContain('Allow once');
  });
});
