import { render } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { ToolCallCard } from './ToolCallCard';
import { diffStats } from './DiffView';
import type { ToolCallView } from '../../lib/types';

const OLD = 'pub fn load() -> Config {\n    parse(read_file(PATH))\n}';
const NEW =
  'static CACHE: OnceLock<Config> = OnceLock::new();\n\npub fn load() -> &\'static Config {\n    CACHE.get_or_init(|| parse(read_file(PATH)))\n}';

function call(): ToolCallView {
  return {
    tool_call_id: 't1',
    title: 'Edit src/config.rs',
    kind: 'edit',
    status: 'completed',
    content: [{ type: 'diff', path: 'src/config.rs', old_text: OLD, new_text: NEW }],
    locations: [],
  };
}

describe('the change counts on a tool card', () => {
  /**
   * The defect this replaced was visible on screen: a card header reading `+5 −3` sitting directly
   * above a diff reading `+4 −2` for the same change. The header approximated with a common-prefix
   * heuristic, so a line shared at the *end* of both versions — the closing brace here — was counted
   * as an addition and a deletion at once, while the real diff called it context.
   */
  it('agrees with the diff rendered underneath it', () => {
    const { container } = render(<ToolCallCard call={call()} />);
    // The two elements, compared to each other rather than each to a number. Asserting the card's
    // whole text contains "+4" is what the first version of this test did, and it could not fail:
    // the card contains the header stat *and* the diff body, so the correct number was always
    // present somewhere in it no matter what the header said.
    const header = container.querySelector('.diff-stat')?.textContent ?? '';
    const body = container.querySelector('.diff-counts')?.textContent ?? '';
    expect(header, 'the collapsed header must show a count').not.toBe('');
    expect(body, 'the diff must show a count').not.toBe('');
    expect(header).toBe(body);
  });

  it('shows the counts the shared diff computes', () => {
    const { added, removed } = diffStats(OLD, NEW);
    const { container } = render(<ToolCallCard call={call()} />);
    expect(container.querySelector('.diff-stat')?.textContent).toBe(
      `+${added}\u2212${removed}`,
    );
  });

  it('does not count a line shared at the end as both added and removed', () => {
    expect(diffStats(OLD, NEW)).toEqual({ added: 4, removed: 2 });
  });

  it('counts every line of a new file as added and nothing as removed', () => {
    expect(diffStats(null, 'a\nb\nc')).toEqual({ added: 3, removed: 0 });
  });

  it('reports nothing for an unchanged file', () => {
    expect(diffStats('a\nb', 'a\nb')).toEqual({ added: 0, removed: 0 });
  });
});
