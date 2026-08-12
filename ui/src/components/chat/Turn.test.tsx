import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { Turn } from './Turn';
import type { TurnView } from '../../lib/types';

/**
 * Structural assertions about the three bands.
 *
 * Visual weight is a stylesheet question and cannot be judged from jsdom, so what is asserted
 * here is the structure the stylesheet depends on: the answer is its own element with its own
 * class, at the top level of the turn rather than nested inside a reasoning block. A review
 * against a real transcript reported the answer as indistinguishable from the reasoning, and
 * this is the part of that which is testable.
 */
function turnWith(items: TurnView['items'], stopReason: TurnView['stop_reason'] = 'end_turn'): TurnView {
  return {
    turn: 1,
    prompt: 'why is config slow',
    items,
    stop_reason: stopReason,
    segmentation_best_effort: false,
  };
}

describe('turn structure', () => {
  it('renders the answer as its own top-level element, not inside a thought block', () => {
    const { container } = render(
      <Turn
        turn={turnWith([
          {
            type: 'segment',
            segment: {
              id: { raw: 'm1', synthesized: false },
              kind: 'thought',
              text: 'I should look at the loader',
              state: 'settled',
            },
          },
          {
            type: 'segment',
            segment: {
              id: { raw: 'm2', synthesized: false },
              kind: 'message',
              text: 'The loader parsed the file on every call.',
              state: 'settled',
            },
          },
        ])}
      />,
    );

    const answer = container.querySelector('.answer');
    expect(answer, 'the answer must have its own class for the stylesheet to target').not.toBeNull();
    expect(answer?.textContent).toBe('The loader parsed the file on every call.');

    // Not nested inside the reasoning band, which would make it inherit the muted, smaller
    // treatment no matter what the answer rule says.
    expect(answer?.closest('.thought-block')).toBeNull();
    expect(answer?.closest('.thought-body')).toBeNull();
    expect(answer?.parentElement?.className).toContain('turn-body');

    // And the reasoning text is in the band that is meant to be quieter.
    const thought = container.querySelector('.thought-block');
    expect(thought).not.toBeNull();
    expect(container.querySelector('.answer .thought-block')).toBeNull();
  });

  it('marks a streaming answer so the caret rule can apply', () => {
    render(
      <Turn
        turn={turnWith(
          [
            {
              type: 'segment',
              segment: {
                id: { raw: 'm2', synthesized: false },
                kind: 'message',
                text: 'partial',
                state: 'live',
              },
            },
          ],
          null,
        )}
      />,
    );
    expect(screen.getByTestId('answer-m2').dataset.live).toBe('true');
  });

  it('renders the prompt as a blockquote', () => {
    const { container } = render(render_target());
    const quote = container.querySelector('blockquote.turn-prompt');
    expect(quote, 'the prompt is a quotation, not a bubble').not.toBeNull();
    expect(quote?.textContent).toBe('why is config slow');
  });

  it('explains a stop reason that is not a clean finish', () => {
    render(
      <Turn
        turn={turnWith(
          [
            {
              type: 'segment',
              segment: {
                id: { raw: 'm1', synthesized: false },
                kind: 'thought',
                text: 'x',
                state: 'settled',
              },
            },
          ],
          'cancelled',
        )}
      />,
    );
    expect(screen.getByText(/Cancelled/)).toBeTruthy();
  });

  it('says nothing extra when the turn finished cleanly', () => {
    const { container } = render(render_target());
    expect(container.querySelector('.turn-stop')).toBeNull();
  });
});

function render_target() {
  return (
    <Turn
      turn={turnWith([
        {
          type: 'segment',
          segment: {
            id: { raw: 'm1', synthesized: false },
            kind: 'message',
            text: 'done',
            state: 'settled',
          },
        },
      ])}
    />
  );
}
