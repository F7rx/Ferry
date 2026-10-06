// @vitest-environment node
import { afterEach, describe, expect, it } from "vitest";
import { equalBytes, randomBytes } from "./bytes";
import { exportPublicKey, generateIdentity } from "./identity";
import { PeerConnector, type PeerConnectorOptions } from "./peer";
import { RtcError } from "./protocol";
import { decodeSdp, encodeSdp, SignalingClient } from "./signaling";
import {
  collect,
  fakeFingerprint,
  FakePeerConnection,
  FakeSignalServer,
  FakeWebSocket,
  MemorySink,
  memorySource,
  nextEvent,
  waitUntil,
} from "./test-fakes";

const cleanup: (() => void)[] = [];

afterEach(() => {
  for (const fn of cleanup.splice(0)) fn();
  FakeWebSocket.instances.length = 0;
  FakePeerConnection.all.clear();
});

async function rejection(p: Promise<unknown>): Promise<RtcError> {
  try {
    await p;
  } catch (err) {
    expect(err).toBeInstanceOf(RtcError);
    return err as RtcError;
  }
  throw new Error("expected a rejection");
}

async function setup(options: { alice?: Partial<PeerConnectorOptions>; bob?: Partial<PeerConnectorOptions> } = {}) {
  const server = new FakeSignalServer();
  const idA = await generateIdentity("ed25519");
  const idB = await generateIdentity("ed25519");
  const keyA = await exportPublicKey(idA);
  const keyB = await exportPublicKey(idB);
  const signaling = (alias: string, publicKey: string) =>
    new SignalingClient({ url: "wss://signal.test/v1/ws", info: { alias, deviceType: "web", token: alias, publicKey }, WebSocket: server.WebSocket });
  const sigA = signaling("Alice", keyA);
  const sigB = signaling("Bob", keyB);
  const helloA = nextEvent(sigA, "hello");
  const helloB = nextEvent(sigB, "hello");
  sigA.connect();
  sigB.connect();
  const [ha, hb] = await Promise.all([helloA, helloB]);
  const sinkB = new MemorySink();
  const PC = FakePeerConnection as unknown as typeof RTCPeerConnection;
  const alice = new PeerConnector({
    signaling: sigA,
    identity: idA,
    device: { alias: "Alice", deviceType: "web", platform: "test" },
    RTCPeerConnection: PC,
    ...options.alice,
  });
  const bob = new PeerConnector({
    signaling: sigB,
    identity: idB,
    device: { alias: "Bob", deviceType: "desktop", platform: "test" },
    sink: sinkB,
    RTCPeerConnection: PC,
    ...options.bob,
  });
  cleanup.push(() => {
    alice.dispose();
    bob.dispose();
    sigA.close();
    sigB.close();
  });
  return { server, alice, bob, aliceId: ha.client.id, bobId: hb.client.id, keyA, keyB, sinkB };
}

