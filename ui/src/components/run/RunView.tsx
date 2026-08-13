import { useState } from 'react';
import { abandonRun, cancelRun, mergeRun } from '../../lib/api';
import { runBlocker, taskById } from '../../lib/store';
import type { RunBlocker, RunState, TaskState, TaskVerificationView } from '../../lib/store';
import type { RunStatus, TaskStatus } from '../../lib/types';

/**
 * One orchestrated run.
 *
 * The screen is arranged around the two questions a reader actually has: what is this run
 * doing, and what does it need from me. The graph answers the first, the gate at the bottom
 * answers the second, and everything the model contributed — the graph itself, and each
 * replan — is shown as a labelled event rather than folded silently into the current state.
 *
 * The graph is a column per wave. That is the whole layout: tasks stacked in one column ran
 * at the same time, columns run left to right. A real graph drawing would need a layout
 * library and would say less, because the only relationships that matter here are "these are
 * concurrent" and "this waits for that", and both are readable as position.
 */
export function RunView({
  run,
  onBack,
  onOpenTranscript,
  onReview,
}: {
  run: RunState;
  onBack?: () => void;
  /**
   * Opens the conversation a task's worker had, when one is still around.
   *
   * The most useful thing missing from this screen was that a card could say a task passed and give
   * no way to see what it did. The transcript is where the answer is — the files it wrote, the
   * commands it ran, what it was refused — and it was already on disk and already in the sidebar,
   * one click away and unreachable from here.
   */
  onOpenTranscript?: (taskId: string) => void;
  /** Opens the full-width diff for one task, or for the candidate when no task is named. */
  onReview?: (taskId: string | null) => void;
}) {
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);
  const blocker = runBlocker(run);
  const active = run.status === 'planning' || run.status === 'running';

  const act = (call: (id: string) => Promise<unknown>) => {
    setPending(true);
    setError(null);
    call(run.id)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setPending(false));
  };

  return (
    <section className="run-view" data-testid={`run-${run.id}`}>
      <header className="run-header">
        {onBack && (
          <button type="button" className="run-back" onClick={onBack} data-testid="run-back">
            All runs
          </button>
        )}
        <h1 className="run-goal" data-testid="run-goal">
          {run.goal}
        </h1>
        <div className="run-meta">
          <code className="run-root">{run.project_root}</code>
          {/* Absent until the run's first event says what it forked from, and drawn as nothing
              until then rather than as an empty pair of ticks. */}
          {run.base_commit && (
            <span className="run-base" title={run.base_commit}>
              base <code>{shortCommit(run.base_commit)}</code>
            </span>
          )}
          <RunStatusBadge status={run.status} />
          {/* Which draft the graph on screen came from. Without it a replanned run looks like
              the first plan quietly changing shape. */}
          {run.planAttempt > 1 && (
            <span className="plan-attempt">plan attempt {run.planAttempt}</span>
          )}
          {active && (
            <button
              type="button"
              className="run-action"
              disabled={pending}
              onClick={() => act(cancelRun)}
              data-testid="cancel-run"
            >
              Cancel run
            </button>
          )}
        </div>
        {run.detail && <p className="run-detail">{run.detail}</p>}
        {blocker && (
          <p className="run-blocker" data-testid="run-blocker" data-kind={blocker.kind}>
            {blockerText(blocker)}
          </p>
        )}
        {error && <p className="run-error">{error}</p>}
      </header>

      {run.planRejections.length > 0 && (
        <section className="run-section" data-testid="plan-rejections">
          <h2>Plans that did not pass validation</h2>
          {/*
            The graph a model drafted and a deterministic check refused. Shown because a planner
            that keeps producing the same invalid shape is a fact about the planner, and because
            these problems are the input to the next attempt: hiding them makes a retry loop look
            like a hang.
          */}
          {run.planRejections.map((rejection, i) => (
            <div className="plan-rejection" key={i} data-testid={`plan-rejection-${i}`}>
              <span className="plan-attempt">attempt {rejection.attempt}</span>
              <ul className="problem-list">
                {rejection.problems.map((problem) => (
                  <li key={problem}>{problem}</li>
                ))}
              </ul>
            </div>
          ))}
        </section>
      )}

      {run.waves.length === 0 ? (
        <p className="run-empty" data-testid="run-no-graph">
          No task graph yet. The planner is still drafting one, and nothing has been dispatched,
          so there is nothing running to show.
        </p>
      ) : (
        <div className="run-waves" data-testid="run-waves">
          {run.waves.map((wave, i) => (
            <section className="run-wave" key={i} data-wave={i} data-testid={`wave-${i}`}>
              <h2 className="wave-title">
                Wave {i + 1}
                <span className="wave-note">
                  {i === 0
                    ? `${wave.length} in parallel from the base commit`
                    : `${wave.length} in parallel, after wave ${i}`}
                </span>
              </h2>
              {wave.map((taskId) => {
                const task = taskById(run, taskId);
                return task ? (
                  <TaskCard
                    key={taskId}
                    task={task}
                    onOpenTranscript={onOpenTranscript}
                    onReview={onReview}
                  />
                ) : (
                  <p className="task-missing" key={taskId} data-testid={`task-missing-${taskId}`}>
                    <code>{taskId}</code> is in the schedule but the plan carried no description
                    of it.
                  </p>
                );
              })}
            </section>
          ))}
        </div>
      )}

      {run.replans.length > 0 && (
        <section className="run-section" data-testid="replan-history">
          <h2>Replans</h2>
          {/*
            The second and only other point at which a model decides anything in a run. It is
            listed rather than applied invisibly: a graph that changed under the reader, with no
            record of what forced the change, is the difference between a run that can be read
            afterwards and one that can only be re-sampled.
          */}
          <ol className="replan-list">
            {run.replans.map((replan, i) => (
              <li className="replan" key={i} data-testid={`replan-${i}`}>
                <span className="replan-trigger">{replan.trigger}</span>
                <span className="replan-task">
                  triggered by <code>{replan.task_id}</code>
                </span>
                <span className="replan-attempt">attempt {replan.attempt}</span>
              </li>
            ))}
          </ol>
        </section>
      )}

      {run.mergeRejections.length > 0 && (
        <section className="run-section run-merge-rejections" data-testid="merge-rejected">
          <h2>Results that would not combine</h2>
          {run.mergeRejections.map((rejection, i) => (
            <div className="merge-rejection" key={i} data-testid={`merge-rejection-${i}`}>
              <p className="rejection-dropped">
                Dropped <code>{rejection.task_id}</code>
              </p>
              <p className="rejection-detail">{rejection.detail}</p>
              {/*
                What survived matters as much as what failed. Without it a conflict reads as
                "the run lost everything", and the usual next question — which work do I still
                have — has no answer on screen.
              */}
              <div className="rejection-kept" data-testid={`merge-rejection-${i}-kept`}>
                <span className="rejection-kept-label">Kept their work:</span>
                <ul className="id-list">
                  {rejection.merged.map((taskId) => (
                    <li key={taskId}>
                      <code>{taskId}</code>
                    </li>
                  ))}
                </ul>
              </div>
            </div>
          ))}
        </section>
      )}

      {run.status === 'awaiting_merge' && run.mergeCandidate && (
        <section className="merge-gate" data-testid="merge-gate">
          <h2>Ready to merge, waiting for you</h2>
          <p className="merge-why">
            Acceptance proved that the assertions each task named pass. It did not prove the
            change is the one you asked for, and a named test suite is the easiest thing in a run
            to satisfy the wrong way. So nothing is merged until you say so.
          </p>
          <p className="merge-candidate">
            candidate <code title={run.mergeCandidate.commit}>{shortCommit(run.mergeCandidate.commit)}</code>
          </p>
          <div className="merge-order" data-testid="merge-order">
            <span className="merge-order-label">Combined in this order:</span>
            <ol className="id-list">
              {run.mergeCandidate.order.map((taskId) => (
                <li key={taskId}>
                  <code>{taskId}</code>
                </li>
              ))}
            </ol>
          </div>
          {run.excluded.length > 0 && (
            <div className="merge-excluded" data-testid="merge-excluded">
              <span className="merge-excluded-label">Left out of the candidate:</span>
              <ul className="id-list">
                {run.excluded.map((taskId) => (
                  <li key={taskId}>
                    <code>{taskId}</code>
                  </li>
                ))}
              </ul>
            </div>
          )}
          {onReview && (
            /* Before the buttons, and deliberately. The gate's whole argument is that acceptance is
               not the same as the change being wanted, and the only way to decide the second is to
               read it. */
            <button
              type="button"
              className="merge-review"
              onClick={() => onReview(null)}
              data-testid="review-candidate"
            >
              Read the whole change first
            </button>
          )}
          <div className="merge-actions">
            <button
              type="button"
              className="merge-button"
              disabled={pending}
              onClick={() => act(mergeRun)}
              data-testid="merge-run"
            >
              Merge
            </button>
            <button
              type="button"
              className="run-action"
              disabled={pending}
              onClick={() => act(abandonRun)}
              title="Throws the candidate away. The task branches stay, so the work can still be read."
              data-testid="abandon-run"
            >
              Abandon
            </button>
          </div>
        </section>
      )}
    </section>
  );
}

