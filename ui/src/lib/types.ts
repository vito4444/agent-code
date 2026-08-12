/**
 * Mirror of the Rust event model in `wkbd-proto`.
 *
 * Kept hand-written rather than generated so the shapes the UI depends on are visible in
 * one file. A test asserts that a recorded fixture from the daemon still parses, which is
 * what catches drift; the alternative, trusting that two hand-maintained models agree, is
 * exactly the seam where mismatches hide.
 */

export type SegmentKindName = 'thought' | 'message' | 'user_echo';
export type SegmentStateName = 'live' | 'settled';

export interface SegmentId {
  raw: string;
  /** True when we invented the id because the agent did not send `messageId`. */
  synthesized: boolean;
}

export type ToolStatusName =
  | 'pending'
  | 'in_progress'
  | 'completed'
  | 'failed'
  | 'cancelled';

export type ToolKindName =
  | 'read'
  | 'edit'
  | 'delete'
  | 'move'
  | 'search'
  | 'execute'
  | 'think'
  | 'fetch'
  | 'switch_mode'
  | 'other'
  | { unknown: string };

export type ToolContent =
  | { type: 'text'; text: string }
  | { type: 'diff'; path: string; old_text: string | null; new_text: string }
  | { type: 'terminal'; terminal_id: string };

export interface ToolLocation {
  path: string;
  line: number | null;
}

export type PermissionOptionKindName =
  | 'allow_once'
  | 'allow_always'
  | 'reject_once'
  | 'reject_always'
  | 'unknown';

export interface PermissionOption {
  option_id: string;
  name: string;
  kind: PermissionOptionKindName;
}

export type ConfigValue =
  | { type: 'select'; current: string; options: ConfigChoice[] }
  | { type: 'boolean'; current: boolean };

export interface ConfigChoice {
  value: string;
  name: string;
  description: string | null;
}

export interface ConfigOption {
  id: string;
  name: string;
  description: string | null;
  /** `model`, `thought_level`, `mode`, a vendor `_`-prefixed name, or absent. */
  category: string | null;
  value: ConfigValue;
  /**
   * False when the setting can only be applied by launching a new process. The UI has to
   * say so rather than offering a control that silently changes nothing.
   */
  live_switchable: boolean;
}

export type StopReasonName =
  | 'end_turn'
  | 'max_tokens'
  | 'max_turn_requests'
  | 'refusal'
  | 'cancelled'
  | 'unknown';

export interface PlanEntry {
  content: string;
  priority: string;
  status: string;
}

export interface Cost {
  amount: number;
  currency: string;
}

/* --------------------------------------------------------------- orchestration */

export type TaskStatus =
  | 'pending'
  | 'ready'
  | 'dispatched'
  | 'verifying'
  | 'completed'
  | 'failed'
  | 'blocked';

export type RunStatus =
  | 'planning'
  | 'running'
  | 'awaiting_merge'
  | 'done'
  | 'failed'
  | 'cancelled';

export interface TaskSummary {
  id: string;
  title: string;
  depends_on: string[];
  declared_paths: string[];
  /** The command the acceptance check runs. */
  verify_cmd: string;
  /**
   * The assertions that decide whether the task succeeded. A task with none of these would
   * have been rejected at validation, so an empty list here means the graph reached us by
   * some other route and the interface should not imply the task has a bar to clear.
   */
  must_pass: string[];
}

/** Discriminated by `event`, matching the Rust serde tag on `RunEvent`. */
export type RunEvent =
  | { event: 'started'; run_id: string; goal: string; project_root: string; base_commit: string }
  | { event: 'planned'; tasks: TaskSummary[]; waves: string[][]; attempt: number }
  | { event: 'plan_rejected'; problems: string[]; attempt: number }
  | { event: 'task_state_changed'; task_id: string; status: TaskStatus; detail: string | null }
  | {
      event: 'task_workspace_ready';
      task_id: string;
      branch: string;
      start_commit: string;
      from_dependencies: string[];
    }
  | {
      event: 'task_verified';
      task_id: string;
      passed: boolean;
      /** Assertions that were supposed to start passing and did not. */
      missing_pass: string[];
      /** Assertions that were passing and stopped. */
      regressed: string[];
      detail: string | null;
    }
  | { event: 'replanning'; trigger: string; task_id: string; attempt: number }
  | { event: 'awaiting_merge'; commit: string; order: string[]; excluded: string[] }
  | { event: 'merge_rejected'; task_id: string; detail: string; merged: string[] }
  | { event: 'finished'; status: RunStatus; detail: string | null };

