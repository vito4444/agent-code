/**
 * The event store.
 *
 * Events arrive from the WebSocket at whatever rate the agent streams, which for a fast
 * model is well above one per animation frame. Committing each one to React state produces
 * a render per token; measured elsewhere that is enough to push commit times past 50ms and
 * make typing in the composer feel late.
 *
 * So events land in a buffer that React does not observe, and a `requestAnimationFrame`
 * callback publishes them. Two details in that are easy to get wrong and both are load
 * bearing:
 *
 * 1. **The frame must be scheduled at most once.** Without the `frame !== null` guard,
 *    every event schedules its own callback and the batching does nothing.
 * 2. **The buffer must be flushed when the stream stops.** The last few events before a
 *    quiet period would otherwise sit in the buffer waiting for a frame that is never
 *    requested, which shows up as an answer that is intermittently missing its final
 *    words — a symptom that is very hard to trace back to here.
 */

import { create } from 'zustand';
import { runIdFromStreamId } from './types';
import type {
  ConfigOption,
  Cost,
  EventPayload,
  PlanEntry,
  QueuedMessage,
  RunEvent,
  RunStatus,
  RunSummary,
  SegmentView,
  SessionSummary,
  TaskStatus,
  TaskSummary,
  ToolCallView,
  TurnItem,
  TurnView,
  WkbdEvent,
} from './types';

export interface SessionState {
  turns: TurnView[];
  configOptions: ConfigOption[];
  usage: { used: number; size: number; cost: Cost | null } | null;
  plan: PlanEntry[];
  unknownUpdates: { discriminant: string; raw: string }[];
  /** True while a prompt is outstanding, i.e. the agent owes us a stop reason. */
  busy: boolean;
  queue: QueuedMessage[];
}

export function emptySession(): SessionState {
  return {
    turns: [],
    configOptions: [],
    usage: null,
    plan: [],
    unknownUpdates: [],
    busy: false,
    queue: [],
  };
}

/* ------------------------------------------------------------------ run state */

export interface TaskWorkspaceView {
  branch: string;
  start_commit: string;
  /** The dependency commits folded into the starting point, in order. */
  from_dependencies: string[];
}

export interface TaskVerificationView {
  passed: boolean;
  missing_pass: string[];
  regressed: string[];
  detail: string | null;
}

export interface TaskState {
  summary: TaskSummary;
  status: TaskStatus;
  /** Why, when the status alone does not say it. */
  detail: string | null;
  workspace: TaskWorkspaceView | null;
  verification: TaskVerificationView | null;
}

export interface RunState {
  id: string;
  goal: string;
  project_root: string;
  base_commit: string;
  status: RunStatus;
  tasks: Record<string, TaskState>;
  /** Task ids grouped so that everything in one group can run at once. */
  waves: string[][];
  planAttempt: number;
  /** Graphs the planner produced that did not survive validation, oldest first. */
  planRejections: { problems: string[]; attempt: number }[];
  replans: { trigger: string; task_id: string; attempt: number }[];
  mergeCandidate: { commit: string; order: string[] } | null;
  /** Tasks that never made it into the candidate. */
  excluded: string[];
  mergeRejections: { task_id: string; detail: string; merged: string[] }[];
  detail: string | null;
}

export function emptyRun(id: string): RunState {
  return {
    id,
    goal: '',
    project_root: '',
    base_commit: '',
    status: 'planning',
    tasks: {},
    waves: [],
    planAttempt: 0,
    planRejections: [],
    replans: [],
    mergeCandidate: null,
    excluded: [],
    mergeRejections: [],
    detail: null,
  };
}

interface StoreShape {
  sessions: Record<string, SessionState>;
  sessionList: SessionSummary[];
  activeSessionId: string | null;
  /** Keyed by run id, not by the `run:<uuid>` stream the events arrive on. */
  runs: Record<string, RunState>;
  highWaterMark: number;
  connected: boolean;
  /** Set when the daemon started degraded, e.g. after a failed migration. */
  degradedReason: string | null;

