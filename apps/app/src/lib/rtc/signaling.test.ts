// @vitest-environment node
import { afterEach, describe, expect, it } from "vitest";
import { b64urlDecode, b64urlEncode, fromUtf8, randomBytes } from "./bytes";
import { RtcError } from "./protocol";
import { decodeSdp, encodeSdp, isValidRoomId, SignalingClient, type SignalingClientOptions } from "./signaling";
import { collect, FakeSignalServer, FakeWebSocket, nextEvent, sleep, waitUntil } from "./test-fakes";

const KEY = b64urlEncode(new Uint8Array(32).fill(5));
const ROOM = `r:${"A".repeat(22)}`;
const SDP = `v=0\r\no=- 4611731400430051336 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n${"a=candidate:1 1 udp 2122260223 192.0.2.1 50000 typ host\r\n".repeat(12)}`;

let clients: SignalingClient[] = [];

afterEach(() => {
  for (const c of clients) c.close();
  clients = [];
  FakeWebSocket.instances.length = 0;
});

function client(options: Partial<SignalingClientOptions> = {}): SignalingClient {
  const c = new SignalingClient({
    url: "wss://signal.test/v1/ws",
    info: { alias: "Ferry Web", deviceType: "web", deviceModel: "Firefox", token: "tok-1", publicKey: KEY },
    WebSocket: FakeWebSocket,
    initialBackoffMs: 5,
    maxBackoffMs: 40,
    random: () => 0,
    ...options,
  });
  clients.push(c);
  return c;
}

const peer = (id: string, extra: Record<string, unknown> = {}) => ({ id, alias: `Peer ${id}`, version: "2.2", token: `t-${id}`, ...extra });

/** Connects `c` to a scripted socket and completes the HELLO. */
async function greeted(c: SignalingClient, hello: Record<string, unknown> = {}): Promise<FakeWebSocket> {
  c.connect();
  const ws = FakeWebSocket.last;
  ws.serverOpen();
  const ev = nextEvent(c, "hello");
  ws.serverSend({ type: "HELLO", client: peer("me"), peers: [], server: { v: 1, caps: ["rooms", "trickle", "ferry-dc"] }, ...hello });
  await ev;
  return ws;
}

