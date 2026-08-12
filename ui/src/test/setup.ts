import '@testing-library/dom';

// jsdom has no rAF timing guarantees worth relying on, and no crypto.randomUUID in older
// versions. Both are needed by code under test, so they are provided explicitly rather than
// left to whatever the environment happens to have.
if (typeof globalThis.requestAnimationFrame !== 'function') {
  globalThis.requestAnimationFrame = ((cb: FrameRequestCallback) =>
    setTimeout(() => cb(performance.now()), 16) as unknown as number) as typeof requestAnimationFrame;
  globalThis.cancelAnimationFrame = ((h: number) => clearTimeout(h)) as typeof cancelAnimationFrame;
}
if (typeof globalThis.crypto?.randomUUID !== 'function') {
  Object.defineProperty(globalThis, 'crypto', {
    value: { ...globalThis.crypto, randomUUID: () => `test-${Math.random().toString(16).slice(2)}` },
  });
}
