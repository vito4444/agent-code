import { useState } from 'react';
import type { SegmentView } from '../../lib/types';

/**
 * The collapsible reasoning band.
 *
 * Expansion rule, in one line: `expanded = userOverride ?? (state === 'live')`.
 *
 * Two things that rule gets right and the obvious implementations get wrong.
 *
 * **It keys off the segment, not the turn.** A real agent thinks several times in one turn,
 * between tool calls. Treating "the turn has not ended" as "this thought is in progress"
 * expands every thought in the turn simultaneously and pushes the answer off screen. The
 * core guarantees at most one segment is live at a time, so following `state` follows the
 * newest one and nothing else.
 *
 * **A user override is permanent for that segment.** Once someone has clicked, the
 * interface stops having opinions about that block. There is deliberately no reset: each
 * segment owns its own override, so a new turn starts fresh because its segments are new,
 * and an old block the user opened stays open. Collapsing something the reader deliberately
 * opened is the specific behaviour that makes a transcript feel like it is fighting back.
 */
export function ThoughtBlock({ segment }: { segment: SegmentView }) {
  const [override, setOverride] = useState<boolean | null>(null);
  const live = segment.state === 'live';
  const expanded = override ?? live;

  const title = live ? 'Thinking' : 'Thought process';

  return (
    <div
      className="thought-block"
      data-live={live}
      data-expanded={expanded}
      data-testid={`thought-${segment.id.raw}`}
    >
      <button
        type="button"
        className="thought-header"
        aria-expanded={expanded}
        onClick={() => setOverride(!expanded)}
      >
        <span className="thought-caret" aria-hidden="true">
          {expanded ? '\u25be' : '\u25b8'}
        </span>
        <span className={live ? 'thought-title shimmer' : 'thought-title'}>{title}</span>
        {!live && (
          <span className="thought-done" aria-label="finished">
            {'\u2713'}
          </span>
        )}
        {segment.id.synthesized && (
          <span
            className="thought-inferred"
            title="This agent does not label its messages, so this boundary was inferred rather than declared."
          >
            inferred
          </span>
        )}
      </button>

      {expanded && (
        <div className="thought-body" data-testid={`thought-body-${segment.id.raw}`}>
          {segment.text}
        </div>
      )}
    </div>
  );
}
