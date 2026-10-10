import { useSyncExternalStore } from "react";

// API controllers need the latest status synchronously between awaits (in
// particular busy/auth guards). React subscribes to immutable UI snapshots;
// controllers never render the root or write control values into the DOM.
export function createState(initial) {
  let snapshot = initial;
  const listeners = new Set();
  return {
    getSnapshot: () => snapshot,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    set(patch) {
      snapshot = { ...snapshot, ...patch };
      for (const listener of listeners) listener();
    },
  };
}

export function useStore(store) {
  return useSyncExternalStore(store.subscribe, store.getSnapshot);
}