describe("signaling client", () => {
  it("connects with the client info in ?d=", () => {
    const c = client({ info: { alias: "😀".repeat(70), deviceType: "web", token: "tok", publicKey: KEY, nearby: true } });
    c.connect();
    expect(c.state).toBe("connecting");
    const url = new URL(FakeWebSocket.last.url);
    expect(`${url.protocol}//${url.host}${url.pathname}`).toBe("wss://signal.test/v1/ws");
    const info = JSON.parse(fromUtf8(b64urlDecode(url.searchParams.get("d")!))) as Record<string, unknown>;
    expect(info).toEqual({
      alias: "😀".repeat(64), // clipped to the server's 64-character limit
      version: "2.2",
      deviceType: "WEB",
      token: "tok",
      ext: { v: 1, caps: ["rooms", "trickle", "ferry-dc"], key: KEY, nearby: true },
    });
    expect(Object.keys(info)).toEqual(["alias", "version", "deviceType", "token", "ext"]);
  });

  it("greets, lists peers and joins rooms only after HELLO", async () => {
    const c = client();
    const states = collect(c, "state");
    c.joinRoom(ROOM);
    c.connect();
    const ws = FakeWebSocket.last;
    ws.serverOpen();
    await sleep(5);
    expect(ws.sentJson()).toEqual([]);
    const hello = nextEvent(c, "hello");
    ws.serverSend({
      type: "HELLO",
      client: peer("me"),
      peers: [peer("p1", { deviceType: "DESKTOP", ext: { v: 1, caps: ["ferry-dc"], key: KEY } }), { junk: true }],
      server: { v: 1, caps: ["rooms", 5] },
    });
    const h = await hello;
    expect(h.client.id).toBe("me");
    expect(h.peers.map((p) => p.id)).toEqual(["p1"]);
    expect(h.peers[0]!.ext).toEqual({ v: 1, caps: ["ferry-dc"], key: KEY });
    expect(h.server).toEqual({ v: 1, caps: ["rooms"] });
    await waitUntil(() => ws.sentJson().length === 1);
    expect(ws.sentJson()).toEqual([{ type: "ROOM_JOIN", room: ROOM }]);
    expect(states).toEqual(["connecting", "open"]);
    expect(c.client?.id).toBe("me");
    expect(c.joinedRooms).toEqual([ROOM]);
    c.leaveRoom(ROOM);
    await waitUntil(() => ws.sentJson().length === 2);
    expect(ws.sentJson()[1]).toEqual({ type: "ROOM_LEAVE", room: ROOM });
  });

  it("emits peer and room events and ignores malformed frames", async () => {
    const c = client();
    const ws = await greeted(c);
    const joins = collect(c, "join");
    const updates = collect(c, "update");
    const lefts = collect(c, "left");
    const roomHellos = collect(c, "roomHello");
    const roomJoins = collect(c, "roomPeerJoined");
    const roomLefts = collect(c, "roomPeerLeft");
    ws.serverSend({ type: "JOIN", peer: peer("p2") });
    ws.serverSend({ type: "UPDATE", peer: peer("p2", { alias: "Renamed" }) });
    ws.serverSend({ type: "LEFT", peerId: "p2" });
    ws.serverSend({ type: "ROOM_HELLO", room: ROOM, peers: [peer("p3")] });
    ws.serverSend({ type: "ROOM_PEER_JOINED", room: ROOM, peer: peer("p4") });
    ws.serverSend({ type: "ROOM_PEER_LEFT", room: ROOM, peerId: "p4" });
    for (const junk of ["not json", '"bare string"', "[]", "{}", '{"type":5}']) ws.serverSend(junk);
    ws.serverSendRaw(new ArrayBuffer(4));
    ws.serverSend({ type: "JOIN" });
    ws.serverSend({ type: "JOIN", peer: { id: 5 } });
    ws.serverSend({ type: "LEFT", peerId: 5 });
    ws.serverSend({ type: "ROOM_HELLO", room: ROOM });
    ws.serverSend({ type: "SOMETHING_NEW" });
    ws.serverSend({ type: "PONG" });
    await sleep(10);
    expect(joins.map((e) => e.peer.id)).toEqual(["p2"]);
    expect(updates.map((e) => e.peer.alias)).toEqual(["Renamed"]);
    expect(lefts).toEqual([{ peerId: "p2" }]);
    expect(roomHellos).toEqual([{ room: ROOM, peers: [peer("p3")] }]);
    expect(roomJoins.map((e) => e.peer.id)).toEqual(["p4"]);
    expect(roomLefts).toEqual([{ room: ROOM, peerId: "p4" }]);
  });

  it("sends SDPs zlib-deflated and base64url-encoded, like LocalSend", async () => {
    const c = client();
    const ws = await greeted(c);
    await c.sendOffer("p1", "s1", SDP);
    const sent = ws.sentJson().at(-1)!;
    expect(sent).toMatchObject({ type: "OFFER", target: "p1", sessionId: "s1" });
    const encoded = sent.sdp as string;
    expect(encoded).toMatch(/^[A-Za-z0-9_-]+$/);
    const raw = b64urlDecode(encoded);
    expect(raw[0]).toBe(0x78); // RFC 1950 zlib header (pako.deflate), not raw deflate
    expect(((raw[0]! << 8) | raw[1]!) % 31).toBe(0);
    expect(encoded.length).toBeLessThan(SDP.length);
    expect(await decodeSdp(encoded)).toBe(SDP);
    await c.sendAnswer("p1", "s1", SDP);
    expect(ws.sentJson().at(-1)).toMatchObject({ type: "ANSWER", target: "p1", sessionId: "s1", sdp: encoded });
  });

  it("decodes inbound SDPs and reports corrupt ones", async () => {
    const c = client();
    const ws = await greeted(c);
    const encoded = await encodeSdp(SDP);
    const offer = nextEvent(c, "offer");
    ws.serverSend({ type: "OFFER", peer: peer("p1"), sessionId: "s9", sdp: encoded });
    expect(await offer).toEqual({ peer: peer("p1"), sessionId: "s9", sdp: SDP });
    // Standard alphabet with padding (btoa-style senders) is tolerated.
    const standard = encoded.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (encoded.length % 4)) % 4);
    expect(await decodeSdp(standard)).toBe(SDP);
    const answer = nextEvent(c, "answer");
    ws.serverSend({ type: "ANSWER", peer: peer("p1"), sessionId: "s9", sdp: standard });
    expect((await answer).sdp).toBe(SDP);
    const bad = nextEvent(c, "error");
    ws.serverSend({ type: "OFFER", peer: peer("p1"), sessionId: "s10", sdp: "!!!not-base64" });
    expect(await bad).toMatchObject({ code: "bad-sdp", sessionId: "s10" });
    // Decompression-bomb guard.
    await expect(decodeSdp(await encodeSdp("a".repeat(300 * 1024)))).rejects.toThrow(/too large/);
  });

  it("trickles ICE candidates as flat RTCIceCandidateInit objects", async () => {
    const c = client();
    const ws = await greeted(c);
    const cand = { candidate: "candidate:1 1 udp 2122260223 192.0.2.1 50000 typ host", sdpMid: "0", sdpMLineIndex: 0, usernameFragment: "abcd" };
    await c.sendIce("p1", "s1", { ...cand, extra: { nested: true } } as RTCIceCandidateInit);
    await c.sendIce("p1", "s1", null);
    const [withCandidate, end] = ws.sentJson().slice(-2);
    expect(withCandidate).toEqual({ type: "ICE", target: "p1", sessionId: "s1", candidate: cand }); // only the standard members
    expect(end).toEqual({ type: "ICE", target: "p1", sessionId: "s1", candidate: null });
    await expect(c.sendIce("p1", "s1", { candidate: "x".repeat(5000) })).rejects.toMatchObject({ code: "too-large" });

    const ices = collect(c, "ice");
    const line = "candidate:2 1 udp 1 192.0.2.9 9 typ host";
    ws.serverSend({ type: "ICE", peer: peer("p1"), sessionId: "s1", candidate: cand });
    ws.serverSend({ type: "ICE", peer: peer("p1"), sessionId: "s1", candidate: null });
    ws.serverSend({ type: "ICE", peer: peer("p1"), sessionId: "s1", candidate: line }); // bare SDP line: the only m-line
    ws.serverSend({ type: "ICE", peer: peer("p1"), sessionId: "s1", candidate: "not a candidate" });
    ws.serverSend({ type: "ICE", peer: peer("p1"), sessionId: "s1", candidate: 42 });
    ws.serverSend({ type: "ICE", peer: peer("p1"), sessionId: "s1", candidate: { sdpMid: "0" } });
    ws.serverSend({ type: "ICE", peer: peer("p1"), sessionId: "s1" });
    await sleep(10);
    expect(ices.map((e) => e.candidate)).toEqual([cand, null, { candidate: line, sdpMLineIndex: 0 }]);
  });

  it("fetches TURN credentials when the server offers them", async () => {
    const urls: string[] = [];
    const body = { iceServers: [{ urls: ["turn:turn.example:3478", "turns:turn.example:5349"], username: "1600:me", credential: "c2VjcmV0" }], ttl: 600 };
    let answer: { ok: boolean; status: number; body: unknown } = { ok: true, status: 200, body };
    const c = client({
      fetch: async (url) => {
        urls.push(url);
        return { ok: answer.ok, status: answer.status, json: async () => answer.body };
      },
    });
    await greeted(c, { server: { v: 1, caps: ["rooms", "turn"] } });
    expect(await c.fetchTurn()).toEqual(body);
    expect(urls).toEqual(["https://signal.test/v1/turn?peer=me"]);
    answer = { ok: false, status: 404, body: {} };
    await expect(c.fetchTurn()).rejects.toMatchObject({ code: "turn" });
    answer = { ok: true, status: 200, body: { iceServers: [{ urls: ["http://evil"], username: "u", credential: "c" }], ttl: 1 } };
    await expect(c.fetchTurn()).rejects.toMatchObject({ code: "turn" });

    const noTurn = client({ fetch: async () => Promise.reject(new Error("must not be called")) });
    await greeted(noTurn);
    expect(await noTurn.fetchTurn()).toBeNull();
  });

  it("checks the server's limits before sending", async () => {
    const c = client();
    const ws = await greeted(c);
    await expect(c.sendOffer("p1", "s".repeat(65), SDP)).rejects.toMatchObject({ code: "invalid" });
    await expect(c.sendCancel("p1", "")).rejects.toMatchObject({ code: "invalid" });
    await expect(c.sendIce("p1", "", null)).rejects.toMatchObject({ code: "invalid" });
    const noise = Array.from(randomBytes(60_000), (b) => String.fromCharCode(33 + (b % 90))).join("");
    await expect(c.sendOffer("p1", "s1", noise)).rejects.toMatchObject({ code: "too-large" });
    for (const room of ["lobby", "r:short", `r:${"a".repeat(65)}`, "r:has space here!", "c:12345", "c:1234567", "c:12345a"]) {
      expect(() => c.joinRoom(room), room).toThrow(RtcError);
      expect(isValidRoomId(room)).toBe(false);
    }
    for (const room of ["c:012345", `r:${"a".repeat(16)}`, `r:${"A-_9".repeat(16)}`]) expect(isValidRoomId(room), room).toBe(true);
    expect(ws.sentJson().filter((m) => m.type !== "ROOM_JOIN")).toEqual([]);
  });

  it("reports ERROR frames with their correlation", async () => {
    const c = client();
    const ws = await greeted(c);
    const errors = collect(c, "error");
    ws.serverSend({ type: "ERROR", code: 409, message: "room is full", room: "c:123456" });
    ws.serverSend({ type: "ERROR", code: 404, message: "unknown target", sessionId: "s1" });
    ws.serverSend({ type: "ERROR", code: 413, message: "x".repeat(5000) });
    ws.serverSend({ type: "ERROR", message: "no code" });
    await sleep(10);
    expect(errors).toHaveLength(3);
    expect(errors[0]).toEqual({ code: 409, message: "room is full", room: "c:123456" });
    expect(errors[1]).toEqual({ code: 404, message: "unknown target", sessionId: "s1" });
    expect(errors[2]!.message).toHaveLength(1024);
  });

  it("fails sends while disconnected and announces updates while connected", async () => {
    const c = client();
    await expect(c.sendOffer("p1", "s1", SDP)).rejects.toMatchObject({ code: "closed" });
    const ws = await greeted(c);
    c.update({ alias: "Renamed" });
    await waitUntil(() => ws.sentJson().some((m) => m.type === "UPDATE"));
    expect(ws.sentJson().find((m) => m.type === "UPDATE")!.info).toMatchObject({ alias: "Renamed", version: "2.2", ext: { key: KEY } });
    // Reconnects use the new info.
    const d = new URL(c.connectUrl()).searchParams.get("d")!;
    expect(JSON.parse(fromUtf8(b64urlDecode(d)))).toMatchObject({ alias: "Renamed" });
  });

  it("reconnects with exponential backoff and re-joins rooms", async () => {
    const c = client({ initialBackoffMs: 20, maxBackoffMs: 80 });
    c.joinRoom(ROOM);
    const ws1 = await greeted(c);
    await waitUntil(() => ws1.sentJson().some((m) => m.type === "ROOM_JOIN"));
    const states = collect(c, "state");
    const t0 = Date.now();
    ws1.serverClose();
    expect(c.state).toBe("closed");
    // Two failed attempts: delays 10, 20, 40 ms (equal jitter with random() = 0 → d/2).
    await waitUntil(() => FakeWebSocket.instances.length === 2);
    FakeWebSocket.last.serverClose();
    await waitUntil(() => FakeWebSocket.instances.length === 3);
    FakeWebSocket.last.serverClose();
    await waitUntil(() => FakeWebSocket.instances.length === 4);
    expect(Date.now() - t0).toBeGreaterThanOrEqual(65);
    const ws4 = FakeWebSocket.last;
    ws4.serverOpen();
    ws4.serverSend({ type: "HELLO", client: peer("me-again"), peers: [], server: { v: 1, caps: [] } });
    await waitUntil(() => ws4.sentJson().some((m) => m.type === "ROOM_JOIN"));
    expect(states).toEqual(["closed", "connecting", "closed", "connecting", "closed", "connecting", "open"]);
    expect(c.client?.id).toBe("me-again");

    c.close();
    expect(ws4.closedByClient).toBe(true);
    await sleep(60);
    expect(FakeWebSocket.instances).toHaveLength(4); // no reconnect after close()
  });

  it("keeps alive with PING on Ferry servers and empty frames on LocalSend servers", async () => {
    const plain = client({ pingIntervalMs: 10 });
    const ws = await greeted(plain, { server: undefined });
    expect(plain.server).toBeNull();
    await waitUntil(() => ws.sent.includes(""));
    expect(ws.sent.some((s) => s.includes("PING"))).toBe(false);
    plain.close();

    const ferry = client({ pingIntervalMs: 10, idleTimeoutMs: 60 });
    const ws2 = await greeted(ferry);
    await waitUntil(() => ws2.sentJson().some((m) => m.type === "PING"));
    // This scripted server never answers: after the idle timeout the client reconnects.
    const before = FakeWebSocket.instances.length;
    await waitUntil(() => FakeWebSocket.instances.length > before, 2000, "reconnect");
    expect(ws2.closedByClient).toBe(true);
  });
});

