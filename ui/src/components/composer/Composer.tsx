import { useRef, useState } from 'react';
import type { ConfigOption, PathEntry, PromptCapabilities, QueuedMessage } from '../../lib/types';
import { ConfigSelect } from './ConfigSelect';
import { ContextRing } from './ContextRing';
import { DEDICATED_CATEGORIES } from './footerOrder';
import {
  activeMention,
  applyMention,
  MentionPicker,
  useMentionSearch,
} from './MentionPicker';

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
  /**
   * The session whose project the `@` list searches, and what its agent can be handed.
   *
   * Both absent means no completion: without a session there is no project root to search, and
   * an attachment control that cannot resolve anything is worse than none at all.
   */
  sessionId?: string | null;
  promptCapabilities?: PromptCapabilities;
  searchPaths?: (sessionId: string, q: string) => Promise<PathEntry[]>;
  onSend: (text: string, mentions: string[]) => void;
  onQueue: (text: string) => void;
  onStopAndSend: (text: string) => void;
  onCancel: () => void;
  onConfigChange: (optionId: string, value: string | boolean) => void;
  onAutonomyChange: (level: AutonomyLevel) => void;
  onDequeue: (id: string) => void;
}

export type AutonomyLevel = 'ask_every_time' | 'ask_outside_sandbox' | 'ask_for_destructive';

const noSearch = () => Promise.resolve([] as PathEntry[]);

/**
 * What the agent will actually receive for this attachment, said before it is sent.
 *
 * The daemon decides this again at send time and records what it did, so this is a prediction
 * and the transcript is the record. Both exist because the useful moment for "this agent only
 * gets the path" is while the user can still decide to paste the relevant part instead.
 */
function willSendAs(entry: PathEntry, caps: PromptCapabilities | undefined): string {
  if (entry.is_dir) return 'path only';
  if (/\.(png|jpe?g|gif|webp|svg)$/i.test(entry.path)) {
    return caps?.image ? 'image' : 'path only, this agent takes no images';
  }
  if (/\.(wav|mp3|ogg|m4a)$/i.test(entry.path)) {
    return caps?.audio ? 'audio' : 'path only, this agent takes no audio';
  }
  if (caps?.embedded_context) return 'contents';
  return 'path only, the agent reads it itself';
}

const AUTONOMY_LABELS: Record<AutonomyLevel, string> = {
  ask_every_time: 'Ask before every tool call',
  ask_outside_sandbox: 'Ask only when leaving the sandbox',
  ask_for_destructive: 'Ask only before destructive actions',
};

export function Composer(props: ComposerProps) {
  const [text, setText] = useState('');
  const [caret, setCaret] = useState(0);
  const [picked, setPicked] = useState<PathEntry[]>([]);
  const [highlighted, setHighlighted] = useState(0);
  const [dismissed, setDismissed] = useState(false);
  const box = useRef<HTMLTextAreaElement | null>(null);
  const canSubmit = text.trim().length > 0;

  const canAttach = props.sessionId != null && props.searchPaths != null;
  const active = canAttach && !dismissed ? activeMention(text, caret) : null;
  const { entries, loading } = useMentionSearch(
    active ? (props.sessionId ?? null) : null,
    active ? active.query : null,
    props.searchPaths ?? noSearch,
  );
  const open = active !== null && (loading || entries.length > 0);

  // What survived editing. A path the user deleted from the box must not still be attached,
  // and re-deriving from the text on every send is the only version of that which cannot
  // drift — a list kept alongside the text is a second source of truth for the same fact.
  const mentions = picked.map((p) => p.path).filter((p) => text.includes(`@${p}`));
  const attached = picked.filter((p) => mentions.includes(p.path));

  const choose = (entry: PathEntry) => {
    if (!active) return;
    const next = applyMention(text, active, entry.path);
    setText(next);
    setPicked((prev) => (prev.some((p) => p.path === entry.path) ? prev : [...prev, entry]));
    setHighlighted(0);
    const at = active.start + 1 + entry.path.length + 1;
    // The caret belongs after the inserted path. Left where it was, the next keystroke
    // reopens the list on the text we just completed.
    requestAnimationFrame(() => {
      box.current?.focus();
      box.current?.setSelectionRange(at, at);
      setCaret(at);
    });
  };

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
      props.onSend(text.trim(), mentions);
    }
    setText('');
    setPicked([]);
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

      {attached.length > 0 && (
        <ul className="attachments" data-testid="attachments">
          {attached.map((a) => (
            <li key={a.path}>
              <span className="attachment-path">{a.path}</span>
              <span className="attachment-how">{willSendAs(a, props.promptCapabilities)}</span>
              <button
                type="button"
                aria-label={`Remove ${a.path}`}
                data-testid={`attachment-remove-${a.path}`}
                onClick={() => {
                  setText((t) => t.replace(`@${a.path}`, '').replace(/ {2,}/g, ' ').trimStart());
                  setPicked((prev) => prev.filter((p) => p.path !== a.path));
                }}
              >
                {'\u00d7'}
              </button>
            </li>
          ))}
        </ul>
      )}

      <div className="composer-box" data-attaching={open}>
        {open && (
          <MentionPicker
            entries={entries}
            loading={loading}
            highlighted={highlighted}
            onPick={choose}
          />
        )}

        <textarea
          ref={box}
          className="composer-input"
          value={text}
          placeholder={
            props.busy
              ? 'Type to queue a message…'
              : canAttach
                ? 'Describe what you want done…  @ to attach a file'
                : 'Describe what you want done…'
          }
          rows={3}
          data-testid="composer-input"
          onChange={(e) => {
            setText(e.target.value);
            setCaret(e.target.selectionStart ?? e.target.value.length);
            setDismissed(false);
          }}
          onSelect={(e) => setCaret(e.currentTarget.selectionStart ?? 0)}
          onBlur={() => setDismissed(true)}
          onKeyDown={(e) => {
            // Nothing is a command while an input method is composing. Enter, arrows and
            // Escape all belong to the candidate window: on a Chinese or Japanese keyboard,
            // Enter confirms the characters being composed, and a composer that reads it as
            // "send" makes the box unusable for anyone typing in those languages. The
            // keystroke arrives with isComposing set, and it is the only reliable signal —
            // the keyCode is 229 on some browsers and the real key on others.
            if (e.nativeEvent.isComposing) return;

            if (open) {
              if (e.key === 'ArrowDown') {
                e.preventDefault();
                setHighlighted((h) => (entries.length === 0 ? 0 : (h + 1) % entries.length));
                return;
              }
              if (e.key === 'ArrowUp') {
                e.preventDefault();
                setHighlighted((h) =>
                  entries.length === 0 ? 0 : (h - 1 + entries.length) % entries.length,
                );
                return;
              }
              if (e.key === 'Enter' || e.key === 'Tab') {
                const entry = entries[highlighted];
                if (entry) {
                  e.preventDefault();
                  choose(entry);
                  return;
                }
              }
              if (e.key === 'Escape') {
                e.preventDefault();
                setDismissed(true);
                return;
              }
            }

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
