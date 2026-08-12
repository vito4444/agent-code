/**
 * A small unified diff, for embedding inside a tool-call card.
 *
 * Deliberately a hand-rolled line diff rather than a full editor. This view has to appear
 * inline, potentially many times in one transcript, so mounting an editor instance per
 * change is the wrong shape; the full-screen review surface is where an editor belongs.
 *
 * The algorithm is a longest-common-subsequence over lines. It is not the cleverest
 * possible diff, but it is deterministic and has no dependency, and for the sizes that
 * appear inline the difference is not visible. Anything large enough for the difference to
 * matter should be opened in the review surface instead.
 */

export interface DiffViewProps {
  path: string;
  oldText: string | null;
  newText: string;
  /** Lines to show before falling back to a summary. */
  maxLines?: number;
}

type Row = { kind: 'ctx' | 'add' | 'del'; text: string; oldNo: number | null; newNo: number | null };

export function DiffView({ path, oldText, newText, maxLines = 60 }: DiffViewProps) {
  const isNewFile = oldText === null;
  const rows = isNewFile
    ? newText.split('\n').map((text, i) => ({
        kind: 'add' as const,
        text,
        oldNo: null,
        newNo: i + 1,
      }))
    : diffLines(oldText.split('\n'), newText.split('\n'));

  const truncated = rows.length > maxLines;
  const shown = truncated ? rows.slice(0, maxLines) : rows;
  const { added, removed } = diffStats(oldText, newText);

  return (
    <div className="diff" data-testid={`diff-${path}`}>
      <div className="diff-head">
        <code className="diff-path">{path}</code>
        {isNewFile && <span className="diff-badge">new file</span>}
        <span className="diff-counts">
          <span className="added">+{added}</span>
          <span className="removed">&minus;{removed}</span>
        </span>
      </div>
      <table className="diff-table">
        <tbody>
          {shown.map((row, i) => (
            <tr key={i} className={`diff-row ${row.kind}`}>
              <td className="diff-lineno">{row.oldNo ?? ''}</td>
              <td className="diff-lineno">{row.newNo ?? ''}</td>
              <td className="diff-marker" aria-hidden="true">
                {row.kind === 'add' ? '+' : row.kind === 'del' ? '\u2212' : ' '}
              </td>
              <td className="diff-text">{row.text === '' ? '\u00a0' : row.text}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {truncated && (
        <div className="diff-truncated">
          {rows.length - maxLines} more lines. Open the review surface to see the whole change.
        </div>
      )}
    </div>
  );
}

/**
 * How many lines a change adds and removes.
 *
 * Shared with the collapsed card header rather than approximated there. It was approximated there,
 * with a common-prefix heuristic, and the two disagreed on screen: a card reading `+5 −3` above a
 * diff reading `+4 −2` for the same change, because a shared trailing line was counted as both an
 * addition and a deletion by the heuristic and as context by the real diff. Two numbers for one
 * thing, side by side, is worse than either number alone.
 */
export function diffStats(oldText: string | null, newText: string): { added: number; removed: number } {
  if (oldText === null) {
    return { added: newText.split('\n').length, removed: 0 };
  }
  const rows = diffLines(oldText.split('\n'), newText.split('\n'));
  return {
    added: rows.filter((r) => r.kind === 'add').length,
    removed: rows.filter((r) => r.kind === 'del').length,
  };
}

export function diffLines(a: string[], b: string[]): Row[] {
  // Trim the matching head and tail first. Real edits touch a few lines in a long file, so
  // this keeps the quadratic table small enough to be irrelevant in the common case.
  let head = 0;
  while (head < a.length && head < b.length && a[head] === b[head]) head++;
  let tail = 0;
  while (
    tail < a.length - head &&
    tail < b.length - head &&
    a[a.length - 1 - tail] === b[b.length - 1 - tail]
  ) {
    tail++;
  }

  const midA = a.slice(head, a.length - tail);
  const midB = b.slice(head, b.length - tail);

  const rows: Row[] = [];
  let oldNo = 1;
  let newNo = 1;

  const contextBefore = Math.max(0, head - 3);
  for (let i = contextBefore; i < head; i++) {
    rows.push({ kind: 'ctx', text: a[i], oldNo: i + 1, newNo: i + 1 });
  }
  oldNo = head + 1;
  newNo = head + 1;

  for (const step of lcsDiff(midA, midB)) {
    if (step.kind === 'ctx') {
      rows.push({ kind: 'ctx', text: step.text, oldNo, newNo });
      oldNo++;
      newNo++;
    } else if (step.kind === 'del') {
      rows.push({ kind: 'del', text: step.text, oldNo, newNo: null });
      oldNo++;
    } else {
      rows.push({ kind: 'add', text: step.text, oldNo: null, newNo });
      newNo++;
    }
  }

  const tailStart = a.length - tail;
  for (let i = tailStart; i < Math.min(a.length, tailStart + 3); i++) {
    rows.push({ kind: 'ctx', text: a[i], oldNo: oldNo++, newNo: newNo++ });
  }

  return rows;
}

function lcsDiff(a: string[], b: string[]): { kind: 'ctx' | 'add' | 'del'; text: string }[] {
  const n = a.length;
  const m = b.length;
  // Guard against pathological inputs: an inline diff is not the place to spend seconds.
  if (n * m > 250_000) {
    return [
      ...a.map((text) => ({ kind: 'del' as const, text })),
      ...b.map((text) => ({ kind: 'add' as const, text })),
    ];
  }

  const table: number[][] = Array.from({ length: n + 1 }, () => new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      table[i][j] =
        a[i] === b[j] ? table[i + 1][j + 1] + 1 : Math.max(table[i + 1][j], table[i][j + 1]);
    }
  }

  const out: { kind: 'ctx' | 'add' | 'del'; text: string }[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push({ kind: 'ctx', text: a[i] });
      i++;
      j++;
    } else if (table[i + 1][j] >= table[i][j + 1]) {
      out.push({ kind: 'del', text: a[i] });
      i++;
    } else {
      out.push({ kind: 'add', text: b[j] });
      j++;
    }
  }
  while (i < n) out.push({ kind: 'del', text: a[i++] });
  while (j < m) out.push({ kind: 'add', text: b[j++] });
  return out;
}
