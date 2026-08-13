import { useEffect, useState } from 'react';
import type { AgentSummary } from '../../lib/types';

/**
 * The settings screen.
 *
 * Small on purpose, and honest about being small. Three themes existed in the stylesheet with no way
 * to choose one, which is the same shape of defect as a mechanism with no caller: the work was done
 * and nobody could reach it.
 *
 * What it does *not* pretend to offer matters as much. There is no model or provider configuration
 * here, because the daemon does not manage credentials — an agent is a command line it was given, and
 * a settings page with an empty provider list would say a choice exists and is broken. The agents
 * section therefore reports what was configured and where to change it, rather than offering to change
 * it here.
 */
export function Settings({ agents }: { agents: AgentSummary[] }) {
  const [theme, setTheme] = useState<ThemeName>(() => readTheme());

  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  return (
    <section className="settings" data-testid="settings">
      <h1>Settings</h1>

      <section className="settings-group">
        <h2>Appearance</h2>
        <p className="settings-note">
          Themes are a set of variables and nothing else — no component reads a colour directly — so
          switching one cannot change behaviour, only how things look.
        </p>
        <ul className="theme-list">
          {THEMES.map((t) => (
            <li key={t.id}>
              <button
                type="button"
                data-active={t.id === theme}
                onClick={() => setTheme(t.id)}
                data-testid={`theme-${t.id}`}
              >
                <span className="theme-name">{t.name}</span>
                <span className="theme-note">{t.note}</span>
                {/* Swatches drawn from the theme's own variables rather than repeated here as
                    literals, so a theme that changes cannot leave its preview behind. */}
                <span className="theme-swatches" data-theme-preview={t.id} aria-hidden="true">
                  <i data-swatch="bg" />
                  <i data-swatch="fg" />
                  <i data-swatch="accent" />
                  <i data-swatch="success" />
                  <i data-swatch="danger" />
                </span>
              </button>
            </li>
          ))}
        </ul>
      </section>

      <section className="settings-group">
        <h2>Agents</h2>
        <p className="settings-note">
          An agent is a command line the daemon was started with, so this is a list rather than an
          editor. Pass <code>--agent id=Name=command</code> to add one, and{' '}
          <code>--worker-agent id</code> to let it take orchestrated work.
        </p>
        {agents.length === 0 ? (
          <p className="settings-empty" data-testid="settings-no-agents">
            None are configured, which is why the sidebar offers nothing to open.
          </p>
        ) : (
          <ul className="settings-agents" data-testid="settings-agents">
            {agents.map((a) => (
              <li key={a.id}>
                <code>{a.id}</code>
                <span>{a.display_name}</span>
                {/* What it can change without restarting, which is what decides whether a selector
                    switches in place or opens a new session. Absent for almost every agent, and
                    saying so is more useful than an empty list. */}
                <span className="settings-live">
                  {a.live_config_ids.length > 0
                    ? `can change ${a.live_config_ids.join(', ')} in place`
                    : 'changing a setting starts a new session'}
                </span>
              </li>
            ))}
          </ul>
        )}
      </section>
    </section>
  );
}

type ThemeName = 'paper' | 'primer' | 'dark';

const THEMES: { id: ThemeName; name: string; note: string }[] = [
  {
    id: 'paper',
    name: 'Paper',
    note: 'Warm ground, serif for prose. A transcript is mostly read.',
  },
  {
    id: 'primer',
    name: 'Primer',
    note: "GitHub's palette, for matching the rest of a working day.",
  },
  { id: 'dark', name: 'Dark', note: 'The same structure on a dark ground.' },
];

const STORAGE_KEY = 'wkbd.theme';

/**
 * The theme is applied by setting one attribute on the document.
 *
 * Persisted in local storage rather than on the daemon. A daemon-side setting would be shared by every
 * client connected to it, and two people looking at one workbench from different machines do not have
 * to agree about contrast.
 */
export function applyTheme(theme: ThemeName) {
  const root = document.documentElement;
  // `paper` is the default and has no attribute, so the stylesheet's own `:root` block is the theme.
  // Writing `data-theme="paper"` would need a fourth block that duplicated it.
  if (theme === 'paper') {
    root.removeAttribute('data-theme');
  } else {
    root.setAttribute('data-theme', theme);
  }
  try {
    localStorage.setItem(STORAGE_KEY, theme);
  } catch {
    // A browser with storage denied still gets the theme for this session. Failing to remember a
    // preference is not a reason to refuse to apply it.
  }
}

export function readTheme(): ThemeName {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved === 'primer' || saved === 'dark' || saved === 'paper') return saved;
  } catch {
    // As above.
  }
  return 'paper';
}