  setConnected: (v: boolean) => void;
  setDegraded: (reason: string | null) => void;
  setSessionList: (list: SessionSummary[]) => void;
  setActiveSession: (id: string | null) => void;
  ensureSession: (id: string) => void;
  seedRuns: (list: RunSummary[]) => void;
  applyEvents: (events: WkbdEvent[]) => void;
  setBusy: (sessionId: string, busy: boolean) => void;
  enqueue: (sessionId: string, message: QueuedMessage) => void;
  dequeue: (sessionId: string, id: string) => void;
  takeQueued: (sessionId: string) => QueuedMessage | null;
  reorderQueue: (sessionId: string, from: number, to: number) => void;
}

export const useStore = create<StoreShape>((set, get) => ({
  sessions: {},
  sessionList: [],
  activeSessionId: null,
  runs: {},
  highWaterMark: 0,
  connected: false,
  degradedReason: null,

  setConnected: (v) => set({ connected: v }),
  setDegraded: (reason) => set({ degradedReason: reason }),
  setSessionList: (list) => set({ sessionList: list }),
  setActiveSession: (id) => {
    if (id) get().ensureSession(id);
    set({ activeSessionId: id });
  },

  ensureSession: (id) =>
    set((s) =>
      s.sessions[id] ? s : { sessions: { ...s.sessions, [id]: emptySession() } },
    ),

  // Only fills in runs the event stream has not reached yet. The log is authoritative about
  // what happened in a run; a summary that overwrote folded state would let a stale row
  // reset a task board that the events had already moved on from.
  seedRuns: (list) =>
    set((s) => {
      const runs = { ...s.runs };
      let added = false;
      for (const summary of list) {
        if (runs[summary.id]) continue;
        runs[summary.id] = {
          ...emptyRun(summary.id),
          goal: summary.goal,
          project_root: summary.project_root,
          status: summary.status,
        };
        added = true;
      }
      return added ? { runs } : s;
    }),

  applyEvents: (events) =>
    set((s) => {
      if (events.length === 0) return s;
      const sessions = { ...s.sessions };
      const runs = { ...s.runs };
      let hwm = s.highWaterMark;

      for (const ev of events) {
        // Monotonic but not contiguous: a rolled back insert leaves a permanent gap, so a
        // hole is not evidence of loss and must not trigger a re-request.
        if (ev.seq > hwm) hwm = ev.seq;

        if (ev.payload.event === 'run') {
          const id = runIdFromStreamId(ev.session_id) ?? ev.session_id;
          runs[id] = applyRunOne(runs[id] ?? emptyRun(id), ev.payload.run);
          continue;
        }

        const existing = sessions[ev.session_id] ?? emptySession();
        sessions[ev.session_id] = applyOne(existing, ev.payload);
      }

      return { sessions, runs, highWaterMark: hwm };
    }),

  setBusy: (sessionId, busy) =>
    set((s) => {
      const session = s.sessions[sessionId];
      if (!session) return s;
      return { sessions: { ...s.sessions, [sessionId]: { ...session, busy } } };
    }),

  enqueue: (sessionId, message) =>
    set((s) => {
      const session = s.sessions[sessionId] ?? emptySession();
      return {
        sessions: {
          ...s.sessions,
          [sessionId]: { ...session, queue: [...session.queue, message] },
        },
      };
    }),

  dequeue: (sessionId, id) =>
    set((s) => {
      const session = s.sessions[sessionId];
      if (!session) return s;
      return {
        sessions: {
          ...s.sessions,
          [sessionId]: { ...session, queue: session.queue.filter((m) => m.id !== id) },
        },
      };
    }),

  takeQueued: (sessionId) => {
    const session = get().sessions[sessionId];
    if (!session || session.queue.length === 0) return null;
    const next = session.queue[0];
    get().dequeue(sessionId, next.id);
    return next;
  },

  reorderQueue: (sessionId, from, to) =>
    set((s) => {
      const session = s.sessions[sessionId];
      if (!session) return s;
      const queue = [...session.queue];
      if (from < 0 || from >= queue.length || to < 0 || to >= queue.length) return s;
      const [moved] = queue.splice(from, 1);
      queue.splice(to, 0, moved);
      return { sessions: { ...s.sessions, [sessionId]: { ...session, queue } } };
    }),
}));