export function RunStatusBadge({ status }: { status: RunStatus }) {
  return (
    <span className="run-status" data-status={status} data-testid={`run-status-${status}`}>
      {status.replace('_', ' ')}
    </span>
  );
}

/**
 * One task.
 *
 * Everything on the card is what the task committed to before it ran — the files it declared,
 * the command that judges it, the assertions that have to pass — plus what happened. Showing
 * the commitments next to the outcome is what makes an acceptance result readable as a
 * verdict rather than as a colour.
 */
function TaskCard({
  task,
  onOpenTranscript,
  onReview,
}: {
  task: TaskState;
  onOpenTranscript?: (taskId: string) => void;
  /** Opens the full-width diff for one task, or for the candidate when no task is named. */
  onReview?: (taskId: string | null) => void;
}) {
  const { summary } = task;

  return (
    <article
      className="task-card"
      data-status={task.status}
      data-testid={`task-${summary.id}`}
    >
      <header className="task-head">
        <span className="task-title">{summary.title}</span>
        <TaskStatusPill status={task.status} />
      </header>

      {task.detail && <p className="task-detail">{task.detail}</p>}

      <dl className="task-facts">
        {task.routing && (
          <>
            {/* Kept on the card for the life of the run, not just while the task is dispatched.
                The choice used to live in the status change's detail, which the next status
                change replaced — so by the time anybody read the finished run, the answer to
                "why did this go to the expensive agent" had been erased by "completed". */}
            <dt>Ran on</dt>
            <dd>
              <code>{task.routing.agent}</code>
              <span className="task-routed-by">
                {task.routing.by_router ? 'chosen by the router' : 'the only candidate'}
              </span>
            </dd>
          </>
        )}

        <dt>Files</dt>
        <dd>
          {summary.declared_paths.length === 0 ? (
            <span className="task-none">none declared</span>
          ) : (
            <ul className="path-list">
              {summary.declared_paths.map((path) => (
                <li key={path}>
                  <code>{path}</code>
                </li>
              ))}
            </ul>
          )}
        </dd>

        <dt>Acceptance</dt>
        <dd>
          {summary.verify_cmd ? (
            <code className="task-cmd">{summary.verify_cmd}</code>
          ) : (
            <span className="task-none">no command</span>
          )}
        </dd>

        <dt>Must pass</dt>
        <dd>
          {summary.must_pass.length === 0 ? (
            <span className="task-none">nothing named</span>
          ) : (
            <ul className="assertion-list" data-testid={`task-${summary.id}-must-pass`}>
              {summary.must_pass.map((assertion) => (
                <li key={assertion}>
                  <code>{assertion}</code>
                </li>
              ))}
            </ul>
          )}
        </dd>

        {summary.depends_on.length > 0 && (
          <>
            <dt>Waits for</dt>
            <dd>
              <ul className="id-list">
                {summary.depends_on.map((dep) => (
                  <li key={dep}>
                    <code>{dep}</code>
                  </li>
                ))}
              </ul>
            </dd>
          </>
        )}
      </dl>

      {task.workspace && (
        <p className="task-workspace" data-testid={`task-${summary.id}-workspace`}>
          <code>{task.workspace.branch}</code> starts at{' '}
          <code title={task.workspace.start_commit}>
            {shortCommit(task.workspace.start_commit)}
          </code>
          {/*
            The dependency edge stated as what it actually is. A task with dependencies starts
            from a real merge of their results, so the edge carries the work rather than a
            description of it, and this line is the only place that distinction is visible.
          */}
          {task.workspace.from_dependencies.length > 0 ? (
            <>
              , which folds in{' '}
              <span className="task-from-deps">
                {task.workspace.from_dependencies.map((dep, i) => (
                  <span key={dep}>
                    {i > 0 && ', '}
                    <code title={dep}>{shortCommit(dep)}</code>
                  </span>
                ))}
              </span>
            </>
          ) : (
            ', the run base'
          )}
        </p>
      )}

      {task.verification && (
        <Verdict taskId={summary.id} verification={task.verification} />
      )}

      <div className="task-links">
        {onReview && task.workspace && (
          /* Only once the task has a workspace, because before that there are no two commits to
             compare. */
          <button
            type="button"
            className="task-transcript"
            onClick={() => onReview(summary.id)}
            data-testid={`task-${summary.id}-diff`}
          >
            See what it changed
          </button>
        )}
        {onOpenTranscript && (
          /* Only when a session for this task still exists. A link that leads nowhere is worse than
             no link: it says the record is there and then proves it is not. */
          <button
            type="button"
            className="task-transcript"
            onClick={() => onOpenTranscript(summary.id)}
            data-testid={`task-${summary.id}-transcript`}
          >
            Open the worker&rsquo;s transcript
          </button>
        )}
      </div>
    </article>
  );
}

