// @vitest-environment node
// The browser platform end to end: the real web.ts, rtc library and storage
// wrapper over fake signaling, fake RTCPeerConnections and fake IndexedDB.
import { afterEach, describe, expect, it } from "vitest";
import { FakePeerConnection, FakeSignalServer, memorySource, sleep, waitUntil, type MemorySource } from "../lib/rtc/test-fakes";
import type { IncomingOffer, PeerSession } from "../lib/rtc/session";
import { newProfile, setSettings, startBrowser, startRemote, type Browser, type Remote } from "./web-test-env";
import type { EngineEvent, IncomingRequest, OutgoingItem, Settings, TransferSummary } from "./types";

const cleanup: (() => void)[] = [];
afterEach(() => {
  for (const fn of cleanup.splice(0)) fn();
  FakePeerConnection.all.clear();
  FakePeerConnection.stats = null;
});

const bytes = (n: number, seed = 1) => Uint8Array.from({ length: n }, (_, i) => (i * seed + seed) & 0xff);
const ACCEPT_ALL = { accept: null, decline: false, trust: false, saveDir: null };

async function setup(names: string[] = ["Alice"], settings: Partial<Settings> = {}, onOffer?: (offer: IncomingOffer, session: PeerSession) => void) {
  const server = new FakeSignalServer();
  const browser = await startBrowser(server);
  if (Object.keys(settings).length) await setSettings(browser, settings);
  const remotes: Remote[] = [];
  for (const name of names) {
    const r = await startRemote(server, name, onOffer);
    remotes.push(r);
    cleanup.push(() => r.close());
  }
  // The browser sees them too (needed to send to them).
  for (const r of remotes) await waitUntil(() => of(browser, "deviceUpdated").some((e) => e.device.id === r.key && e.device.online), 5000, "browser sees peer");
  return { server, browser, remotes };
}

const of = <T extends EngineEvent["type"]>(b: Browser, type: T) => b.events.filter((e): e is Extract<EngineEvent, { type: T }> => e.type === type);
const requests = (b: Browser): IncomingRequest[] => of(b, "incomingRequest").map((e) => e.request);
const fileRequests = (b: Browser) => requests(b).filter((r) => r.text === null);
const messages = (b: Browser) => requests(b).filter((r) => r.text !== null);
function latest(b: Browser): Map<string, TransferSummary> {
  const out = new Map<string, TransferSummary>();
  for (const e of b.events) {
    if (e.type === "transferUpdated") out.set(e.transfer.id, e.transfer);
    if (e.type === "transferRemoved") out.delete(e.id);
  }
  return out;
}
const transfersWith = (b: Browser, peerKey: string, direction = "receive") => [...latest(b).values()].filter((t) => t.peer.id === peerKey && t.direction === direction);
const statesOf = (b: Browser, id: string) => of(b, "transferUpdated").filter((e) => e.transfer.id === id).map((e) => e.transfer.state);

async function waitRequest(b: Browser, n = 1): Promise<IncomingRequest> {
  await waitUntil(() => fileRequests(b).length >= n, 5000, "incoming request");
  return fileRequests(b)[n - 1]!;
}

async function contentOf(b: Browser, path: string) {
  const file = await b.sink.readStored(path, "f", "");
  return new Uint8Array(await file.arrayBuffer());
}

/** A source whose reads wait until `release()` (an upload that stalls). */
function stalled(id: string, data: Uint8Array, name: string): { source: MemorySource; release: () => void } {
  const inner = memorySource(id, data, name);
  let release!: () => void;
  const gate = new Promise<void>((r) => (release = r));
  return { source: { ...inner, slice: async (s, e) => (await gate, inner.slice(s, e)) }, release };
}

const sendFiles = (b: Browser, to: Remote, items: OutgoingItem[]) => b.platform.send([{ kind: "device", id: to.key }], items);
const fileItem = (name: string, data: Uint8Array): OutgoingItem => ({ kind: "file", file: new File([data as Uint8Array<ArrayBuffer>], name), name, size: data.byteLength });

