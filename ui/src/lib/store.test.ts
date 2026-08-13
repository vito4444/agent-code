import { describe, expect, it, vi } from 'vitest';
import {
  EventBatcher,
  applyOne,
  applyRunOne,
  contextPercent,
  emptyRun,
  emptySession,
  liveSegment,
  runBlocker,
  runList,
  taskById,
  useStore,
} from './store';
import type { TurnItem } from './types';
import type { RunState, SessionState } from './store';
import type { EventPayload, RunEvent, TaskSummary, WkbdEvent } from './types';

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

describe('run folding', () => {
  function foldRun(events: RunEvent[]): RunState {
    return events.reduce(applyRunOne, emptyRun('r1'));
  }

  function task(id: string, overrides: Partial<TaskSummary> = {}): TaskSummary {
    return {
      id,
      title: `Task ${id}`,
      depends_on: [],
      declared_paths: [`src/${id}.rs`],
      verify_cmd: 'cargo test',
      must_pass: [`${id}::works`],
      ...overrides,
    };
  }

  const started: RunEvent = {
    event: 'started',
    run_id: 'r1',
    goal: 'add a login endpoint',
    project_root: '/repo',
    base_commit: '9f1c2d3e4a5b6c7d8e9f0a1b',
  };

  const planned: RunEvent = {
    event: 'planned',
    tasks: [task('a'), task('b'), task('c', { depends_on: ['a', 'b'] })],
    waves: [['a', 'b'], ['c']],
    attempt: 1,
  };

  it('carries a run from its first event to the merge gate', () => {
    const state = foldRun([
      started,
      planned,
      { event: 'task_state_changed', task_id: 'a', status: 'dispatched', detail: null },
      {
        event: 'task_workspace_ready',
        task_id: 'a',
        branch: 'wkbd/r1/a',
        start_commit: 'aaaa1111bbbb2222',
        from_dependencies: [],
      },
      { event: 'task_state_changed', task_id: 'b', status: 'dispatched', detail: null },
      { event: 'task_state_changed', task_id: 'a', status: 'verifying', detail: null },
      {
        event: 'task_verified',
        task_id: 'a',
        passed: true,
        missing_pass: [],
        regressed: [],
        detail: null,
      },
      { event: 'task_state_changed', task_id: 'a', status: 'completed', detail: null },
      {
        event: 'awaiting_merge',
        commit: 'cccc3333dddd4444',
        order: ['a'],
        excluded: ['b', 'c'],
      },
    ]);

    expect(state.goal).toBe('add a login endpoint');
    expect(state.project_root).toBe('/repo');
    expect(state.base_commit).toBe('9f1c2d3e4a5b6c7d8e9f0a1b');
    expect(state.waves).toEqual([['a', 'b'], ['c']]);
    expect(Object.keys(state.tasks)).toEqual(['a', 'b', 'c']);

    expect(taskById(state, 'a')?.status).toBe('completed');
    expect(taskById(state, 'a')?.workspace?.branch).toBe('wkbd/r1/a');
    expect(taskById(state, 'a')?.verification?.passed).toBe(true);
    expect(taskById(state, 'b')?.status).toBe('dispatched');
    expect(taskById(state, 'c')?.status).toBe('pending');

    // Acceptance passing moves the run to the gate, not past it.
    expect(state.status).toBe('awaiting_merge');
    expect(state.mergeCandidate).toEqual({ commit: 'cccc3333dddd4444', order: ['a'] });
    expect(state.excluded).toEqual(['b', 'c']);
  });

  it('keeps the progress of tasks a replan did not touch', () => {
    const state = foldRun([
      started,
      planned,
      { event: 'task_state_changed', task_id: 'a', status: 'completed', detail: null },
      { event: 'replanning', trigger: 'acceptance_failed', task_id: 'b', attempt: 2 },
      {
        event: 'planned',
        tasks: [task('a'), task('b2')],
        waves: [['a'], ['b2']],
        attempt: 2,
      },
    ]);

    expect(taskById(state, 'a')?.status).toBe('completed');
    // The replaced task is gone rather than lingering as a card nothing will ever update.
    expect(taskById(state, 'b')).toBeNull();
    expect(state.replans).toEqual([
      { trigger: 'acceptance_failed', task_id: 'b', attempt: 2 },
    ]);
  });

  it('records a rejected plan rather than showing only the retry', () => {
    const state = foldRun([
      started,
      { event: 'plan_rejected', problems: ['task c declares no assertions'], attempt: 1 },
    ]);
    expect(state.status).toBe('planning');
    expect(state.waves).toEqual([]);
    expect(state.planRejections[0].problems).toEqual(['task c declares no assertions']);
  });

  it('creates a task from a state change the plan never mentioned', () => {
    const state = foldRun([
      started,
      { event: 'task_state_changed', task_id: 'ghost', status: 'failed', detail: 'timed out' },
    ]);
    expect(taskById(state, 'ghost')?.status).toBe('failed');
    expect(taskById(state, 'ghost')?.detail).toBe('timed out');
  });

  it('routes run events to runs and leaves the session map alone', () => {
    useStore.getState().applyEvents([
      { seq: 1, session_id: 'run:r1', at_ms: 0, payload: { event: 'run', run: started } },
      { seq: 2, session_id: 'run:r1', at_ms: 0, payload: { event: 'run', run: planned } },
    ]);

    const store = useStore.getState();
    expect(store.sessions['run:r1']).toBeUndefined();
    expect(store.runs['r1'].waves).toEqual([['a', 'b'], ['c']]);
    expect(runList(store.runs).map((r) => r.id)).toContain('r1');
    expect(store.highWaterMark).toBe(2);
  });
});