/* --------------------------------------------------------------- event folding */

/**
 * Folds one event into session state.
 *
 * This mirrors `ViewBuilder` in the Rust core. Both exist because the live stream and a
 * replayed log must produce the same view; the Rust one is authoritative and is what the
 * fixture test compares against.
 */
export function applyOne(state: SessionState, payload: EventPayload): SessionState {
  switch (payload.event) {
    case 'turn_started': {
      const turn: TurnView = {
        turn: payload.turn,
        prompt: payload.prompt,
        items: [],
        stop_reason: null,
        segmentation_best_effort: false,
      };
      return { ...state, turns: [...state.turns, turn], busy: true };
    }

    case 'turn_ended': {
      const turns = [...state.turns];
      const last = turns.length - 1;
      if (last >= 0) {
        turns[last] = {
          ...turns[last],
          stop_reason: payload.stop_reason,
          // A finished turn may not leave a segment live. Without this a dropped
          // settle event would leave a spinner running for the rest of the session.
          items: turns[last].items.map(settleSegment),
        };
      }
      return { ...state, turns, busy: false };
    }

    case 'segment_started': {
      const turns = [...state.turns];
      const ti = turns.length - 1;
      if (ti < 0) return state;

      const existingIndex = turns[ti].items.findIndex(
        (i) => i.type === 'segment' && i.segment.id.raw === payload.segment.raw,
      );
      if (existingIndex >= 0) {
        // Reopening: the agent came back to the same messageId after a tool call. This is
        // one interrupted message, not two.
        const items = [...turns[ti].items];
        const item = items[existingIndex];
        if (item.type === 'segment') {
          items[existingIndex] = {
            ...item,
            segment: { ...item.segment, state: 'live' },
          };
        }
        turns[ti] = { ...turns[ti], items };
        return { ...state, turns };
      }

      const segment: SegmentView = {
        id: payload.segment,
        kind: payload.kind,
        text: '',
        state: 'live',
      };
      turns[ti] = {
        ...turns[ti],
        items: [...turns[ti].items, { type: 'segment', segment }],
        segmentation_best_effort:
          turns[ti].segmentation_best_effort || payload.segment.synthesized,
      };
      return { ...state, turns };
    }

    case 'segment_chunk':
      return mapSegment(state, payload.segment.raw, (s) => ({
        ...s,
        text: s.text + payload.text,
      }));

    case 'segment_settled':
      return mapSegment(state, payload.segment.raw, (s) => ({ ...s, state: 'settled' }));

    case 'tool_call_started': {
      const turns = [...state.turns];
      const ti = turns.length - 1;
      if (ti < 0) return state;
      const call: ToolCallView = {
        tool_call_id: payload.tool_call_id,
        title: payload.title,
        kind: payload.kind,
        status: payload.status,
        content: [],
        locations: [],
      };
      turns[ti] = { ...turns[ti], items: [...turns[ti].items, { type: 'tool_call', call }] };
      return { ...state, turns };
    }

    case 'tool_call_updated': {
      const turns = [...state.turns];
      const ti = turns.length - 1;
      if (ti < 0) return state;

      let items = [...turns[ti].items];
      let index = items.findIndex(
        (i) => i.type === 'tool_call' && i.call.tool_call_id === payload.tool_call_id,
      );
      if (index < 0) {
        // Upsert. An update for a call we never saw created must not be dropped: that is
        // how a diff disappears.
        items = [
          ...items,
          {
            type: 'tool_call',
            call: {
              tool_call_id: payload.tool_call_id,
              title: payload.title ?? payload.tool_call_id,
              kind: 'other',
              status: payload.status ?? 'in_progress',
              content: [],
              locations: [],
            },
          },
        ];
        index = items.length - 1;
      }
      const item = items[index];
      if (item.type === 'tool_call') {
        items[index] = {
          type: 'tool_call',
          call: {
            ...item.call,
            title: payload.title ?? item.call.title,
            status: payload.status ?? item.call.status,
            content:
              payload.content.length > 0
                ? [...item.call.content, ...payload.content]
                : item.call.content,
            locations:
              payload.locations.length > 0 ? payload.locations : item.call.locations,
          },
        };
      }
      turns[ti] = { ...turns[ti], items };
      return { ...state, turns };
    }

    case 'permission_requested': {
      const turns = [...state.turns];
      const ti = turns.length - 1;
      if (ti < 0) return state;
      turns[ti] = {
        ...turns[ti],
        items: [
          ...turns[ti].items,
          {
            type: 'permission',
            request_id: payload.request_id,
            title: payload.title,
            options: payload.options,
            resolved_with: null,
            auto: false,
          },
        ],
      };
      return { ...state, turns };
    }

    case 'permission_resolved': {
      const turns = state.turns.map((t) => ({
        ...t,
        items: t.items.map((i) =>
          i.type === 'permission' && i.request_id === payload.request_id
            ? { ...i, resolved_with: payload.option_id, auto: payload.auto }
            : i,
        ),
      }));
      return { ...state, turns };
    }

    case 'config_options_changed':
      return { ...state, configOptions: payload.options };

    case 'usage_changed':
      return {
        ...state,
        usage: { used: payload.used, size: payload.size, cost: payload.cost },
      };

    case 'plan_changed':
      return { ...state, plan: payload.entries };

    case 'unknown_update':
      return {
        ...state,
        unknownUpdates: [
          ...state.unknownUpdates,
          { discriminant: payload.discriminant, raw: payload.raw },
        ],
      };

    case 'agent_error': {
      const turns = [...state.turns];
      const ti = turns.length - 1;
      if (ti < 0) return state;
      turns[ti] = {
        ...turns[ti],
        items: [...turns[ti].items, { type: 'error', message: payload.message }],
      };
      return { ...state, turns };
    }

    case 'agent_exited': {
      const turns = [...state.turns];
      const ti = turns.length - 1;
      if (ti < 0) return { ...state, busy: false };
      turns[ti] = {
        ...turns[ti],
        stop_reason: turns[ti].stop_reason ?? 'unknown',
        items: [
          ...turns[ti].items.map(settleSegment),
          {
            type: 'error',
            message: `agent exited (code=${payload.code ?? '-'} signal=${payload.signal ?? '-'})`,
          },
        ],
      };
      return { ...state, turns, busy: false };
    }

    default:
      return state;
  }
}

