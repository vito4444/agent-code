import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Composer, type AutonomyLevel } from './components/composer/Composer';
import { Turn } from './components/chat/Turn';
import { RulesScreen } from './components/rules/RulesScreen';
import { RawInspector } from './components/inspector/RawInspector';
import { RunList, StartRun } from './components/run/RunList';
import { RunView } from './components/run/RunView';
import { SessionList, asWorker } from './components/sidebar/SessionList';
import * as api from './lib/api';
import { contextPercent, emptySession, runList, useStore } from './lib/store';
import { EventStream, defaultStreamUrl } from './lib/ws';

type Screen = 'chat' | 'rules' | 'inspector' | 'runs';

/**
 * The session a run's worker had, by task.
 *
 * Matched on the worktree path, which already contains the run and the task, so nothing extra has to
 * be recorded to make the link. Called with no task to ask the weaker question — "is any of this
 * run's work still open?" — which is what decides whether to offer the link at all.
 */
export function workerSessionFor(
  sessions: api.SessionSummary[],
  runId: string,
  taskId?: string,
): string | null {
  for (const s of sessions) {
    const worker = asWorker(s.project_root);
    if (!worker || worker.runId !== runId) continue;
    if (taskId === undefined || worker.task === taskId) return s.id;
  }
  return null;
}

/**
 * What the transcript says before there is one.
 *
 * "No session selected." was accurate and useless: it named a state without saying what to do about
 * it, in the largest empty area in the application. The two things worth saying here are the two
 * things this workbench does that a single chat window does not, so they are what fills the space.
 */
function StartHere({ hasSession, onRuns }: { hasSession: boolean; onRuns: () => void }) {
  return (
    <div className="start-here" data-testid="start-here">
      <h1>Workbench</h1>
      {hasSession ? (
        <p>Describe what you want done. The reasoning, every tool call and every file touched will
          appear here as it happens.</p>
      ) : (
        <p>Open a session from the sidebar to talk to one agent, or start a run to have a goal
          broken into tasks and worked on in parallel.</p>
      )}
      <div className="start-here-cards">
        <article>
          <h2>One agent, one conversation</h2>
          <p>
            Streaming reasoning you can collapse, tool calls with the diff inside them, and every
            file read or written on the agent's behalf — including the ones it was refused.
          </p>
        </article>
        <article>
          <h2>A goal, planned and split</h2>
          <p>
            One sentence becomes a task graph. Each task gets its own worktree, its own acceptance
            check, and nothing merges until you say so.
          </p>
          <button type="button" onClick={onRuns} data-testid="start-here-runs">
            Go to runs
          </button>
        </article>
      </div>
    </div>
  );
}

