/**
 * The context-usage ring.
 *
 * Renders nothing at all when `percent` is null.
 *
 * That is the whole design. Most agents never report usage — the protocol's usage
 * notification is optional and its own specification says an agent that cannot give a
 * meaningful window size should send nothing rather than send a null — and as a client we
 * cannot compute the number ourselves: we do not know the agent's system prompt, which
 * rule files it loaded, how large its tool schemas are, or whether it has just compacted.
 * A locally estimated figure would be a different quantity wearing the same label.
 *
 * So the choices are: show the agent's number, or show nothing. A greyed-out control or the
 * word "unknown" both occupy the slot and imply the feature exists but is broken, and 0%
 * is an outright lie.
 */
export function ContextRing({
  percent,
  used,
  size,
  cost,
}: {
  percent: number | null;
  used?: number;
  size?: number;
  cost?: { amount: number; currency: string } | null;
}) {
  if (percent === null) return null;

  const clamped = Math.max(0, Math.min(100, percent));
  const radius = 7;
  const circumference = 2 * Math.PI * radius;
  const dash = (clamped / 100) * circumference;

  const level = clamped >= 90 ? 'critical' : clamped >= 75 ? 'warning' : 'normal';

  const title = [
    `${used?.toLocaleString() ?? '?'} of ${size?.toLocaleString() ?? '?'} tokens in context`,
    cost ? `cost so far: ${cost.amount.toFixed(4)} ${cost.currency}` : null,
    'Reported by the agent. This client does not estimate context usage.',
  ]
    .filter(Boolean)
    .join('\n');

  return (
    <span className="context-ring" data-level={level} title={title} data-testid="context-ring">
      <svg width="18" height="18" viewBox="0 0 18 18" aria-hidden="true">
        <circle className="context-track" cx="9" cy="9" r={radius} fill="none" strokeWidth="2" />
        <circle
          className="context-fill"
          cx="9"
          cy="9"
          r={radius}
          fill="none"
          strokeWidth="2"
          strokeDasharray={`${dash} ${circumference - dash}`}
          strokeLinecap="round"
          transform="rotate(-90 9 9)"
        />
      </svg>
      <span className="context-percent">{Math.round(clamped)}%</span>
    </span>
  );
}
