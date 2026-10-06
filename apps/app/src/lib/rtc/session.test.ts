// @vitest-environment node
import { sha256 } from "@noble/hashes/sha2.js";
import { describe, expect, it } from "vitest";
import { equalBytes, fromUtf8, randomBytes, toHex, utf8 } from "./bytes";
import { BUFFER_HIGH_WATER, MAX_TEXT_BYTES, RECV_WINDOW, RtcError } from "./protocol";
import type { FileSink, IncomingOffer } from "./session";
import {
  BUFFER_CEILING,
  collect,
  createSessionPair,
  MemorySink,
  memorySource,
  nextEvent,
  sleep,
  waitUntil,
  type SessionPairOptions,
} from "./test-fakes";

async function ready(options: SessionPairOptions = {}) {
  const sink = new MemorySink();
  const pair = await createSessionPair({ ...options, b: { sink, ...options.b } });
  await Promise.all([pair.a.ready, pair.b.ready]);
  return { ...pair, sink };
}

async function rejection(p: Promise<unknown>): Promise<RtcError> {
  try {
    await p;
  } catch (err) {
    expect(err).toBeInstanceOf(RtcError);
    return err as RtcError;
  }
  throw new Error("expected a rejection");
}

const digest = (bytes: Uint8Array) => toHex(sha256(bytes));

function same(actual: Uint8Array, expected: Uint8Array): void {
  expect(actual.byteLength).toBe(expected.byteLength);
  expect(equalBytes(actual, expected)).toBe(true);
}

