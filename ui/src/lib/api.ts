/**
 * HTTP calls to the daemon.
 *
 * The base URL is relative, because the daemon serves the interface itself. That is what
 * makes a browser a first-class client rather than a fallback: the same build runs inside
 * the desktop shell and in Chrome, and on Linux — where the embedded webview is the least
 * predictable part of the stack — a browser is a working escape route rather than a rewrite.
 */

import type { Rule } from '../components/rules/RulesScreen';
import type { SessionSummary } from './types';

const base = '/api';

async function json<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${base}${path}`, {
    ...init,
    headers: { 'content-type': 'application/json', ...(init?.headers ?? {}) },
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`${res.status} ${res.statusText}: ${body}`);
  }
  return (await res.json()) as T;
}

export function listSessions(): Promise<SessionSummary[]> {
  return json('/sessions');
}

export function createSession(agentId: string, projectRoot: string): Promise<SessionSummary> {
  return json('/sessions', {
    method: 'POST',
    body: JSON.stringify({ agent_id: agentId, project_root: projectRoot }),
  });
}

export function sendPrompt(sessionId: string, text: string): Promise<void> {
  return json(`/sessions/${encodeURIComponent(sessionId)}/prompt`, {
    method: 'POST',
    body: JSON.stringify({ text }),
  });
}

export function cancelTurn(sessionId: string): Promise<void> {
  return json(`/sessions/${encodeURIComponent(sessionId)}/cancel`, { method: 'POST' });
}

export function answerPermission(
  sessionId: string,
  requestId: string,
  optionId: string | null,
): Promise<void> {
  return json(`/sessions/${encodeURIComponent(sessionId)}/permission`, {
    method: 'POST',
    body: JSON.stringify({ request_id: requestId, option_id: optionId }),
  });
}

export function setConfigOption(
  sessionId: string,
  optionId: string,
  value: string | boolean,
): Promise<{ applied: boolean; requires_new_session: boolean }> {
  return json(`/sessions/${encodeURIComponent(sessionId)}/config`, {
    method: 'POST',
    body: JSON.stringify({ option_id: optionId, value }),
  });
}

export function fetchTerminalOutput(
  terminalId: string,
): Promise<{ output: string; truncated: boolean }> {
  return json(`/terminals/${encodeURIComponent(terminalId)}`);
}

export function listRules(projectRoot: string | null): Promise<Rule[]> {
  const q = projectRoot ? `?project_root=${encodeURIComponent(projectRoot)}` : '';
  return json(`/rules${q}`);
}

export function saveRule(rule: {
  scope: 'global' | 'project';
  project_root: string | null;
  body: string;
}): Promise<Rule> {
  return json('/rules', { method: 'POST', body: JSON.stringify(rule) });
}

export function deleteRule(id: string): Promise<void> {
  return json(`/rules/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

/** Raw ACP frames, for the message inspector. */
export function fetchRawFrames(
  limit = 500,
): Promise<{ at_ms: number; direction: string; agent_id: string; line: string; malformed: boolean }[]> {
  return json(`/raw?limit=${limit}`);
}