describe("signaling through ferry-signal", () => {
  it("lists peers, shares rooms and relays offer, answer, ICE and cancel", async () => {
    const server = new FakeSignalServer();
    const alice = client({ WebSocket: server.WebSocket, info: { alias: "Alice", deviceType: "web", token: "a", publicKey: KEY } });
    const bob = client({ WebSocket: server.WebSocket, info: { alias: "Bob", deviceType: "desktop", token: "b", publicKey: KEY } });
    const helloA = nextEvent(alice, "hello");
    alice.connect();
    const ha = await helloA;
    const helloB = nextEvent(bob, "hello");
    const aliceSeesBob = nextEvent(alice, "join");
    bob.connect();
    const hb = await helloB;
    expect(hb.peers.map((p) => p.alias)).toEqual(["Alice"]);
    expect(hb.server?.caps).toContain("rooms");
    expect((await aliceSeesBob).peer).toMatchObject({ alias: "Bob", deviceType: "DESKTOP" });

    const roomHelloA = nextEvent(alice, "roomHello");
    alice.joinRoom(ROOM);
    expect((await roomHelloA).peers).toEqual([]);
    const roomHelloB = nextEvent(bob, "roomHello");
    const joined = nextEvent(alice, "roomPeerJoined");
    bob.joinRoom(ROOM);
    expect((await roomHelloB).peers.map((p) => p.alias)).toEqual(["Alice"]);
    expect((await joined).peer.alias).toBe("Bob");

    const aliceId = ha.client.id;
    const bobId = hb.client.id;
    const offer = nextEvent(bob, "offer");
    await alice.sendOffer(bobId, "s1", SDP);
    expect(await offer).toMatchObject({ sessionId: "s1", sdp: SDP, peer: { id: aliceId, alias: "Alice" } });
    const answer = nextEvent(alice, "answer");
    await bob.sendAnswer(aliceId, "s1", SDP);
    expect(await answer).toMatchObject({ sessionId: "s1", sdp: SDP, peer: { id: bobId } });
    const ices = collect(bob, "ice");
    const cand = { candidate: "candidate:1 1 udp 1 192.0.2.1 9 typ host", sdpMid: "0", sdpMLineIndex: 0 };
    await alice.sendIce(bobId, "s1", cand);
    await alice.sendIce(bobId, "s1", null);
    await waitUntil(() => ices.length === 2);
    expect(ices.map((e) => e.candidate)).toEqual([cand, null]);
    const cancel = nextEvent(bob, "cancel");
    await alice.sendCancel(bobId, "s1");
    expect(await cancel).toMatchObject({ sessionId: "s1", peer: { id: aliceId } });

    const error = nextEvent(alice, "error");
    await alice.sendOffer("00000000-0000-0000-0000-000000000000", "s2", SDP);
    expect(await error).toMatchObject({ code: 404, sessionId: "s2" });

    const roomLeft = nextEvent(alice, "roomPeerLeft");
    const left = nextEvent(alice, "left");
    bob.close();
    expect(await roomLeft).toEqual({ room: ROOM, peerId: bobId });
    expect(await left).toEqual({ peerId: bobId });
  });

  it("re-joins its rooms after the server drops the connection", async () => {
    const server = new FakeSignalServer();
    const c = client({ WebSocket: server.WebSocket });
    c.joinRoom(ROOM);
    const first = nextEvent(c, "hello");
    c.connect();
    const h1 = await first;
    await waitUntil(() => server.rooms.get(ROOM)?.has(h1.client.id) === true);
    const second = nextEvent(c, "hello");
    server.kick(h1.client.id);
    const h2 = await second;
    expect(h2.client.id).not.toBe(h1.client.id);
    await waitUntil(() => server.rooms.get(ROOM)?.has(h2.client.id) === true);
    expect(server.rooms.get(ROOM)?.has(h1.client.id)).toBe(false);
  });
});