describe("transfers", () => {
  it("streams several files, verifies every SHA-256 and writes byte-identical output", async () => {
    const { a, b, sink, chA } = await ready();
    const blobs: Record<string, Uint8Array> = { f1: utf8("hello"), f2: randomBytes(300_000), f3: randomBytes(2_500_000) };
    const files = [memorySource("f1", blobs.f1!, "note.txt", "text/plain"), memorySource("f2", blobs.f2!, "dir/f2.bin"), memorySource("f3", blobs.f3!)];
    const offers = collect(b, "offer");
    b.on("offer", (o) => o.accept());
    const accepted = nextEvent(a, "accepted");
    const receiverDone = nextEvent(b, "done");
    const sent = collect(a, "fileComplete");
    const received = collect(b, "fileComplete");
    const progress = collect(b, "progress");

    const outcome = await a.sendTransfer({ transferId: "t1", files, text: "for you" });

    expect(outcome).toEqual({ transferId: "t1", declined: false, completed: ["f1", "f2", "f3"], failed: [], skipped: [] });
    expect(await accepted).toMatchObject({ transferId: "t1", files: ["f1", "f2", "f3"] });
    expect(await receiverDone).toEqual({ transferId: "t1", direction: "receive", completed: ["f1", "f2", "f3"], failed: [], skipped: [] });
    expect(offers).toHaveLength(1);
    expect(offers[0]!.files).toEqual([
      { id: "f1", name: "note.txt", size: 5, mime: "text/plain" },
      { id: "f2", name: "dir/f2.bin", size: 300_000, mime: "application/octet-stream" },
      { id: "f3", name: "f3.bin", size: 2_500_000, mime: "application/octet-stream" },
    ]);
    expect(offers[0]!.text).toBe("for you");
    expect(offers[0]!.peer.device.alias).toBe("Alice");
    for (const [id, blob] of Object.entries(blobs)) {
      same(sink.data(id), blob);
      expect(sink.files.get(id)!.state).toBe("closed");
    }
    const expected = Object.entries(blobs).map(([id, blob]) => ({ fileId: id, ok: true, sha256: digest(blob) }));
    expect(sent).toMatchObject(expected);
    expect(received).toMatchObject(expected);
    expect(progress.filter((p) => p.fileId === "f3").at(-1)).toMatchObject({ bytes: 2_500_000, totalBytes: 2_500_000 });
    expect(chA.maxBinaryFrame).toBe(64 * 1024); // max-message-size 262144 allows 64 KiB chunks
  });

  it("uses 16 KiB chunks when the remote SDP does not allow more", async () => {
    const { a, b, sink, chA } = await ready({ a: { maxMessageSize: undefined } });
    b.on("offer", (o) => o.accept());
    const blob = randomBytes(100_000);
    await a.sendTransfer({ transferId: "t1", files: [memorySource("f1", blob)] });
    expect(chA.maxBinaryFrame).toBe(16 * 1024);
    same(sink.data("f1"), blob);
  });

  it("transfers only the files the receiver selected", async () => {
    const { a, b, sink } = await ready();
    const blob = randomBytes(70_000);
    b.on("offer", (o) => o.accept(["f2"]));
    const accepted = nextEvent(a, "accepted");
    const receiverDone = nextEvent(b, "done");
    const outcome = await a.sendTransfer({
      transferId: "t1",
      files: [memorySource("f1", randomBytes(10)), memorySource("f2", blob), memorySource("f3", randomBytes(10))],
    });
    expect(outcome).toEqual({ transferId: "t1", declined: false, completed: ["f2"], failed: [], skipped: ["f1", "f3"] });
    expect((await accepted).files).toEqual(["f2"]);
    expect((await receiverDone).skipped).toEqual(["f1", "f3"]);
    expect([...sink.files.keys()]).toEqual(["f2"]);
    same(sink.data("f2"), blob);
  });

  it("handles a decline and stays usable", async () => {
    const { a, b, sink } = await ready();
    b.once("offer", (o) => o.decline());
    const declined = await a.sendTransfer({ transferId: "t1", files: [memorySource("f1", randomBytes(10))] });
    expect(declined).toEqual({ transferId: "t1", declined: true, completed: [], failed: [], skipped: ["f1"] });
    expect(sink.files.size).toBe(0);

    b.once("offer", (o) => o.accept());
    const accepted = await a.sendTransfer({ transferId: "t2", files: [memorySource("g1", utf8("again"))] });
    expect(accepted.completed).toEqual(["g1"]);
  });

  it("declines automatically when nobody listens for offers", async () => {
    const { a } = await ready();
    const outcome = await a.sendTransfer({ transferId: "t1", files: [memorySource("f1", randomBytes(10))] });
    expect(outcome.declined).toBe(true);
  });

  it("transfers zero-byte files", async () => {
    const { a, b, sink, chA } = await ready();
    b.on("offer", (o) => o.accept());
    const received = collect(b, "fileComplete");
    const outcome = await a.sendTransfer({
      transferId: "t1",
      files: [memorySource("e1", new Uint8Array(0)), memorySource("x", utf8("x")), memorySource("e2", new Uint8Array(0))],
    });
    expect(outcome.completed).toEqual(["e1", "x", "e2"]);
    expect(sink.data("e1").byteLength).toBe(0);
    expect(sink.files.get("e2")!.state).toBe("closed");
    const EMPTY = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    expect(received.find((r) => r.fileId === "e1")).toMatchObject({ ok: true, sha256: EMPTY });
    expect(chA.binaryFrames).toBe(1);
  });

  it("delivers text-only offers without a sink", async () => {
    const { a, b } = await ready({ b: { sink: undefined } });
    const offer = nextEvent(b, "offer");
    const receiverDone = nextEvent(b, "done");
    b.on("offer", (o) => o.accept());
    const outcome = await a.sendTransfer({ transferId: "t1", files: [], text: "https://example.com/✨" });
    expect((await offer).text).toBe("https://example.com/✨");
    expect(outcome).toEqual({ transferId: "t1", declined: false, completed: [], failed: [], skipped: [] });
    expect((await receiverDone).transferId).toBe("t1");
  });

  it("limits text messages to MAX_TEXT_BYTES", async () => {
    const { a, b } = await ready();
    const offer = nextEvent(b, "offer");
    b.on("offer", (o) => o.accept());
    const longest = "é".repeat((MAX_TEXT_BYTES - 2) / 2); // a JSON string of exactly MAX_TEXT_BYTES
    await a.sendTransfer({ transferId: "t1", files: [], text: longest });
    expect((await offer).text).toBe(longest);
    const err = await rejection(a.sendTransfer({ transferId: "t2", files: [], text: `${longest}x` }));
    expect(err.code).toBe("too-large");
  });

  it("runs transfers in both directions at once", async () => {
    const sinkA = new MemorySink();
    const { a, b, sink } = await ready({ a: { sink: sinkA } });
    a.on("offer", (o) => o.accept());
    b.on("offer", (o) => o.accept());
    const x = randomBytes(1_500_000);
    const y = randomBytes(1_200_000);
    const [ab, ba] = await Promise.all([
      a.sendTransfer({ transferId: "ab", files: [memorySource("x", x)] }),
      b.sendTransfer({ transferId: "ba", files: [memorySource("y", y)] }),
    ]);
    expect(ab.completed).toEqual(["x"]);
    expect(ba.completed).toEqual(["y"]);
    same(sink.data("x"), x);
    same(sinkA.data("y"), y);
  });

  it("splits offers and answers that exceed one control frame", { timeout: 30_000 }, async () => {
    const { a, b, sink, chA, chB } = await ready();
    const files = Array.from({ length: 2500 }, (_, i) =>
      memorySource(`file-${String(i).padStart(4, "0")}-${"x".repeat(40)}`, utf8(`#${i}`), `album/photo ${i} ${"y".repeat(60)}.jpg`),
    );
    let offered = 0;
    b.on("offer", (o) => {
      offered = o.files.length;
      o.accept();
    });
    const outcome = await a.sendTransfer({ transferId: "t1", files });
    expect(offered).toBe(2500);
    expect(outcome.completed).toHaveLength(2500);
    expect(chA.sentControl("offer").length).toBeGreaterThan(1);
    expect(chB.sentControl("answer").length).toBeGreaterThan(1);
    expect(fromUtf8(sink.data(files[1234]!.id))).toBe("#1234");
  });
});

