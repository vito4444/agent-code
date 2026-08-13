/**
 * The event stream.
 *
 * Reconnects with the highest sequence number already seen, and asks for everything after
 * it. Resumption is by high-water mark, never by checking that sequence numbers are
 * contiguous: the log's ids are monotonic but not gapless, because a rolled back insert
 * leaves a permanent hole. A client that treated a hole as loss would ask for the same
 * range forever.
 */

import { EventBatcher } from './store';
import type { WkbdEvent } from './types';

export interface StreamHandlers {
  onEvents: (events: WkbdEvent[]) => void;
  onConnected: (connected: boolean) => void;
  onHello?: (hello: { degraded: string | null; high_water_mark: number }) => void;
}

export class EventStream {
  private socket: WebSocket | null = null;
  private batcher: EventBatcher;
  private closed = false;
  private attempt = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private readonly url: string,
    private readonly handlers: StreamHandlers,
    private readonly getSince: () => number,
  ) {
    this.batcher = new EventBatcher((events) => handlers.onEvents(events));
  }

  start(): void {
    this.closed = false;
    this.connect();
  }

  stop(): void {
    this.closed = true;
    if (this.timer !== null) clearTimeout(this.timer);
    // Anything buffered has to be published before we go, or the tail of the transcript is
    // lost to a frame that will never be requested.
    this.batcher.flush();
    this.socket?.close();
    this.socket = null;
  }

  private connect(): void {
    const socket = new WebSocket(this.url);
    this.socket = socket;

    socket.onopen = () => {
      this.attempt = 0;
      this.handlers.onConnected(true);
      socket.send(JSON.stringify({ type: 'subscribe', since_seq: this.getSince() }));
    };

    socket.onmessage = (msg) => {
      let parsed: unknown;
      try {
        parsed = JSON.parse(msg.data as string);
      } catch {
        return;
      }
      const frame = parsed as { type?: string };
      if (frame.type === 'events') {
        const events = (parsed as { events: WkbdEvent[] }).events;
        this.batcher.push(...events);
      } else if (frame.type === 'hello') {
        const hello = parsed as { degraded: string | null; high_water_mark: number };
        this.handlers.onHello?.(hello);
      } else if (frame.type === 'idle') {
        // The daemon says the stream has gone quiet. This is the flush that stops the last
        // few events sitting in the frame buffer indefinitely.
        this.batcher.flush();
      }
    };

    socket.onclose = () => {
      this.handlers.onConnected(false);
      this.batcher.flush();
      if (this.closed) return;
      // Backoff, capped. A daemon that is restarting should be waited for, not hammered.
      const delay = Math.min(30_000, 250 * 2 ** Math.min(this.attempt, 7));
      this.attempt += 1;
      this.timer = setTimeout(() => this.connect(), delay);
    };

    socket.onerror = () => {
      // Close handling does the reconnect; doing it here as well produces two sockets.
    };
  }
}

export function defaultStreamUrl(): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${location.host}/api/stream`;
}
