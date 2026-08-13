import { useState } from 'react';
import type { PlanEntry } from '../../lib/types';

/**
 * What the agent said it was going to do, and how far along it is.
 *
 * This data has been arriving since the first day and was rendered nowhere. An agent that reports a
 * plan is telling you the shape of the work before it does it — the one piece of information that
 * makes a long turn readable while it is still running — and it was being stored and dropped.
 *
 * ## Above the transcript, not in it
 *
 * A plan is replaced wholesale on every update: the protocol requires the agent to send every entry
 * each time, and the client to replace what it had. So it is not a transcript entry, which is a thing
 * that happened at a point in time — it is current state, and putting a mutating block in a scrolling
 * record would make the same block appear to say different things at different scroll positions.
 *
 * ## Open while there is anything left to do
 *
 * Not "open while something is running", which was the first rule and was wrong: a plan that has just
 * arrived has every entry pending, and that is the moment it is most worth reading — the agent is
 * saying what it is about to do. Collapsing it then shows `0/4` and makes somebody click to learn
 * anything. A plan with nothing left is history, and collapses to its count.
 *
 * Once somebody has clicked, the interface stops having opinions about that block, on the same rule as
 * the reasoning band and for the same reason.
 */
export function PlanPanel({
  plans,
}: {
  /** By plan id. An agent may keep several at once and the protocol requires them kept apart. */
  plans: Record<string, PlanEntry[]>;
}) {
  const ids = Object.keys(plans).filter((id) => plans[id].length > 0);
  if (ids.length === 0) return null;

  return (
    <div className="plans" data-testid="plans">
      {ids.map((id) => (
        <Plan key={id} id={id} entries={plans[id]} labelled={ids.length > 1} />
      ))}
    </div>
  );
}

function Plan({
  id,
  entries,
  labelled,
}: {
  id: string;
  entries: PlanEntry[];
  labelled: boolean;
}) {
  const [override, setOverride] = useState<boolean | null>(null);
  const done = entries.filter((e) => e.status === 'completed').length;
  const running = entries.some((e) => e.status === 'in_progress');
  // Anything not finished counts as outstanding, including cancelled — a cancelled step is something
  // the reader may well want to see, and hiding the plan the moment the agent gives up on part of it
  // is the opposite of useful.
  const outstanding = entries.some((e) => e.status !== 'completed');
  const expanded = override ?? outstanding;

  return (
    <section className="plan" data-testid={`plan-${id}`} data-expanded={expanded}>
      <button
        type="button"
        className="plan-head"
        aria-expanded={expanded}
        onClick={() => setOverride(!expanded)}
        data-testid={`plan-${id}-toggle`}
      >
        <span className="plan-caret" aria-hidden="true" />
        <span className="plan-label">
          {labelled ? id : 'Plan'}
        </span>
        {/* The count is the headline. It is the number somebody glances at between messages, and it
            has to be legible with the entries closed. */}
        <span className="plan-count" data-testid={`plan-${id}-count`}>
          {done}/{entries.length}
        </span>
        {running && <span className="plan-running">working</span>}
      </button>

      {expanded && (
        <ol className="plan-entries" data-testid={`plan-${id}-entries`}>
          {entries.map((entry, i) => (
            <li key={i} data-status={entry.status} data-priority={entry.priority}>
              <span className="plan-mark" aria-hidden="true">
                {mark(entry.status)}
              </span>
              <span className="plan-text">{entry.content}</span>
              {/* A status this build does not recognise is shown as itself. The protocol reserves
                  plain names for future versions and requires custom ones to start with an
                  underscore, so an unfamiliar value means a newer agent or a deliberate extension —
                  and drawing it as "pending" would state something the agent did not say. */}
              {!KNOWN_STATUSES.includes(entry.status) && (
                <span className="plan-unknown" data-testid={`plan-unknown-${entry.status}`}>
                  {entry.status}
                </span>
              )}
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}

const KNOWN_STATUSES = ['pending', 'in_progress', 'completed', 'cancelled'];

/**
 * The mark against one entry.
 *
 * Text rather than an icon font, and per status rather than a single bullet: the whole value of a plan
 * while it runs is telling apart what is done, what is happening, and what has not started.
 */
function mark(status: string): string {
  switch (status) {
    case 'completed':
      return '\u2713';
    case 'in_progress':
      return '\u203a';
    case 'cancelled':
      return '\u00d7';
    default:
      return '\u00b7';
  }
}
