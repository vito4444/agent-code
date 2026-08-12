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

    case 'file':
      return <FileAccessRow item={item} />;

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

/**
 * One file the client read or wrote for the agent.
 *
 * A row rather than a card. There can be many reads in a turn and giving each the weight of a tool
 * call would bury the two or three that changed something — but leaving them out entirely, which is
 * what happened first, means an agent that edits through the protocol's file methods produces a
 * transcript with no sign that any file changed.
 *
 * A refusal is not quiet. It is the one line in a transcript that says the agent tried to leave its
 * workspace, and enforcement the reader cannot see is enforcement they cannot audit. So it keeps the
 * attention colour, states the reason in words rather than as a code, and never collapses into the
 * run of ordinary rows above it.
 */
function FileAccessRow({
  item,
}: {
  item: Extract<TurnItem, { type: 'file' }>;
}) {
  const verb = item.op === 'write' ? 'Wrote' : 'Read';
  if (item.allowed) {
    return (
      <div className="file-row" data-testid={`file-${item.requested}`}>
        <span className="file-op">{verb}</span>
        <code className="file-path">{item.resolved ?? item.requested}</code>
        {item.bytes !== null && <span className="file-bytes">{formatBytes(item.bytes)}</span>}
      </div>
    );
  }
  return (
    <div className="file-row refused" data-testid={`file-refused-${item.requested}`}>
      <span className="file-op">Refused</span>
      <code className="file-path">{item.requested}</code>
      <span className="file-reason">{refusalText(item.refusal)}</span>
    </div>
  );
}

/**
 * Plain words for a refusal.
 *
 * The stored value is a stable identifier so that logs and tests can match on it, which is the right
 * shape for a machine and the wrong one for the person being told their agent was stopped. Anything
 * unrecognised falls through verbatim rather than becoming "refused": a reason nobody has written a
 * sentence for yet is still more useful than no reason.
 */
function refusalText(kind: string | null): string {
  switch (kind) {
    case 'outside-root':
      return 'outside the workspace';
    case 'symlink-encountered':
      return 'a symlink led outside the workspace';
    case 'parent-traversal':
      return 'the path tried to climb above the workspace';
    case 'not-found':
      return 'no such file';
    case 'not-a-directory':
      return 'a component of the path is not a directory';
    case 'too-large':
      return 'too large to return in one response';
    case null:
      return 'refused';
    default:
      return kind;
  }
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} kB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

function itemKey(item: TurnItem, index: number): string {
  switch (item.type) {
    case 'segment':
      return `s:${item.segment.id.raw}`;
    case 'tool_call':
      return `t:${item.call.tool_call_id}`;
    case 'permission':
      return `p:${item.request_id}`;
    case 'file':
      return `f:${index}:${item.requested}`;
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
