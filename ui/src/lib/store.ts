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
import type {
  ConfigOption,
  Cost,
  EventPayload,
  PlanEntry,
  QueuedMessage,
  SegmentView,
  SessionSummary,
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

interface StoreShape {
  sessions: Record<string, SessionState>;
  sessionList: SessionSummary[];
  activeSessionId: string | null;
  highWaterMark: number;
  connected: boolean;
  /** Set when the daemon started degraded, e.g. after a failed migration. */
  degradedReason: string | null;

  setConnected: (v: boolean) => void;
  setDegraded: (reason: string | null) => void;
  setSessionList: (list: SessionSummary[]) => void;
  setActiveSession: (id: string | null) => void;
  ensureSession: (id: string) => void;
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

  applyEvents: (events) =>
    set((s) => {
      if (events.length === 0) return s;
      const sessions = { ...s.sessions };
      let hwm = s.highWaterMark;

      for (const ev of events) {
        // Monotonic but not contiguous: a rolled back insert leaves a permanent gap, so a
        // hole is not evidence of loss and must not trigger a re-request.
        if (ev.seq > hwm) hwm = ev.seq;
        const existing = sessions[ev.session_id] ?? emptySession();
        sessions[ev.session_id] = applyOne(existing, ev.payload);
      }

      return { sessions, highWaterMark: hwm };
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
    private readonly schedule: (cb: () => void) => number = requestAnimationFrame,
    private readonly cancel: (handle: number) => void = cancelAnimationFrame,
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
