// @vitest-environment node
// Received-file storage in both backends: OPFS (a small in-memory fake) and
// the IndexedDB fallback (fake-indexeddb), including startup pruning.
import { afterEach, describe, expect, it } from "vitest";
import { loadWeb, newProfile } from "./web-test-env";

const HOUR = 3600_000;
const id = (c: string) => c.repeat(32);

/** In-memory origin-private file system: just what web-sink.ts uses. */
class FakeDir {
  readonly files = new Map<string, { data: Uint8Array; lastModified: number }>();
  readonly dirs = new Map<string, FakeDir>();
  failRemove = new Set<string>();
  async getDirectoryHandle(name: string, opts?: { create?: boolean }) {
    let d = this.dirs.get(name);
    if (!d && opts?.create) this.dirs.set(name, (d = new FakeDir()));
    if (!d) throw Object.assign(new Error("not found"), { name: "NotFoundError" });
    return d;
  }
  async getFileHandle(name: string, opts?: { create?: boolean }) {
    if (!this.files.has(name)) {
      if (!opts?.create) throw Object.assign(new Error("not found"), { name: "NotFoundError" });
      this.files.set(name, { data: new Uint8Array(0), lastModified: Date.now() });
    }
    const dir = this;
    return {
      async getFile() {
        const f = dir.files.get(name);
        if (!f) throw Object.assign(new Error("not found"), { name: "NotFoundError" });
        return new File([f.data as Uint8Array<ArrayBuffer>], name, { lastModified: f.lastModified });
      },
      async createWritable(o?: { keepExistingData?: boolean }) {
        let buf = o?.keepExistingData ? dir.files.get(name)!.data.slice() : new Uint8Array(0);
        let pos = 0;
        return {
          async write(chunk: Uint8Array) {
            if (dir.quotaLeft !== null && chunk.byteLength > dir.quotaLeft) throw Object.assign(new Error("quota"), { name: "QuotaExceededError" });
            const next = new Uint8Array(Math.max(buf.byteLength, pos + chunk.byteLength));
            next.set(buf);
            next.set(chunk, pos);
            buf = next;
            pos += chunk.byteLength;
          },
          async truncate(n: number) {
            buf = buf.slice(0, n);
          },
          async seek(n: number) {
            pos = n;
          },
          async close() {
            dir.files.set(name, { data: buf, lastModified: Date.now() });
          },
          async abort() {},
        };
      },
    };
  }
  quotaLeft: number | null = null;
  async removeEntry(name: string) {
    if (this.failRemove.has(name)) throw Object.assign(new Error("busy"), { name: "NoModificationAllowedError" });
    if (!this.files.delete(name)) throw Object.assign(new Error("not found"), { name: "NotFoundError" });
  }
  async *keys() {
    yield* [...this.files.keys()];
  }
}

function installOpfs(root: FakeDir | null) {
  Object.defineProperty(globalThis.navigator, "storage", {
    configurable: true,
    value: root ? { getDirectory: async () => root } : undefined,
  });
}

afterEach(() => installOpfs(null));

const context = { peerKey: "peer", transferId: "t1" };
const meta = (size: number) => ({ id: "f", name: "a.bin", size, mime: "application/octet-stream" });

