import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { Turn } from './Turn';
import { applyOne, emptyTurn } from '../../lib/store';
import type { SessionState } from '../../lib/store';
import type { TurnView } from '../../lib/types';

function blank(): SessionState {
  return {
    turns: [],
    busy: false,
    usage: null,
    configOptions: [],
    plans: {},
    queue: [],
    fileAccesses: [],
  } as unknown as SessionState;
}

describe('what a turn cost', () => {
  it('reports how long it took', () => {
    const turn: TurnView = { ...emptyTurn(1, 'go'), startedMs: 1000, endedMs: 10_400 };
    render(<Turn turn={turn} />);
    expect(screen.getByTestId('turn-1-cost').textContent).toBe('9.4s');
  });

  /**
   * A running total attributed to one turn is the difference of two readings. Reporting the
   * total instead would say "1.1k tokens" about a turn that added twelve.
   */
  it('reports tokens as the difference across the turn, not the running total', () => {
    const turn: TurnView = {
      ...emptyTurn(1, 'go'),
      startedMs: 0,
      endedMs: 9000,
      usedBefore: 4000,
      usedAfter: 5100,
    };
    render(<Turn turn={turn} />);
    expect(screen.getByTestId('turn-1-cost').textContent).toBe('9s · 1.1k tokens');
  });

  it('says nothing about tokens when the agent never reported any', () => {
    const turn: TurnView = { ...emptyTurn(1, 'go'), startedMs: 0, endedMs: 3000 };
    render(<Turn turn={turn} />);
    expect(screen.getByTestId('turn-1-cost').textContent).toBe('3s');
  });

  it('shows nothing at all while the turn is still running', () => {
    const turn: TurnView = { ...emptyTurn(1, 'go'), startedMs: 1000 };
    render(<Turn turn={turn} />);
    expect(screen.queryByTestId('turn-1-cost')).toBeNull();
  });

  /** The fold has to capture the before-reading when the turn opens; by the end it is gone. */
  it('is derived by the fold from the event stream', () => {
    let s = blank();
    s = applyOne(s, { event: 'usage_changed', used: 4000, size: 100_000, cost: null }, 900);
    s = applyOne(s, { event: 'turn_started', turn: 1, prompt: 'go' }, 1000);
    s = applyOne(s, { event: 'usage_changed', used: 5100, size: 100_000, cost: null }, 9000);
    s = applyOne(s, { event: 'turn_ended', turn: 1, stop_reason: 'end_turn' }, 10_000);

    render(<Turn turn={s.turns[0]} />);
    expect(screen.getByTestId('turn-1-cost').textContent).toBe('9s · 1.1k tokens');
  });
});

describe('what was attached', () => {
  it('separates a file the agent received from one it only got a path to', () => {
    const turn: TurnView = {
      ...emptyTurn(1, 'compare these'),
      attachments: [
        {
          uri: 'file:///w/src/main.rs',
          name: 'src/main.rs',
          sent_as: 'embedded',
          bytes: 240,
          degraded: null,
        },
        {
          uri: 'file:///w/big.log',
          name: 'big.log',
          sent_as: 'link',
          bytes: 9_000_000,
          degraded: 'too-large',
        },
      ],
    };
    render(<Turn turn={turn} />);

    const list = screen.getByTestId('turn-1-attachments');
    expect(list.textContent).toContain('contents sent');
    expect(list.textContent).toContain('path only, too large to inline');
    expect(list.textContent).toContain('8.6 MB');
  });

  it('renders nothing when nothing was attached', () => {
    render(<Turn turn={emptyTurn(1, 'go')} />);
    expect(screen.queryByTestId('turn-1-attachments')).toBeNull();
  });
});
