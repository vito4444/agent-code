import type { ConfigOption } from '../../lib/types';

/**
 * A dropdown for one session config option.
 *
 * Two rules, both of which exist because the alternative lies to the user.
 *
 * **Nothing is drawn when the agent declared nothing.** An empty menu says "there is a
 * choice here and it is broken", which is worse than no menu. The caller is responsible for
 * not rendering this component at all in that case; the guard here is a backstop.
 *
 * **When the setting cannot be changed in place, the control says so.** The protocol has no
 * method for switching a model mid-session in general: an agent either accepts
 * `session/set_config_option` or reads its configuration once at startup. For the second
 * kind, changing the value means starting a new session on a new process and handing it a
 * summary of the old one. There is no in-place switch to offer, so the label offers the
 * thing that actually happens.
 */
export function ConfigSelect({
  option,
  onChange,
  disabled,
}: {
  option: ConfigOption;
  onChange: (optionId: string, value: string | boolean) => void;
  disabled?: boolean;
}) {
  if (option.value.type === 'boolean') {
    return (
      <label className="config-toggle" title={option.description ?? option.name}>
        <input
          type="checkbox"
          checked={option.value.current}
          disabled={disabled}
          onChange={(e) => onChange(option.id, e.target.checked)}
        />
        <span>{option.name}</span>
      </label>
    );
  }

  const { current, options } = option.value;
  if (options.length === 0) return null;

  const restartNote = option.live_switchable
    ? ''
    : ' — changing this starts a new session on a new process and hands it a summary of this one';

  return (
    <label className="config-select" data-live={option.live_switchable}>
      <span className="config-select-label">{option.name}</span>
      <select
        value={current}
        disabled={disabled}
        title={`${option.description ?? option.name}${restartNote}`}
        onChange={(e) => onChange(option.id, e.target.value)}
        data-testid={`config-${option.id}`}
      >
        {options.map((choice) => (
          <option key={choice.value} value={choice.value}>
            {choice.name}
          </option>
        ))}
      </select>
      {!option.live_switchable && (
        <span
          className="config-restart-hint"
          title="This agent reads this setting only when it starts. Choosing a different value opens a new session."
        >
          restarts
        </span>
      )}
    </label>
  );
}