describe("integrity", () => {
  it("reports a hash mismatch as a failure and aborts the sink", async () => {
    const { a, b, sink, chA } = await ready();
    let binary = 0;
    chA.transform = (frame) => {
      if (typeof frame === "string" || ++binary !== 3) return frame;
      const copy = new Uint8Array(frame.slice(0));
      copy[100]! ^= 0xff;
      return copy.buffer;
    };
    b.on("offer", (o) => o.accept());
    const sent = collect(a, "fileComplete");
    const received = collect(b, "fileComplete");
    const good = randomBytes(100_000);
    const outcome = await a.sendTransfer({ transferId: "t1", files: [memorySource("f1", randomBytes(300_000)), memorySource("f2", good)] });
    expect(outcome).toMatchObject({ completed: ["f2"], failed: ["f1"] });
    expect(received.find((r) => r.fileId === "f1")).toMatchObject({ ok: false, error: "sha256 mismatch" });
    expect(sent.find((r) => r.fileId === "f1")).toMatchObject({ ok: false, error: "sha256 mismatch" });
    expect(sink.files.get("f1")).toMatchObject({ state: "aborted", abortReason: "integrity" });
    expect(sink.files.get("f2")!.state).toBe("closed");
    same(sink.data("f2"), good);
  });

  it("fails only the file whose write fails", async () => {
    const { a, b, sink } = await ready();
    sink.failWrites.add("f1");
    b.on("offer", (o) => o.accept());
    const received = collect(b, "fileComplete");
    const outcome = await a.sendTransfer({ transferId: "t1", files: [memorySource("f1", randomBytes(200_000)), memorySource("f2", utf8("ok"))] });
    expect(outcome).toMatchObject({ completed: ["f2"], failed: ["f1"] });
    expect(received.find((r) => r.fileId === "f1")).toMatchObject({ ok: false, error: "disk full" });
    expect(sink.files.get("f1")).toMatchObject({ state: "aborted", abortReason: "error" });
  });

  it("resumes from an offset and still verifies the whole file", async () => {
    const { a, b, sink, chA } = await ready();
    const blob = randomBytes(3_000_000);
    const offset = 1_234_567;
    sink.stored.set("f1", blob.slice(0, offset));
    b.on("offer", (o) => o.accept(undefined, { f1: offset }));
    const accepted = nextEvent(a, "accepted");
    const source = memorySource("f1", blob);
    const outcome = await a.sendTransfer({ transferId: "t1", files: [source] });
    expect(outcome.completed).toEqual(["f1"]);
    expect({ ...(await accepted).offsets }).toEqual({ f1: offset });
    expect(chA.binaryBytes).toBe(blob.byteLength - offset); // only the missing part travelled
    expect(sink.prefixReads).toEqual([["f1", offset]]);
    expect(source.reads.slice(0, 2)).toEqual([
      [0, 1_048_576],
      [1_048_576, offset],
    ]); // the sender hashed the prefix
    same(sink.data("f1"), blob);
  });

  it("detects a corrupted resume prefix", async () => {
    const { a, b, sink } = await ready();
    const blob = randomBytes(500_000);
    const stored = blob.slice(0, 200_000);
    stored[7]! ^= 1;
    sink.stored.set("f1", stored);
    b.on("offer", (o) => o.accept(undefined, { f1: 200_000 }));
    const outcome = await a.sendTransfer({ transferId: "t1", files: [memorySource("f1", blob)] });
    expect(outcome.failed).toEqual(["f1"]);
    expect(sink.files.get("f1")).toMatchObject({ state: "aborted", abortReason: "integrity" });
  });

  it("validates accept() arguments", async () => {
    const sink = new MemorySink();
    const noResume: FileSink = { open: (meta, offset) => sink.open(meta, offset) };
    const { a, b } = await ready({ b: { sink: noResume } });
    const checked = new Promise<void>((resolve) => {
      b.on("offer", (o) => {
        const code = (fn: () => void) => {
          try {
            fn();
          } catch (err) {
            return (err as RtcError).code;
          }
          return "ok";
        };
        expect(code(() => o.accept(["nope"]))).toBe("invalid");
        expect(code(() => o.accept(["f1", "f1"]))).toBe("invalid");
        expect(code(() => o.accept(undefined, { f1: -1 }))).toBe("invalid");
        expect(code(() => o.accept(undefined, { f1: 11 }))).toBe("invalid");
        expect(code(() => o.accept(undefined, { f1: 5 }))).toBe("no-resume");
        o.accept();
        expect(code(() => o.accept())).toBe("invalid-state");
        expect(code(() => o.decline())).toBe("invalid-state");
        resolve();
      });
    });
    const outcome = await a.sendTransfer({ transferId: "t1", files: [memorySource("f1", randomBytes(10))] });
    await checked;
    expect(outcome.completed).toEqual(["f1"]);
  });

  it("refuses invalid requests and reused transfer ids", async () => {
    const { a, b } = await ready();
    b.on("offer", (o) => o.accept());
    expect((await rejection(a.sendTransfer({ transferId: "has space", files: [] }))).code).toBe("invalid");
    expect((await rejection(a.sendTransfer({ transferId: "t0", files: [memorySource("f1", utf8("x"), "../x")] }))).code).toBe("invalid");
    await a.sendTransfer({ transferId: "t1", files: [memorySource("f1", utf8("x"))] });
    expect((await rejection(a.sendTransfer({ transferId: "t1", files: [] }))).message).toMatch(/already used/);
  });
});