function settleSegment(item: TurnItem): TurnItem {
  return item.type === 'segment'
    ? { ...item, segment: { ...item.segment, state: 'settled' } }
    : item;
}

function mapSegment(
  state: SessionState,
  rawId: string,
  f: (s: SegmentView) => SegmentView,
): SessionState {
  const turns = [...state.turns];
  for (let ti = turns.length - 1; ti >= 0; ti--) {
    const index = turns[ti].items.findIndex(
      (i) => i.type === 'segment' && i.segment.id.raw === rawId,
    );
    if (index >= 0) {
      const items = [...turns[ti].items];
      const item = items[index];
      if (item.type === 'segment') {
        items[index] = { type: 'segment', segment: f(item.segment) };
      }
      turns[ti] = { ...turns[ti], items };
      return { ...state, turns };
    }
  }
  return state;
}

/**
 * Folds one orchestration event into run state.
 *
 * A run's status is only partly carried by the events: `finished` and `awaiting_merge` state
 * it, and the rest is inferred from the transition — a graph that survived validation means
 * the run is running, and nothing else in the stream says so. Inferring it here rather than
 * in the view keeps the answer the same for a live run and a replayed one.
 */
export function applyRunOne(state: RunState, event: RunEvent): RunState {
  switch (event.event) {
    case 'started':
      return {
        ...state,
        id: event.run_id,
        goal: event.goal,
        project_root: event.project_root,
        base_commit: event.base_commit,
        status: 'planning',
      };

    case 'planned': {
      const tasks: Record<string, TaskState> = {};
      for (const summary of event.tasks) {
        const previous = state.tasks[summary.id];
        // A replan reissues the whole graph. Tasks that survive it keep their progress:
        // blanking the board would throw away the record of work that was never redone, and
        // anything the orchestrator does redo arrives as its own state change.
        tasks[summary.id] = previous
          ? { ...previous, summary }
          : { summary, status: 'pending', detail: null, workspace: null, verification: null };
      }
      return {
        ...state,
        tasks,
        waves: event.waves,
        planAttempt: event.attempt,
        status: state.status === 'planning' ? 'running' : state.status,
      };
    }

    case 'plan_rejected':
      return {
        ...state,
        planRejections: [
          ...state.planRejections,
          { problems: event.problems, attempt: event.attempt },
        ],
      };

    case 'task_state_changed':
      return mapTask(state, event.task_id, (t) => ({
        ...t,
        status: event.status,
        detail: event.detail,
      }));

    case 'task_workspace_ready':
      return mapTask(state, event.task_id, (t) => ({
        ...t,
        workspace: {
          branch: event.branch,
          start_commit: event.start_commit,
          from_dependencies: event.from_dependencies,
        },
      }));

    case 'task_verified':
      return mapTask(state, event.task_id, (t) => ({
        ...t,
        verification: {
          passed: event.passed,
          missing_pass: event.missing_pass,
          regressed: event.regressed,
          detail: event.detail,
        },
      }));

    case 'replanning':
      return {
        ...state,
        replans: [
          ...state.replans,
          { trigger: event.trigger, task_id: event.task_id, attempt: event.attempt },
        ],
      };

    case 'awaiting_merge':
      return {
        ...state,
        status: 'awaiting_merge',
        mergeCandidate: { commit: event.commit, order: event.order },
        excluded: event.excluded,
      };

    case 'merge_rejected':
      return {
        ...state,
        mergeRejections: [
          ...state.mergeRejections,
          { task_id: event.task_id, detail: event.detail, merged: event.merged },
        ],
      };

    case 'finished':
      return { ...state, status: event.status, detail: event.detail };

    default:
      return state;
  }
}

