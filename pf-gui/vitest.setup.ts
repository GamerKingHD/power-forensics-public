// Vitest setup: jsdom lacks a few browser APIs used by react-virtuoso and
// uPlot. Provide inert stubs so component tests can mount.
import { configure } from '@testing-library/dom'

// Components debounce bridge calls (e.g. Compare waits 150 ms before running a
// deep comparison). The default 1 s async-utility timeout can be exceeded on a
// loaded CI runner, which made timing-sensitive assertions flaky; allow a
// generous margin without weakening any assertion.
configure({ asyncUtilTimeout: 5000 })

class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}

if (!('ResizeObserver' in globalThis)) {
  ;(globalThis as unknown as { ResizeObserver: unknown }).ResizeObserver = ResizeObserverStub
}

if (!('matchMedia' in window)) {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    }),
  })
}

if (!HTMLElement.prototype.scrollTo) {
  HTMLElement.prototype.scrollTo = () => {}
}