export function App() {
  const store = useStore();
  const [screen, setScreen] = useState<Screen>('chat');
  const [autonomy, setAutonomy] = useState<AutonomyLevel>('ask_outside_sandbox');
  const [activeRunId, setActiveRunId] = useState<string | null>(null);
  const [agents, setAgents] = useState<api.AgentSummary[]>([]);
  const streamRef = useRef<EventStream | null>(null);

  // The stream is created once. Recreating it on state change would reconnect on every
  // event, and reconnecting resends from the high-water mark, so the cost compounds.
  useEffect(() => {
    const stream = new EventStream(
      defaultStreamUrl(),
      {
        onEvents: (events) => useStore.getState().applyEvents(events),
        onConnected: (c) => useStore.getState().setConnected(c),
        onHello: (hello) => useStore.getState().setDegraded(hello.degraded),
      },
      () => useStore.getState().highWaterMark,
    );
    streamRef.current = stream;
    stream.start();
    return () => stream.stop();
  }, []);

  useEffect(() => {
    api
      .listSessions()
      .then((list) => {
        useStore.getState().setSessionList(list);
        if (list.length > 0 && useStore.getState().activeSessionId === null) {
          useStore.getState().setActiveSession(list[0].id);
        }
      })
      .catch(() => {
        // A daemon that is not up yet is normal during startup; the stream's reconnect
        // loop will bring the list in when it arrives.
      });
  }, []);

  // A session this client has not heard of.
  //
  // The list is fetched once at startup, which is right for the common case and wrong for every other
  // one: the daemon serves more than one client, and the orchestrator opens sessions of its own. A
  // session created anywhere else would otherwise stay invisible until somebody reloaded — and for
  // the orchestrator's workers, whose transcripts are the only record of what a task actually did,
  // that is the information least worth hiding.
  //
  // Keyed on the *set* of ids rather than on a count, so a session closing and another opening in the
  // same batch still triggers a refetch.
  const knownIds = store.sessionList.map((s) => s.id).join(',');
  const streamedIds = Object.keys(store.sessions).sort().join(',');
  useEffect(() => {
    const known = new Set(knownIds.split(',').filter(Boolean));
    const unknown = streamedIds.split(',').filter((id) => id !== '' && !known.has(id));
    if (unknown.length === 0) return;
    api
      .listSessions()
      .then((list) => {
        useStore.getState().setSessionList(list);
        if (useStore.getState().activeSessionId === null && list.length > 0) {
          useStore.getState().setActiveSession(list[0].id);
        }
      })
      .catch(() => {});
  }, [knownIds, streamedIds]);

  // Which agents exist at all. Fetched once: the set is fixed at daemon startup, since an agent is a
  // command line the daemon was told about.
  useEffect(() => {
    api.listAgents().then(setAgents).catch(() => {});
  }, []);

  // Runs are folded from the event stream, so this only covers the window before the replay
  // arrives, and a run whose events have been trimmed from the log.
  useEffect(() => {
    api
      .listRuns()
      .then((list) => useStore.getState().seedRuns(list))
      .catch(() => {});
  }, []);

  const activeId = store.activeSessionId;
  const session = activeId ? (store.sessions[activeId] ?? emptySession()) : emptySession();
  const summary = store.sessionList.find((s) => s.id === activeId);

  const percent = useMemo(() => contextPercent(session), [session]);

  const runs = useMemo(() => runList(store.runs), [store.runs]);
  const activeRun = activeRunId ? (store.runs[activeRunId] ?? null) : null;

  // When a turn finishes and something is queued, send the next one. This is where the
  // queue is honest: it waits for the turn to actually end rather than guessing from a
  // pause in the stream, because the only authoritative signal is the daemon reporting a
  // stop reason, which is what clears `busy`.
  useEffect(() => {
    if (!activeId || session.busy) return;
    const next = useStore.getState().takeQueued(activeId);
    if (!next) return;
    useStore.getState().setBusy(activeId, true);
    api.sendPrompt(activeId, next.text).catch(() => {
      useStore.getState().setBusy(activeId, false);
    });
  }, [activeId, session.busy]);

  const send = useCallback(
    (text: string) => {
      if (!activeId) return;
      useStore.getState().setBusy(activeId, true);
      api.sendPrompt(activeId, text).catch(() => {
        useStore.getState().setBusy(activeId, false);
      });
    },
    [activeId],
  );

  const answerPermission = useCallback(
    (requestId: string, optionId: string | null) => {
      if (!activeId) return;
      api.answerPermission(activeId, requestId, optionId).catch(() => {});
    },
    [activeId],
  );

  return (
    <div className="app">
      <nav className="sidebar">
        <div className="sidebar-brand">Workbench</div>
        <SessionList
          sessions={store.sessionList}
          agents={agents}
          activeId={activeId}
          onSelect={(id) => {
            useStore.getState().setActiveSession(id);
            setScreen('chat');
          }}
          onNew={(agentId, root) => {
            api
              .createSession(agentId, root)
              .then((created) => {
                useStore.getState().setSessionList([...store.sessionList, created]);
                useStore.getState().setActiveSession(created.id);
                setScreen('chat');
              })
              .catch(() => {
                // Reported by the daemon's own error, which the list will reflect on its next load.
                // Swallowing it here rather than throwing keeps a bad directory from taking the
                // whole interface down.
              });
          }}
        />
        <div className="sidebar-footer">
          <button
            type="button"
            data-active={screen === 'runs'}
            onClick={() => setScreen('runs')}
            title="One sentence, planned into a task graph and worked on in parallel"
            data-testid="nav-runs"
          >
            Runs
          </button>
          <button type="button" data-active={screen === 'rules'} onClick={() => setScreen('rules')}>
            User rules
          </button>
          <button
            type="button"
            data-active={screen === 'inspector'}
            onClick={() => setScreen('inspector')}
            title="Raw ACP frames, exactly as they crossed the wire"
          >
            Protocol log
          </button>
          <span className="connection" data-connected={store.connected}>
            {store.connected ? 'connected' : 'reconnecting…'}
          </span>
        </div>
      </nav>

      <main className="main">
        {store.degradedReason && (
          <div className="banner banner-degraded">
            Running read-only: {store.degradedReason}. Your data is intact and can be exported;
            new work cannot be saved until this is resolved.
          </div>
        )}

        {screen === 'rules' && <RulesScreen projectRoot={summary?.project_root ?? null} />}
        {screen === 'inspector' && <RawInspector />}

        {screen === 'runs' && (
          <section className="runs">
            {activeRun ? (
              <RunView
                run={activeRun}
                onBack={() => setActiveRunId(null)}
                onOpenTranscript={
                  // Only offered when the worker's session is still around. The link is resolved
                  // here rather than inside the run view because this is where the session list is,
                  // and a run view that had to know about sessions would need both.
                  workerSessionFor(store.sessionList, activeRun.id) !== null
                    ? (taskId) => {
                        const id = workerSessionFor(store.sessionList, activeRun.id, taskId);
                        if (id) {
                          useStore.getState().setActiveSession(id);
                          setScreen('chat');
                        }
                      }
                    : undefined
                }
              />
            ) : (
              <>
                <h1>Runs</h1>
                <StartRun
                  onStarted={(run) => {
                    useStore.getState().seedRuns([run]);
                    setActiveRunId(run.id);
                  }}
                />
                <RunList
                  runs={runs}
                  activeRunId={activeRunId}
                  onSelect={(id) => setActiveRunId(id)}
                />
              </>
            )}
          </section>
        )}

        {screen === 'chat' && (
          <>
            <div className="transcript" data-testid="transcript">
              {session.turns.length === 0 && (
                <StartHere hasSession={activeId !== null} onRuns={() => setScreen('runs')} />
              )}
              {session.turns.map((turn) => (
                <Turn key={turn.turn} turn={turn} onAnswerPermission={answerPermission} />
              ))}
              {session.unknownUpdates.length > 0 && (
                <div className="unknown-note">
                  {session.unknownUpdates.length} protocol message
                  {session.unknownUpdates.length === 1 ? '' : 's'} from this agent were not
                  recognized and were skipped. They are in the protocol log.
                </div>
              )}
            </div>

            {/* No session means nothing to send to, and a composer with a Send button that cannot
                send is an affordance for something impossible. The empty state above already says
                what to do instead, so this is absent rather than disabled: a disabled control still
                asks the reader to work out why. */}
            {activeId && (
            <Composer
              agentName={summary?.agent_display_name ?? 'no agent'}
              configOptions={session.configOptions}
              contextPercent={percent}
              usage={session.usage}
              busy={session.busy}
              queue={session.queue}
              autonomy={autonomy}
              steeringSupported={false}
              onSend={send}
              onQueue={(text) => {
                if (!activeId) return;
                useStore.getState().enqueue(activeId, {
                  id: crypto.randomUUID(),
                  text,
                  mode: 'queue',
                });
              }}
              onStopAndSend={(text) => {
                if (!activeId) return;
                api
                  .cancelTurn(activeId)
                  .then(() => send(text))
                  .catch(() => {});
              }}
              onCancel={() => {
                if (activeId) api.cancelTurn(activeId).catch(() => {});
              }}
              onConfigChange={(optionId, value) => {
                if (!activeId) return;
                api.setConfigOption(activeId, optionId, value).catch(() => {});
              }}
              onAutonomyChange={setAutonomy}
              onDequeue={(id) => {
                if (activeId) useStore.getState().dequeue(activeId, id);
              }}
            />
            )}
          </>
        )}
      </main>
    </div>
  );
}
