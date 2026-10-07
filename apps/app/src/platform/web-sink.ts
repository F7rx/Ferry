// Where the PWA keeps received files: the origin-private file system (OPFS),
// streamed to disk chunk by chunk, so memory stays bounded whatever the size.
// Files live under `files/<storage id>`; the id is derived from (peer key,
// transfer id, file id, a nonce this receiver chose when accepting) so an
// interrupted transfer finds its partial file again, while a peer can never
// aim a new transfer at files already stored. Browsers without OPFS writers fall back to Blobs in
// IndexedDB, capped in size.
import type { FileMeta, FileSink, SinkAbortReason, SinkContext, SinkWriter } from "../lib/rtc";
import { RtcError } from "../lib/rtc";
import { store } from "../lib/idb";

/** Stored-file references use this scheme in history entries (`HistoryEntry.path`). */
export const STORED_PREFIX = "browser:";
/** Largest file the IndexedDB fallback accepts (it buffers the whole file). */
const FALLBACK_LIMIT = 256 * 1024 * 1024;
/** Keys read per step while pruning the IndexedDB fallback. */
const PRUNE_BATCH = 200;
const STORED_ID = /^[0-9a-f]{32}$/;

/**
 * A file in the IndexedDB fallback. Version 1 stored bare Blobs (no time):
 * those are treated as old.
 */
interface StoredBlob {
  blob: Blob;
  /** What arrived before an interruption, kept so the transfer can resume. */
  partial: boolean;
  /** When it was last written (ms since the epoch). */
  at: number;
}
type BlobValue = Blob | StoredBlob;

const blobs = store<BlobValue>("blobs");

function blobOf(value: BlobValue | undefined): Blob | undefined {
  if (!value) return undefined;
  return value instanceof Blob ? value : value.blob;
}

/** Called when a file was verified and written: records it so the Inbox can reach it. Throws when that fails. */
export type CommitHandler = (file: FileMeta, context: SinkContext, path: string) => Promise<void>;

async function filesDir(): Promise<FileSystemDirectoryHandle | null> {
  try {
    const root = await navigator.storage.getDirectory();
    return await root.getDirectoryHandle("files", { create: true });
  } catch {
    return null;
  }
}

let opfsWritable: Promise<boolean> | null = null;
/** OPFS with `createWritable` (Chromium, Firefox, Safari 18.2+). */
export function hasOpfs(): Promise<boolean> {
  opfsWritable ??= (async () => {
    const dir = await filesDir();
    if (!dir) return false;
    try {
      const probe = await dir.getFileHandle(".probe", { create: true });
      if (typeof probe.createWritable !== "function") return false;
      const w = await probe.createWritable();
      await w.close();
      await dir.removeEntry(".probe");
      return true;
    } catch {
      return false;
    }
  })();
  return opfsWritable;
}

/** Turns a storage failure into a message that says what to do. */
export function storageError(err: unknown): RtcError {
  if (err instanceof RtcError) return err;
  const name = (err as { name?: string } | null)?.name;
  if (name === "QuotaExceededError") {
    return new RtcError("storage-full", "This browser is out of space for received files. Save files from the Inbox and delete them here, then try again.");
  }
  return new RtcError("storage", `This browser couldn't store the file (${err instanceof Error ? err.message : String(err)}).`);
}

/** Stable storage id for one file of one transfer from one peer. */
export async function storageId(context: SinkContext, fileId: string, nonce: string): Promise<string> {
  const data = new TextEncoder().encode(`${context.peerKey}\n${context.transferId}\n${nonce}\n${fileId}`);
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", data));
  return [...hash.slice(0, 16)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

export function storedPath(id: string): string {
  return `${STORED_PREFIX}${id}`;
}

function idOf(path: string): string {
  if (!path.startsWith(STORED_PREFIX)) throw new Error("Not a file received in this browser");
  const id = path.slice(STORED_PREFIX.length);
  if (!STORED_ID.test(id)) throw new Error("Invalid stored file reference");
  return id;
}

async function opfsFile(id: string): Promise<File | undefined> {
  const handle = await (await filesDir())?.getFileHandle(id).catch(() => null);
  return handle ? await handle.getFile() : undefined;
}

/** Bytes already stored for one file of a transfer (0 when none): the resume offset. */
export async function storedSize(context: SinkContext, fileId: string, nonce: string): Promise<number> {
  const id = await storageId(context, fileId, nonce);
  if (await hasOpfs()) return (await opfsFile(id))?.size ?? 0;
  return blobOf(await blobs.get(id))?.size ?? 0;
}

/** The received file behind a history path, typed and named for saving. */
export async function readStored(path: string, name: string, mime: string): Promise<File> {
  const id = idOf(path);
  const type = mime || "application/octet-stream";
  if (await hasOpfs()) {
    const file = await opfsFile(id);
    if (file) return new File([file], name, { type, lastModified: file.lastModified });
  }
  // Also where files received before this browser offered OPFS are kept.
  const blob = blobOf(await blobs.get(id));
  if (!blob) throw new Error("This file is no longer stored in the browser");
  return new File([blob], name, { type });
}

/** Deletes a stored file wherever it is. Throws when the browser refuses (a missing file is fine). */
export async function deleteStored(path: string): Promise<void> {
  const id = idOf(path);
  if (await hasOpfs()) {
    const dir = await filesDir();
    try {
      await dir?.removeEntry(id);
    } catch (err) {
      if ((err as { name?: string } | null)?.name !== "NotFoundError") throw err;
    }
  }
  await blobs.delete(id);
}

/** The sink handed to the WebRTC sessions. `onCommitted` records the file so the Inbox lists it. */
export function createBrowserSink(onCommitted: CommitHandler, nonceOf: (context: SinkContext) => string): FileSink {
  return {
    async open(file, offset, context) {
      const id = await storageId(context, file.id, nonceOf(context));
      if (await hasOpfs()) return openOpfs(id, file, offset, context, onCommitted);
      return openFallback(id, file, offset, context, onCommitted);
    },
    async *readPrefix(file, offset, context) {
      const id = await storageId(context, file.id, nonceOf(context));
      const blob = (await hasOpfs()) ? await opfsFile(id) : blobOf(await blobs.get(id));
      if (!blob || blob.size < offset) throw new RtcError("no-resume", "The partial file is gone");
      const reader = blob.slice(0, offset).stream().getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) return;
        yield value;
      }
    },
  };
}

