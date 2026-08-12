import { useState } from 'react';
import type { ConfigOption, QueuedMessage } from '../../lib/types';
import { ConfigSelect } from './ConfigSelect';
import { ContextRing } from './ContextRing';
import { DEDICATED_CATEGORIES } from './footerOrder';

/**
 * The composer.
 *
 * Layout, top to bottom:
 *
 * 1. The autonomy row, above the input. It answers "under what conditions do my messages go
 *    out" — a standing policy.
 * 2. Queued messages, if any.
 * 3. The text area.
 * 4. The footer row, inside the input's border: which agent, which model, which thinking
 *    level, context usage, send.
 *
 * The autonomy setting is kept out of the footer on purpose. The footer is a row of
 * right-now state; autonomy is a policy that persists across turns. Putting them together
 * makes a cramped row where two unrelated kinds of thing look alike.
 */
export interface ComposerProps {
  agentName: string;
  configOptions: ConfigOption[];
  contextPercent: number | null;
  usage: { used: number; size: number; cost: { amount: number; currency: string } | null } | null;
  busy: boolean;
  queue: QueuedMessage[];
  autonomy: AutonomyLevel;
  /** True only when the connected agent has told us it can accept mid-turn input. */
  steeringSupported?: boolean;
  onSend: (text: string) => void;
  onQueue: (text: string) => void;
  onStopAndSend: (text: string) => void;
  onCancel: () => void;
  onConfigChange: (optionId: string, value: string | boolean) => void;
  onAutonomyChange: (level: AutonomyLevel) => void;
  onDequeue: (id: string) => void;
}

export type AutonomyLevel = 'ask_every_time' | 'ask_outside_sandbox' | 'ask_for_destructive';

const AUTONOMY_LABELS: Record<AutonomyLevel, string> = {
  ask_every_time: 'Ask before every tool call',
  ask_outside_sandbox: 'Ask only when leaving the sandbox',
  ask_for_destructive: 'Ask only before destructive actions',
};

export function Composer(props: ComposerProps) {
  const [text, setText] = useState('');
  const canSubmit = text.trim().length > 0;

  const dedicated: Record<string, ConfigOption | undefined> = {};
  const overflow: ConfigOption[] = [];
  for (const option of props.configOptions) {
    const slot = option.category ? DEDICATED_CATEGORIES[option.category] : undefined;
    if (slot && !dedicated[slot]) {
      dedicated[slot] = option;
    } else {
      overflow.push(option);
    }
  }

  const submit = () => {
    if (!canSubmit) return;
    if (props.busy) {
      props.onQueue(text.trim());
    } else {
      props.onSend(text.trim());
    }
    setText('');
  };

  return (
    <div className="composer">
      <div className="autonomy-row">
        <label>
          <span className="autonomy-label">Autonomy</span>
          <select
            value={props.autonomy}
            onChange={(e) => props.onAutonomyChange(e.target.value as AutonomyLevel)}
            data-testid="autonomy-select"
          >
            {Object.entries(AUTONOMY_LABELS).map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </label>
      </div>

      {props.queue.length > 0 && (
        <ul className="queue" data-testid="queue">
          {props.queue.map((m) => (
            <li key={m.id} className="queue-item">
              <span className="queue-text">{m.text}</span>
              <span className="queue-when">will be sent when this turn ends</span>
              <button
                type="button"
                className="queue-remove"
                aria-label={`Remove queued message: ${m.text}`}
                onClick={() => props.onDequeue(m.id)}
              >
                {'\u00d7'}
              </button>
            </li>
          ))}
        </ul>
      )}

      <div className="composer-box">
        <textarea
          className="composer-input"
          value={text}
          placeholder={props.busy ? 'Type to queue a message…' : 'Describe what you want done…'}
          rows={3}
          data-testid="composer-input"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault();
              submit();
            }
          }}
        />

        <div className="composer-footer" data-testid="composer-footer">
          <span className="footer-agent" data-testid="footer-agent">
            {props.agentName}
          </span>

          {dedicated.model && (
            <ConfigSelect option={dedicated.model} onChange={props.onConfigChange} />
          )}
          {dedicated.thought_level && (
            <ConfigSelect option={dedicated.thought_level} onChange={props.onConfigChange} />
          )}
          {overflow.length > 0 && (
            <details className="footer-overflow">
              <summary title="Other settings this agent exposes">
                {overflow.length} more
              </summary>
              <div className="footer-overflow-body">
                {overflow.map((o) => (
                  <ConfigSelect key={o.id} option={o} onChange={props.onConfigChange} />
                ))}
              </div>
            </details>
          )}

          <ContextRing
            percent={props.contextPercent}
            used={props.usage?.used}
            size={props.usage?.size}
            cost={props.usage?.cost}
          />

          <span className="footer-spacer" />

          {props.busy ? (
            <SendWhileBusy
              canSubmit={canSubmit}
              steeringSupported={props.steeringSupported === true}
              onQueue={() => {
                if (canSubmit) {
                  props.onQueue(text.trim());
                  setText('');
                }
              }}
              onStopAndSend={() => {
                if (canSubmit) {
                  props.onStopAndSend(text.trim());
                  setText('');
                }
              }}
              onCancel={props.onCancel}
            />
          ) : (
            <button
              type="button"
              className="send-button"
              disabled={!canSubmit}
              onClick={submit}
              data-testid="send"
            >
              Send
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * The send control while the agent is working.
 *
 * Two actions, not three. "Add to queue" holds the message until the turn ends; "stop and
 * send" cancels the turn and sends immediately, discarding whatever the agent had in flight.
 *
 * There is deliberately no "steer" that interrupts at the next convenient moment. The
 * protocol has no way to inject a message into a running turn — the proposal for it is
 * unmerged and has no owner — so a steer control could only be a queue with a different
 * label, or a cancel pretending to be something gentler. It appears only when the connected
 * agent has actually told us it can do it, which today no agent does.
 */
function SendWhileBusy({
  canSubmit,
  steeringSupported,
  onQueue,
  onStopAndSend,
  onCancel,
}: {
  canSubmit: boolean;
  steeringSupported: boolean;
  onQueue: () => void;
  onStopAndSend: () => void;
  onCancel: () => void;
}) {
  return (
    <span className="send-group">
      <button
        type="button"
        className="send-button"
        disabled={!canSubmit}
        onClick={onQueue}
        title="Held until the current turn finishes. Nothing is interrupted."
        data-testid="send-queue"
      >
        Add to queue
      </button>
      <button
        type="button"
        className="send-secondary"
        disabled={!canSubmit}
        onClick={onStopAndSend}
        title="Cancels the current turn, discarding work in flight, then sends this message."
        data-testid="send-stop"
      >
        Stop and send
      </button>
      {steeringSupported && (
        <button type="button" className="send-secondary" data-testid="send-steer">
          Steer
        </button>
      )}
      <button
        type="button"
        className="send-secondary"
        onClick={onCancel}
        data-testid="cancel-turn"
      >
        Stop
      </button>
    </span>
  );
}
