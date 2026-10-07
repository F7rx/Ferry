// @vitest-environment node
import { describe, expect, it } from "vitest";
import { loadWeb, newProfile } from "../platform/web-test-env";

interface Entry {
  id: number;
  name: string;
}

/** A version 1 database as the first releases created it. */
function createV1(profile: IDBFactory, history: Entry[], blobs: [string, unknown][] = []): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = profile.open("ferry", 1);
    req.onupgradeneeded = () => {
      const db = req.result;
      db.createObjectStore("kv");
      db.createObjectStore("blobs");
      db.createObjectStore("devices", { keyPath: "id" });
      const h = db.createObjectStore("history", { keyPath: "id" });
      for (const e of history) h.put(e);
      const b = req.transaction!.objectStore("blobs");
      for (const [k, v] of blobs) b.put(v, k);
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

describe("IndexedDB wrapper", () => {
  it("migrates version 1 history to database-allocated ids without losing entries", async () => {
    const profile = newProfile();
    const old = [
      { id: 1_700_000_000_000_00, name: "a" },
      { id: 1_700_000_000_000_01, name: "b" },
      { id: 1_700_000_000_500_99, name: "c" },
    ];
    (await createV1(profile, old, [["k", "blob"]])).close();
    const { idb } = await loadWeb(profile);
    const history = idb.store<Entry>("history");
    expect(await history.all()).toEqual(old);
    expect(await idb.store<unknown>("blobs").get("k")).toBe("blob");
    // New ids come after every migrated one, so "newest first" stays right.
    const id = (await history.put({ name: "new" } as Entry)) as number;
    expect(id).toBeGreaterThan(old.at(-1)!.id);
    expect((await history.page({ limit: 2, reverse: true })).map((e) => e.name)).toEqual(["new", "c"]);
    // Opening again (a reload) changes nothing.
    const again = await loadWeb(profile);
    expect(await again.idb.store<Entry>("history").all()).toHaveLength(4);
  });

  it("allocates unique ids for bursts from two tabs at once", async () => {
    const profile = newProfile();
    const tabA = (await loadWeb(profile)).idb.store<Entry>("history");
    await tabA.all(); // opened
    const tabB = (await loadWeb(profile)).idb.store<Entry>("history");
    const writes = [];
    for (let i = 0; i < 100; i++) {
      writes.push(tabA.put({ name: `a${i}` } as Entry));
      writes.push(tabB.put({ name: `b${i}` } as Entry));
    }
    const ids = (await Promise.all(writes)) as number[];
    expect(new Set(ids).size).toBe(200);
    expect(await tabA.all()).toHaveLength(200);
    expect((await tabB.all()).every((e) => typeof e.id === "number")).toBe(true);
  });

  it("pages by key: stable while entries are added and removed", async () => {
    const { idb } = await loadWeb(newProfile());
    const s = idb.store<Entry>("history");
    for (let i = 0; i < 10; i++) await s.put({ name: `e${i}` } as Entry);
    const first = await s.page({ limit: 4, reverse: true });
    expect(first.map((e) => e.name)).toEqual(["e9", "e8", "e7", "e6"]);
    await s.put({ name: "newer" } as Entry);
    await s.delete(first[0]!.id);
    const second = await s.page({ limit: 4, reverse: true, after: first.at(-1)!.id });
    expect(second.map((e) => e.name)).toEqual(["e5", "e4", "e3", "e2"]);
    const filtered = await s.page({ limit: 2, filter: (e) => Number(e.name.slice(1)) % 2 === 1 });
    expect(filtered.map((e) => e.name)).toEqual(["e1", "e3"]);
    expect(await s.page({ limit: 0 })).toEqual([]);
  });

  it("enumerates keys in bounded batches", async () => {
    const { idb } = await loadWeb(newProfile());
    const blobs = idb.store<string>("blobs");
    for (let i = 0; i < 25; i++) await blobs.put(`v${i}`, `k${String(i).padStart(2, "0")}`);
    const seen: IDBValidKey[] = [];
    let after: IDBValidKey | undefined;
    for (;;) {
      const batch = await blobs.keys(10, after);
      expect(batch.length).toBeLessThanOrEqual(10);
      if (!batch.length) break;
      seen.push(...batch);
      after = batch.at(-1);
    }
    expect(seen).toHaveLength(25);
  });

  it("updates and rewrites in single transactions", async () => {
    const { idb } = await loadWeb(newProfile());
    const kv = idb.store<number[]>("kv");
    await Promise.all(Array.from({ length: 20 }, (_, i) => kv.update("list", (l) => [...(l ?? []), i])));
    expect((await kv.get("list"))!.sort((a, b) => a - b)).toEqual(Array.from({ length: 20 }, (_, i) => i));
    expect(await kv.update("list", () => undefined)).toBeUndefined();
    expect(await kv.get("list")).toBeUndefined();

    const s = idb.store<Entry>("history");
    for (let i = 0; i < 6; i++) await s.put({ name: `e${i}` } as Entry);
    expect(await s.rewrite((e) => (Number(e.name.slice(1)) < 2 ? null : Number(e.name.slice(1)) < 4 ? { ...e, name: e.name.toUpperCase() } : undefined))).toEqual({
      updated: 2,
      deleted: 2,
    });
    expect((await s.all()).map((e) => e.name)).toEqual(["E2", "E3", "e4", "e5"]);
    // A failure inside rolls everything back.
    await expect(
      s.rewrite((e) => {
        if (e.name === "e4") throw new Error("boom");
        return null;
      }),
    ).rejects.toBeTruthy();
    expect(await s.all()).toHaveLength(4);
  });

  it("says what to do when an old tab blocks the upgrade", async () => {
    const profile = newProfile();
    const v1 = await createV1(profile, []);
    const { idb } = await loadWeb(profile);
    await expect(idb.store("kv").get("x")).rejects.toThrow(/Reload or close the other Ferry tabs/);
    v1.close();
    // Once the old tab is gone, the next attempt opens it.
    await expect(idb.store("kv").get("x")).resolves.toBeUndefined();
  }, 15_000);
});
