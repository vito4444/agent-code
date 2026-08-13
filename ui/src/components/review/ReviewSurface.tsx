import { useEffect, useState } from 'react';
import { DiffView, diffStats } from '../chat/DiffView';
import type { ChangeSet } from '../../lib/types';

/**
 * A change, read at full width.
 *
 * The inline diffs in a transcript are deliberately small — one card per tool call, several to a turn,
 * so an editor instance each is the wrong shape — and they said "open the review surface" for the
 * rest. There was no review surface. A dead reference is worse than the truncation it apologises for,
 * because it tells the reader a way to see the whole thing exists.
 *
 * One surface serves three questions, because they are the same question about different pairs of
 * commits: what did this task do, what would this run add to my branch, and what is the rest of this
 * truncated diff.
 *
 * ## Files are listed before they are read
 *
 * The list of paths comes first and is complete. A surface that renders every diff immediately makes
 * the shape of a twelve-file change something you discover by scrolling, and the first question about
 * a change is almost always which files it touched.
 */
export function ReviewSurface({
  title,
  subtitle,
  load,
  onClose,
}: {
  title: string;
  subtitle?: string;
  /** Deferred so the surface can be mounted before anybody knows what it will show. */
  load: () => Promise<ChangeSet>;
  onClose?: () => void;
}) {
  const [set, setSet] = useState<ChangeSet | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  // Escape closes it.
  //
  // A full-width surface that covers everything else needs a way out that does not require finding a
  // button, and this is the binding every reader already has. Bound while it is open and unbound when
  // it is not, so it cannot swallow the key from whatever is underneath.
  useEffect(() => {
    if (!onClose) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  useEffect(() => {
    let live = true;
    load()
      .then((s) => {
        if (!live) return;
        setSet(s);
        // The first file open, and only the first. Opening all of them is the behaviour this screen
        // exists to avoid, and opening none makes the common case — a one-file change — take a click
        // to say anything at all.
        setOpen(s.files[0]?.path ?? null);
      })
      .catch((e: unknown) => {
        if (live) setError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      live = false;
    };
  }, [load]);

  return (
    <section className="review" data-testid="review">
      <header className="review-head">
        <div>
          <h1>{title}</h1>
          {subtitle && <p className="review-subtitle">{subtitle}</p>}
        </div>
        {onClose && (
          <button type="button" className="review-close" onClick={onClose} data-testid="review-close">
            Close
          </button>
        )}
      </header>

      {error && <p className="review-error" data-testid="review-error">{error}</p>}
      {!set && !error && <p className="review-empty">Reading the change&hellip;</p>}

      {set?.files.length === 0 && (
        <p className="review-empty" data-testid="review-nothing">
          Nothing changed between these two commits.
        </p>
      )}

      {set && set.files.length > 0 && (
        <>
          <ol className="review-files" data-testid="review-files">
            {set.files.map((f) => (
              <li key={f.path}>
                <button
                  type="button"
                  data-open={f.path === open}
                  onClick={() => setOpen(f.path === open ? null : f.path)}
                  data-testid={`review-file-${f.path}`}
                >
                  <code>{f.path}</code>
                  <FileTag file={f} />
                </button>
              </li>
            ))}
          </ol>

          {set.truncated > 0 && (
            /* Said out loud. A surface that silently shows the first two hundred files of a larger
               change is showing a different change from the one being merged. */
            <p className="review-truncated" data-testid="review-truncated">
              {set.truncated} more file{set.truncated === 1 ? '' : 's'} changed and are not listed.
              This view caps how much it will read, so what is above is not the whole change.
            </p>
          )}

          {set.files
            .filter((f) => f.path === open)
            .map((f) =>
              f.omitted ? (
                <p className="review-omitted" key={f.path} data-testid={`review-omitted-${f.path}`}>
                  {f.omitted === 'binary'
                    ? 'This file is not text, so there is nothing to show line by line.'
                    : 'This file is too large to read here. Its contents are not shown rather than shown in part.'}
                </p>
              ) : (
                <DiffView
                  key={f.path}
                  path={f.path}
                  oldText={f.old_text}
                  newText={f.new_text ?? ''}
                  /* Far larger than inline. This is the screen somebody opened to see the whole
                     thing, so capping it at the inline limit would send them somewhere that does not
                     exist — which is the defect this surface was built to fix. */
                  maxLines={4000}
                />
              ),
            )}
        </>
      )}
    </section>
  );
}

/**
 * What happened to one file, in a word.
 *
 * Derived from which sides are present rather than from a status letter, because that is the same
 * source the diff itself renders from and two derivations of one fact can disagree.
 */
function FileTag({ file }: { file: ChangeSet['files'][number] }) {
  if (file.omitted) {
    return <span className="review-tag" data-kind="omitted">{file.omitted}</span>;
  }
  if (file.old_text === null) {
    return <span className="review-tag" data-kind="added">added</span>;
  }
  if (file.new_text === null) {
    return <span className="review-tag" data-kind="deleted">deleted</span>;
  }
  const { added, removed } = diffStats(file.old_text, file.new_text ?? '');
  return (
    <span className="review-tag" data-kind="changed">
      <span className="added">+{added}</span>
      <span className="removed">&minus;{removed}</span>
    </span>
  );
}
