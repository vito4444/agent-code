import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { Composer, type ComposerProps } from './Composer';
import { ContextRing } from './ContextRing';
import type { ConfigOption } from '../../lib/types';

function props(overrides: Partial<ComposerProps> = {}): ComposerProps {
  return {
    agentName: 'Fake Agent',
    configOptions: [],
    contextPercent: null,
    usage: null,
    busy: false,
    queue: [],
    autonomy: 'ask_outside_sandbox',
    onSend: vi.fn(),
    onQueue: vi.fn(),
    onStopAndSend: vi.fn(),
    onCancel: vi.fn(),
    onConfigChange: vi.fn(),
    onAutonomyChange: vi.fn(),
    onDequeue: vi.fn(),
    ...overrides,
  };
}

const modelOption: ConfigOption = {
  id: 'model',
  name: 'Model',
  description: null,
  category: 'model',
  value: {
    type: 'select',
    current: 'fast',
    options: [
      { value: 'fast', name: 'Fast', description: null },
      { value: 'deep', name: 'Deep', description: null },
    ],
  },
  live_switchable: true,
};

describe('composer footer', () => {
  it('draws no model selector when the agent declared none', () => {
    render(<Composer {...props()} />);
    expect(screen.queryByTestId('config-model')).toBeNull();
    // The agent name slot stays, so the row does not collapse to nothing.
    expect(screen.getByTestId('footer-agent').textContent).toBe('Fake Agent');
  });

  it('draws the model selector from what the agent declared, with no hardcoded names', () => {
    render(<Composer {...props({ configOptions: [modelOption] })} />);
    const select = screen.getByTestId('config-model') as HTMLSelectElement;
    expect([...select.options].map((o) => o.value)).toEqual(['fast', 'deep']);
    expect(select.value).toBe('fast');
  });

  it('warns that a launch-only setting restarts the session', () => {
    render(
      <Composer
        {...props({ configOptions: [{ ...modelOption, live_switchable: false }] })}
      />,
    );
    expect(screen.getByText('restarts')).toBeTruthy();
  });

  it('does not warn about restarting when the agent accepts runtime changes', () => {
    render(<Composer {...props({ configOptions: [modelOption] })} />);
    expect(screen.queryByText('restarts')).toBeNull();
  });

  it('puts options with unknown or absent categories into an overflow rather than dropping them', () => {
    const vendor: ConfigOption = {
      id: '_vendor_mode',
      name: 'Vendor mode',
      description: null,
      category: '_vendor_private',
      value: { type: 'select', current: 'a', options: [{ value: 'a', name: 'A', description: null }] },
      live_switchable: false,
    };
    const noCategory: ConfigOption = {
      id: 'verbose',
      name: 'Verbose',
      description: null,
      category: null,
      value: { type: 'boolean', current: false },
      live_switchable: true,
    };
    render(<Composer {...props({ configOptions: [modelOption, vendor, noCategory] })} />);
    expect(screen.getByText('2 more')).toBeTruthy();
  });

  it('keeps the autonomy control out of the footer row', () => {
    const { container } = render(<Composer {...props()} />);
    const footer = container.querySelector('.composer-footer');
    expect(footer?.querySelector('[data-testid="autonomy-select"]')).toBeNull();
    expect(screen.getByTestId('autonomy-select')).toBeTruthy();
  });
});

describe('context ring', () => {
  it('renders nothing when usage was never reported', () => {
    const { container } = render(<ContextRing percent={null} />);
    expect(container.firstChild).toBeNull();
  });

  it('renders a percentage once usage is reported', () => {
    render(<ContextRing percent={26.5} used={53000} size={200000} cost={null} />);
    expect(screen.getByTestId('context-ring').textContent).toContain('27%');
  });

  it('escalates its level as the window fills', () => {
    const { rerender } = render(<ContextRing percent={50} />);
    expect(screen.getByTestId('context-ring').dataset.level).toBe('normal');
    rerender(<ContextRing percent={80} />);
    expect(screen.getByTestId('context-ring').dataset.level).toBe('warning');
    rerender(<ContextRing percent={95} />);
    expect(screen.getByTestId('context-ring').dataset.level).toBe('critical');
  });

  it('is absent from the footer when the agent reports no usage', () => {
    render(<Composer {...props({ contextPercent: null })} />);
    expect(screen.queryByTestId('context-ring')).toBeNull();
  });
});

describe('sending while the agent is busy', () => {
  it('offers queueing and stopping, and does not offer steering', () => {
    render(<Composer {...props({ busy: true })} />);
    expect(screen.getByTestId('send-queue')).toBeTruthy();
    expect(screen.getByTestId('send-stop')).toBeTruthy();
    // No agent can honour mid-turn injection today, so offering it would be a control that
    // either queues under a different name or cancels while pretending not to.
    expect(screen.queryByTestId('send-steer')).toBeNull();
  });

  it('offers steering only when the agent declared support for it', () => {
    render(<Composer {...props({ busy: true, steeringSupported: true })} />);
    expect(screen.getByTestId('send-steer')).toBeTruthy();
  });

  it('queues rather than interrupts when the user presses enter mid-turn', async () => {
    const user = userEvent.setup();
    const onQueue = vi.fn();
    const onSend = vi.fn();
    render(<Composer {...props({ busy: true, onQueue, onSend })} />);

    await user.type(screen.getByTestId('composer-input'), 'also update the readme{Enter}');

    expect(onQueue).toHaveBeenCalledWith('also update the readme');
    expect(onSend).not.toHaveBeenCalled();
  });

  it('sends immediately when the agent is idle', async () => {
    const user = userEvent.setup();
    const onSend = vi.fn();
    render(<Composer {...props({ onSend })} />);

    await user.type(screen.getByTestId('composer-input'), 'do the thing{Enter}');
    expect(onSend).toHaveBeenCalledWith('do the thing');
  });

  it('says when a queued message will be delivered, and lets it be withdrawn', async () => {
    const user = userEvent.setup();
    const onDequeue = vi.fn();
    render(
      <Composer
        {...props({
          busy: true,
          queue: [{ id: 'q1', text: 'also fix the docs', mode: 'queue' }],
          onDequeue,
        })}
      />,
    );

    expect(screen.getByText('will be sent when this turn ends')).toBeTruthy();
    await user.click(screen.getByLabelText('Remove queued message: also fix the docs'));
    expect(onDequeue).toHaveBeenCalledWith('q1');
  });
});
