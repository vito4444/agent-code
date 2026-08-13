import { act, render, screen } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it } from 'vitest';
import { useStickToBottom } from './stickToBottom';

/**
 * jsdom has no layout, so `scrollHeight` and `clientHeight` are always 0 and nothing scrolls on
 * its own. These are defined per element to describe a record taller than its window, which is
 * the only situation where any of this behaviour exists.
 */
function makeScrollable(el: HTMLElement, contentHeight: number, windowHeight = 100) {
  Object.defineProperty(el, 'scrollHeight', { configurable: true, get: () => contentHeight });
  Object.defineProperty(el, 'clientHeight', { configurable: true, get: () => windowHeight });
}

function Record({ id = 's-1', lines = 1 }: { id?: string; lines?: number }) {
  const stick = useStickToBottom(id);
  return (
    <div>
      <div data-testid="scroller" ref={stick.ref} onScroll={stick.onScroll}>
        {Array.from({ length: lines }, (_, i) => (
          <p key={i}>line {i}</p>
        ))}
      </div>
      <span data-testid="stuck">{String(stick.stuck)}</span>
      {!stick.stuck && (
        <button type="button" onClick={stick.jump} data-testid="jump">
          Jump to latest
        </button>
      )}
    </div>
  );
}

/** A scroll event carrying a new offset, the way a browser delivers one. */
function scrollTo(el: HTMLElement, top: number) {
  el.scrollTop = top;
  act(() => {
    el.dispatchEvent(new Event('scroll', { bubbles: true }));
  });
}

describe('following a growing record', () => {
  it('starts attached, with nothing offering to take you anywhere', () => {
    render(<Record />);
    expect(screen.getByTestId('stuck').textContent).toBe('true');
    expect(screen.queryByTestId('jump')).toBeNull();
  });

  /**
   * The bug this exists for. A reader who scrolled up to read a diff was dragged back to the
   * bottom by the next streamed chunk, repeatedly, which makes a long turn unreadable.
   */
  it('lets go when the reader scrolls up', () => {
    render(<Record />);
    const el = screen.getByTestId('scroller');
    makeScrollable(el, 1000);

    scrollTo(el, 200);
    expect(screen.getByTestId('stuck').textContent).toBe('false');
    expect(screen.getByTestId('jump')).toBeTruthy();
  });

  it('re-attaches on its own when the reader scrolls back down', () => {
    render(<Record />);
    const el = screen.getByTestId('scroller');
    makeScrollable(el, 1000);

    scrollTo(el, 200);
    expect(screen.getByTestId('stuck').textContent).toBe('false');

    // Within the band rather than exactly at the end, which is what a real scroll produces.
    scrollTo(el, 1000 - 100 - 20);
    expect(screen.getByTestId('stuck').textContent).toBe('true');
  });

  it('goes back on request', () => {
    render(<Record />);
    const el = screen.getByTestId('scroller');
    makeScrollable(el, 1000);

    scrollTo(el, 0);
    expect(screen.getByTestId('stuck').textContent).toBe('false');

    act(() => {
      screen.getByTestId('jump').click();
    });
    expect(el.scrollTop).toBe(1000);
    expect(screen.getByTestId('stuck').textContent).toBe('true');
  });

  it('follows content appended while attached', async () => {
    function Growing() {
      const [lines, setLines] = useState(1);
      return (
        <div>
          <Record lines={lines} />
          <button type="button" onClick={() => setLines((n) => n + 1)} data-testid="grow">
            grow
          </button>
        </div>
      );
    }
    render(<Growing />);
    const el = screen.getByTestId('scroller');
    makeScrollable(el, 1000);
    el.scrollTop = 0;

    act(() => {
      screen.getByTestId('grow').click();
    });
    // The observer fires asynchronously, as a microtask.
    await act(async () => {
      await Promise.resolve();
    });

    expect(el.scrollTop).toBe(1000);
  });

  it('does not follow content appended after the reader scrolled away', async () => {
    function Growing() {
      const [lines, setLines] = useState(1);
      return (
        <div>
          <Record lines={lines} />
          <button type="button" onClick={() => setLines((n) => n + 1)} data-testid="grow">
            grow
          </button>
        </div>
      );
    }
    render(<Growing />);
    const el = screen.getByTestId('scroller');
    makeScrollable(el, 1000);

    scrollTo(el, 150);
    act(() => {
      screen.getByTestId('grow').click();
    });
    await act(async () => {
      await Promise.resolve();
    });

    expect(el.scrollTop).toBe(150);
  });

  it('starts a different conversation at its own end', () => {
    const { rerender } = render(<Record id="s-1" />);
    const el = screen.getByTestId('scroller');
    makeScrollable(el, 1000);

    scrollTo(el, 0);
    expect(screen.getByTestId('stuck').textContent).toBe('false');

    rerender(<Record id="s-2" />);
    expect(el.scrollTop).toBe(1000);
    expect(screen.getByTestId('stuck').textContent).toBe('true');
  });
});