describe("browser platform: transfers are kept apart by peer, direction and wire id", () => {
  it("receives a file into the Inbox", async () => {
    const { browser, remotes } = await setup();
    const [alice] = remotes;
    const session = await alice!.connect(browser.key);
    const sent = session.sendTransfer({ transferId: "t1", files: [memorySource("f", bytes(70_000), "a.bin")] });
    const req = await waitRequest(browser);
    expect(await browser.platform.respond(req.id, ACCEPT_ALL)).toBe(true);
    expect((await sent).completed).toEqual(["f"]);
    await waitUntil(() => transfersWith(browser, alice!.key)[0]?.state === "completed", 5000, "completed");
    const inbox = await browser.platform.inbox(10);
    expect(inbox).toHaveLength(1);
    expect(await contentOf(browser, inbox[0]!.path!)).toEqual(bytes(70_000));
  });

  it("two peers using the same transfer id get separate transfers, progress, files and history", async () => {
    const { browser, remotes } = await setup(["Alice", "Carol"]);
    const [alice, carol] = remotes as [Remote, Remote];
    const slow = stalled("f", bytes(50_000, 3), "alice.bin");
    const sa = await alice.connect(browser.key);
    const sc = await carol.connect(browser.key);
    const fromAlice = sa.sendTransfer({ transferId: "same", files: [slow.source] });
    fromAlice.catch(() => {});
    await browser.platform.respond((await waitRequest(browser, 1)).id, ACCEPT_ALL);
    const fromCarol = sc.sendTransfer({ transferId: "same", files: [memorySource("f", bytes(60_000, 5), "carol.bin")] });
    await browser.platform.respond((await waitRequest(browser, 2)).id, ACCEPT_ALL);
    expect((await fromCarol).completed).toEqual(["f"]);

    const [a] = transfersWith(browser, alice.key);
    const [c] = transfersWith(browser, carol.key);
    await waitUntil(() => latest(browser).get(c!.id)?.state === "completed", 5000, "Carol's completes");
    expect(a!.id).not.toBe(c!.id);
    expect([a!.id, c!.id]).not.toContain("same");
    // Carol's progress and completion never touched Alice's.
    expect(latest(browser).get(a!.id)!.state).toBe("transferring");
    expect(latest(browser).get(a!.id)!.bytesDone).toBe(0);
    expect((await browser.platform.transferFiles(c!.id))[0]).toMatchObject({ name: "carol.bin", state: "done" });
    expect((await browser.platform.transferFiles(a!.id))[0]).toMatchObject({ name: "alice.bin", state: "transferring" });

    // Alice cancels: only her transfer ends.
    sa.cancel("changed my mind", "same");
    await waitUntil(() => latest(browser).get(a!.id)?.state === "cancelled", 5000, "Alice's cancelled");
    expect(latest(browser).get(c!.id)!.state).toBe("completed");
    const inbox = await browser.platform.inbox(10);
    expect(inbox.map((e) => [e.peerId, e.name])).toEqual([[carol.key, "carol.bin"]]);
    expect(await contentOf(browser, inbox[0]!.path!)).toEqual(bytes(60_000, 5));
    slow.release();
  });

  it("a send and a receive with the same wire id stay separate", async () => {
    let offerToAlice: { offer: IncomingOffer; session: PeerSession } | null = null;
    const { browser, remotes } = await setup(["Alice"], {}, (offer, session) => (offerToAlice = { offer, session }));
    const [alice] = remotes as [Remote];
    const [sendId] = await sendFiles(browser, alice, [fileItem("out.bin", bytes(30_000, 7))]);
    await waitUntil(() => offerToAlice !== null, 5000, "Alice gets the offer");
    const { offer, session } = offerToAlice!;
    // Alice answers with a transfer of her own under the very same id.
    const back = session.sendTransfer({ transferId: offer.transferId, files: [memorySource("x", bytes(20_000, 9), "in.bin")] });
    const req = await waitRequest(browser);
    await browser.platform.respond(req.id, ACCEPT_ALL);
    offer.accept();
    expect((await back).completed).toEqual(["x"]);
    await waitUntil(() => latest(browser).get(sendId!)?.state === "completed", 5000, "send completes");
    const [received] = transfersWith(browser, alice.key, "receive");
    await waitUntil(() => latest(browser).get(received!.id)?.state === "completed", 5000, "receive completes");
    expect(received!.id).not.toBe(sendId);
    expect(latest(browser).get(sendId!)!.title).toBe("out.bin");
    expect(received!.title).toBe("in.bin");
    expect(alice.sink.data("0")).toEqual(bytes(30_000, 7));
  });

  it("a resume on a new session takes over; the old session can't touch it any more", async () => {
    const { browser, remotes } = await setup();
    const [alice] = remotes as [Remote];
    const s1 = await alice.connect(browser.key);
    const slow = stalled("f", bytes(40_000, 11), "doc.bin");
    const first = s1.sendTransfer({ transferId: "r1", files: [slow.source], text: "here you go" });
    first.catch(() => {});
    await browser.platform.respond((await waitRequest(browser)).id, ACCEPT_ALL);
    const [t] = transfersWith(browser, alice.key);
    await waitUntil(() => messages(browser).length === 1, 5000, "message shown once accepted");

    // The sender comes back on a second connection while the first is still open.
    const s2 = await alice.connect(browser.key);
    const again = await s2.sendTransfer({ transferId: "r1", files: [memorySource("f", bytes(40_000, 11), "doc.bin")], text: "here you go" });
    expect(again.completed).toEqual(["f"]);
    await waitUntil(() => latest(browser).get(t!.id)?.state === "completed", 5000, "resumed transfer completes");
    await expect(first).rejects.toBeTruthy();
    expect(s1.state).toBe("closed");
    slow.release();
    await sleep(20);

    // No second prompt, no second card, no duplicate message; the closed session's report was ignored.
    expect(fileRequests(browser)).toHaveLength(1);
    expect(transfersWith(browser, alice.key)).toHaveLength(1);
    expect(messages(browser)).toHaveLength(1);
    expect(statesOf(browser, t!.id)).not.toContain("cancelled");
    expect(statesOf(browser, t!.id)).not.toContain("failed");
    expect(latest(browser).get(t!.id)!.state).toBe("completed");
    const inbox = await browser.platform.inbox(10);
    expect(inbox.filter((e) => e.kind === "file")).toHaveLength(1);
  });

  it("a transfer cancelled here sends the wire id to the peer and finishes only that transfer", async () => {
    const { browser, remotes } = await setup();
    const [alice] = remotes as [Remote];
    const s = await alice.connect(browser.key);
    const slow = stalled("f", bytes(10_000), "a.bin");
    const sent = s.sendTransfer({ transferId: "c1", files: [slow.source] });
    await browser.platform.respond((await waitRequest(browser)).id, ACCEPT_ALL);
    const [t] = transfersWith(browser, alice.key);
    expect(await browser.platform.cancel(t!.id)).toBe(true);
    await expect(sent).rejects.toMatchObject({ code: "cancelled" });
    expect(latest(browser).get(t!.id)!.state).toBe("cancelled");
    expect(await browser.platform.dismiss(t!.id)).toBe(true);
    expect(latest(browser).has(t!.id)).toBe(false);
    slow.release();
  });
});