describe("received-file storage", () => {
  for (const backend of ["opfs", "indexeddb"] as const) {
    describe(backend, () => {
      async function load() {
        const root = backend === "opfs" ? new FakeDir() : null;
        installOpfs(root);
        const mods = await loadWeb(newProfile());
        expect(await mods.sink.hasOpfs()).toBe(backend === "opfs");
        const files = root ? await root.getDirectoryHandle("files", { create: true }) : null;
        const blobs = mods.idb.store<unknown>("blobs");
        /** Plants a stored file `age` ms old. */
        const plant = async (sid: string, age: number, partial = true) => {
          if (files) files.files.set(sid, { data: new Uint8Array([1, 2, 3]), lastModified: Date.now() - age });
          else await blobs.put({ blob: new Blob([new Uint8Array([1, 2, 3])]), partial, at: Date.now() - age }, sid);
        };
        const exists = async (sid: string) => (files ? files.files.has(sid) : (await blobs.get(sid)) !== undefined);
        return { ...mods, files, blobs, plant, exists };
      }

      it("writes, commits, reads back and resumes a file", async () => {
        const { sink, exists } = await load();
        const committed: string[] = [];
        const s = sink.createBrowserSink(async (_f, _c, path) => void committed.push(path), () => "nonce");
        const w = await s.open(meta(6), 0, context);
        await w.write(new Uint8Array([1, 2, 3]));
        await w.abort("closed"); // interrupted: kept for a resume
        const sid = await sink.storageId(context, "f", "nonce");
        expect(await exists(sid)).toBe(true);
        expect(await sink.storedSize(context, "f", "nonce")).toBe(3);
        const prefix: Uint8Array[] = [];
        for await (const p of s.readPrefix!(meta(6), 3, context)) prefix.push(p);
        expect(prefix.flatMap((p) => [...p])).toEqual([1, 2, 3]);
        const w2 = await s.open(meta(6), 3, context);
        await w2.write(new Uint8Array([4, 5, 6]));
        await w2.close();
        expect(committed).toEqual([sink.storedPath(sid)]);
        const file = await sink.readStored(committed[0]!, "a.bin", "text/plain");
        expect([...new Uint8Array(await file.arrayBuffer())]).toEqual([1, 2, 3, 4, 5, 6]);
        expect(file.type).toBe("text/plain");
      });

      it("drops a file it couldn't list, and says why", async () => {
        const { sink, exists } = await load();
        const s = sink.createBrowserSink(async () => {
          throw Object.assign(new Error("full"), { name: "QuotaExceededError" });
        }, () => "n");
        const w = await s.open(meta(1), 0, context);
        await w.write(new Uint8Array([9]));
        await expect(w.close()).rejects.toMatchObject({ code: "storage-full", message: expect.stringMatching(/out of space/) });
        expect(await exists(await sink.storageId(context, "f", "n"))).toBe(false);
      });

      it("prunes only unreferenced files older than the grace period, and is repeatable", async () => {
        const { sink, plant, exists } = await load();
        const listed = id("1"); // committed, in the Inbox
        const retained = id("2"); // interrupted, resume record still valid
        const active = id("3"); // another tab is receiving it right now
        const expired = id("4"); // interrupted long ago, record gone
        await plant(listed, 48 * HOUR, false);
        await plant(retained, 20 * HOUR);
        await plant(active, 1000);
        await plant(expired, 30 * HOUR);
        const keep = new Set([listed, retained]);
        expect(await sink.pruneStored(keep, { graceMs: HOUR })).toEqual({ removed: 1, failed: 0 });
        expect(await exists(listed)).toBe(true);
        expect(await exists(retained)).toBe(true);
        expect(await exists(active)).toBe(true);
        expect(await exists(expired)).toBe(false);
        expect(await sink.pruneStored(keep, { graceMs: HOUR })).toEqual({ removed: 0, failed: 0 });
        // Later, once the other tab's file is old and unreferenced, it goes too.
        expect(await sink.pruneStored(keep, { graceMs: HOUR, now: Date.now() + 2 * HOUR })).toEqual({ removed: 1, failed: 0 });
        expect(await exists(active)).toBe(false);
      });

      it("deletes a stored file and reports refusals", async () => {
        const { sink, plant, exists, files } = await load();
        await plant(id("5"), 0, false);
        await sink.deleteStored(sink.storedPath(id("5")));
        expect(await exists(id("5"))).toBe(false);
        await sink.deleteStored(sink.storedPath(id("5"))); // already gone: fine
        if (files) {
          await plant(id("6"), 30 * HOUR);
          files.failRemove.add(id("6"));
          await expect(sink.deleteStored(sink.storedPath(id("6")))).rejects.toThrow();
          expect(await sink.pruneStored(new Set(), { graceMs: HOUR })).toEqual({ removed: 0, failed: 1 });
        }
      });
    });
  }

  it("prunes a large IndexedDB fallback in bounded batches, keeping committed files", async () => {
    installOpfs(null);
    const { sink, idb } = await loadWeb(newProfile());
    const blobs = idb.store<unknown>("blobs");
    const old = Date.now() - 30 * HOUR;
    const keep = new Set<string>();
    for (let i = 0; i < 450; i++) {
      const sid = i.toString(16).padStart(32, "0");
      await blobs.put(i % 3 === 0 ? new Blob(["legacy"]) : { blob: new Blob(["x"]), partial: true, at: old }, sid);
      if (i % 10 === 0) keep.add(sid);
    }
    const result = await sink.pruneStored(keep, { graceMs: HOUR });
    expect(result).toEqual({ removed: 450 - keep.size, failed: 0 });
    expect((await blobs.keys(1000)).sort()).toEqual([...keep].sort());
  });

  it("maps a full disk while writing to a message that says what to do", async () => {
    const root = new FakeDir();
    installOpfs(root);
    const { sink } = await loadWeb(newProfile());
    (await root.getDirectoryHandle("files", { create: true })).quotaLeft = 0;
    const w = await sink.createBrowserSink(async () => {}, () => "n").open(meta(1), 0, context);
    await expect(w.write(new Uint8Array([1]))).rejects.toMatchObject({ code: "storage-full" });
  });
});
