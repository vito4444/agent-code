import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { RawInspector } from './RawInspector';
import * as api from '../../lib/api';
import type { RawFrame } from '../../lib/api';

vi.mock('../../lib/api');
const mocked = vi.mocked(api);

function frame(over: Partial<RawFrame> = {}): RawFrame {
  return {
    seq: 1,
    at_ms: 1_700_000_000_000,
    direction: 'to_agent',
    agent_id: 'rich',
    line: '{"jsonrpc":"2.0","id":1,"method":"initialize"}',
    clipped_bytes: null,
    malformed: false,
    ...over,
  };
}

beforeEach(() => {
  vi.resetAllMocks();
  mocked.fetchRawFramesSince.mockResolvedValue([]);
});

describe('a frame carrying an attachment', () => {
  /**
   * The two limits this screen has to keep apart. The daemon holds 4 kB of a frame, which is the
   * right amount to hold and about forty lines to show — so one prompt with a file in it filled
   * the viewport and buried every frame around it.
   */
  it('is short until it is asked to open', async () => {
    const user = userEvent.setup();
    const long = `{"method":"session/prompt","params":{"text":"${'x'.repeat(3000)}"}}`;
    mocked.fetchRawFrames.mockResolvedValue([frame({ seq: 7, line: long })]);

    render(<RawInspector />);
    const more = await screen.findByTestId('frame-more-7');

    const short = document.querySelector('.frame-line')?.textContent ?? '';
    expect(short.length).toBeLessThan(500);
    // The front is what carries the shape, so that is the end that is kept.
    expect(short).toContain('session/prompt');

    await user.click(more);
    expect((document.querySelector('.frame-line')?.textContent ?? '').length).toBe(long.length);
  });

  it('says how much the daemon did not keep', async () => {
    mocked.fetchRawFrames.mockResolvedValue([frame({ clipped_bytes: 250_000 })]);
    render(<RawInspector />);
    expect((await screen.findByText(/more, not kept/)).textContent).toContain('244 kB');
  });

  it('leaves a short frame alone', async () => {
    mocked.fetchRawFrames.mockResolvedValue([frame({ seq: 3 })]);
    render(<RawInspector />);
    await waitFor(() => expect(screen.getByText(/initialize/)).toBeTruthy());
    expect(screen.queryByTestId('frame-more-3')).toBeNull();
  });
});

describe('keeping up with the log', () => {
  /**
   * Fake timers rather than waiting a real second: the poll interval and the default `waitFor`
   * timeout are both one second, so the honest version of this test races itself.
   */
  const tick = async () => {
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1100);
    });
  };

  /**
   * The tail once, then only what is new. Re-fetching the whole log every second re-sent,
   * re-parsed and re-rendered every frame whether or not anything had changed — on the screen
   * somebody opens when an agent is already misbehaving.
   */
  it('asks only for what it has not seen', async () => {
    vi.useFakeTimers();
    mocked.fetchRawFrames.mockResolvedValue([frame({ seq: 4 }), frame({ seq: 9 })]);
    render(<RawInspector />);

    await tick();
    expect(mocked.fetchRawFramesSince).toHaveBeenCalledWith(9);

    // And moves on: the next poll asks from the newest it has, not from where it started.
    mocked.fetchRawFramesSince.mockResolvedValue([frame({ seq: 14 })]);
    await tick();
    await tick();
    expect(mocked.fetchRawFramesSince).toHaveBeenLastCalledWith(14);
    vi.useRealTimers();
  });

  it('starts from nothing when the log is empty', async () => {
    vi.useFakeTimers();
    mocked.fetchRawFrames.mockResolvedValue([]);
    render(<RawInspector />);
    await tick();
    expect(mocked.fetchRawFramesSince).toHaveBeenCalledWith(0);
    vi.useRealTimers();
  });
});