describe("browser platform: messages and history settings", () => {
  const combos: [boolean, boolean][] = [
    [true, true],
    [true, false],
    [false, true],
    [false, false],
  ];
  for (const [historyEnabled, keepMessageText] of combos) {
    it(`history ${historyEnabled ? "on" : "off"}, message text ${keepMessageText ? "kept" : "not kept"}`, async () => {
      const { browser, remotes } = await setup(["Alice"], { historyEnabled, keepMessageText });
      const [alice] = remotes as [Remote];
      const s = await alice.connect(browser.key);
      await s.sendTransfer({ transferId: "m1", files: [], text: "secret plans\nline two" });
      const mixed = s.sendTransfer({ transferId: "m2", files: [memorySource("f", bytes(5000), "pic.bin")], text: "with a file" });
      await browser.platform.respond((await waitRequest(browser)).id, ACCEPT_ALL);
      await mixed;
      await waitUntil(() => messages(browser).length === 2, 5000, "both messages shown");
      expect(messages(browser).map((m) => m.text)).toEqual(["secret plans\nline two", "with a file"]);
      await sleep(30);

      const history = await browser.platform.history(50);
      const texts = history.filter((h) => h.kind === "text");
      const files = history.filter((h) => h.kind === "file");
      if (!historyEnabled) {
        expect(history).toEqual([]);
      } else {
        expect(texts).toHaveLength(2);
        expect(files).toHaveLength(1);
        for (const h of texts) {
          if (keepMessageText) expect(h.text).toBeTruthy();
          else expect(h).toMatchObject({ text: null, name: "Message" });
        }
      }
      // Received files stay reachable whatever the settings.
      const inbox = await browser.platform.inbox(50);
      const inboxFiles = inbox.filter((h) => h.kind === "file");
      expect(inboxFiles).toHaveLength(1);
      expect(await contentOf(browser, inboxFiles[0]!.path!)).toEqual(bytes(5000));
      expect(inbox.filter((h) => h.kind === "text")).toHaveLength(historyEnabled ? 2 : 0);
      // Nothing optional was persisted: check the database itself, after a reload.
      const stored = await browser.idb.store<Record<string, unknown>>("history").all();
      expect(stored.some((e) => e.text === "secret plans\nline two")).toBe(historyEnabled && keepMessageText);
      if (!historyEnabled) expect(stored.every((e) => e.kind === "file" && e.inbox === true && e.activity === false)).toBe(true);
    });
  }

  it("text that comes with files: not shown when declined, shown on partial acceptance", async () => {
    const { browser, remotes } = await setup();
    const [alice] = remotes as [Remote];
    const s = await alice.connect(browser.key);
    const declined = s.sendTransfer({ transferId: "d1", files: [memorySource("f", bytes(100), "a.bin")], text: "declined text" });
    await browser.platform.respond((await waitRequest(browser, 1)).id, { ...ACCEPT_ALL, decline: true });
    expect((await declined).declined).toBe(true);
    expect(messages(browser)).toHaveLength(0);

    const partial = s.sendTransfer({
      transferId: "p1",
      files: [memorySource("a", bytes(100), "a.bin"), memorySource("b", bytes(200), "b.bin")],
      text: "partial text",
    });
    await browser.platform.respond((await waitRequest(browser, 2)).id, { ...ACCEPT_ALL, accept: ["b"] });
    const outcome = await partial;
    expect(outcome).toMatchObject({ completed: ["b"], skipped: ["a"] });
    expect(messages(browser).map((m) => m.text)).toEqual(["partial text"]);

    // Accepting none of the files is a decline: the text isn't shown.
    const none = s.sendTransfer({ transferId: "n1", files: [memorySource("c", bytes(100), "c.bin")], text: "nothing accepted" });
    await browser.platform.respond((await waitRequest(browser, 3)).id, { ...ACCEPT_ALL, accept: [] });
    expect((await none).declined).toBe(true);
    expect(messages(browser).map((m) => m.text)).toEqual(["partial text"]);
  });

  it("a text-only transfer sent again is shown once; a flood of messages is declined past the limit", async () => {
    const { browser, remotes } = await setup();
    const [alice] = remotes as [Remote];
    const s1 = await alice.connect(browser.key);
    await s1.sendTransfer({ transferId: "same-msg", files: [], text: "hello" });
    const s2 = await alice.connect(browser.key);
    await s2.sendTransfer({ transferId: "same-msg", files: [], text: "hello" });
    await sleep(20);
    expect(messages(browser)).toHaveLength(1);

    const outcomes = [];
    for (let i = 0; i < 12; i++) outcomes.push(await s2.sendTransfer({ transferId: `m${i}`, files: [], text: `n${i}` }));
    expect(outcomes.filter((o) => o.declined)).toHaveLength(3); // 10 per peer per minute, one already used
    expect(messages(browser)).toHaveLength(10);
  });

  it("history off: received files stay listed and readable after a reload; nothing else is kept", async () => {
    const { server, browser, remotes } = await setup(["Alice"], { historyEnabled: false });
    const [alice] = remotes as [Remote];
    const s = await alice.connect(browser.key);
    const sent = s.sendTransfer({ transferId: "h1", files: [memorySource("f", bytes(9000, 13), "kept.bin")], text: "note" });
    await browser.platform.respond((await waitRequest(browser)).id, ACCEPT_ALL);
    await sent;
    await sleep(30);
    const reloaded = await startBrowser(server, browser.profile);
    expect(await reloaded.platform.history(50)).toEqual([]);
    const inbox = await reloaded.platform.inbox(50);
    expect(inbox.map((e) => e.name)).toEqual(["kept.bin"]);
    expect(await contentOf(reloaded, inbox[0]!.path!)).toEqual(bytes(9000, 13));
  });
});