describe("cancellation", () => {
  it("lets the sender cancel mid-file", async () => {
    const { a, b, sink } = await ready({ channel: { bytesPerTick: 64 * 1024, tickMs: 1 } });
    b.on("offer", (o) => o.accept());
    const cancelledA = nextEvent(a, "cancelled");
    const cancelledB = nextEvent(b, "cancelled");
    b.once("progress", () => a.cancel("changed my mind"));
    const err = await rejection(a.sendTransfer({ transferId: "t1", files: [memorySource("f1", randomBytes(4_000_000))] }));
    expect(err.code).toBe("cancelled");
    expect(await cancelledA).toEqual({ transferId: "t1", direction: "send", byRemote: false, interrupted: false, reason: "changed my mind" });
    expect(await cancelledB).toEqual({ transferId: "t1", direction: "receive", byRemote: true, interrupted: false, reason: "changed my mind" });
    await waitUntil(() => sink.files.get("f1")?.state === "aborted", 5000, "sink abort");
    expect(sink.files.get("f1")!.abortReason).toBe("cancelled");
    expect(sink.data("f1").byteLength).toBeLessThan(4_000_000);

    // Trailing frames of the cancelled transfer are dropped; the session stays usable.
    const blob = randomBytes(200_000);
    const next = await a.sendTransfer({ transferId: "t2", files: [memorySource("g1", blob)] });
    expect(next.completed).toEqual(["g1"]);
    same(sink.data("g1"), blob);
    expect(b.state).toBe("ready");
  });

  it("lets the receiver cancel mid-file", async () => {
    const { a, b, sink } = await ready({ channel: { bytesPerTick: 64 * 1024, tickMs: 1 } });
    b.on("offer", (o) => o.accept());
    const cancelledA = nextEvent(a, "cancelled");
    const cancelledB = nextEvent(b, "cancelled");
    b.once("progress", () => b.cancel("no space left"));
    const err = await rejection(a.sendTransfer({ transferId: "t1", files: [memorySource("f1", randomBytes(4_000_000))] }));
    expect(err.code).toBe("cancelled");
    expect(err.message).toBe("no space left");
    expect(await cancelledA).toEqual({ transferId: "t1", direction: "send", byRemote: true, interrupted: false, reason: "no space left" });
    expect(await cancelledB).toEqual({ transferId: "t1", direction: "receive", byRemote: false, interrupted: false, reason: "no space left" });
    expect(sink.files.get("f1")).toMatchObject({ state: "aborted", abortReason: "cancelled" });

    const next = await a.sendTransfer({ transferId: "t2", files: [memorySource("g1", utf8("still here"))] });
    expect(next.completed).toEqual(["g1"]);
  });

  it("cancels a queued transfer before it is offered", async () => {
    const { a, b, sink } = await ready();
    const pending: IncomingOffer[] = [];
    b.on("offer", (o) => pending.push(o));
    const first = a.sendTransfer({ transferId: "t1", files: [memorySource("f1", utf8("one"))] });
    const second = a.sendTransfer({ transferId: "t2", files: [memorySource("f2", utf8("two"))] });
    const cancelled = nextEvent(a, "cancelled");
    a.cancel("not needed", "t2");
    expect((await rejection(second)).code).toBe("cancelled");
    expect(await cancelled).toMatchObject({ transferId: "t2", direction: "send", byRemote: false });
    await waitUntil(() => pending.length === 1);
    pending[0]!.accept();
    expect((await first).completed).toEqual(["f1"]);
    await sleep(30);
    expect(pending).toHaveLength(1); // t2 was never offered
    expect(sink.files.has("f2")).toBe(false);
  });

  it("cancels offers nobody decides on", async () => {
    const { a, b } = await ready({ b: { decisionTimeoutMs: 80 } });
    const offer = nextEvent(b, "offer");
    const cancelledB = nextEvent(b, "cancelled");
    const started = Date.now();
    const err = await rejection(a.sendTransfer({ transferId: "t1", files: [memorySource("f1", utf8("x"))] }));
    expect(err).toMatchObject({ code: "cancelled", message: "timeout" });
    const o = await offer;
    expect(o.expiresAt).toBeGreaterThanOrEqual(started + 80);
    expect(await cancelledB).toEqual({ transferId: "t1", direction: "receive", byRemote: false, interrupted: false, reason: "timeout" });
    expect(() => o.accept()).toThrow(/no longer pending/);
  });

  it("aborts when the source becomes unreadable and tells the receiver", async () => {
    const { a, b, sink } = await ready();
    const source = memorySource("f1", randomBytes(3_000_000));
    b.on("offer", (o) => {
      source.failReads = true;
      o.accept();
    });
    const cancelledB = nextEvent(b, "cancelled");
    const err = await rejection(a.sendTransfer({ transferId: "t1", files: [source] }));
    expect(err.code).toBe("source");
    expect(err.message).toMatch(/could not read/);
    expect(await cancelledB).toMatchObject({ byRemote: true, interrupted: false });
    expect((await cancelledB).reason).toMatch(/could not read/);
    expect(b.state).toBe("ready");
    expect(sink.files.get("f1")?.state ?? "aborted").toBe("aborted");
  });
});

