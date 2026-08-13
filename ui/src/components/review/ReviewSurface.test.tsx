import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { ReviewSurface } from './ReviewSurface';
import type { ChangeSet, FileChange } from '../../lib/types';

function file(over: Partial<FileChange> = {}): FileChange {
  return {
    path: 'src/a.rs',
    old_text: 'one\ntwo\n',
    new_text: 'one\ntwo\nthree\n',
    omitted: null,
    ...over,
  };
}

function set(over: Partial<ChangeSet> = {}): ChangeSet {
  return { from: 'aaaa', to: 'bbbb', files: [file()], truncated: 0, ...over };
}

const load = (s: ChangeSet) => () => Promise.resolve(s);

describe('reading a change at full width', () => {
  /** The first question about a change is which files it touched, so the paths come first. */
  it('lists every path before rendering any diff', async () => {
    render(
      <ReviewSurface
        title="t"
        load={load(
          set({ files: [file({ path: 'a.rs' }), file({ path: 'b.rs' }), file({ path: 'c.rs' })] }),
        )}
      />,
    );
    const list = await screen.findByTestId('review-files');
    expect(list.querySelectorAll('li')).toHaveLength(3);
  });

  /**
   * The first file open and only the first. Opening all of them is the behaviour this screen exists
   * to avoid; opening none makes a one-file change take a click to say anything.
   */
  it('opens the first file and no others', async () => {
    render(
      <ReviewSurface title="t" load={load(set({ files: [file({ path: 'a.rs' }), file({ path: 'b.rs' })] }))} />,
    );
    await screen.findByTestId('review-files');
    expect(screen.getByTestId('diff-a.rs')).toBeTruthy();
    expect(screen.queryByTestId('diff-b.rs')).toBeNull();
  });

  it('opens another when asked, and closes the one that was open', async () => {
    const user = userEvent.setup();
    render(
      <ReviewSurface title="t" load={load(set({ files: [file({ path: 'a.rs' }), file({ path: 'b.rs' })] }))} />,
    );
    await user.click(await screen.findByTestId('review-file-b.rs'));
    expect(screen.getByTestId('diff-b.rs')).toBeTruthy();
    expect(screen.queryByTestId('diff-a.rs')).toBeNull();
  });

  it('marks an added file as added rather than as a change from nothing', async () => {
    render(<ReviewSurface title="t" load={load(set({ files: [file({ old_text: null })] }))} />);
    const row = await screen.findByTestId('review-file-src/a.rs');
    expect(row.textContent ?? '').toContain('added');
  });

  it('marks a deleted file as deleted', async () => {
    render(<ReviewSurface title="t" load={load(set({ files: [file({ new_text: null })] }))} />);
    const row = await screen.findByTestId('review-file-src/a.rs');
    expect(row.textContent ?? '').toContain('deleted');
  });

  /**
   * Marked rather than shown in part. A truncated diff looks like a small change, which is the one
   * wrong impression a review surface must not give.
   */
  it('says a file is not text instead of rendering something', async () => {
    render(
      <ReviewSurface
        title="t"
        load={load(set({ files: [file({ path: 'x.bin', old_text: null, new_text: null, omitted: 'binary' })] }))}
      />,
    );
    expect((await screen.findByTestId('review-omitted-x.bin')).textContent ?? '').toMatch(/not text/i);
    expect(screen.queryByTestId('diff-x.bin')).toBeNull();
  });

  /** A surface showing the first N files of a larger change is showing a different change. */
  it('says out loud when it did not read everything', async () => {
    render(<ReviewSurface title="t" load={load(set({ truncated: 7 }))} />);
    expect((await screen.findByTestId('review-truncated')).textContent ?? '').toContain('7 more');
  });

  it('says nothing changed rather than looking broken', async () => {
    render(<ReviewSurface title="t" load={load(set({ files: [] }))} />);
    expect(await screen.findByTestId('review-nothing')).toBeTruthy();
  });

  it('reports a failure to read instead of an empty surface', async () => {
    render(<ReviewSurface title="t" load={() => Promise.reject(new Error('no such run'))} />);
    expect((await screen.findByTestId('review-error')).textContent ?? '').toContain('no such run');
  });

  it('closes when asked', async () => {
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ReviewSurface title="t" load={load(set())} onClose={onClose} />);
    await user.click(await screen.findByTestId('review-close'));
    expect(onClose).toHaveBeenCalled();
  });
});

describe('getting out of it', () => {
  /**
   * A surface that covers everything else needs a way out that does not require finding a button, and
   * this is the binding every reader already has.
   */
  it('closes on escape', async () => {
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ReviewSurface title="t" load={load(set())} onClose={onClose} />);
    await screen.findByTestId('review-files');
    await user.keyboard('{Escape}');
    expect(onClose).toHaveBeenCalled();
  });

  /** Unbound when there is nowhere to go, so it cannot swallow the key from what is underneath. */
  it('does not listen when it cannot close', async () => {
    const user = userEvent.setup();
    render(<ReviewSurface title="t" load={load(set())} />);
    await screen.findByTestId('review-files');
    await user.keyboard('{Escape}');
    expect(screen.getByTestId('review')).toBeTruthy();
  });
});
