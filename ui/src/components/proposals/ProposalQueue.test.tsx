import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ProposalQueue } from './ProposalQueue';
import * as api from '../../lib/api';
import type { ProposalReview, ProposalSummary } from '../../lib/types';

vi.mock('../../lib/api');

const mocked = vi.mocked(api);

function summary(over: Partial<ProposalSummary> = {}): ProposalSummary {
  return {
    id: 'p1',
    kind: 'playbook',
    scope: '/repo',
    risk: 'normal',
    created_ms: 1,
    requires_distinct_confirmation: false,
    ...over,
  };
}

function review(over: Partial<ProposalReview> = {}): ProposalReview {
  return {
    id: 'p1',
    kind: 'playbook',
    scope: '/repo',
    risk: 'normal',
    content_hash: 'abcdef0123456789',
    changes: ['Add to the playbook: Declare paths more broadly when a task strays.'],
    body_for_human: '{"payload":"playbook","deltas":[{"op":"add","body":"Declare paths"}]}',
    hidden_summary: '',
    hidden: [],
    requires_distinct_confirmation: false,
    confirmation_phrase: null,
    evidence: {
      supporting_runs: ['run-abcdef12'],
      verified_signals: ['1 task left its declared paths'],
      note: '2 tasks passed, 1 failed',
    },
    ...over,
  };
}

beforeEach(() => {
  vi.resetAllMocks();
  mocked.approveProposal.mockResolvedValue(undefined);
  mocked.rejectProposal.mockResolvedValue(undefined);
});

