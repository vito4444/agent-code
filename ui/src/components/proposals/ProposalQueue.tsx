import { useCallback, useEffect, useState } from 'react';
import * as api from '../../lib/api';
import type { ProposalSummary, ProposalReview } from '../../lib/types';

/**
 * Everything the system wants to tell itself, waiting for somebody to agree.
 *
 * Nothing in this queue is in effect, and that is the whole point of it existing. A run's transcript
 * carries text from tools, from files and possibly from a hostile repository, and text that can reach
 * the model's own instructions is the most valuable thing an injection can reach. So instructions
 * arrive here with their evidence and wait, while facts — which are evidence rather than instruction,
 * and which anything reading them treats as such — are written directly.
 *
 * The screen is built around two failure modes rather than around the data.
 *
 * **Approving from a list.** A list is skim-read, so it deliberately carries no proposal bodies: the
 * decision needs the body, and the body needs to be read with the invisible characters already
 * stripped. Opening one is a separate act from seeing that one exists.
 *
 * **Approving something other than what was read.** The approval carries the hash the reviewer was
 * shown, and the daemon compares it against the bytes on disk right now. An edit that lands between
 * rendering this and pressing the button invalidates the press instead of being carried along by it.
 */
/**
 * A run reference, short enough to read and complete enough to find.
 *
 * Run ids are uuids, so only the first few characters carry any information and the rest is
 * noise. A distilled procedure cites `<run>/<task>` instead, where the part after the slash is
 * the only human-readable thing in the string — truncating to eight characters threw it away and
 * left three citations that all looked identical.
 */
function shortenRun(id: string): string {
  const slash = id.indexOf('/');
  if (slash === -1) return id.slice(0, 8);
  return `${id.slice(0, Math.min(slash, 8))}/${id.slice(slash + 1)}`;
}