async function openOpfs(id: string, file: FileMeta, offset: number, context: SinkContext, onCommitted: CommitHandler): Promise<SinkWriter> {
  const dir = (await filesDir())!;
  let writable: FileSystemWritableFileStream;
  try {
    const handle = await dir.getFileHandle(id, { create: true });
    // keepExistingData + truncate: resume after the verified prefix, drop the rest.
    writable = await handle.createWritable({ keepExistingData: offset > 0 });
    await writable.truncate(offset);
    await writable.seek(offset);
  } catch (err) {
    throw storageError(err);
  }
  let finished = false;
  return {
    async write(chunk) {
      try {
        await writable.write(chunk as Uint8Array<ArrayBuffer>);
      } catch (err) {
        throw storageError(err);
      }
    },
    async close() {
      finished = true;
      try {
        await writable.close();
      } catch (err) {
        throw storageError(err);
      }
      try {
        await onCommitted(file, context, storedPath(id));
      } catch (err) {
        // Unlisted, nobody could reach it: don't keep it.
        await dir.removeEntry(id).catch(() => {});
        throw storageError(err);
      }
    },
    async abort(reason: SinkAbortReason) {
      if (finished) return;
      finished = true;
      if (reason === "closed" || reason === "timeout") {
        // Keep what arrived so the same transfer can resume later.
        await writable.close().catch(() => {});
      } else {
        await writable.abort().catch(() => {});
        await dir.removeEntry(id).catch(() => {});
      }
    },
  };
}

async function openFallback(id: string, file: FileMeta, offset: number, context: SinkContext, onCommitted: CommitHandler): Promise<SinkWriter> {
  if (file.size > FALLBACK_LIMIT) {
    throw new RtcError("too-large", "This browser can't store files over 256 MB. Use Chrome, Edge or Firefox, or the Ferry app.");
  }
  const parts: BlobPart[] = [];
  if (offset > 0) {
    const previous = blobOf(await blobs.get(id));
    if (!previous || previous.size < offset) throw new RtcError("no-resume", "The partial file is gone");
    parts.push(previous.slice(0, offset));
  }
  let finished = false;
  return {
    async write(chunk) {
      parts.push(chunk.slice());
    },
    async close() {
      finished = true;
      try {
        await blobs.put({ blob: new Blob(parts, { type: file.mime }), partial: false, at: Date.now() }, id);
      } catch (err) {
        throw storageError(err);
      }
      try {
        await onCommitted(file, context, storedPath(id));
      } catch (err) {
        await blobs.delete(id).catch(() => {});
        throw storageError(err);
      }
    },
    async abort(reason) {
      if (finished) return;
      finished = true;
      if (reason === "closed" || reason === "timeout") await blobs.put({ blob: new Blob(parts), partial: true, at: Date.now() }, id).catch(() => {});
      else await blobs.delete(id).catch(() => {});
    },
  };
}

export interface PruneOptions {
  /** Files written more recently than this are left alone (another tab may be receiving them). */
  graceMs: number;
  now?: number;
}

export interface PruneResult {
  removed: number;
  /** Files the browser refused to delete. */
  failed: number;
}

/**
 * Deletes stored files that nothing refers to any more (expired partial files,
 * files whose Inbox entry is gone), in both OPFS and the IndexedDB fallback.
 * `keep` holds the storage ids still referenced.
 */
export async function pruneStored(keep: ReadonlySet<string>, options: PruneOptions): Promise<PruneResult> {
  const now = options.now ?? Date.now();
  const old = (at: number | undefined) => at === undefined || now - at >= options.graceMs;
  const result: PruneResult = { removed: 0, failed: 0 };
  const remove = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
      result.removed++;
    } catch {
      result.failed++;
    }
  };

  if (await hasOpfs()) {
    const dir = await filesDir();
    if (dir) {
      const names: string[] = [];
      for await (const name of (dir as unknown as { keys(): AsyncIterable<string> }).keys()) {
        if (STORED_ID.test(name) && !keep.has(name)) names.push(name);
      }
      for (const name of names) {
        const file = await opfsFile(name).catch(() => undefined);
        if (file && !old(file.lastModified)) continue;
        await remove(() => dir.removeEntry(name));
      }
    }
  }

  // The fallback store, a bounded batch of keys at a time.
  let after: IDBValidKey | undefined;
  for (;;) {
    const batch = await blobs.keys(PRUNE_BATCH, after);
    if (!batch.length) break;
    after = batch.at(-1);
    for (const key of batch) {
      if (typeof key !== "string" || !STORED_ID.test(key) || keep.has(key)) continue;
      const value = await blobs.get(key);
      if (value && !(value instanceof Blob) && !old(value.at)) continue;
      await remove(() => blobs.delete(key));
    }
    if (batch.length < PRUNE_BATCH) break;
  }
  return result;
}

export function idOfPath(path: string): string | null {
  return path.startsWith(STORED_PREFIX) ? path.slice(STORED_PREFIX.length) : null;
}
