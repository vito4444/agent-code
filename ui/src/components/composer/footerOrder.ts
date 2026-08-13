/**
 * The left-to-right order of the controls inside the composer footer.
 *
 * Kept as one list because the ordering is the least certain part of the interface
 * specification: it was derived from source and release notes rather than from screenshots,
 * so it is the thing most likely to need correcting once real screenshots exist. Keeping it
 * here means correcting it is a one-line change rather than a hunt through JSX.
 *
 * Every slot is independently omittable. On an agent that declares no model options and
 * never reports usage, three of these disappear, and the ones that remain must not move.
 */
export const FOOTER_SLOTS = [
  'agent',
  'model',
  'thought_level',
  'other_config',
  'context',
  'send',
] as const;

export type FooterSlot = (typeof FOOTER_SLOTS)[number];

/**
 * Which config categories are given their own dedicated slot.
 *
 * Everything else collapses into a single overflow control, so a vendor-private category
 * still reaches the user without being given a position it did not earn.
 */
export const DEDICATED_CATEGORIES: Record<string, FooterSlot> = {
  model: 'model',
  thought_level: 'thought_level',
};
