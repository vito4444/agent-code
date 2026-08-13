import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Composer, type AutonomyLevel } from './components/composer/Composer';
import { Turn } from './components/chat/Turn';
import { RulesScreen } from './components/rules/RulesScreen';
import { RawInspector } from './components/inspector/RawInspector';
import { RunList, StartRun } from './components/run/RunList';
import { RunView } from './components/run/RunView';
import { SessionList, asWorker } from './components/sidebar/SessionList';
import { StartHere } from './components/chat/StartHere';
import { PlanPanel } from './components/chat/PlanPanel';
import { Settings, applyTheme, readTheme } from './components/settings/Settings';
import { ProposalQueue } from './components/proposals/ProposalQueue';
import { ReviewSurface } from './components/review/ReviewSurface';
import * as api from './lib/api';
import { contextPercent, emptySession, runList, useStore } from './lib/store';
import { EventStream, defaultStreamUrl } from './lib/ws';
import { useStickToBottom } from './lib/stickToBottom';

type Screen = 'chat' | 'rules' | 'inspector' | 'runs' | 'proposals' | 'settings';

/**
 * The session a run's worker had, by task.
 *
 * Matched on the worktree path, which already contains the run and the task, so nothing extra has to
 * be recorded to make the link. Called with no task to ask the weaker question — "is any of this
 * run's work still open?" — which is what decides whether to offer the link at all.
 */
/**
 * The directory a rule written "for this project" should attach to.
 *
 * The session's memory scope, never its working directory. A worker's working directory is a git
 * worktree the run created and will delete, so scoping a rule there saves something the screen
 * shows as enabled and nothing ever reads again — worse than not offering it, which is the same
 * argument the rules screen makes for keeping rules and inferred memories apart.
 */
export function ruleScopeFor(session: api.SessionSummary | undefined): string | null {
  return session?.memory_scope ?? null;
}

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

export function App() {
  const store = useStore();
  const [screen, setScreen] = useState<Screen>('chat');
  const [autonomy, setAutonomy] = useState<AutonomyLevel>('ask_outside_sandbox');
  const [activeRunId, setActiveRunId] = useState<string | null>(null);
  const [agents, setAgents] = useState<api.AgentSummary[]>([]);
  /** `{ runId, taskId }`, where a null task means the run's whole candidate. */
  const [reviewing, setReviewing] = useState<{ runId: string; taskId: string | null } | null>(
    null,
  );
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

  // The saved theme, applied before anything is drawn. Without this the interface flashes the default
  // and then corrects itself, which reads as a bug in the theme rather than in the order of operations.
  useEffect(() => {
    applyTheme(readTheme());
  }, []);

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
  // Keyed on the session, so opening a different conversation starts at its newest turn rather
  // than at whatever offset the previous one was left at.
  const transcript = useStickToBottom(activeId);

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
    (text: string, mentions: string[] = []) => {
      if (!activeId) return;
      useStore.getState().setBusy(activeId, true);
      api.sendPrompt(activeId, text, mentions).catch(() => {
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
          <button
            type="button"
            data-active={screen === 'proposals'}
            onClick={() => setScreen('proposals')}
            title="Things the workbench wants to tell itself. Nothing here is in effect until you agree."
            data-testid="nav-proposals"
          >
            Proposals
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
          <button
            type="button"
            data-active={screen === 'settings'}
            onClick={() => setScreen('settings')}
            data-testid="nav-settings"
          >
            Settings
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

        {screen === 'proposals' && <ProposalQueue />}
        {screen === 'settings' && <Settings agents={agents} />}
        {screen === 'rules' && <RulesScreen projectRoot={ruleScopeFor(summary)} />}
        {screen === 'inspector' && <RawInspector />}

        {screen === 'runs' && (
          <section className="runs">
            {reviewing ? (
              <ReviewSurface
                title={
                  reviewing.taskId
                    ? `What ${reviewing.taskId} changed`
                    : 'Everything this run would add'
                }
                subtitle={
                  reviewing.taskId
                    ? 'From this task\u2019s own starting commit, so it excludes whatever its dependencies produced.'
                    : 'From the commit the run started at to the candidate.'
                }
                load={() =>
                  reviewing.taskId
                    ? api.taskDiff(reviewing.runId, reviewing.taskId)
                    : api.candidateDiff(reviewing.runId)
                }
                onClose={() => setReviewing(null)}
              />
            ) : activeRun ? (
              <RunView
                run={activeRun}
                onBack={() => setActiveRunId(null)}
                onReview={(taskId) => setReviewing({ runId: activeRun.id, taskId })}
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
            {/* The wrapper exists so the jump-to-latest pill can be positioned against the foot
                of the visible record. Inside the scrolling element it would scroll away with the
                content; outside it, the only available anchor is the whole column, whose bottom
                moves whenever the composer grows a queue or an attachment strip. */}
            <div className="transcript-frame">
            <div
              className="transcript"
              data-testid="transcript"
              ref={transcript.ref}
              onScroll={transcript.onScroll}
            >
              {session.turns.length === 0 && !activeId && (
                <StartHere
                  agents={agents}
                  onRuns={() => setScreen('runs')}
                  onStart={(agentId, root, text) => {
                    api
                      .createSession(agentId, root)
                      .then((created) => {
                        useStore.getState().setSessionList([...store.sessionList, created]);
                        useStore.getState().setActiveSession(created.id);
                        useStore.getState().setBusy(created.id, true);
                        return api.sendPrompt(created.id, text);
                      })
                      .catch(() => {
                        // The daemon's own refusal — a directory that is not there, an agent that
                        // will not launch — comes back through the session list on its next load.
                        // Swallowing it here keeps a bad path from taking the interface down with
                        // it.
                      });
                  }}
                />
              )}
              {session.turns.length === 0 && activeId && (
                <div className="empty">Nothing yet. Describe what you want done.</div>
              )}
              {session.turns.map((turn) => (
                <Turn
                  key={turn.turn}
                  turn={turn}
                  projectRoot={summary?.project_root ?? null}
                  onAnswerPermission={answerPermission}
                />
              ))}
              {session.unknownUpdates.length > 0 && (
                <div className="unknown-note">
                  {session.unknownUpdates.length} protocol message
                  {session.unknownUpdates.length === 1 ? '' : 's'} from this agent were not
                  recognized and were skipped. They are in the protocol log.
                </div>
              )}
            </div>

            {!transcript.stuck && (
              <button
                type="button"
                className="jump-latest"
                onClick={transcript.jump}
                data-testid="jump-latest"
              >
                Jump to latest
              </button>
            )}
            </div>

            {/* Between the transcript and the composer, and outside the scrolling record.
                Outside because a plan is replaced wholesale on every update — it is current state
                rather than something that happened at a point in time, and a mutating block inside a
                scrolling record appears to say different things at different scroll positions.
                Here rather than above the transcript for the same reason the context ring is in the
                composer: progress is what somebody glances at between two messages, so it belongs on
                the path their eye takes back to the input box. Above the transcript it also arrived
                before the prompt it was a plan for, which reads backwards. */}
            {activeId && <PlanPanel plans={session.plans} busy={session.busy} />}

            {/* With no session open the start screen carries its own input, which opens a session
                and sends the first turn in one act. Two composers on one screen would be two answers
                to "where do I type", so this one waits until there is a conversation to add to. */}
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
              sessionId={activeId}
              promptCapabilities={summary?.prompt_capabilities}
              searchPaths={api.sessionPaths}
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
                  .then(() => send(text, []))
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
