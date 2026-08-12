import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import { ThoughtBlock } from './ThoughtBlock';
import { Turn } from './Turn';
import type { SegmentView, TurnView } from '../../lib/types';

function segment(raw: string, state: 'live' | 'settled', text: string): SegmentView {
  return { id: { raw, synthesized: false }, kind: 'thought', text, state };
}

describe('ThoughtBlock', () => {
  it('is expanded while streaming and collapsed once settled', () => {
    const { rerender } = render(<ThoughtBlock segment={segment('m1', 'live', 'thinking')} />);
    expect(screen.getByTestId('thought-body-m1')).toBeTruthy();

    rerender(<ThoughtBlock segment={segment('m1', 'settled', 'thinking')} />);
    expect(screen.queryByTestId('thought-body-m1')).toBeNull();
  });

  it('keeps a user collapse even while the segment is still streaming', async () => {
    const user = userEvent.setup();
    render(<ThoughtBlock segment={segment('m1', 'live', 'thinking')} />);

    await user.click(screen.getByRole('button'));
    expect(screen.queryByTestId('thought-body-m1')).toBeNull();
  });

  it('keeps a user expand after the segment settles', async () => {
    const user = userEvent.setup();
    const { rerender } = render(<ThoughtBlock segment={segment('m1', 'settled', 'done')} />);

    await user.click(screen.getByRole('button'));
    expect(screen.getByTestId('thought-body-m1')).toBeTruthy();

    // A later unrelated update must not close what the reader deliberately opened.
    rerender(<ThoughtBlock segment={segment('m1', 'settled', 'done, with more detail')} />);
    expect(screen.getByTestId('thought-body-m1')).toBeTruthy();
  });

  it('says so when the boundary was inferred rather than declared', () => {
    render(
      <ThoughtBlock
        segment={{
          id: { raw: 'syn:1:1', synthesized: true },
          kind: 'thought',
          text: 'x',
          state: 'settled',
        }}
      />,
    );
    expect(screen.getByText('inferred')).toBeTruthy();
  });
});

describe('a turn with several thoughts', () => {
  /**
   * The scenario the layering exists for. Three thoughts separated by tool calls, with only
   * the newest still streaming. An implementation that drives expansion from "the turn has
   * not ended" opens all three and pushes the answer off screen.
   */
  const turn: TurnView = {
    turn: 1,
    prompt: 'why is config slow',
    stop_reason: null,
    segmentation_best_effort: false,
    items: [
      { type: 'segment', segment: segment('m1', 'settled', 'look at the loader') },
      {
        type: 'tool_call',
        call: {
          tool_call_id: 't1',
          title: 'Read config.rs',
          kind: 'read',
          status: 'completed',
          content: [],
          locations: [],
        },
      },
      { type: 'segment', segment: segment('m2', 'settled', 'it reads twice') },
      {
        type: 'tool_call',
        call: {
          tool_call_id: 't2',
          title: 'Run tests',
          kind: 'execute',
          status: 'completed',
          content: [],
          locations: [],
        },
      },
      { type: 'segment', segment: segment('m3', 'live', 'so I will cache it') },
    ],
  };

  it('expands only the newest thought', () => {
    render(<Turn turn={turn} />);

    expect(screen.queryByTestId('thought-body-m1')).toBeNull();
    expect(screen.queryByTestId('thought-body-m2')).toBeNull();
    expect(screen.getByTestId('thought-body-m3')).toBeTruthy();
  });

  it('renders the prompt as a quotation rather than a bubble', () => {
    const { container } = render(<Turn turn={turn} />);
    const quote = container.querySelector('blockquote.turn-prompt');
    expect(quote?.textContent).toBe('why is config slow');
  });
});