describe('the approval queue', () => {
  /**
   * A list is skim-read, and the decision needs the body read carefully with the invisible
   * characters already stripped. Carrying bodies in the list invites approving from it.
   */
  it('shows no proposal bodies in the list', async () => {
    mocked.listProposals.mockResolvedValue([summary()]);
    render(<ProposalQueue />);
    await screen.findByTestId('proposal-p1');
    expect(screen.queryByTestId('proposal-p1-changes')).toBeNull();
    expect(mocked.reviewProposal).not.toHaveBeenCalled();
  });

  it('fetches the prepared body only when one is opened', async () => {
    const user = userEvent.setup();
    mocked.listProposals.mockResolvedValue([summary()]);
    mocked.reviewProposal.mockResolvedValue(review());
    render(<ProposalQueue />);

    await user.click(await screen.findByTestId('proposal-p1'));
    expect((await screen.findByTestId('proposal-p1-changes')).textContent).toContain(
      'Declare paths more broadly',
    );
  });

  /**
   * The stored body is a serialised payload, and asking somebody to dig one sentence out of a line
   * of JSON is how an approval gets given to something nobody read. The bytes stay reachable,
   * because the hash is over them and not over the rendering.
   */
  it('reads as sentences, with the exact bytes still available', async () => {
    const user = userEvent.setup();
    mocked.listProposals.mockResolvedValue([summary()]);
    mocked.reviewProposal.mockResolvedValue(review());
    render(<ProposalQueue />);

    await user.click(await screen.findByTestId('proposal-p1'));
    const changes = await screen.findByTestId('proposal-p1-changes');
    expect(changes.textContent ?? '').not.toContain('{"payload"');
    expect((screen.getByTestId('proposal-p1-body').textContent ?? '')).toContain('{"payload"');
  });

  /**
   * The whole mechanism. An approval that does not say what it approved cannot be checked against
   * what is on disk now, so the hash the reviewer was shown is what goes back.
   */
  it('approves against the hash it displayed', async () => {
    const user = userEvent.setup();
    mocked.listProposals.mockResolvedValue([summary()]);
    mocked.reviewProposal.mockResolvedValue(review());
    render(<ProposalQueue />);

    await user.click(await screen.findByTestId('proposal-p1'));
    await user.click(await screen.findByTestId('proposal-p1-approve'));
    expect(mocked.approveProposal).toHaveBeenCalledWith('p1', 'abcdef0123456789', undefined);
  });

  /**
   * Located rather than counted. "We removed 3 invisible characters" is not something anyone can act
   * on; deciding whether the removal changed the meaning needs to know where they were.
   */
  it('says where the invisible characters were, not just how many', async () => {
    const user = userEvent.setup();
    mocked.listProposals.mockResolvedValue([summary()]);
    mocked.reviewProposal.mockResolvedValue(
      review({
        hidden_summary: '1 removed',
        hidden: [{ codepoint: 'U+200B', line: 2, column: 14, kind: 'ZeroWidth' }],
      }),
    );
    render(<ProposalQueue />);

    await user.click(await screen.findByTestId('proposal-p1'));
    const box = await screen.findByTestId('proposal-p1-hidden');
    expect(box.textContent ?? '').toContain('U+200B');
    expect(box.textContent ?? '').toContain('line 2');
    expect(box.textContent ?? '').toContain('column 14');
  });

  /**
   * A different gesture, not a scarier button. This kind of proposal changes the machinery that asks
   * for approval, and approving it with the same click that approves a note about a test name makes
   * the two indistinguishable at the moment of deciding.
   */
  it('will not approve an elevated proposal on a plain click', async () => {
    const user = userEvent.setup();
    mocked.listProposals.mockResolvedValue([summary({ risk: 'elevated' })]);
    mocked.reviewProposal.mockResolvedValue(
      review({
        risk: 'elevated',
        requires_distinct_confirmation: true,
        confirmation_phrase: 'apply policy abcdef01',
      }),
    );
    render(<ProposalQueue />);

    await user.click(await screen.findByTestId('proposal-p1'));
    const approve = (await screen.findByTestId('proposal-p1-approve')) as HTMLButtonElement;
    expect(approve.disabled).toBe(true);
    await user.click(approve);
    expect(mocked.approveProposal).not.toHaveBeenCalled();
  });

  it('accepts an elevated one once the phrase is typed, and sends the phrase', async () => {
    const user = userEvent.setup();
    mocked.listProposals.mockResolvedValue([summary({ risk: 'elevated' })]);
    mocked.reviewProposal.mockResolvedValue(
      review({
        risk: 'elevated',
        requires_distinct_confirmation: true,
        confirmation_phrase: 'apply policy abcdef01',
      }),
    );
    render(<ProposalQueue />);

    await user.click(await screen.findByTestId('proposal-p1'));
    await user.type(
      await screen.findByTestId('proposal-p1-phrase'),
      'apply policy abcdef01',
    );
    await user.click(screen.getByTestId('proposal-p1-approve'));
    expect(mocked.approveProposal).toHaveBeenCalledWith(
      'p1',
      'abcdef0123456789',
      'apply policy abcdef01',
    );
  });

  it('refuses a phrase that is nearly right', async () => {
    const user = userEvent.setup();
    mocked.listProposals.mockResolvedValue([summary({ risk: 'elevated' })]);
    mocked.reviewProposal.mockResolvedValue(
      review({
        risk: 'elevated',
        requires_distinct_confirmation: true,
        confirmation_phrase: 'apply policy abcdef01',
      }),
    );
    render(<ProposalQueue />);

    await user.click(await screen.findByTestId('proposal-p1'));
    await user.type(await screen.findByTestId('proposal-p1-phrase'), 'apply policy abcdef02');
    expect((screen.getByTestId('proposal-p1-approve') as HTMLButtonElement).disabled).toBe(true);
  });

  it('shows the evidence rather than an assertion from nowhere', async () => {
    const user = userEvent.setup();
    mocked.listProposals.mockResolvedValue([summary()]);
    mocked.reviewProposal.mockResolvedValue(review());
    render(<ProposalQueue />);

    await user.click(await screen.findByTestId('proposal-p1'));
    const evidence = await screen.findByTestId('proposal-p1-evidence');
    expect(evidence.textContent ?? '').toContain('2 tasks passed, 1 failed');
    expect(evidence.textContent ?? '').toContain('run-abcd');
  });

  /** A clean run proposes nothing, and the screen has to say that rather than looking broken. */
  it('says why it is empty', async () => {
    mocked.listProposals.mockResolvedValue([]);
    render(<ProposalQueue />);
    expect((await screen.findByTestId('proposals-empty')).textContent ?? '').toMatch(
      /a gate nobody reads is not a gate/i,
    );
  });

  it('reloads after a decision so a retired proposal leaves the list', async () => {
    const user = userEvent.setup();
    mocked.listProposals.mockResolvedValueOnce([summary()]).mockResolvedValueOnce([]);
    mocked.reviewProposal.mockResolvedValue(review());
    render(<ProposalQueue />);

    await user.click(await screen.findByTestId('proposal-p1'));
    await user.click(await screen.findByTestId('proposal-p1-reject'));
    await waitFor(() => expect(screen.queryByTestId('proposal-p1')).toBeNull());
  });
});
