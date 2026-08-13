import { useCallback, useEffect, useRef, useState } from 'react';

/**
 * Keeps a scrolling record pinned to its newest content, until the reader says otherwise.
 *
 * ## Why "until"
 *
 * Unconditional auto-scroll is the worse of the two bugs. A turn streams for a minute and emits
 * tool calls the whole time; a reader who scrolled up to look at a diff gets yanked back to the
 * bottom on the next chunk, over and over, and cannot read anything until the agent stops. So
 * scrolling up detaches and the record stays where it was put, with a way back.
 *
 * Re-attaching must not need mouse precision, which is why the bottom is a band rather than a
 * point: scrolling back to within a few dozen pixels counts as returning to the live edge.
 * Requiring an exact zero also fails outright on fractional device pixel ratios, where
 * `scrollHeight - scrollTop - clientHeight` settles at 0.5 and never reaches 0.
 *
 * ## Why a MutationObserver
 *
 * The alternative is for the caller to pass something that changes when the content grows, and
 * the thing that grows most often is the text inside the last segment — one character at a time.
 * Every cheap summary of that (turn count, item count) misses it, and the symptom is a record
 * that follows new tool calls but not the answer being typed. Observing the subtree catches
 * appended nodes and changed characters alike, and needs nothing from the caller.
 */
const BOTTOM_BAND_PX = 48;

export interface StickToBottom {
  ref: (node: HTMLElement | null) => void;
  /** True while the record is following new content. */
  stuck: boolean;
  /** Scrolls to the newest content and re-attaches. */
  jump: () => void;
  onScroll: () => void;
}

/**
 * @param key Identifies the record. A change jumps to the bottom and re-attaches, because
 *   arriving at a conversation scrolled to wherever the last one was is disorienting.
 */
export function useStickToBottom(key: string | null): StickToBottom {
  const node = useRef<HTMLElement | null>(null);
  const observer = useRef<MutationObserver | null>(null);
  const [stuck, setStuck] = useState(true);
  // Read from the scroll and mutation handlers, which must not change identity on every render:
  // a listener re-attached each time can drop the event that lands in the gap.
  const stuckRef = useRef(true);

  const toBottom = () => {
    const el = node.current;
    if (el) el.scrollTop = el.scrollHeight;
  };

  const onScroll = useCallback(() => {
    const el = node.current;
    if (!el) return;
    const next = el.scrollHeight - el.scrollTop - el.clientHeight <= BOTTOM_BAND_PX;
    if (next !== stuckRef.current) {
      stuckRef.current = next;
      setStuck(next);
    }
  }, []);

  const ref = useCallback((el: HTMLElement | null) => {
    observer.current?.disconnect();
    observer.current = null;
    node.current = el;
    if (!el) return;

    el.scrollTop = el.scrollHeight;

    // Not available in every environment this code is imported into — a test renderer without a
    // DOM, for one — and a missing observer must degrade to "does not follow" rather than throw.
    if (typeof MutationObserver === 'undefined') return;
    const obs = new MutationObserver(() => {
      if (stuckRef.current) el.scrollTop = el.scrollHeight;
    });
    obs.observe(el, { childList: true, subtree: true, characterData: true });
    observer.current = obs;
  }, []);

  const jump = useCallback(() => {
    toBottom();
    stuckRef.current = true;
    setStuck(true);
  }, []);

  useEffect(() => {
    toBottom();
    stuckRef.current = true;
    setStuck(true);
  }, [key]);

  useEffect(() => () => observer.current?.disconnect(), []);

  return { ref, stuck, jump, onScroll };
}