describe("browser platform: sending", () => {
  it("sends files and text in one offer and records history only while it is on", async () => {
    const { browser, remotes } = await setup();
    const [alice] = remotes as [Remote];
    const [id] = await sendFiles(browser, alice, [fileItem("a.txt", bytes(1234)), { kind: "text", text: "see attached", name: "Text" }]);
    await waitUntil(() => latest(browser).get(id!)?.state === "completed", 5000, "sent");
    expect(alice.offers).toHaveLength(1);
    expect(alice.offers[0]!.text).toBe("see attached");
    expect(alice.offers[0]!.files.map((f) => f.name)).toEqual(["a.txt"]);
    await sleep(20);
    expect((await browser.platform.history(10)).map((h) => [h.direction, h.name])).toEqual([["send", "a.txt"]]);

    await setSettings(browser, { historyEnabled: false });
    const [id2] = await sendFiles(browser, alice, [fileItem("b.txt", bytes(10))]);
    await waitUntil(() => latest(browser).get(id2!)?.state === "completed", 5000, "sent");
    await sleep(20);
    expect(await browser.platform.history(10)).toHaveLength(1);
  });

  it("keeps at most 50 finished transfers in memory", async () => {
    const { browser, remotes } = await setup(["Alice"], {}, (offer) => offer.decline());
    const [alice] = remotes as [Remote];
    const ids: string[] = [];
    for (let i = 0; i < 55; i++) ids.push(...(await sendFiles(browser, alice, [fileItem(`f${i}`, bytes(10))])));
    await waitUntil(() => ids.every((id) => !latest(browser).has(id) || latest(browser).get(id)!.state === "declined"), 10_000, "all declined");
    expect(latest(browser).size).toBe(50);
    expect(of(browser, "transferRemoved").map((e) => e.id)).toEqual(ids.slice(0, 5));
  });
});

