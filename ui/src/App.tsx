import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Composer, type AutonomyLevel } from './components/composer/Composer';
import { Turn } from './components/chat/Turn';
import { RulesScreen } from './components/rules/RulesScreen';
import { RawInspector } from './components/inspector/RawInspector';
import { RunList, StartRun } from './components/run/RunList';
import { RunView } from './components/run/RunView';
import * as api from './lib/api';
import { contextPercent, emptySession, runList, useStore } from './lib/store';
import { EventStream, defaultStreamUrl } from './lib/ws';

type Screen = 'chat' | 'rules' | 'inspector' | 'runs';

export function App() {
  const store = useStore();
  const [screen, setScreen] = useState<Screen>('chat');
  const [autonomy, setAutonomy] = useState<AutonomyLevel>('ask_outside_sandbox');
  const [activeRunId, setActiveRunId] = useState<string | null>(null);
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
        <ul className="sidebar-sessions">
          {store.sessionList.map((s) => (
            <li key={s.id}>
              <button
                type="button"
                data-active={s.id === activeId}
                onClick={() => {
                  useStore.getState().setActiveSession(s.id);
                  setScreen('chat');
                }}
              >
                <span className="session-agent">{s.agent_display_name}</span>
                <span className="session-title">{s.title ?? s.project_root}</span>
              </button>
            </li>
          ))}
        </ul>
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
              <RunView run={activeRun} onBack={() => setActiveRunId(null)} />
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
                <div className="empty">
                  {activeId
                    ? 'No turns yet. Describe what you want done.'
                    : 'No session selected.'}
                </div>
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
          </>
        )}
      </main>
    </div>
  );
}