/**
 * Updates one task, creating it when the graph never mentioned it.
 *
 * The upsert is deliberate. An event about a task we have no summary for is the orchestrator
 * telling us something ran; dropping it would leave a task that is working, failing or
 * blocked entirely invisible, which is the one thing a run view must not do.
 */
function mapTask(state: RunState, taskId: string, f: (t: TaskState) => TaskState): RunState {
  const existing: TaskState = state.tasks[taskId] ?? {
    summary: {
      id: taskId,
      title: taskId,
      depends_on: [],
      declared_paths: [],
      verify_cmd: '',
      must_pass: [],
    },
    status: 'pending',
    detail: null,
    workspace: null,
    verification: null,
  };
  return { ...state, tasks: { ...state.tasks, [taskId]: f(existing) } };
}

/* ------------------------------------------------------------- selectors */

export function liveSegment(turn: TurnView): SegmentView | null {
  for (let i = turn.items.length - 1; i >= 0; i--) {
    const item = turn.items[i];
    if (item.type === 'segment' && item.segment.state === 'live') return item.segment;
  }
  return null;
}

/**
 * The context percentage, or null when the agent has never reported usage.
 *
 * Null means draw nothing. Not zero, not "unknown", not an estimate: as an ACP client we
 * cannot see the agent's system prompt, its loaded rule files or its tool schemas, so any
 * number we computed ourselves would be a different quantity with the same label.
 */
export function contextPercent(session: SessionState): number | null {
  if (!session.usage || session.usage.size === 0) return null;
  return (session.usage.used / session.usage.size) * 100;
}

