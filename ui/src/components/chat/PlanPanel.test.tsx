import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import { PlanPanel } from './PlanPanel';
import type { PlanEntry } from '../../lib/types';

function entry(over: Partial<PlanEntry> = {}): PlanEntry {
  return { content: 'Read the loader', priority: 'high', status: 'pending', ...over };
}

describe("the agent's plan", () => {
  /**
   * This data had been arriving since the first day and was rendered nowhere: stored, and dropped. An
   * agent that reports a plan is telling you the shape of the work before it does it, which is the one
   * thing that makes a long turn readable while it is still running.
   */
  it('is shown at all', () => {
    render(<PlanPanel busy plans={{ main: [entry()] }} />);
    expect(screen.getByTestId('plan-main')).toBeTruthy();
    expect(screen.getByTestId('plan-main-entries').textContent ?? '').toContain('Read the loader');
  });

  it('shows nothing when there is no plan', () => {
    expect(render(<PlanPanel busy plans={{}} />).container.textContent).toBe('');
    // An empty plan is the same as no plan: the agent replaced its entries with none.
    expect(render(<PlanPanel busy plans={{ main: [] }} />).container.textContent).toBe('');
  });

  /** The number somebody glances at between messages, and it has to read with the entries closed. */
  it('counts what is done against the whole', () => {
    render(
      <PlanPanel
        busy
        plans={{
          main: [
            entry({ status: 'completed' }),
            entry({ status: 'completed' }),
            entry({ status: 'in_progress' }),
            entry({ status: 'pending' }),
          ],
        }}
      />,
    );
    expect(screen.getByTestId('plan-main-count').textContent).toBe('2/4');
  });

  it('is expanded while a step is in progress', () => {
    render(<PlanPanel busy plans={{ main: [entry({ status: 'in_progress' })] }} />);
    expect(screen.getByTestId('plan-main-entries')).toBeTruthy();
  });

  /**
   * The rule is "anything left to do", not "something running". A plan that has just arrived has every
   * entry pending, and that is the moment it is most worth reading — the agent is saying what it is
   * about to do.
   */
  it('is expanded when it has only just arrived and nothing has started', () => {
    render(<PlanPanel busy plans={{ main: [entry({ status: 'pending' })] }} />);
    expect(screen.getByTestId('plan-main-entries')).toBeTruthy();
  });

  it('is collapsed once everything is finished', () => {
    render(<PlanPanel busy plans={{ main: [entry({ status: 'completed' })] }} />);
    expect(screen.queryByTestId('plan-main-entries')).toBeNull();
  });

  /**
   * Once somebody has clicked, the interface stops having opinions — the same rule as the reasoning
   * band, and for the same reason: a block that reopens itself feels like it is fighting back.
   */
  it('stays closed after somebody closes it', async () => {
    const user = userEvent.setup();
    render(<PlanPanel busy plans={{ main: [entry({ status: 'in_progress' })] }} />);
    await user.click(screen.getByTestId('plan-main-toggle'));
    expect(screen.queryByTestId('plan-main-entries')).toBeNull();
  });

  /**
   * The protocol lets an agent keep several plans at once and requires the client to keep them apart.
   * One slot would let the second silently replace the first, which reads as a plan that keeps
   * rewriting itself.
   */
  it('keeps two concurrent plans apart, and names them', () => {
    render(
      <PlanPanel busy plans={{ strategy: [entry()], checklist: [entry(), entry()] }} />,
    );
    expect(screen.getByTestId('plan-strategy-count').textContent).toBe('0/1');
    expect(screen.getByTestId('plan-checklist-count').textContent).toBe('0/2');
    expect(screen.getByText('strategy')).toBeTruthy();
  });

  /** With one plan the id is noise — it is `main` for anything mapped from the older shape. */
  it('does not name a single plan', () => {
    render(<PlanPanel busy plans={{ main: [entry()] }} />);
    expect(screen.queryByText('main')).toBeNull();
    expect(screen.getByText('Plan')).toBeTruthy();
  });

  /**
   * The protocol reserves plain names for future versions and requires custom ones to begin with an
   * underscore, so an unfamiliar value means a newer agent or a deliberate extension. Drawing it as
   * "pending" would state something the agent did not say.
   */
  it('shows a status it has never heard of as itself', () => {
    render(<PlanPanel busy plans={{ main: [entry({ status: '_blocked' })] }} />);
    expect(screen.getByTestId('plan-unknown-_blocked').textContent).toBe('_blocked');
  });

  it('does not label the four it does know', () => {
    render(
      <PlanPanel
        busy
        plans={{
          main: [
            entry({ status: 'pending' }),
            entry({ status: 'in_progress' }),
            entry({ status: 'completed' }),
            entry({ status: 'cancelled' }),
          ],
        }}
      />,
    );
    expect(screen.queryByTestId('plan-unknown-pending')).toBeNull();
    expect(screen.queryByTestId('plan-unknown-cancelled')).toBeNull();
  });
});

/**
 * "working" is a claim about right now.
 *
 * An entry left `in_progress` when a turn ends is not work in progress — the agent said it was
 * doing that step and then stopped saying anything, which is what a refusal, a token limit or a
 * cancellation looks like from here. Read from the entry alone, the panel went on claiming to be
 * working for the rest of the session, next to a turn that had visibly finished.
 */
describe('whether anything is actually running', () => {
  const midway = {
    main: [entry({ status: 'completed' }), entry({ status: 'in_progress' })],
  };

  it('says so while the turn is running', () => {
    render(<PlanPanel busy plans={midway} />);
    expect(screen.getByTestId('plan-main-toggle').textContent ?? '').toContain('working');
    expect(screen.queryByTestId('plan-main-stopped')).toBeNull();
  });

  it('says where the agent stopped once the turn is over', () => {
    render(<PlanPanel busy={false} plans={midway} />);
    const head = screen.getByTestId('plan-main-toggle').textContent ?? '';
    expect(head).not.toContain('working');
    expect(head).toContain('stopped here');
  });

  it('says neither when every step finished', () => {
    render(
      <PlanPanel
        busy={false}
        plans={{ main: [entry({ status: 'completed' }), entry({ status: 'completed' })] }}
      />,
    );
    const head = screen.getByTestId('plan-main-toggle').textContent ?? '';
    expect(head).not.toContain('working');
    expect(head).not.toContain('stopped here');
  });
});