/**
 * The run a stream belongs to, or null for a conversation stream.
 *
 * Runs share the event log with conversations, and `session_id` is really a stream
 * identifier — mirrors `run_stream_id` in the core. Reading the id back out here is what
 * keeps run state out of `sessions`, which otherwise gains an entry per run that no screen
 * can render.
 */
export function runIdFromStreamId(streamId: string): string | null {
  return streamId.startsWith('run:') ? streamId.slice('run:'.length) : null;
}

/** Discriminated by `event`, matching the Rust serde tag. */
export type EventPayload =
  | { event: 'turn_started'; turn: number; prompt: string }
  | { event: 'turn_ended'; turn: number; stop_reason: StopReasonName }
  | { event: 'segment_started'; segment: SegmentId; kind: SegmentKindName }
  | { event: 'segment_chunk'; segment: SegmentId; text: string }
  | { event: 'segment_settled'; segment: SegmentId }
  | {
      event: 'tool_call_started';
      tool_call_id: string;
      title: string;
      kind: ToolKindName;
      status: ToolStatusName;
    }
  | {
      event: 'tool_call_updated';
      tool_call_id: string;
      title: string | null;
      status: ToolStatusName | null;
      content: ToolContent[];
      locations: ToolLocation[];
    }
  | {
      event: 'permission_requested';
      request_id: string;
      tool_call_id: string | null;
      title: string;
      options: PermissionOption[];
    }
  | {
      event: 'permission_resolved';
      request_id: string;
      option_id: string | null;
      auto: boolean;
    }
  | { event: 'config_options_changed'; options: ConfigOption[] }
  | { event: 'usage_changed'; used: number; size: number; cost: Cost | null }
  | { event: 'plan_changed'; entries: PlanEntry[] }
  | { event: 'run'; run: RunEvent }
  | { event: 'unknown_update'; discriminant: string; raw: string }
  | { event: 'agent_error'; message: string }
  | { event: 'agent_exited'; code: number | null; signal: number | null };

export interface WkbdEvent {
  seq: number;
  session_id: string;
  at_ms: number;
  payload: EventPayload;
}

/* ---------------------------------------------------------------- view model */

export interface SegmentView {
  id: SegmentId;
  kind: SegmentKindName;
  text: string;
  state: SegmentStateName;
}

export interface ToolCallView {
  tool_call_id: string;
  title: string;
  kind: ToolKindName;
  status: ToolStatusName;
  content: ToolContent[];
  locations: ToolLocation[];
}

export type TurnItem =
  | { type: 'segment'; segment: SegmentView }
  | { type: 'tool_call'; call: ToolCallView }
  | {
      type: 'permission';
      request_id: string;
      title: string;
      options: PermissionOption[];
      resolved_with: string | null;
      auto: boolean;
    }
  | { type: 'error'; message: string };

export interface TurnView {
  turn: number;
  prompt: string;
  items: TurnItem[];
  stop_reason: StopReasonName | null;
  /** True when at least one segment boundary was inferred rather than declared. */
  segmentation_best_effort: boolean;
}

export interface SessionSummary {
  id: string;
  agent_id: string;
  agent_display_name: string;
  project_root: string;
  title: string | null;
}

/**
 * A run as the list endpoint reports it.
 *
 * Carries only what a list row needs. Everything else about a run — its graph, its task states,
 * its base commit — is folded from the run's events, because a summary that duplicated them
 * would be a second source of truth able to disagree with the log about what happened.
 */
export interface RunSummary {
  id: string;
  goal: string;
  project_root: string;
  status: RunStatus;
}

/** A message the user composed while the agent was busy. */
export interface QueuedMessage {
  id: string;
  text: string;
  /**
   * How it will be delivered. `queue` is the only mode the protocol can honour today:
   * there is no mid-turn injection, so a "steer" control would either be a queue wearing
   * a different label or a cancel that discards in-flight work.
   */
  mode: 'queue';
}

export function toolKindName(kind: ToolKindName): string {
  return typeof kind === 'string' ? kind : kind.unknown;
}
