import { useEffect, useState } from 'react';
import { deleteRule, listRules, saveRule } from '../../lib/api';

/**
 * User rules.
 *
 * Two scopes: this machine, or this project only.
 *
 * The important property is not on this screen but behind it: a user rule and a memory the
 * system inferred are different kinds of record, and the automatic extractor cannot produce
 * a user rule even in principle — the kind is absent from its output vocabulary, the storage
 * layer rejects it, and every consolidation query excludes it. A preference that could be
 * rewritten by a model, or quietly retired by confidence decay, is a setting the program has
 * stopped honouring while this screen still shows it as enabled. That is worse than not
 * having the feature.
 *
 * This screen therefore only ever shows rules, and says where they apply. Inferred memories
 * live on their own screen and are labelled as evidence.
 */
export interface Rule {
  id: string;
  scope: 'global' | 'project';
  project_root: string | null;
  body: string;
  enabled: boolean;
  /** Which session entry points have been observed applying this rule. */
  applied_at: string[];
}

export function RulesScreen({ projectRoot }: { projectRoot: string | null }) {
  const [rules, setRules] = useState<Rule[]>([]);
  const [draft, setDraft] = useState('');
  const [scope, setScope] = useState<'global' | 'project'>('global');
  const [error, setError] = useState<string | null>(null);

  const reload = () => {
    listRules(projectRoot)
      .then(setRules)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  };

  useEffect(reload, [projectRoot]);

  const add = async () => {
    const body = draft.trim();
    if (!body) return;
    try {
      await saveRule({ scope, project_root: scope === 'project' ? projectRoot : null, body });
      setDraft('');
      setError(null);
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const global = rules.filter((r) => r.scope === 'global');
  const project = rules.filter((r) => r.scope === 'project');

  return (
    <section className="rules">
      <header className="rules-header">
        <h1>User rules</h1>
        <p className="rules-intro">
          Written once, applied to every session. These are instructions, not guesses: they
          are passed through verbatim, they are never rewritten, and nothing in the learning
          system can retire them.
        </p>
      </header>

      {error && <div className="rules-error">{error}</div>}

      <div className="rules-compose">
        <textarea
          value={draft}
          rows={2}
          placeholder="e.g. Always answer in Chinese. Ask before adding a dependency."
          onChange={(e) => setDraft(e.target.value)}
          data-testid="rule-input"
        />
        <div className="rules-compose-actions">
          <label>
            <span>Applies to</span>
            <select
              value={scope}
              onChange={(e) => setScope(e.target.value as 'global' | 'project')}
              data-testid="rule-scope"
            >
              <option value="global">Every project on this machine</option>
              <option value="project" disabled={projectRoot === null}>
                {projectRoot ? `Only ${projectRoot}` : 'Only this project (none open)'}
              </option>
            </select>
          </label>
          <button type="button" onClick={add} disabled={!draft.trim()} data-testid="rule-add">
            Add rule
          </button>
        </div>
      </div>

      <RuleGroup
        title="Every project on this machine"
        rules={global}
        onDelete={async (id) => {
          await deleteRule(id);
          reload();
        }}
      />
      <RuleGroup
        title={projectRoot ? `Only ${projectRoot}` : 'This project'}
        rules={project}
        onDelete={async (id) => {
          await deleteRule(id);
          reload();
        }}
      />
    </section>
  );
}

function RuleGroup({
  title,
  rules,
  onDelete,
}: {
  title: string;
  rules: Rule[];
  onDelete: (id: string) => void;
}) {
  return (
    <div className="rules-group">
      <h2>{title}</h2>
      {rules.length === 0 ? (
        <p className="rules-empty">No rules here yet.</p>
      ) : (
        <ul>
          {rules.map((r) => (
            <li key={r.id} className="rule" data-testid={`rule-${r.id}`}>
              <span className="rule-body">{r.body}</span>
              {/*
                Which entry points have been seen applying this rule. Rules going missing in
                one situation and not another is the classic failure here — a resumed
                session, a worker the orchestrator started, a session restarted to change a
                model — so the answer is shown rather than asserted.
              */}
              {r.applied_at.length > 0 && (
                <span
                  className="rule-applied"
                  title={`Observed applying at: ${r.applied_at.join(', ')}`}
                >
                  applied at {r.applied_at.length} entry point
                  {r.applied_at.length === 1 ? '' : 's'}
                </span>
              )}
              <button
                type="button"
                className="rule-delete"
                aria-label={`Delete rule: ${r.body}`}
                onClick={() => onDelete(r.id)}
              >
                Delete
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
