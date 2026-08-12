import type { PermissionOption, TurnItem, TurnView } from '../../lib/types';
import { ThoughtBlock } from './ThoughtBlock';
import { ToolCallCard } from './ToolCallCard';

/**
 * One turn, laid out as three bands: how it thought, what it did, what it concluded.
 *
 * The user's own message is a quoted line, not a bubble. This is not two people talking; it
 * is an instruction followed by a record of the work. Bubbles imply symmetry that is not
 * there and waste horizontal space that the diffs and terminal output need.
 *
 * The answer gets the highest contrast in the turn. Everything above it is provenance, and
 * a reader skimming for the result should find it without reading the provenance first.
 */
export function Turn({ turn, onAnswerPermission }: {
  turn: TurnView;
  onAnswerPermission?: (requestId: string, optionId: string | null) => void;
}) {
  const running = turn.stop_reason === null;

  return (
    <article className="turn" data-turn={turn.turn} data-testid={`turn-${turn.turn}`}>
      <blockquote className="turn-prompt">{turn.prompt}</blockquote>

      <div className="turn-body">
        {turn.items.map((item, i) => (
          <ItemView key={itemKey(item, i)} item={item} onAnswerPermission={onAnswerPermission} />
        ))}
      </div>

      {turn.stop_reason !== null && turn.stop_reason !== 'end_turn' && (
        <div className="turn-stop" data-reason={turn.stop_reason}>
          {stopReasonText(turn.stop_reason)}
        </div>
      )}
      {running && <div className="turn-running">working…</div>}
      {turn.segmentation_best_effort && (
        <div className="turn-note">
          This agent does not label its messages, so the split between thinking and answering
          was inferred.
        </div>
      )}
    </article>
  );
}

function ItemView({
  item,
  onAnswerPermission,
}: {
  item: TurnItem;
  onAnswerPermission?: (requestId: string, optionId: string | null) => void;
}) {
  switch (item.type) {
    case 'segment':
      if (item.segment.kind === 'thought') {
        return <ThoughtBlock segment={item.segment} />;
      }
      if (item.segment.kind === 'user_echo') {
        return <div className="echo">{item.segment.text}</div>;
      }
      return (
        <div
          className="answer"
          data-live={item.segment.state === 'live'}
          data-testid={`answer-${item.segment.id.raw}`}
        >
          {item.segment.text}
        </div>
      );

    case 'tool_call':
      return <ToolCallCard call={item.call} />;

    case 'permission':
      return (
        <PermissionCard
          requestId={item.request_id}
          title={item.title}
          options={item.options}
          resolvedWith={item.resolved_with}
          auto={item.auto}
          onAnswer={onAnswerPermission}
        />
      );

    case 'error':
      return <div className="turn-error">{item.message}</div>;

    default:
      return null;
  }
}

/**
 * The inline permission prompt.
 *
 * Rendered in the transcript at the point where the agent asked, so the reader can see what
 * it was doing when it asked. A modal would lose that context, and an approval given
 * without context is the approval most likely to be wrong.
 */
export function PermissionCard({
  requestId,
  title,
  options,
  resolvedWith,
  auto,
  onAnswer,
}: {
  requestId: string;
  title: string;
  options: PermissionOption[];
  resolvedWith: string | null;
  auto: boolean;
  onAnswer?: (requestId: string, optionId: string | null) => void;
}) {
  const answered = resolvedWith !== null;
  const chosen = options.find((o) => o.option_id === resolvedWith);

  return (
    <div className="permission" data-answered={answered} data-testid={`permission-${requestId}`}>
      <div className="permission-title">
        <span className="permission-icon" aria-hidden="true">
          {'\u26a0'}
        </span>
        {title}
      </div>

      {answered ? (
        <div className="permission-outcome">
          {chosen ? chosen.name : 'cancelled'}
          {auto && (
            <span
              className="permission-auto"
              title="Answered from a remembered decision. The decision is bound to the exact content of this operation, so a change to the command or its paths asks again."
            >
              remembered
            </span>
          )}
        </div>
      ) : (
        <div className="permission-actions">
          {options.map((o) => (
            <button
              key={o.option_id}
              type="button"
              className="permission-button"
              data-kind={o.kind}
              onClick={() => onAnswer?.(requestId, o.option_id)}
            >
              {o.name}
            </button>
          ))}
          <button
            type="button"
            className="permission-button"
            data-kind="cancel"
            onClick={() => onAnswer?.(requestId, null)}
          >
            Cancel the turn
          </button>
        </div>
      )}
    </div>
  );
}

function itemKey(item: TurnItem, index: number): string {
  switch (item.type) {
    case 'segment':
      return `s:${item.segment.id.raw}`;
    case 'tool_call':
      return `t:${item.call.tool_call_id}`;
    case 'permission':
      return `p:${item.request_id}`;
    default:
      return `e:${index}`;
  }
}

function stopReasonText(reason: string): string {
  switch (reason) {
    case 'cancelled':
      return 'Cancelled. Tool calls that were still open are marked cancelled.';
    case 'max_tokens':
      return 'Stopped: the model reached its output limit.';
    case 'max_turn_requests':
      return 'Stopped: the agent reached its limit on requests within one turn.';
    case 'refusal':
      return 'The agent declined to continue.';
    case 'unknown':
      return 'The turn ended for a reason this client does not recognize.';
    default:
      return reason;
  }
}
