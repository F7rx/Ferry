// A tiny typed event emitter. Listener exceptions are contained so a faulty UI
// handler can never corrupt protocol state.

export type Listener<T> = (payload: T) => void;

export class Emitter<Events extends object> {
  /** Type-only (never emitted): lets generic helpers infer `Events` from a subclass. */
  declare private readonly eventMap?: Events;
  private readonly listeners = new Map<keyof Events, Set<Listener<never>>>();

  /** Subscribes to `type`; returns an unsubscribe function. */
  on<K extends keyof Events>(type: K, fn: Listener<Events[K]>): () => void {
    let set = this.listeners.get(type);
    if (!set) {
      set = new Set();
      this.listeners.set(type, set);
    }
    set.add(fn as Listener<never>);
    return () => this.off(type, fn);
  }

  once<K extends keyof Events>(type: K, fn: Listener<Events[K]>): () => void {
    const off = this.on(type, (payload) => {
      off();
      fn(payload);
    });
    return off;
  }

  off<K extends keyof Events>(type: K, fn: Listener<Events[K]>): void {
    this.listeners.get(type)?.delete(fn as Listener<never>);
  }

  listenerCount(type: keyof Events): number {
    return this.listeners.get(type)?.size ?? 0;
  }

  protected emit<K extends keyof Events>(type: K, payload: Events[K]): void {
    const set = this.listeners.get(type);
    if (!set) return;
    for (const fn of [...set]) {
      try {
        (fn as Listener<Events[K]>)(payload);
      } catch (err) {
        console.error(`[rtc] "${String(type)}" listener failed`, err);
      }
    }
  }
}