describe("flow control", () => {
  it("pauses the sender while bufferedAmount is above the high-water mark", async () => {
    const { a, b, sink, chA } = await ready({ channel: { bytesPerTick: 128 * 1024, tickMs: 1 } });
    b.on("offer", (o) => o.accept());
    const blob = randomBytes(6 * 1024 * 1024);
    const outcome = await a.sendTransfer({ transferId: "t1", files: [memorySource("big", blob)] });
    expect(outcome.completed).toEqual(["big"]);
    expect(chA.maxBuffered).toBeGreaterThan(BUFFER_HIGH_WATER); // the mark was reached …
    expect(chA.maxBuffered).toBeLessThanOrEqual(BUFFER_CEILING); // … overshot by at most one chunk
    expect(chA.maxBufferedBeforeBinarySend).toBeLessThanOrEqual(BUFFER_HIGH_WATER + 4096); // never sent above it
    same(sink.data("big"), blob);
  });

  it("keeps at most RECV_WINDOW unprocessed bytes in flight when the sink stalls", async () => {
    const { a, b, sink, chA, chB } = await ready();
    const release = sink.blockWrites();
    b.on("offer", (o) => o.accept());
    const blob = randomBytes(20 * 1024 * 1024);
    const outcome = a.sendTransfer({ transferId: "t1", files: [memorySource("f1", blob)] });
    // The sender stops exactly at the window while the receiver processes nothing.
    await waitUntil(() => chA.binaryBytes >= RECV_WINDOW, 10_000, "window");
    await sleep(100);
    expect(chA.binaryBytes).toBe(RECV_WINDOW);
    expect((b as unknown as { queuedBinary: number }).queuedBinary).toBeLessThanOrEqual(RECV_WINDOW);
    release();
    expect((await outcome).completed).toEqual(["f1"]);
    same(sink.data("f1"), blob);
    expect(chB.sentControl("progress").length).toBeGreaterThanOrEqual(19); // one report per MiB processed
  });

  it("fails the session when a sender ignores the window", async () => {
    const { a, b, sink, chA } = await ready();
    const release = sink.blockWrites();
    b.on("offer", (o) => o.accept());
    void a.sendTransfer({ transferId: "t1", files: [memorySource("f1", randomBytes(20 * 1024 * 1024))] }).catch(() => {});
    await waitUntil(() => chA.binaryBytes >= RECV_WINDOW, 10_000, "window");
    const closed = nextEvent(b, "closed");
    for (let i = 0; i < 8; i++) chA.send(new Uint8Array(64 * 1024)); // a rogue sender pushing past the window
    expect(await closed).toEqual({ reason: "protocol", message: "the peer ignored the flow-control window" });
    release();
  });
});