describe("browser platform: pause and try again", () => {
  it("transfers can't be paused or resumed in either direction", async () => {
    const { browser, remotes } = await setup();
    const [alice] = remotes as [Remote];
    const s = await alice.connect(browser.key);
    const slow = stalled("f", bytes(10_000), "a.bin");
    const sent = s.sendTransfer({ transferId: "p1", files: [slow.source] });
    sent.catch(() => {});
    await browser.platform.respond((await waitRequest(browser)).id, ACCEPT_ALL);
    const [incoming] = transfersWith(browser, alice.key);
    const [outId] = await sendFiles(browser, alice, [fileItem("b.bin", bytes(10))]);
    const outgoing = latest(browser).get(outId!)!;
    for (const t of [incoming!, outgoing]) {
      // Reconnecting after a lost connection is automatic; pausing isn't offered.
      expect(t.resumable).toBe(true);
      expect(t.canPause).toBe(false);
      expect(t.canResume).toBe(false);
      expect(await browser.platform.pause(t.id)).toBe(false);
    }
    expect(await browser.platform.resume(incoming!.id)).toBe(false);
    slow.release();
  });

  it("trying a failed send again continues where it stopped: received bytes aren't sent again, history lists each file once", async () => {
    const server = new FakeSignalServer();
    // No automatic reconnects: the send fails as soon as the connection drops and waits for "Try again".
    const browser = await startBrowser(server, newProfile(), undefined, { retryDelaysMs: [] });
    const small = bytes(5_000, 3);
    const big = bytes(300_000, 7);
    let offers = 0;
    let first: PeerSession | null = null;
    /** Bytes the receiver wrote (and acknowledged) in the current attempt. */
    let written = 0;
    const alice: Remote = await startRemote(server, "Alice", (offer, session) => {
      offers++;
      if (offers === 1) {
        first = session;
        offer.accept();
        return;
      }
      // The same transfer offered again: continue from what is stored.
      offer.accept(undefined, Object.fromEntries(offer.files.map((f) => [f.id, alice.sink.stored.get(f.id)?.byteLength ?? 0])));
    });
    cleanup.push(() => alice.close());
    await waitUntil(() => of(browser, "deviceUpdated").some((e) => e.device.id === alice.key && e.device.online), 5000, "browser sees Alice");
    alice.sink.onWrite = (meta, chunk) => {
      written += chunk.byteLength;
      // The connection drops partway through the second file.
      if (offers === 1 && meta.id === "1" && written > 100_000) first?.close("network lost");
    };

    const [id] = await sendFiles(browser, alice, [fileItem("small.txt", small), fileItem("big.bin", big)]);
    await waitUntil(() => latest(browser).get(id!)?.state === "failed", 5000, "send fails");
    expect(latest(browser).get(id!)!.error?.code).toBe("connection_lost");
    await sleep(20);
    expect((await browser.platform.history(10)).map((h) => h.name)).toEqual(["small.txt"]);

    // What the receiver kept from the first attempt.
    for (const fid of ["0", "1"]) alice.sink.stored.set(fid, alice.sink.data(fid));
    const kept = alice.sink.stored.get("1")!.byteLength;
    expect(alice.sink.stored.get("0")).toEqual(small);
    expect(kept).toBeGreaterThan(0);
    expect(kept).toBeLessThan(big.byteLength);

    written = 0;
    // A second press while the first is starting is ignored.
    expect(await Promise.all([browser.platform.resume(id!), browser.platform.resume(id!)])).toEqual([true, false]);
    await waitUntil(() => latest(browser).get(id!)?.state === "completed", 5000, "completes after trying again");

    expect(offers).toBe(2);
    expect(alice.sink.files.get("1")!.offset).toBe(kept);
    // Only the rest of the unfinished file crossed the wire.
    expect(written).toBe(big.byteLength - kept);
    expect(alice.sink.data("1")).toEqual(big);
    expect(latest(browser).get(id!)!.bytesDone).toBe(small.byteLength + big.byteLength);
    // One card, and history lists each file once.
    expect(transfersWith(browser, alice.key, "send")).toHaveLength(1);
    await sleep(20);
    expect((await browser.platform.history(10)).map((h) => h.name).sort()).toEqual(["big.bin", "small.txt"]);
    // Nothing to try again once it is done.
    expect(await browser.platform.resume(id!)).toBe(false);
  }, 20_000);

  it("try again is refused for a declined send and while one is reconnecting", async () => {
    const { browser, remotes } = await setup(["Alice"], {}, (offer) => offer.decline());
    const [alice] = remotes as [Remote];
    const [id] = await sendFiles(browser, alice, [fileItem("a.bin", bytes(10))]);
    await waitUntil(() => latest(browser).get(id!)?.state === "declined", 5000, "declined");
    expect(await browser.platform.resume(id!)).toBe(false);
    expect(alice.offers).toHaveLength(1);
  });
});

