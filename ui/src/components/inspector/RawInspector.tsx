import { useEffect, useRef, useState } from 'react';
import { fetchRawFrames, fetchRawFramesSince, type RawFrame } from '../../lib/api';

/**
 * Raw protocol frames, in both directions, exactly as they crossed the wire.
 *
 * Built on day one rather than added when something breaks. Every client of this protocol
 * that has got far enough has ended up building one, because when an agent's behaviour and
 * the interface disagree there is no other way to find out which of the two is wrong. It is
 * also the only place an unparseable line is visible: those are skipped so a chatty agent
 * cannot end a session, and skipped-and-invisible would be indistinguishable from
 * never-sent.
 */


/** The size of what is missing, which is the part that decides whether to go and look elsewhere. */
function formatClip(bytes: number): string {
  const size =
    bytes < 1024 ? `${bytes} B` : bytes < 1024 * 1024
      ? `${Math.round(bytes / 1024)} kB`
      : `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${size} more, not kept — the inspector holds the first 4 kB of a frame`;
}

/** Characters shown before a frame has to be asked to open. */
const HEAD_CHARS = 420;

/**
 * One frame's text, short by default.
 *
 * Two limits, and conflating them was the first mistake here. The daemon keeps 4 kB of a frame,
 * which is the right amount to *hold*: it covers the method, the parameters and the front of any
 * payload. It is the wrong amount to *show* — 4 kB is about forty lines, so a single prompt
 * carrying an attachment still filled the viewport and buried every frame around it. This is a
 * log; a reader scans it and opens the one line they care about.
 *
 * Expanding needs no refetch, because the 4 kB is already here.
 */
function FrameBody({ frame }: { frame: RawFrame }) {
  const [open, setOpen] = useState(false);
  const long = frame.line.length > HEAD_CHARS;
  const body = open || !long ? frame.line : `${frame.line.slice(0, HEAD_CHARS)}\u2026`;
  const hidden = frame.line.length - HEAD_CHARS;

  return (
    <>
      <pre className="frame-line" data-open={open}>
        {body}
      </pre>
      {long && (
        <button
          type="button"
          className="frame-more"
          onClick={() => setOpen((v) => !v)}
          data-testid={`frame-more-${frame.seq}`}
        >
          {open ? 'show less' : `show ${hidden} more characters`}
        </button>
      )}
      {frame.clipped_bytes !== null && frame.clipped_bytes > 0 && (
        /* Outside the <pre>, because everything inside it is exactly what crossed the pipe, and a
           note wearing the same clothes as the data is how a debugging aid starts lying. */
        <p className="frame-clipped">{formatClip(frame.clipped_bytes)}</p>
      )}
    </>
  );
}

export function RawInspector() {
  const [frames, setFrames] = useState<RawFrame[]>([]);
  const [filter, setFilter] = useState('');
  const [onlyMalformed, setOnlyMalformed] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // The tail once, then only what is new. Polling the whole log every second re-sent, re-parsed
  // and re-rendered every frame whether or not anything had changed — on the screen somebody opens
  // when an agent is already misbehaving.
  //
  // A ref rather than state for the cursor: the interval closes over it, and a cursor in state
  // would either re-create the interval on every batch or be read stale by the one that exists.
  const cursor = useRef(0);

  useEffect(() => {
    let live = true;
    const fail = (e: unknown) => {
      if (live) setError(e instanceof Error ? e.message : String(e));
    };

    fetchRawFrames()
      .then((first) => {
        if (!live) return;
        setFrames(first);
        cursor.current = first.length > 0 ? first[first.length - 1].seq : 0;
      })
      .catch(fail);

    const timer = setInterval(() => {
      fetchRawFramesSince(cursor.current)
        .then((batch) => {
          if (!live || batch.length === 0) return;
          cursor.current = batch[batch.length - 1].seq;
          // Trimmed to the same bound the daemon keeps, so a long session does not grow this list
          // without limit — the buffer over there is a ring for the same reason.
          setFrames((prev) => [...prev, ...batch].slice(-5000));
        })
        .catch(fail);
    }, 1000);

    return () => {
      live = false;
      clearInterval(timer);
    };
  }, []);

  const shown = frames.filter((f) => {
    if (onlyMalformed && !f.malformed) return false;
    if (filter && !f.line.toLowerCase().includes(filter.toLowerCase())) return false;
    return true;
  });

  return (
    <section className="inspector">
      <header className="inspector-header">
        <h1>Protocol log</h1>
        <input
          type="search"
          placeholder="filter…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <label>
          <input
            type="checkbox"
            checked={onlyMalformed}
            onChange={(e) => setOnlyMalformed(e.target.checked)}
          />
          only unparseable
        </label>
      </header>

      {error && <div className="inspector-error">{error}</div>}

      <ol className="inspector-frames">
        {shown.map((f, i) => (
          <li key={i} className="frame" data-direction={f.direction} data-malformed={f.malformed}>
            <span
              className="frame-dir"
              title={f.direction === 'to_agent' ? 'sent to the agent' : 'received from the agent'}
            >
              {f.direction === 'to_agent' ? '\u25b6' : '\u25c0'}
            </span>
            <span className="frame-agent">{f.agent_id}</span>
            <span className="frame-time">{new Date(f.at_ms).toLocaleTimeString()}</span>
            {f.malformed && <span className="frame-bad">not JSON</span>}
            <FrameBody frame={f} />
          </li>
        ))}
      </ol>
      {shown.length === 0 && <div className="inspector-empty">nothing yet</div>}
    </section>
  );
}
