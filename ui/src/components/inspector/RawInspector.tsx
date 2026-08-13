import { useEffect, useState } from 'react';
import { fetchRawFrames, type RawFrame } from '../../lib/api';

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

export function RawInspector() {
  const [frames, setFrames] = useState<RawFrame[]>([]);
  const [filter, setFilter] = useState('');
  const [onlyMalformed, setOnlyMalformed] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const load = () => {
      fetchRawFrames()
        .then(setFrames)
        .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
    };
    load();
    const timer = setInterval(load, 1000);
    return () => clearInterval(timer);
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
            <pre className="frame-line">{f.line}</pre>
            {f.clipped_bytes !== null && f.clipped_bytes > 0 && (
              /* Outside the <pre>, because everything inside it is exactly what crossed the pipe
                 and a note in the same clothes as the data is how a debugging aid starts lying.
                 An attached file arrives here as a JSON string of its whole contents, so one
                 prompt used to fill the screen and bury every frame around it. */
              <p className="frame-clipped">{formatClip(f.clipped_bytes)}</p>
            )}
          </li>
        ))}
      </ol>
      {shown.length === 0 && <div className="inspector-empty">nothing yet</div>}
    </section>
  );
}