describe("browser platform: connection route", () => {
  const stats = (local: string, remote: string) =>
    new Map<string, Record<string, unknown>>([
      ["T", { type: "transport", selectedCandidatePairId: "P" }],
      ["P", { type: "candidate-pair", localCandidateId: "L", remoteCandidateId: "R", nominated: true, state: "succeeded" }],
      ["L", { type: "local-candidate", candidateType: local }],
      ["R", { type: "remote-candidate", candidateType: remote }],
    ]);

  for (const [label, report, expected] of [
    ["relayed", stats("relay", "srflx"), true],
    ["direct", stats("host", "host"), false],
    ["unknown", null, null],
  ] as const) {
    it(`shows a ${label} route`, async () => {
      FakePeerConnection.stats = report;
      const { browser, remotes } = await setup();
      const [alice] = remotes as [Remote];
      const [id] = await sendFiles(browser, alice, [fileItem("a", bytes(10))]);
      await waitUntil(() => latest(browser).get(id!)?.state === "completed", 5000, "sent");
      await sleep(10);
      expect(latest(browser).get(id!)!.connection).toMatchObject({ transport: "webrtc", relayed: expected });
    });
  }
});

describe("browser platform: signaling status", () => {
  it("reports connecting, open, closed and a new server, with the identity key", async () => {
    const server = new FakeSignalServer();
    const browser = await startBrowser(server);
    const states = () => of(browser, "signalingStatus").map((e) => e.status.state);
    expect(states().slice(0, 2)).toEqual(["connecting", "open"]);
    expect(states()).toContain("open");
    const open = of(browser, "signalingStatus").at(-1)!.status;
    expect(open).toMatchObject({ url: "wss://ferry.test/v1/ws", state: "open", error: null, identityKey: browser.key });

    // The server drops it: closed (with an error), then it reconnects.
    const id = [...server.conns.values()].find((c) => c.info.ext?.key === browser.key)!.info.id;
    server.kick(id);
    await waitUntil(() => states().at(-1) === "closed", 5000, "closed");
    expect(of(browser, "signalingStatus").at(-1)!.status.error).toMatch(/Can't reach the signaling server/);
    await waitUntil(() => states().at(-1) === "open", 5000, "reopened");

    // Another server: the status names it, and the replaced client no longer reports.
    const before = of(browser, "signalingStatus").length;
    await setSettings(browser, { signalingUrl: "wss://other.test/v1/ws" });
    await waitUntil(() => states().at(-1) === "open", 5000, "open on the new server");
    const after = of(browser, "signalingStatus").slice(before).map((e) => e.status);
    expect(after.every((s) => s.url === "wss://other.test/v1/ws")).toBe(true);
    expect(await browser.platform.signalingStatus()).toMatchObject({ url: "wss://other.test/v1/ws", state: "open" });
    await sleep(50);
    expect(states().at(-1)).toBe("open");
  });
});

describe("browser platform: clearing history and received files", () => {
  async function received(names = ["one.bin", "two.bin"]) {
    const ctx = await setup();
    const [alice] = ctx.remotes as [Remote];
    const s = await alice.connect(ctx.browser.key);
    await s.sendTransfer({ transferId: "msg", files: [], text: "hi" });
    for (const [i, name] of names.entries()) {
      const sent = s.sendTransfer({ transferId: `f${i}`, files: [memorySource("f", bytes(1000, i + 1), name)] });
      await ctx.browser.platform.respond((await waitRequest(ctx.browser, i + 1)).id, ACCEPT_ALL);
      await sent;
    }
    await sleep(30);
    return ctx;
  }

  it("clearing history keeps received files in the Inbox, also after a reload", async () => {
    const { server, browser } = await received();
    expect(await browser.platform.history(10)).toHaveLength(3);
    await browser.platform.clearHistory();
    expect(await browser.platform.history(10)).toEqual([]);
    const inbox = await browser.platform.inbox(10);
    expect(inbox.map((e) => e.name).sort()).toEqual(["one.bin", "two.bin"]);
    const reloaded = await startBrowser(server, browser.profile);
    await sleep(30); // startup cleanup ran
    const after = await reloaded.platform.inbox(10);
    expect(after.map((e) => e.name).sort()).toEqual(["one.bin", "two.bin"]);
    for (const e of after) expect((await contentOf(reloaded, e.path!)).length).toBe(1000);
    expect(await reloaded.platform.history(10)).toEqual([]);
  });

  it("deleting received files removes them and keeps the history entries without the file", async () => {
    const { browser } = await received();
    const paths = (await browser.platform.inbox(10)).filter((e) => e.path).map((e) => e.path!);
    expect(await browser.platform.clearReceivedFiles()).toEqual({ deleted: 2, failed: 0 });
    for (const p of paths) await expect(contentOf(browser, p)).rejects.toThrow();
    expect((await browser.platform.inbox(10)).filter((e) => e.kind === "file")).toEqual([]);
    const history = await browser.platform.history(10);
    expect(history.filter((e) => e.kind === "file").map((e) => e.path)).toEqual([null, null]);
  });

  it("deleting one Inbox entry deletes its file", async () => {
    const { browser } = await received(["only.bin"]);
    const [entry] = (await browser.platform.inbox(10)).filter((e) => e.kind === "file");
    expect(await browser.platform.deleteHistory(entry!.id)).toBe(true);
    await expect(contentOf(browser, entry!.path!)).rejects.toThrow();
    expect((await browser.platform.inbox(10)).filter((e) => e.kind === "file")).toEqual([]);
  });

  it("pages history by id while new entries arrive", async () => {
    const { browser, remotes } = await received(["a.bin", "b.bin", "c.bin"]);
    const first = await browser.platform.history(2);
    const s = await remotes[0]!.connect(browser.key);
    for (let i = 0; i < 3; i++) await s.sendTransfer({ transferId: `late${i}`, files: [], text: `late ${i}` });
    await sleep(30);
    const second = await browser.platform.history(10, first.at(-1)!.id);
    const ids = [...first, ...second].map((e) => e.id);
    expect(new Set(ids).size).toBe(ids.length);
    expect(ids).toHaveLength(4); // the 4 entries that existed, none skipped, none repeated
    expect([...ids].sort((a, b) => b - a)).toEqual(ids);
  });
});

describe("browser platform: startup cleanup", () => {
  it("deletes expired fallback files, keeps listed files, retained and recent partial files", async () => {
    const server = new FakeSignalServer();
    const old = Date.now() - 24 * 3600_000;
    const ids = { orphan: "a".repeat(32), legacy: "b".repeat(32), recent: "c".repeat(32), listed: "d".repeat(32) };
    const browser = await startBrowser(server, undefined, async ({ idb }) => {
      const blobs = idb.store<unknown>("blobs");
      await blobs.put({ blob: new Blob(["x"]), partial: true, at: old }, ids.orphan);
      await blobs.put(new Blob(["legacy"]), ids.legacy);
      await blobs.put({ blob: new Blob(["y"]), partial: true, at: Date.now() }, ids.recent);
      await blobs.put({ blob: new Blob(["z"]), partial: false, at: old }, ids.listed);
      await idb.store<unknown>("history").put({
        transferId: "t",
        direction: "receive",
        peerId: "p",
        peerAlias: "P",
        peerKind: "web",
        kind: "file",
        name: "z.txt",
        size: 1,
        mime: "text/plain",
        path: `browser:${ids.listed}`,
        text: null,
        timestampMs: old,
        status: "completed",
        verified: true,
      });
    });
    const blobs = browser.idb.store<unknown>("blobs");
    await waitUntil(() => true);
    for (let i = 0; i < 50 && (await blobs.get(ids.orphan)); i++) await sleep(10);
    expect(await blobs.get(ids.orphan)).toBeUndefined();
    expect(await blobs.get(ids.legacy)).toBeUndefined();
    expect(await blobs.get(ids.recent)).toBeDefined();
    expect(await blobs.get(ids.listed)).toBeDefined();
    expect((await browser.platform.inbox(10)).map((e) => e.name)).toEqual(["z.txt"]);
  });
});