describe("liveness", () => {
  it("keeps an idle session alive with pings and times out on silence", async () => {
    const timing = { pingIntervalMs: 20, timeoutMs: 150 };
    const { a, b, chA, chB } = await ready({ a: timing, b: timing });
    await sleep(400);
    expect(a.state).toBe("ready");
    expect(b.state).toBe("ready");
    expect(chA.sentControl("ping").length).toBeGreaterThan(5);
    expect(chB.sentControl("pong").length).toBeGreaterThan(5);
    const closedA = nextEvent(a, "closed");
    const closedB = nextEvent(b, "closed");
    chA.blackhole = chB.blackhole = true;
    expect(await closedA).toEqual({ reason: "timeout", message: "no frames from the peer" });
    expect((await closedB).reason).toBe("timeout");
  });

  it("reports transfers interrupted by a dead connection as resumable", async () => {
    const timing = { timeoutMs: 200 };
    const { a, b, sink, chA, chB } = await ready({ a: timing, b: timing, channel: { bytesPerTick: 64 * 1024, tickMs: 1 } });
    b.on("offer", (o) => o.accept());
    const cancelledA = nextEvent(a, "cancelled");
    const cancelledB = nextEvent(b, "cancelled");
    b.once("progress", () => {
      chA.blackhole = chB.blackhole = true;
    });
    const err = await rejection(a.sendTransfer({ transferId: "t1", files: [memorySource("f1", randomBytes(4_000_000))] }));
    expect(err.code).toBe("timeout");
    expect(await cancelledA).toMatchObject({ transferId: "t1", direction: "send", interrupted: true });
    expect(await cancelledB).toMatchObject({ transferId: "t1", direction: "receive", interrupted: true });
    await waitUntil(() => sink.files.get("f1")?.state === "aborted", 5000, "sink abort");
    expect(sink.files.get("f1")!.abortReason).toBe("timeout"); // keep the partial file
  });

  it("reports a peer that closes mid-transfer", async () => {
    const { a, b, sink } = await ready({ channel: { bytesPerTick: 64 * 1024, tickMs: 1 } });
    b.on("offer", (o) => o.accept());
    const cancelledB = nextEvent(b, "cancelled");
    const closedB = nextEvent(b, "closed");
    b.once("progress", () => a.close());
    const err = await rejection(a.sendTransfer({ transferId: "t1", files: [memorySource("f1", randomBytes(4_000_000))] }));
    expect(err.code).toBe("closed");
    expect(await cancelledB).toMatchObject({ direction: "receive", byRemote: true, interrupted: true });
    expect(await closedB).toMatchObject({ reason: "remote" });
    expect(sink.files.get("f1")).toMatchObject({ state: "aborted", abortReason: "closed" });
    // Queued work fails fast once the session is closed.
    expect((await rejection(a.sendTransfer({ transferId: "t2", files: [] }))).code).toBe("closed");
  });
});

