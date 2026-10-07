// A minimal promise wrapper around IndexedDB for the PWA's own state
// (identity, settings, devices, history, received files). Nothing here leaves
// the browser.

export interface PageOptions<T> {
  /** At most this many values are returned. */
  limit: number;
  /** Start after this key (exclusive). */
  after?: IDBValidKey;
  /** Newest (highest key) first. */
  reverse?: boolean;
  /** Only values this accepts count toward `limit`. */
  filter?: (value: T) => boolean;
}

export interface Store<T> {
  get(key: IDBValidKey): Promise<T | undefined>;
  /** Stores `value`; with an auto-increment store and no key, the database allocates one and returns it. */
  put(value: T, key?: IDBValidKey): Promise<IDBValidKey>;
  delete(key: IDBValidKey): Promise<void>;
  clear(): Promise<void>;
  all(): Promise<T[]>;
  /** Up to `limit` keys after `after` (exclusive), ascending: bounded enumeration of a large store. */
  keys(limit: number, after?: IDBValidKey): Promise<IDBValidKey[]>;
  /** Values in key order, walked with a cursor (stable while other values are added or removed). */
  page(options: PageOptions<T>): Promise<T[]>;
  /** First value whose `index` equals `value`. */
  getBy(index: string, value: IDBValidKey): Promise<T | undefined>;
  /** Reads and rewrites one value in a single transaction; returning undefined deletes it. */
  update(key: IDBValidKey, fn: (current: T | undefined) => T | undefined): Promise<T | undefined>;
  /**
   * Visits every value in one transaction: `fn` returns a replacement, null to
   * delete it, or undefined to keep it. All or nothing.
   */
  rewrite(fn: (value: T) => T | null | undefined): Promise<{ updated: number; deleted: number }>;
}

const DB_NAME = "ferry";
/**
 * 1: kv, blobs, devices, history (keys chosen by the app).
 * 2: history keys are allocated by the database (autoIncrement) and indexed by
 *    `path`; existing entries keep their ids.
 */
const DB_VERSION = 2;
/** Object stores: `kv` and `blobs` (out-of-line keys), the rest keyed by `id`. */
export const STORES = ["kv", "blobs", "devices", "history"] as const;
export type StoreName = (typeof STORES)[number];
/** How long to wait for older Ferry tabs to release the database during an upgrade. */
const BLOCKED_MS = 4000;

let opening: Promise<IDBDatabase> | null = null;

function createHistory(db: IDBDatabase): IDBObjectStore {
  const history = db.createObjectStore("history", { keyPath: "id", autoIncrement: true });
  history.createIndex("path", "path", { unique: false });
  return history;
}

function upgrade(db: IDBDatabase, tx: IDBTransaction) {
  if (!db.objectStoreNames.contains("kv")) db.createObjectStore("kv");
  if (!db.objectStoreNames.contains("blobs")) db.createObjectStore("blobs");
  if (!db.objectStoreNames.contains("devices")) db.createObjectStore("devices", { keyPath: "id" });
  if (!db.objectStoreNames.contains("history")) {
    createHistory(db);
    return;
  }
  const old = tx.objectStore("history");
  if (old.autoIncrement) return;
  // Version 1 keyed history by timestamp: copy every entry into an
  // auto-increment store. Explicit numeric keys move the key generator past
  // them, so new ids stay above (newer than) every migrated one.
  const read = old.getAll();
  read.onsuccess = () => {
    db.deleteObjectStore("history");
    const next = createHistory(db);
    for (const entry of read.result) next.put(entry);
  };
}