describe('where a run is stuck', () => {
  const base = emptyRun('r1');

  it('is nothing while the run is simply working', () => {
    expect(runBlocker({ ...base, status: 'running' })).toBeNull();
  });

  it('is the waiting candidate even when a task failed earlier', () => {
    const state: RunState = {
      ...base,
      status: 'awaiting_merge',
      mergeCandidate: { commit: 'abc', order: ['a'] },
      excluded: ['b'],
      tasks: {
        b: {
          summary: {
            id: 'b',
            title: 'b',
            depends_on: [],
            declared_paths: [],
            verify_cmd: '',
            must_pass: [],
          },
          status: 'failed',
          detail: null,
          routing: null,
          workspace: null,
          verification: null,
        },
      },
    };
    // A run that is finished except for a decision must not read as still working.
    expect(runBlocker(state)).toEqual({ kind: 'awaiting_merge', commit: 'abc', excluded: ['b'] });
  });

  it('is the failing tasks while the run is still going', () => {
    const state: RunState = {
      ...base,
      status: 'running',
      tasks: {
        a: {
          summary: {
            id: 'a',
            title: 'a',
            depends_on: [],
            declared_paths: [],
            verify_cmd: '',
            must_pass: [],
          },
          status: 'failed',
          detail: null,
          routing: null,
          workspace: null,
          verification: null,
        },
      },
    };
    expect(runBlocker(state)).toEqual({ kind: 'failed', task_ids: ['a'] });
  });

  it('is nothing once the run is over', () => {
    expect(runBlocker({ ...base, status: 'done' })).toBeNull();
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

describe('when a run was created', () => {
  /**
   * Ordering by arrival looks right until a reload, at which point the list is in whatever order the
   * log replayed and two clients open on the same daemon disagree about which run is newest.
   */
  it('takes the date from the log rather than from when this client heard about it', () => {
    const seeded = applyRunOne(emptyRun('r1'), runStarted('r1'), 5_000);
    expect(seeded.createdMs).toBe(5_000);
  });

  /**
   * A row this client added optimistically carries this client's clock. Two clocks in one sortable
   * field disagree about ordering for as long as the guess survives, so it survives only until the
   * real one lands.
   */
  it('replaces an optimistic guess with the log timestamp', () => {
    const guessed = { ...emptyRun('r1'), createdMs: 999_999 };
    expect(applyRunOne(guessed, runStarted('r1'), 5_000).createdMs).toBe(5_000);
  });

  it('is null when nothing has said, rather than defaulting to now', () => {
    expect(emptyRun('r1').createdMs).toBeNull();
    expect(applyRunOne(emptyRun('r1'), runStarted('r1')).createdMs).toBeNull();
  });
});

function runStarted(id: string) {
  return {
    event: 'started' as const,
    run_id: id,
    goal: 'g',
    project_root: '/r',
    base_commit: 'abc1234',
  };
}

describe('files the client touched, folded into a turn', () => {
  function fileEvent(over: Partial<Extract<EventPayload, { event: 'file_access' }>>): EventPayload {
    return {
      event: 'file_access',
      op: 'write',
      requested: '/w/a.rs',
      resolved: '/w/a.rs',
      allowed: true,
      refusal: null,
      bytes: 12,
      ...over,
    };
  }

  function turnWith(payloads: EventPayload[]) {
    let state = emptySession();
    state = applyOne(state, { event: 'turn_started', turn: 1, prompt: 'go' });
    for (const p of payloads) state = applyOne(state, p);
    return state.turns[state.turns.length - 1];
  }

  /**
   * The gap this closes was not in the rendering, which had a test, but in the folding, which did
   * not. The component was handed a file item by hand and drew it correctly, while nothing produced
   * one from an event — so an agent that edited through the protocol's file methods still produced a
   * transcript with a thought, an answer, and no sign that a file had changed.
   */
  it('turns an allowed write into an item in the turn', () => {
    const turn = turnWith([fileEvent({})]);
    expect(turn.items).toContainEqual(
      expect.objectContaining({ type: 'file', requested: '/w/a.rs', allowed: true }),
    );
  });

  it('turns a refusal into an item carrying its reason', () => {
    const turn = turnWith([
      fileEvent({ requested: '/etc/passwd', resolved: null, allowed: false, refusal: 'outside-root', bytes: null }),
    ]);
    expect(turn.items).toContainEqual(
      expect.objectContaining({ type: 'file', allowed: false, refusal: 'outside-root' }),
    );
  });

  /** Order is why these live in the turn at all rather than in a list beside it. */
  it('keeps them in arrival order among the other items', () => {
    const turn = turnWith([
      fileEvent({ requested: '/w/first.rs' }),
      { event: 'tool_call_started', tool_call_id: 't1', title: 'Run tests', kind: 'execute', status: 'in_progress' },
      fileEvent({ requested: '/w/second.rs' }),
    ]);
    const shape = turn.items.map((i) =>
      i.type === 'file' ? i.requested : i.type === 'tool_call' ? 'tool' : i.type,
    );
    expect(shape).toEqual(['/w/first.rs', 'tool', '/w/second.rs']);
  });

  /** An access arriving with no turn open must not invent one or crash the fold. */
  it('is dropped rather than crashing when no turn is open', () => {
    const state = applyOne(emptySession(), fileEvent({}));
    expect(state.turns).toHaveLength(0);
  });
});

describe('a permission nobody answered', () => {
  function turnWithPermission(extra: EventPayload[] = []) {
    let state = emptySession();
    state = applyOne(state, { event: 'turn_started', turn: 1, prompt: 'go' });
    state = applyOne(state, {
      event: 'permission_requested',
      request_id: 'p1',
      tool_call_id: 't1',
      title: 'Edit src/a.rs',
      options: [{ option_id: 'allow-once', name: 'Allow once', kind: 'allow_once' }],
    });
    for (const e of extra) state = applyOne(state, e);
    return state.turns[0];
  }

  it('is open while nothing has happened to it', () => {
    const item = turnWithPermission().items.find((i) => i.type === 'permission');
    expect(item).toMatchObject({ resolved_with: null, expired: false });
  });

  /**
   * Distinct from a refusal. Nobody refused; the agent stopped waiting. Recording the lapse as a
   * decision would put a choice in the log that no person made, which a later reader has no way to
   * question.
   */
  it('is marked expired rather than refused when the turn ends first', () => {
    const item = turnWithPermission([
      { event: 'permission_expired', request_id: 'p1' },
    ]).items.find((i) => i.type === 'permission');
    expect(item).toMatchObject({ resolved_with: null, expired: true });
  });

  it('does not touch a different request', () => {
    const item = turnWithPermission([
      { event: 'permission_expired', request_id: 'somebody-else' },
    ]).items.find((i) => i.type === 'permission');
    expect(item).toMatchObject({ expired: false });
  });
});