function TaskStatusPill({ status }: { status: TaskStatus }) {
  const running = status === 'dispatched' || status === 'verifying';
  return (
    <span className="task-status" data-status={status}>
      {running && <span className="spinner" aria-hidden="true" />}
      {status}
    </span>
  );
}

/**
 * The acceptance verdict.
 *
 * A failure names the assertions. "Failed" on its own tells the reader to go and run the
 * command themselves, which is the work the acceptance step already did.
 *
 * The two lists are kept apart because they mean different things. An assertion that never
 * started passing is the task not being finished; an assertion that was passing and stopped is
 * the task breaking something that already worked, which is worse, and is why it goes first.
 */
function Verdict({
  taskId,
  verification,
}: {
  taskId: string;
  verification: TaskVerificationView;
}) {
  if (verification.passed) {
    return (
      <p className="task-verdict" data-passed="true" data-testid={`task-${taskId}-verdict`}>
        <span className="verdict-check" aria-hidden="true">
          {'\u2713'}
        </span>
        acceptance passed
      </p>
    );
  }

  return (
    <div className="task-verdict" data-passed="false" data-testid={`task-${taskId}-verdict`}>
      <p className="verdict-head">acceptance failed</p>

      {verification.regressed.length > 0 && (
        <div className="verdict-group" data-severity="regressed">
          <h4>Was passing, now fails</h4>
          <ul className="assertion-list" data-testid={`task-${taskId}-regressed`}>
            {verification.regressed.map((assertion) => (
              <li key={assertion}>
                <code>{assertion}</code>
              </li>
            ))}
          </ul>
        </div>
      )}

      {verification.missing_pass.length > 0 && (
        <div className="verdict-group" data-severity="missing-pass">
          <h4>Was supposed to start passing, did not</h4>
          <ul className="assertion-list" data-testid={`task-${taskId}-missing-pass`}>
            {verification.missing_pass.map((assertion) => (
              <li key={assertion}>
                <code>{assertion}</code>
              </li>
            ))}
          </ul>
        </div>
      )}

      {verification.detail && <p className="verdict-detail">{verification.detail}</p>}
    </div>
  );
}

