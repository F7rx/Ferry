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

const blobs = store<Blob>("blobs");

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
  if (!/^[0-9a-f]{32}$/.test(id)) throw new Error("Invalid stored file reference");
  return id;
}

/** Bytes already stored for one file of a transfer (0 when none): the resume offset. */
export async function storedSize(context: SinkContext, fileId: string, nonce: string): Promise<number> {
  const id = await storageId(context, fileId, nonce);
  if (await hasOpfs()) {
    const handle = await (await filesDir())?.getFileHandle(id).catch(() => null);
    return handle ? (await handle.getFile()).size : 0;
  }
  return (await blobs.get(id))?.size ?? 0;
}

/** The received file behind a history path, typed and named for saving. */
export async function readStored(path: string, name: string, mime: string): Promise<File> {
  const id = idOf(path);
  if (await hasOpfs()) {
    const dir = await filesDir();
    const handle = await dir!.getFileHandle(id);
    const file = await handle.getFile();
    return new File([file], name, { type: mime || "application/octet-stream", lastModified: file.lastModified });
  }
  const blob = await blobs.get(id);
  if (!blob) throw new Error("This file is no longer stored in the browser");
  return new File([blob], name, { type: mime || "application/octet-stream" });
}

export async function deleteStored(path: string): Promise<void> {
  const id = idOf(path);
  if (await hasOpfs()) await (await filesDir())?.removeEntry(id).catch(() => {});
  else await blobs.delete(id);
}

/** The sink handed to the WebRTC sessions. `onCommitted` records the file in history. */
export function createBrowserSink(
  onCommitted: (file: FileMeta, context: SinkContext, path: string) => void,
  nonceOf: (context: SinkContext) => string,
): FileSink {
  return {
    async open(file, offset, context) {
      const id = await storageId(context, file.id, nonceOf(context));
      if (await hasOpfs()) return openOpfs(id, file, offset, context, onCommitted);
      return openFallback(id, file, offset, context, onCommitted);
    },
    async *readPrefix(file, offset, context) {
      const id = await storageId(context, file.id, nonceOf(context));
      let blob: Blob | undefined;
      if (await hasOpfs()) {
        const handle = await (await filesDir())!.getFileHandle(id).catch(() => null);
        blob = handle ? await handle.getFile() : undefined;
      } else {
        blob = await blobs.get(id);
      }
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

async function openOpfs(
  id: string,
  file: FileMeta,
  offset: number,
  context: SinkContext,
  onCommitted: (file: FileMeta, context: SinkContext, path: string) => void,
): Promise<SinkWriter> {
  const dir = (await filesDir())!;
  const handle = await dir.getFileHandle(id, { create: true });
  // keepExistingData + truncate: resume after the verified prefix, drop the rest.
  const writable = await handle.createWritable({ keepExistingData: offset > 0 });
  await writable.truncate(offset);
  await writable.seek(offset);
  let finished = false;
  return {
    async write(chunk) {
      await writable.write(chunk as Uint8Array<ArrayBuffer>);
    },
    async close() {
      finished = true;
      await writable.close();
      onCommitted(file, context, storedPath(id));
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

async function openFallback(
  id: string,
  file: FileMeta,
  offset: number,
  context: SinkContext,
  onCommitted: (file: FileMeta, context: SinkContext, path: string) => void,
): Promise<SinkWriter> {
  if (file.size > FALLBACK_LIMIT) {
    throw new RtcError("too-large", "This browser can't store files over 256 MB. Use Chrome, Edge or Firefox, or the Ferry app.");
  }
  const parts: BlobPart[] = [];
  if (offset > 0) {
    const previous = await blobs.get(id);
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
      await blobs.put(new Blob(parts, { type: file.mime }), id);
      onCommitted(file, context, storedPath(id));
    },
    async abort(reason) {
      if (finished) return;
      finished = true;
      if (reason === "closed" || reason === "timeout") await blobs.put(new Blob(parts), id).catch(() => {});
      else await blobs.delete(id).catch(() => {});
    },
  };
}

/** Deletes stored files that nothing refers to any more (expired partial files). */
export async function pruneStored(keep: Set<string>): Promise<number> {
  let removed = 0;
  if (await hasOpfs()) {
    const dir = await filesDir();
    if (!dir) return 0;
    const names: string[] = [];
    for await (const name of (dir as unknown as { keys(): AsyncIterable<string> }).keys()) names.push(name);
    for (const name of names) {
      if (/^[0-9a-f]{32}$/.test(name) && !keep.has(name)) {
        await dir.removeEntry(name).catch(() => {});
        removed++;
      }
    }
  }
  return removed;
}

export function idOfPath(path: string): string | null {
  return path.startsWith(STORED_PREFIX) ? path.slice(STORED_PREFIX.length) : null;
}
