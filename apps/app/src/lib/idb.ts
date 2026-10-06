// A minimal promise wrapper around IndexedDB for the PWA's own state
// (identity, settings, devices, history). Nothing here leaves the browser.

export interface Store<T> {
  get(key: IDBValidKey): Promise<T | undefined>;
  put(value: T, key?: IDBValidKey): Promise<IDBValidKey>;
  delete(key: IDBValidKey): Promise<void>;
  clear(): Promise<void>;
  all(): Promise<T[]>;
}

const DB_NAME = "ferry";
const DB_VERSION = 1;
/** Object stores: `kv` and `blobs` (out-of-line keys), the rest keyed by `id`. */
export const STORES = ["kv", "blobs", "devices", "history"] as const;
export type StoreName = (typeof STORES)[number];

let opening: Promise<IDBDatabase> | null = null;

function open(): Promise<IDBDatabase> {
  opening ??= new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains("kv")) db.createObjectStore("kv");
      if (!db.objectStoreNames.contains("blobs")) db.createObjectStore("blobs");
      if (!db.objectStoreNames.contains("devices")) db.createObjectStore("devices", { keyPath: "id" });
      if (!db.objectStoreNames.contains("history")) db.createObjectStore("history", { keyPath: "id" });
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
    req.onblocked = () => reject(new Error("Storage is in use by another Ferry tab"));
  });
  return opening;
}

function run<R>(name: StoreName, mode: IDBTransactionMode, fn: (s: IDBObjectStore) => IDBRequest<R>): Promise<R> {
  return open().then(
    (db) =>
      new Promise<R>((resolve, reject) => {
        const tx = db.transaction(name, mode);
        const req = fn(tx.objectStore(name));
        tx.oncomplete = () => resolve(req.result);
        tx.onerror = () => reject(tx.error ?? req.error);
        tx.onabort = () => reject(tx.error ?? new Error("transaction aborted"));
      }),
  );
}

export function store<T>(name: StoreName): Store<T> {
  return {
    get: (key) => run<T | undefined>(name, "readonly", (s) => s.get(key) as IDBRequest<T | undefined>),
    put: (value, key) => run(name, "readwrite", (s) => s.put(value, key)),
    delete: (key) => run(name, "readwrite", (s) => s.delete(key)).then(() => undefined),
    clear: () => run(name, "readwrite", (s) => s.clear()).then(() => undefined),
    all: () => run<T[]>(name, "readonly", (s) => s.getAll() as IDBRequest<T[]>),
  };
}