function blockerText(blocker: RunBlocker): string {
  switch (blocker.kind) {
    case 'awaiting_merge':
      return blocker.excluded.length > 0
        ? `Waiting for you: a candidate is ready, with ${blocker.excluded.length} task${
            blocker.excluded.length === 1 ? '' : 's'
          } left out of it.`
        : 'Waiting for you: a candidate is ready to merge.';
    case 'merge_rejected':
      return `Stuck combining results: ${blocker.task_id} was dropped.`;
    case 'failed':
      return `Stuck on ${blocker.task_ids.join(', ')}: acceptance failed.`;
    case 'blocked':
      return `Stuck on ${blocker.task_ids.join(', ')}: blocked by something upstream.`;
    case 'plan_rejected':
      return `Stuck planning: the last graph had ${blocker.problems.length} problem${
        blocker.problems.length === 1 ? '' : 's'
      }.`;
    default:
      return '';
  }
}

/**
 * Git's own seven-character abbreviation, and only for something shaped like a hash.
 *
 * The same field can carry a task id in a graph that names its dependencies rather than their
 * commits, and truncating one of those to seven characters would produce a plausible-looking
 * identifier that matches nothing.
 */
function shortCommit(value: string): string {
  return /^[0-9a-f]{12,}$/i.test(value) ? value.slice(0, 7) : value;
}