function open(): Promise<IDBDatabase> {
  opening ??= new Promise<IDBDatabase>((resolve, reject) => {
    let blocked: ReturnType<typeof setTimeout> | null = null;
    const failed = (err: unknown) => {
      if (blocked) clearTimeout(blocked);
      opening = null;
      reject(err);
    };
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => upgrade(req.result, req.transaction!);
    req.onsuccess = () => {
      if (blocked) clearTimeout(blocked);
      const db = req.result;
      // Another tab upgrades the database: step aside so it isn't blocked.
      db.onversionchange = () => {
        db.close();
        opening = null;
      };
      resolve(db);
    };
    req.onerror = () => failed(req.error);
    req.onblocked = () => {
      blocked ??= setTimeout(
        () => failed(new Error("Ferry is open in another tab that needs reloading. Reload or close the other Ferry tabs, then reload this one.")),
        BLOCKED_MS,
      );
    };
  });
  return opening;
}

function run<R>(name: StoreName, mode: IDBTransactionMode, fn: (s: IDBObjectStore) => IDBRequest<R> | (() => R)): Promise<R> {
  return open().then(
    (db) =>
      new Promise<R>((resolve, reject) => {
        const tx = db.transaction(name, mode);
        const req = fn(tx.objectStore(name));
        tx.oncomplete = () => resolve(typeof req === "function" ? req() : req.result);
        tx.onerror = () => reject(tx.error ?? (typeof req === "function" ? null : req.error));
        tx.onabort = () => reject(tx.error ?? new Error("transaction aborted"));
      }),
  );
}

/** Walks a cursor; `step` returns false to stop. */
function walk(
  s: IDBObjectStore | IDBIndex,
  range: IDBKeyRange | null,
  direction: IDBCursorDirection,
  step: (cursor: IDBCursorWithValue) => boolean,
): void {
  const req = s.openCursor(range, direction);
  req.onsuccess = () => {
    const cursor = req.result;
    if (cursor && step(cursor)) cursor.continue();
  };
}

export function store<T>(name: StoreName): Store<T> {
  return {
    get: (key) => run<T | undefined>(name, "readonly", (s) => s.get(key) as IDBRequest<T | undefined>),
    put: (value, key) => run(name, "readwrite", (s) => s.put(value, key)),
    delete: (key) => run(name, "readwrite", (s) => s.delete(key)).then(() => undefined),
    clear: () => run(name, "readwrite", (s) => s.clear()).then(() => undefined),
    all: () => run<T[]>(name, "readonly", (s) => s.getAll() as IDBRequest<T[]>),
    keys: (limit, after) =>
      run<IDBValidKey[]>(name, "readonly", (s) => s.getAllKeys(after === undefined ? null : IDBKeyRange.lowerBound(after, true), limit)),
    page: ({ limit, after, reverse, filter }) =>
      run<T[]>(name, "readonly", (s) => {
        const out: T[] = [];
        if (limit <= 0) return () => out;
        const range = after === undefined ? null : reverse ? IDBKeyRange.upperBound(after, true) : IDBKeyRange.lowerBound(after, true);
        walk(s, range, reverse ? "prev" : "next", (cursor) => {
          const value = cursor.value as T;
          if (!filter || filter(value)) out.push(value);
          return out.length < limit;
        });
        return () => out;
      }),
    getBy: (index, value) => run<T | undefined>(name, "readonly", (s) => s.index(index).get(value) as IDBRequest<T | undefined>),
    update: (key, fn) =>
      run<T | undefined>(name, "readwrite", (s) => {
        let next: T | undefined;
        const req = s.get(key);
        req.onsuccess = () => {
          next = fn(req.result as T | undefined);
          if (next === undefined) s.delete(key);
          else if (s.keyPath === null) s.put(next, key);
          else s.put(next);
        };
        return () => next;
      }),
    rewrite: (fn) =>
      run(name, "readwrite", (s) => {
        const counts = { updated: 0, deleted: 0 };
        walk(s, null, "next", (cursor) => {
          const next = fn(cursor.value as T);
          if (next === null) {
            cursor.delete();
            counts.deleted++;
          } else if (next !== undefined) {
            cursor.update(next);
            counts.updated++;
          }
          return true;
        });
        return () => counts;
      }),
  };
}