describe("protocol violations", () => {
  it("close the session on a binary frame outside a file", async () => {
    const { a, b, chA } = await ready();
    const closedB = nextEvent(b, "closed");
    const errorA = nextEvent(a, "error");
    chA.send(new Uint8Array(10));
    expect(await closedB).toMatchObject({ reason: "protocol" });
    expect(await errorA).toMatchObject({ code: "protocol", remote: true });
  });

  it("never surface hostile offers", async () => {
    const { b, chA } = await ready();
    const offers = collect(b, "offer");
    const closed = nextEvent(b, "closed");
    chA.send(JSON.stringify({ t: "offer", transferId: "t1", files: [{ id: "f1", name: "../../.bashrc", size: 1, mime: "" }] }));
    expect((await closed).message).toMatch(/file name/);
    expect(offers).toHaveLength(0);
  });

  it("reject bytes beyond the declared size", async () => {
    const { b, chA, chB, sink } = await ready();
    b.on("offer", (o) => o.accept());
    const closed = nextEvent(b, "closed");
    chA.send(JSON.stringify({ t: "offer", transferId: "t1", files: [{ id: "f1", name: "a.txt", size: 4, mime: "text/plain" }] }));
    await waitUntil(() => chB.sentControl("answer").length === 1);
    chA.send(JSON.stringify({ t: "file", id: "f1", offset: 0 }));
    chA.send(new Uint8Array(3));
    chA.send(new Uint8Array(3));
    expect(await closed).toMatchObject({ reason: "protocol" });
    expect(sink.files.get("f1")).toMatchObject({ state: "aborted", abortReason: "overrun" });
  });

  it("reject a second offer while one is pending", async () => {
    const { b, chA } = await ready();
    b.on("offer", () => {});
    const closed = nextEvent(b, "closed");
    const offer = { t: "offer", files: [{ id: "f1", name: "a", size: 1, mime: "" }] };
    chA.send(JSON.stringify({ ...offer, transferId: "t1" }));
    chA.send(JSON.stringify({ ...offer, transferId: "t2" }));
    expect((await closed).message).toMatch(/another incoming transfer/);
  });
});
