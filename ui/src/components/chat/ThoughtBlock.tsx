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
 *
 * The expanded body is drawn as a timeline: a marker where the thinking started, a rule running
 * down beside the text, and a closing marker that says it finished. That is not decoration. A
 * paragraph of reasoning indented under a heading reads as part of the answer, and the whole point
 * of this band is that it is *not* the answer — it is how the answer was arrived at. A rule down
 * the side says "this is an aside" in a way indentation cannot.
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
        {/* Empty: the arrow is drawn in CSS from the block's `data-expanded`. Every glyph tried for
            it sat wrong in at least one font, and a control that lands below the baseline reads as
            punctuation. */}
        <span className="thought-caret" aria-hidden="true" />
        <span className={live ? 'thought-title shimmer' : 'thought-title'}>{title}</span>
        {!live && !expanded && (
          /* The tick belongs on the header while the block is closed, because closed is the state a
             finished thought spends its life in and "this finished" is the only thing worth saying
             about it from the outside. Expanded, the closing row below carries it instead. */
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
        <div className="thought-timeline">
          {/* The nodes are drawn in CSS rather than set as glyphs. A glyph's position depends on the
              font's metrics, and the first version of this put a dot where a superscript would go and
              looked like a typo in the middle of the sentence. */}
          <span className="thought-node" data-live={live} aria-hidden="true" />
          <div className="thought-body" data-testid={`thought-body-${segment.id.raw}`}>
            {segment.text}
          </div>
          {!live && (
            <div className="thought-closed">
              <span className="thought-node done" aria-hidden="true" />
              <span>Done</span>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
