import { useEffect, useState } from 'react';
import { fetchTerminalOutput } from '../../lib/api';

/**
 * Embedded terminal output.
 *
 * ACP references a terminal by id rather than inlining its output, so this fetches it. It
 * renders as a monospaced block with ANSI colour resolved, not as a live terminal emulator:
 * mounting one emulator per tool call in a long transcript is both slow and, on Linux under
 * WebKitGTK, one of the specific things that degrades. A real emulator belongs in the
 * dedicated terminal surface, where there is one of it.
 */
export function TerminalView({ terminalId }: { terminalId: string }) {
  const [state, setState] = useState<
    { kind: 'loading' } | { kind: 'ready'; text: string; truncated: boolean } | { kind: 'error'; message: string }
  >({ kind: 'loading' });

  useEffect(() => {
    let cancelled = false;
    fetchTerminalOutput(terminalId)
      .then((res) => {
        if (!cancelled) {
          setState({ kind: 'ready', text: res.output, truncated: res.truncated });
        }
      })
      .catch((e: unknown) => {
        if (!cancelled) {
          setState({ kind: 'error', message: e instanceof Error ? e.message : String(e) });
        }
      });
    return () => {
      cancelled = true;
    };
  }, [terminalId]);

  if (state.kind === 'loading') {
    return <div className="terminal terminal-loading">loading terminal output…</div>;
  }
  if (state.kind === 'error') {
    // Saying which terminal could not be read is more useful than a generic failure: the
    // id appears in the raw message inspector, so the two can be lined up.
    return (
      <div className="terminal terminal-error">
        could not read terminal {terminalId}: {state.message}
      </div>
    );
  }

  return (
    <div className="terminal" data-testid={`terminal-${terminalId}`}>
      {state.truncated && (
        <div className="terminal-truncated">
          output was truncated from the beginning to stay within the byte limit
        </div>
      )}
      <pre className="terminal-body">{stripAnsi(state.text)}</pre>
    </div>
  );
}

/**
 * Removes ANSI escape sequences.
 *
 * Colour is dropped rather than translated, for now. Translating it means either trusting
 * agent output enough to emit styled markup or writing a parser, and dropping it never
 * misrenders. The sequences themselves must go: shown literally they make output unreadable.
 */
export function stripAnsi(input: string): string {
  // CSI sequences plus the OSC form that sets window titles, which ConPTY injects.
  return input
    .replace(/\u001b\[[0-9;?]*[ -/]*[@-~]/g, '')
    .replace(/\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)/g, '')
    .replace(/\u001b[()][0-9A-B]/g, '');
}