export function ProposalQueue() {
  const [items, setItems] = useState<ProposalSummary[] | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .listProposals()
      .then((list) => {
        setItems(list);
        // A proposal that is no longer pending is one somebody else decided, or one this decision
        // just retired. Closing it beats leaving a detail view of something that is gone.
        setOpenId((current) => (list.some((p) => p.id === current) ? current : null));
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  useEffect(refresh, [refresh]);

  return (
    <section className="proposals" data-testid="proposals">
      <h1>Proposals</h1>
      <p className="proposals-lede">
        Things the workbench wants to tell itself, from what it saw in your runs. None of it is in
        effect. A transcript carries text from tools, from files and sometimes from a repository
        nobody vetted, and the system&rsquo;s own instructions are the most valuable thing for that
        text to reach — so they wait here, with the evidence, until you agree.
      </p>

      {error && <p className="proposals-error" data-testid="proposals-error">{error}</p>}

      {items === null && <p className="proposals-empty">Loading&hellip;</p>}

      {items?.length === 0 && (
        <p className="proposals-empty" data-testid="proposals-empty">
          Nothing is waiting. A run that goes cleanly proposes nothing, which is deliberate: a queue
          that fills after every run stops being read, and a gate nobody reads is not a gate.
        </p>
      )}

      <ul className="proposal-list">
        {items?.map((p) => (
          <li key={p.id}>
            <button
              type="button"
              className="proposal-row"
              data-open={p.id === openId}
              data-risk={p.risk}
              onClick={() => setOpenId(p.id === openId ? null : p.id)}
              data-testid={`proposal-${p.id}`}
            >
              <span className="proposal-kind">{p.kind}</span>
              <span className="proposal-scope">
                <bdi>{p.scope}</bdi>
              </span>
              {p.risk === 'elevated' && (
                <span className="proposal-risk" data-testid={`proposal-${p.id}-elevated`}>
                  elevated
                </span>
              )}
            </button>
            {p.id === openId && <ProposalDetail id={p.id} onDecided={refresh} />}
          </li>
        ))}
      </ul>
    </section>
  );
}

/**
 * One proposal, prepared to be read.
 *
 * Fetched when it is opened rather than with the list, because the prepared body is the expensive
 * and dangerous part: it is the text that has been through the sanitiser, and it is the text the
 * hash is over.
 */
function ProposalDetail({ id, onDecided }: { id: string; onDecided: () => void }) {
  const [review, setReview] = useState<ProposalReview | null>(null);
  const [typed, setTyped] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .reviewProposal(id)
      .then(setReview)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  }, [id]);

  if (error) return <p className="proposals-error">{error}</p>;
  if (!review) return <p className="proposals-empty">Loading&hellip;</p>;

  const phraseOk =
    !review.requires_distinct_confirmation || typed.trim() === review.confirmation_phrase;

  const decide = (call: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    call()
      .then(onDecided)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setBusy(false));
  };

  return (
    <div className="proposal-detail" data-testid={`proposal-detail-${id}`}>
      <h2>What it wants to change</h2>
      {/* Sentences, not the serialised payload. The stored body is JSON, and asking somebody to dig
          one line of prose out of it is how an approval gets given to something nobody read. The
          bytes are still here, below, because this is a rendering of them and not a replacement:
          the hash covers those and not this. */}
      <ul className="proposal-changes" data-testid={`proposal-${id}-changes`}>
        {review.changes.map((c, i) => (
          <li key={i}>{c}</li>
        ))}
      </ul>

      <details className="proposal-raw">
        <summary>Exactly what will be stored</summary>
        {/* Sanitised, and this is the one place in the product where the text on screen is known to
            differ from the text on disk — which is what the removals below are for. */}
        <pre className="proposal-body" data-testid={`proposal-${id}-body`}>
          {review.body_for_human}
        </pre>
      </details>

      {review.hidden.length > 0 && (
        /* Located rather than counted. "We removed 3 invisible characters" is not something anyone
           can act on; deciding whether the removal changed the meaning needs to know where they
           were. */
        <div className="proposal-hidden" data-testid={`proposal-${id}-hidden`}>
          <h3>Invisible characters removed before showing you this</h3>
          <p>{review.hidden_summary}</p>
          <ul>
            {review.hidden.map((h, i) => (
              <li key={i}>
                <code>{h.codepoint}</code> at line {h.line}, column {h.column} ({h.kind})
              </li>
            ))}
          </ul>
        </div>
      )}

      <h2>Why it thinks so</h2>
      <div className="proposal-evidence" data-testid={`proposal-${id}-evidence`}>
        <p>{review.evidence.note}</p>
        {review.evidence.verified_signals.length > 0 && (
          <ul>
            {review.evidence.verified_signals.map((s, i) => (
              <li key={i}>{s}</li>
            ))}
          </ul>
        )}
        {review.evidence.supporting_runs.length > 0 && (
          <p className="proposal-runs">
            From{' '}
            {review.evidence.supporting_runs.map((r, i) => (
              <span key={r}>
                {i > 0 && ', '}
                <code title={r}>{shortenRun(r)}</code>
              </span>
            ))}
          </p>
        )}
      </div>

      {review.requires_distinct_confirmation && (
        /* A different gesture, not a scarier button. This proposal changes the machinery that asks
           for approval, so approving it with the same click that approves a note about a test name
           would make the two indistinguishable at the moment of deciding. */
        <div className="proposal-confirm" data-testid={`proposal-${id}-confirm`}>
          <label htmlFor={`confirm-${id}`}>
            This one changes how approval itself works. Type{' '}
            <code>{review.confirmation_phrase}</code> to allow it.
          </label>
          <input
            id={`confirm-${id}`}
            type="text"
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            data-testid={`proposal-${id}-phrase`}
          />
        </div>
      )}

      <div className="proposal-actions">
        <button
          type="button"
          disabled={busy || !phraseOk}
          onClick={() =>
            decide(() =>
              api.approveProposal(
                id,
                // The hash that was shown, not one read back at press time. The daemon compares it
                // with the bytes on disk, so an edit in between invalidates this press rather than
                // riding along with it.
                review.content_hash,
                review.requires_distinct_confirmation ? typed.trim() : undefined,
              ),
            )
          }
          data-testid={`proposal-${id}-approve`}
        >
          Approve
        </button>
        <button
          type="button"
          className="quiet"
          disabled={busy}
          onClick={() => decide(() => api.rejectProposal(id))}
          data-testid={`proposal-${id}-reject`}
        >
          Reject
        </button>
        <span className="proposal-hash" title={review.content_hash}>
          {review.content_hash.slice(0, 12)}
        </span>
      </div>
    </div>
  );
}
