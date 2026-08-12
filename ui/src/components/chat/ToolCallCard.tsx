import { useState } from 'react';
import type { ToolCallView, ToolContent } from '../../lib/types';
import { toolKindName } from '../../lib/types';
import { DiffView } from './DiffView';
import { TerminalView } from './TerminalView';

/**
 * One tool call.
 *
 * Collapsed by default, with two exceptions that matter more than the default:
 *
 * - **A failed call stays open.** The output of a command that exited non-zero is the thing
 *   the reader needs, and making them click for it is the wrong trade.
 * - **A call carrying a diff stays open.** A file change is a result, not a detail.
 *
 * Content is dispatched by type rather than concatenated into text. A diff rendered as
 * grey text is not a diff, and terminal output rendered as prose loses the alignment that
 * makes it readable. This is the whole reason for taking structured events from a protocol
 * instead of scraping a terminal.
 */
export function ToolCallCard({ call }: { call: ToolCallView }) {
  const failed = call.status === 'failed';
  const hasDiff = call.content.some((c) => c.type === 'diff');
  const [override, setOverride] = useState<boolean | null>(null);
  const expanded = override ?? (failed || hasDiff);

  const running = call.status === 'in_progress' || call.status === 'pending';

  return (
    <div
      className="tool-card"
      data-status={call.status}
      data-kind={toolKindName(call.kind)}
      data-testid={`tool-${call.tool_call_id}`}
    >
      <button
        type="button"
        className="tool-header"
        aria-expanded={expanded}
        onClick={() => setOverride(!expanded)}
      >
        <span className="tool-caret" aria-hidden="true">
          {expanded ? '\u25be' : '\u25b8'}
        </span>
        <span className="tool-kind">{toolKindName(call.kind)}</span>
        <span className="tool-title">{call.title}</span>
        <StatusPill status={call.status} running={running} />
        {hasDiff && <DiffStat content={call.content} />}
      </button>

      {expanded && (
        <div className="tool-body">
          {call.content.length === 0 && !running && (
            <div className="tool-empty">no output</div>
          )}
          {call.content.map((content, i) => (
            <ToolContentView key={i} content={content} />
          ))}
          {call.locations.length > 0 && (
            <ul className="tool-locations">
              {call.locations.map((l, i) => (
                <li key={i}>
                  <code>{l.path}</code>
                  {l.line !== null && <span className="tool-line">:{l.line}</span>}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}

function ToolContentView({ content }: { content: ToolContent }) {
  switch (content.type) {
    case 'diff':
      return (
        <DiffView path={content.path} oldText={content.old_text} newText={content.new_text} />
      );
    case 'terminal':
      return <TerminalView terminalId={content.terminal_id} />;
    case 'text':
      return <pre className="tool-text">{content.text}</pre>;
    default:
      return null;
  }
}

function StatusPill({ status, running }: { status: string; running: boolean }) {
  return (
    <span className="tool-status" data-running={running}>
      {running && <span className="spinner" aria-hidden="true" />}
      {status.replace('_', ' ')}
    </span>
  );
}

/** The `+N −M` counts shown on the collapsed header, so a change is legible while closed. */
function DiffStat({ content }: { content: ToolContent[] }) {
  let added = 0;
  let removed = 0;
  for (const c of content) {
    if (c.type !== 'diff') continue;
    const oldLines = (c.old_text ?? '').split('\n');
    const newLines = c.new_text.split('\n');
    const common = commonPrefixLength(oldLines, newLines);
    added += newLines.length - common;
    removed += (c.old_text === null ? 0 : oldLines.length) - common;
  }
  return (
    <span className="diff-stat">
      <span className="added">+{Math.max(added, 0)}</span>
      <span className="removed">&minus;{Math.max(removed, 0)}</span>
    </span>
  );
}

function commonPrefixLength(a: string[], b: string[]): number {
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  return i;
}
