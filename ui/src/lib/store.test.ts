import { describe, expect, it, vi } from 'vitest';
import { EventBatcher, applyOne, contextPercent, emptySession, liveSegment } from './store';
import type { TurnItem } from './types';
import type { SessionState } from './store';
import type { EventPayload, WkbdEvent } from './types';

function fold(payloads: EventPayload[]): SessionState {
  return payloads.reduce(applyOne, emptySession());
}

function seg(raw: string, synthesized = false) {
  return { raw, synthesized };
}

describe('event folding', () => {
  it('keeps at most one segment live across a multi-thought turn', () => {
    const state = fold([
      { event: 'turn_started', turn: 1, prompt: 'go' },
      { event: 'segment_started', segment: seg('m1'), kind: 'thought' },
      { event: 'segment_chunk', segment: seg('m1'), text: 'first' },
      { event: 'segment_settled', segment: seg('m1') },
      { event: 'tool_call_started', tool_call_id: 't1', title: 'read', kind: 'read', status: 'completed' },
      { event: 'segment_started', segment: seg('m2'), kind: 'thought' },
      { event: 'segment_chunk', segment: seg('m2'), text: 'second' },
    ]);

    const turn = state.turns[0];
    const live = turn.items.filter((i: TurnItem) => i.type === 'segment' && i.segment.state === 'live');
    expect(live).toHaveLength(1);
    expect(liveSegment(turn)?.id.raw).toBe('m2');
  });

  it('settles every segment when the turn ends', () => {
    const state = fold([
      { event: 'turn_started', turn: 1, prompt: 'go' },
      { event: 'segment_started', segment: seg('m1'), kind: 'thought' },
      { event: 'segment_chunk', segment: seg('m1'), text: 'thinking' },
      // The settle event is deliberately missing, as a dropped frame would be.
      { event: 'turn_ended', turn: 1, stop_reason: 'end_turn' },
    ]);
    expect(liveSegment(state.turns[0])).toBeNull();
    expect(state.busy).toBe(false);
  });

  it('reopens a segment when the agent returns to the same message id', () => {
    const state = fold([
      { event: 'turn_started', turn: 1, prompt: 'go' },
      { event: 'segment_started', segment: seg('m1'), kind: 'thought' },
      { event: 'segment_chunk', segment: seg('m1'), text: 'start' },
      { event: 'segment_settled', segment: seg('m1') },
      { event: 'tool_call_started', tool_call_id: 't1', title: 'x', kind: 'read', status: 'completed' },
      { event: 'segment_started', segment: seg('m1'), kind: 'thought' },
      { event: 'segment_chunk', segment: seg('m1'), text: ' and finish' },
    ]);
    const segments = state.turns[0].items.filter((i: TurnItem) => i.type === 'segment');
    expect(segments).toHaveLength(1);
    expect(segments[0].type === 'segment' && segments[0].segment.text).toBe('start and finish');
  });

  it('marks a turn best effort when a boundary was inferred', () => {
    const state = fold([
      { event: 'turn_started', turn: 1, prompt: 'go' },
      { event: 'segment_started', segment: seg('syn:1:1', true), kind: 'thought' },
    ]);
    expect(state.turns[0].segmentation_best_effort).toBe(true);
  });

  it('creates a tool call from an update for a call it never saw start', () => {
    const state = fold([
      { event: 'turn_started', turn: 1, prompt: 'go' },
      {
        event: 'tool_call_updated',
        tool_call_id: 'orphan',
        title: 'edit main.rs',
        status: 'completed',
        content: [{ type: 'diff', path: '/a', old_text: 'x', new_text: 'y' }],
        locations: [],
      },
    ]);
    const calls = state.turns[0].items.filter((i: TurnItem) => i.type === 'tool_call');
    expect(calls).toHaveLength(1);
    expect(calls[0].type === 'tool_call' && calls[0].call.content[0].type).toBe('diff');
  });

  it('records unknown updates rather than dropping them', () => {
    const state = fold([
      { event: 'turn_started', turn: 1, prompt: 'go' },
      { event: 'unknown_update', discriminant: 'future_thing', raw: '{}' },
    ]);
    expect(state.unknownUpdates).toHaveLength(1);
  });

  it('settles the turn when the agent exits', () => {
    const state = fold([
      { event: 'turn_started', turn: 1, prompt: 'go' },
      { event: 'segment_started', segment: seg('m1'), kind: 'thought' },
      { event: 'agent_exited', code: 7, signal: null },
    ]);
    expect(state.busy).toBe(false);
    expect(state.turns[0].stop_reason).toBe('unknown');
    expect(liveSegment(state.turns[0])).toBeNull();
  });
});

