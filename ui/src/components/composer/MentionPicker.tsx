import { useEffect, useState } from 'react';
import type { PathEntry } from '../../lib/types';

/**
 * The `@` completion list.
 *
 * ## Why a plain textarea and not inline chips
 *
 * Every reference implementation of this renders mentions as inline widgets inside a rich text
 * editor. That needs `contenteditable`, and `contenteditable` plus an input method editor is a
 * well-known bad time: composing Chinese, Japanese or Korean text inside one produces duplicated
 * characters, lost candidates and carets that jump on commit, and the workarounds are per-browser.
 * A prompt is a place people write prose in their own language, so the editor has to be the
 * boring one that IMEs are tested against. The mention stays as `@path` text, and the strip under
 * the box says what will actually be sent.
 *
 * ## What counts as a mention
 *
 * An `@` at the start of a word, with no whitespace between it and the caret. `me@example.com`
 * does not open the list, because the `@` there follows a letter.
 */
export interface ActiveMention {
  /** Index of the `@`. */
  start: number;
  /** Text between the `@` and the caret, which is the query. */
  query: string;
}

export function activeMention(text: string, caret: number): ActiveMention | null {
  for (let i = caret - 1; i >= 0; i--) {
    const ch = text[i];
    if (ch === '@') {
      const before = i > 0 ? text[i - 1] : ' ';
      if (!/\s/.test(before)) return null;
      return { start: i, query: text.slice(i + 1, caret) };
    }
    // A mention is one word. Once whitespace intervenes, any earlier `@` belongs to a
    // mention the user has already finished typing.
    if (/\s/.test(ch)) return null;
  }
  return null;
}

/** Replaces the active `@query` with the chosen path, and leaves a trailing space. */
export function applyMention(text: string, active: ActiveMention, path: string): string {
  const before = text.slice(0, active.start);
  const after = text.slice(active.start + 1 + active.query.length);
  const spacer = after.startsWith(' ') ? '' : ' ';
  return `${before}@${path}${spacer}${after}`;
}

export function MentionPicker({
  entries,
  loading,
  highlighted,
  onPick,
}: {
  entries: PathEntry[];
  loading: boolean;
  highlighted: number;
  onPick: (entry: PathEntry) => void;
}) {
  if (!loading && entries.length === 0) {
    return (
      <div className="mention-picker" data-testid="mention-picker">
        <p className="mention-empty">Nothing in this project matches.</p>
      </div>
    );
  }

  return (
    <ul className="mention-picker" role="listbox" data-testid="mention-picker">
      {entries.map((e, i) => (
        <li key={e.path} role="option" aria-selected={i === highlighted}>
          <button
            type="button"
            data-highlighted={i === highlighted}
            data-testid={`mention-option-${e.path}`}
            // Pointer-down rather than click: a click fires after the textarea has already
            // lost focus, and the blur handler closes the list out from under it.
            onMouseDown={(ev) => {
              ev.preventDefault();
              onPick(e);
            }}
          >
            <span className="mention-name">
              {e.name}
              {e.is_dir ? '/' : ''}
            </span>
            <span className="mention-path">{e.path}</span>
          </button>
        </li>
      ))}
    </ul>
  );
}

/**
 * Debounced lookup for the completion list.
 *
 * The delay is not for the daemon's benefit — it walks a bounded tree — but for the list's: a
 * fetch per keystroke arrives out of order often enough that the list flickers between the
 * results for `ma` and `mai` while the user is still typing `main`.
 */
export function useMentionSearch(
  sessionId: string | null,
  query: string | null,
  search: (sessionId: string, q: string) => Promise<PathEntry[]>,
): { entries: PathEntry[]; loading: boolean } {
  const [entries, setEntries] = useState<PathEntry[]>([]);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (sessionId === null || query === null) {
      setEntries([]);
      setLoading(false);
      return;
    }
    let live = true;
    setLoading(true);
    const timer = setTimeout(() => {
      search(sessionId, query)
        .then((r) => {
          if (live) {
            setEntries(r);
            setLoading(false);
          }
        })
        .catch(() => {
          if (live) {
            setEntries([]);
            setLoading(false);
          }
        });
    }, 80);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [sessionId, query, search]);

  return { entries, loading };
}
