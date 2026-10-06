import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// Node 25 ships an experimental global localStorage that shadows jsdom's; use a plain in-memory one.
class MemoryStorage implements Storage {
  private m = new Map<string, string>();
  get length() { return this.m.size; }
  clear() { this.m.clear(); }
  getItem(k: string) { return this.m.get(k) ?? null; }
  key(i: number) { return [...this.m.keys()][i] ?? null; }
  removeItem(k: string) { this.m.delete(k); }
  setItem(k: string, v: string) { this.m.set(k, String(v)); }
}
Object.defineProperty(globalThis, "localStorage", { configurable: true, value: new MemoryStorage() });

afterEach(() => {
  cleanup();
  localStorage.clear();
});

// jsdom has no layout: give scroll containers a size so the virtualizer renders rows.
Object.defineProperty(HTMLElement.prototype, "offsetHeight", { configurable: true, value: 600 });
Object.defineProperty(HTMLElement.prototype, "offsetWidth", { configurable: true, value: 900 });
HTMLElement.prototype.getBoundingClientRect = function () {
  return { width: 900, height: 600, top: 0, left: 0, right: 900, bottom: 600, x: 0, y: 0, toJSON: () => ({}) };
};
class RO {
  constructor(private cb: ResizeObserverCallback) {}
  observe(target: Element) {
    this.cb([{ target, contentRect: { width: 900, height: 600 } } as ResizeObserverEntry], this as unknown as ResizeObserver);
  }
  unobserve() {}
  disconnect() {}
}
globalThis.ResizeObserver = RO as unknown as typeof ResizeObserver;
Element.prototype.scrollTo = function () {};