describe("PeerConnector", () => {
  it("connects through signaling, authenticates both ends and transfers a file", async () => {
    const { alice, bob, aliceId, bobId, keyA, keyB, sinkB } = await setup();
    const incoming = nextEvent(bob, "incoming");
    const bobSession = nextEvent(bob, "session");
    const aliceSession = nextEvent(alice, "session");
    bob.on("incoming", (req) => void req.accept().catch(() => {}));

    const session = await alice.connect(bobId);
    expect(session.state).toBe("ready");
    expect(session.peer?.key).toBe(keyB);
    expect(await aliceSession).toMatchObject({ peerId: bobId, role: "offerer" });
    const req = await incoming;
    expect(req.peer).toMatchObject({ id: aliceId, alias: "Alice" });
    expect(req.sessionId).toBe(session.sessionId);
    const { session: sb, role, peerId } = await bobSession;
    expect(role).toBe("answerer");
    expect(peerId).toBe(aliceId);
    expect((await sb.ready).key).toBe(keyA);
    expect(sb.peer?.shortCode).toBe(session.peer?.shortCode);
    expect(session.chunkSize).toBe(64 * 1024); // from the remote SDP's max-message-size

    // Trickled ICE went through ferry-signal (as strings) and reached both connections.
    const pcs = [...FakePeerConnection.all.values()];
    expect(pcs).toHaveLength(2);
    await waitUntil(() => pcs.every((pc) => pc.remoteCandidates.length === 2), 2000, "ICE");
    for (const pc of pcs) expect(pc.remoteCandidates).toEqual([expect.objectContaining({ sdpMid: "0", sdpMLineIndex: 0 }), null]);

    sb.on("offer", (o) => o.accept());
    const blob = randomBytes(500_000);
    const outcome = await session.sendTransfer({ transferId: "t1", files: [memorySource("f1", blob)] });
    expect(outcome.completed).toEqual(["f1"]);
    expect(equalBytes(sinkB.data("f1"), blob)).toBe(true);

    // Disposing closes the sessions on both ends.
    const closed = nextEvent(sb, "closed");
    alice.dispose();
    expect(session.state).toBe("closed");
    expect((await closed).reason).toBe("remote");
  });

  it("asks the iceServers provider for every new connection", async () => {
    const servers = [{ urls: "turn:turn.example:3478", username: "1600:me", credential: "c" }];
    let calls = 0;
    const provider = async () => {
      calls++;
      return servers;
    };
    const { alice, bob, bobId } = await setup({ alice: { iceServers: provider }, bob: { iceServers: servers } });
    bob.on("incoming", (req) => void req.accept().catch(() => {}));
    await alice.connect(bobId);
    expect(calls).toBe(1);
    expect([...FakePeerConnection.all.values()].map((pc) => pc.config?.iceServers)).toEqual([servers, servers]);

    const failing = await setup({ alice: { iceServers: () => Promise.reject(new RtcError("turn", "TURN credentials unavailable")) } });
    expect((await rejection(failing.alice.connect(failing.bobId))).code).toBe("turn");
  });

  it("is rejected when nobody listens for incoming connections", async () => {
    const { alice, bobId } = await setup();
    const err = await rejection(alice.connect(bobId));
    expect(err.code).toBe("cancelled");
  });

  it("is rejected when the peer declines", async () => {
    const { alice, bob, bobId } = await setup();
    bob.on("incoming", (req) => req.reject());
    expect((await rejection(alice.connect(bobId))).code).toBe("cancelled");
  });

  it("withdraws the request on the other side when the caller gives up", async () => {
    const { alice, bob, bobId } = await setup({ alice: { connectTimeoutMs: 100 } });
    const incoming = nextEvent(bob, "incoming"); // a listener that never decides
    const withdrawn = nextEvent(bob, "withdrawn");
    expect((await rejection(alice.connect(bobId))).code).toBe("timeout");
    const req = await incoming;
    expect(await withdrawn).toEqual({ peerId: req.peer.id, sessionId: req.sessionId, reason: "the peer cancelled" });
    expect((await rejection(req.accept())).code).toBe("invalid-state");
  });

  it("withdraws requests nobody decides on in time", async () => {
    const { alice, bob, bobId } = await setup({ bob: { decisionTimeoutMs: 60 } });
    bob.on("incoming", () => {});
    const withdrawn = nextEvent(bob, "withdrawn");
    expect((await rejection(alice.connect(bobId))).code).toBe("cancelled");
    expect((await withdrawn).reason).toBe("timeout");
  });

  it("binds the room secret on both ends", async () => {
    const secret = randomBytes(16);
    const ok = await setup();
    ok.bob.on("incoming", (req) => void req.accept({ roomSecret: secret }).catch(() => {}));
    const session = await ok.alice.connect(ok.bobId, undefined, { roomSecret: secret });
    expect(session.peer?.roomVerified).toBe(true);

    const bad = await setup();
    const bobFailed = new Promise<RtcError>((resolve) => {
      bad.bob.on("incoming", (req) => void req.accept({ roomSecret: randomBytes(16) }).catch(resolve));
    });
    expect((await rejection(bad.alice.connect(bad.bobId, undefined, { roomSecret: secret }))).code).toBe("auth");
    expect((await bobFailed).code).toMatch(/auth|cancelled/);
  });

  it("pins a known peer key", async () => {
    const { alice, bob, bobId, keyA } = await setup();
    bob.on("incoming", (req) => void req.accept({ expectedPeerKey: keyA }).catch(() => {}));
    const stranger = await exportPublicKey(await generateIdentity("ed25519"));
    expect((await rejection(alice.connect(bobId, undefined, { expectedPeerKey: stranger }))).code).toBe("auth");
  });

  it("fails when the signaling server tampers with an SDP fingerprint", async () => {
    const { server, alice, bob, bobId } = await setup();
    server.relayHook = async (msg) => {
      if (msg.type !== "ANSWER") return msg;
      const sdp = (await decodeSdp(msg.sdp as string)).replace(/^a=fingerprint:.*$/m, `a=fingerprint:${fakeFingerprint()}`);
      return { ...msg, sdp: await encodeSdp(sdp) };
    };
    const bobFailed = new Promise<RtcError>((resolve) => {
      bob.on("incoming", (req) => void req.accept().catch(resolve));
    });
    expect((await rejection(alice.connect(bobId))).code).toBe("webrtc");
    expect((await bobFailed).code).toMatch(/webrtc|cancelled/);
  });

  it("caps undecided incoming requests", async () => {
    const { alice, bob, bobId } = await setup({ alice: { connectTimeoutMs: 10_000 } });
    const incoming = collect(bob, "incoming");
    const attempts = Array.from({ length: 17 }, () =>
      alice.connect(bobId).then(
        () => new RtcError("connected", "unexpected"),
        (err: unknown) => err as RtcError,
      ),
    );
    const first = await Promise.race(attempts);
    expect(first).toMatchObject({ code: "cancelled" }); // the 17th was turned away immediately
    await waitUntil(() => incoming.length === 16);
    alice.dispose();
    const results = await Promise.all(attempts);
    expect(results.filter((r) => r.code === "cancelled")).toHaveLength(1);
    expect(results.filter((r) => r.code === "closed")).toHaveLength(16);
  });
});