/**
 * Runs in the order their first event arrived.
 *
 * Arrival order is the only total order a client has: the log's sequence numbers are the real
 * ordering and are not carried on the folded state, and a wall-clock field would order runs by
 * a clock that is not the one that wrote them.
 */
export function runList(runs: Record<string, RunState>): RunState[] {
  return Object.values(runs);
}

export function taskById(run: RunState, taskId: string): TaskState | null {
  return run.tasks[taskId] ?? null;
}

/**
 * What a run is waiting on, or null when it is simply working or over.
 *
 * The variants are ordered by who has to act. A candidate waiting for a human outranks a
 * failure, because a run that is finished except for a decision reads as "still going" on a
 * board that only shows the newest failure.
 */
export type RunBlocker =
  | { kind: 'awaiting_merge'; commit: string; excluded: string[] }
  | { kind: 'merge_rejected'; task_id: string; detail: string }
  | { kind: 'failed'; task_ids: string[] }
  | { kind: 'blocked'; task_ids: string[] }
  | { kind: 'plan_rejected'; problems: string[] };

export function runBlocker(run: RunState): RunBlocker | null {
  if (run.status === 'done' || run.status === 'cancelled') return null;

  if (run.status === 'awaiting_merge' && run.mergeCandidate) {
    return {
      kind: 'awaiting_merge',
      commit: run.mergeCandidate.commit,
      excluded: run.excluded,
    };
  }

  const latestRejection = run.mergeRejections[run.mergeRejections.length - 1];
  if (latestRejection) {
    return {
      kind: 'merge_rejected',
      task_id: latestRejection.task_id,
      detail: latestRejection.detail,
    };
  }

  const tasks = Object.values(run.tasks);
  const failed = tasks.filter((t) => t.status === 'failed').map((t) => t.summary.id);
  if (failed.length > 0) return { kind: 'failed', task_ids: failed };

  const blocked = tasks.filter((t) => t.status === 'blocked').map((t) => t.summary.id);
  if (blocked.length > 0) return { kind: 'blocked', task_ids: blocked };

  const latestPlanRejection = run.planRejections[run.planRejections.length - 1];
  if (run.status === 'planning' && latestPlanRejection) {
    return { kind: 'plan_rejected', problems: latestPlanRejection.problems };
  }

  return null;
}

/* ------------------------------------------------- frame-batched ingestion */

/**
 * Buffers events and publishes them once per animation frame.
 *
 * Exported as a class so tests can drive the frame clock instead of waiting on a real one.
 */
export class EventBatcher {
  private buffer: WkbdEvent[] = [];
  private frame: number | null = null;

  constructor(
    private readonly commit: (events: WkbdEvent[]) => void,
    // Wrapped rather than passed by reference. `requestAnimationFrame` is a method on the window
    // object, and handing the bare function to something that calls it unbound throws
    // "Illegal invocation" — which surfaces as the event stream silently never publishing.
    private readonly schedule: (cb: () => void) => number = (cb) => requestAnimationFrame(cb),
    private readonly cancel: (handle: number) => void = (h) => cancelAnimationFrame(h),
  ) {}

  push(...events: WkbdEvent[]): void {
    this.buffer.push(...events);
    // The guard is the entire optimization. Without it each event schedules its own
    // callback and the batching is a no-op with extra bookkeeping.
    if (this.frame !== null) return;
    this.frame = this.schedule(() => {
      this.frame = null;
      this.drain();
    });
  }

  /**
   * Publishes immediately.
   *
   * Must be called when a stream ends. Anything still buffered is otherwise waiting for a
   * frame nobody will request, which presents as an answer missing its last words.
   */
  flush(): void {
    if (this.frame !== null) {
      this.cancel(this.frame);
      this.frame = null;
    }
    this.drain();
  }

  get pending(): number {
    return this.buffer.length;
  }

  private drain(): void {
    if (this.buffer.length === 0) return;
    const batch = this.buffer;
    this.buffer = [];
    this.commit(batch);
  }
}