describe('context percentage', () => {
  it('is null until the agent reports usage', () => {
    expect(contextPercent(emptySession())).toBeNull();
  });

  it('is null when the reported window is zero rather than dividing by it', () => {
    const state = fold([{ event: 'usage_changed', used: 10, size: 0, cost: null }]);
    expect(contextPercent(state)).toBeNull();
  });

  it('is the reported ratio once available', () => {
    const state = fold([{ event: 'usage_changed', used: 53_000, size: 200_000, cost: null }]);
    expect(contextPercent(state)).toBeCloseTo(26.5);
  });
});

describe('EventBatcher', () => {
  function ev(seq: number): WkbdEvent {
    return {
      seq,
      session_id: 's',
      at_ms: 0,
      payload: { event: 'segment_chunk', segment: seg('m1'), text: `${seq}` },
    };
  }

  it('schedules exactly one frame no matter how many events arrive', () => {
    const schedule = vi.fn().mockReturnValue(1);
    const commit = vi.fn();
    const b = new EventBatcher(commit, schedule, vi.fn());

    for (let i = 1; i <= 50; i++) b.push(ev(i));

    // Without the "already scheduled" guard this would be 50, and the batching would be a
    // no-op with extra bookkeeping.
    expect(schedule).toHaveBeenCalledTimes(1);
    expect(commit).not.toHaveBeenCalled();
    expect(b.pending).toBe(50);
  });

  it('commits everything buffered in one call when the frame runs', () => {
    // Collected rather than held in a single mutable binding: with one binding the compiler
    // cannot see that the scheduler ever ran, and narrows it to null.
    const frames: Array<() => void> = [];
    const schedule = (cb: () => void) => {
      frames.push(cb);
      return 1;
    };
    const commit = vi.fn();
    const b = new EventBatcher(commit, schedule, vi.fn());

    b.push(ev(1), ev(2), ev(3));
    for (const frame of frames) frame();

    expect(commit).toHaveBeenCalledTimes(1);
    expect(commit.mock.calls[0][0]).toHaveLength(3);
  });

  it('flush publishes the tail that no frame would have picked up', () => {
    // This is the failure mode the flush exists for: the stream stops, the pending frame is
    // never serviced, and the last tokens of the answer are silently missing.
    const commit = vi.fn();
    const b = new EventBatcher(
      commit,
      () => 1,
      () => {},
    );

    b.push(ev(1), ev(2));
    expect(commit).not.toHaveBeenCalled();

    b.flush();
    expect(commit).toHaveBeenCalledTimes(1);
    expect(commit.mock.calls[0][0]).toHaveLength(2);
    expect(b.pending).toBe(0);
  });

  it('flush with nothing buffered does not call commit', () => {
    const commit = vi.fn();
    const b = new EventBatcher(
      commit,
      () => 1,
      () => {},
    );
    b.flush();
    expect(commit).not.toHaveBeenCalled();
  });

  it('schedules a new frame after a flush', () => {
    const schedule = vi.fn().mockReturnValue(1);
    const b = new EventBatcher(vi.fn(), schedule, vi.fn());
    b.push(ev(1));
    b.flush();
    b.push(ev(2));
    expect(schedule).toHaveBeenCalledTimes(2);
  });
});
