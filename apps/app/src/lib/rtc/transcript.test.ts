// @vitest-environment node
import { describe, expect, it } from "vitest";
import { b64urlDecode, b64urlEncode, randomBytes, utf8 } from "./bytes";
import {
  deserializeFromIdb,
  exportPublicKey,
  generateIdentity,
  serializeForIdb,
  shortCode,
  sign,
  verify,
} from "./identity";
import { RtcError } from "./protocol";
import { PeerSession } from "./session";
import { isValidRoomId } from "./signaling";
import { createChannelPair, createSessionPair, fakeFingerprint, FakeDataChannel, nextEvent } from "./test-fakes";
import {
  authPayload,
  deriveRoomKey,
  extractFingerprint,
  roomIdFromSecret,
  roomMac,
  transcriptHash,
  verifyRoomMac,
  type TranscriptParts,
} from "./transcript";

async function rejection(p: Promise<unknown>): Promise<RtcError> {
  try {
    await p;
  } catch (err) {
    expect(err).toBeInstanceOf(RtcError);
    return err as RtcError;
  }
  throw new Error("expected a rejection");
}

describe("identity", () => {
  it("signs and verifies with Ed25519", async () => {
    const id = await generateIdentity("ed25519");
    expect(id.alg).toBe("ed25519");
    const key = await exportPublicKey(id);
    expect(b64urlDecode(key).byteLength).toBe(32);
    const data = utf8("ferry");
    const sig = await sign(id, data);
    expect(b64urlDecode(sig).byteLength).toBe(64);
    expect(await verify("ed25519", key, data, sig)).toBe(true);
    expect(await verify("ed25519", key, utf8("ferrY"), sig)).toBe(false);
    const other = await exportPublicKey(await generateIdentity("ed25519"));
    expect(await verify("ed25519", other, data, sig)).toBe(false);
    // Malformed input never throws.
    expect(await verify("ed25519", "!!", data, sig)).toBe(false);
    expect(await verify("ed25519", key, data, sig.slice(0, -2))).toBe(false);
    expect(await verify("p256", key, data, sig)).toBe(false);
    expect(await verify("rsa" as never, key, data, sig)).toBe(false);
  });

  it("falls back to P-256 with the same 64-byte signature format", async () => {
    const id = await generateIdentity("p256");
    const raw = b64urlDecode(await exportPublicKey(id));
    expect(raw.byteLength).toBe(65);
    expect(raw[0]).toBe(4);
    const sig = await sign(id, utf8("x"));
    expect(b64urlDecode(sig).byteLength).toBe(64);
    expect(await verify("p256", await exportPublicKey(id), utf8("x"), sig)).toBe(true);
  });

  it("round-trips through the IndexedDB shape and rejects junk", async () => {
    const id = await generateIdentity("ed25519");
    const restored = deserializeFromIdb(structuredClone(serializeForIdb(id)));
    expect(restored?.alg).toBe("ed25519");
    expect(await verify("ed25519", await exportPublicKey(id), utf8("a"), await sign(restored!, utf8("a")))).toBe(true);
    for (const junk of [null, 1, {}, { v: 2 }, { v: 1, alg: "ed25519", privateKey: {}, publicKey: {} }]) {
      expect(deserializeFromIdb(junk)).toBeNull();
    }
  });

  it("derives the 6-digit verification code from T", () => {
    expect(shortCode(new Uint8Array([0, 0, 0, 42, 9, 9]))).toBe("000042");
    expect(shortCode(new Uint8Array([0xff, 0xff, 0xff, 0xff]))).toBe("967295");
    expect(() => shortCode(new Uint8Array(3))).toThrow();
  });
});

describe("transcript", () => {
  const parts = (): TranscriptParts => ({
    sessionId: "s1",
    fpOfferer: fpO,
    fpAnswerer: fpA,
    nonceOfferer: new Uint8Array(32).fill(1),
    nonceAnswerer: new Uint8Array(32).fill(2),
    keyOfferer: new Uint8Array(32).fill(3),
    keyAnswerer: new Uint8Array(32).fill(4),
  });
  const fpO = fakeFingerprint();
  const fpA = fakeFingerprint();

  it("is deterministic and binds every part", () => {
    const base = transcriptHash(parts());
    expect(base.byteLength).toBe(32);
    expect(transcriptHash(parts())).toEqual(base);
    const variants: Partial<TranscriptParts>[] = [
      { sessionId: "s2" },
      { fpOfferer: fpA, fpAnswerer: fpO }, // swapped legs
      { fpAnswerer: fakeFingerprint() },
      { nonceOfferer: new Uint8Array(32).fill(9) },
      { nonceAnswerer: new Uint8Array(32).fill(9) },
      { keyOfferer: new Uint8Array(32).fill(9) },
      { keyAnswerer: new Uint8Array(32).fill(9) },
    ];
    for (const v of variants) expect(transcriptHash({ ...parts(), ...v })).not.toEqual(base);
    // Length prefixes: moving bytes between adjacent fields changes T.
    expect(transcriptHash({ ...parts(), sessionId: "ab", fpOfferer: "c" })).not.toEqual(
      transcriptHash({ ...parts(), sessionId: "a", fpOfferer: "bc" }),
    );
  });

  it("signs role-separated payloads", () => {
    const t = transcriptHash(parts());
    expect(authPayload("offerer", t)).not.toEqual(authPayload("answerer", t));
  });

  it("normalizes SDP fingerprints", () => {
    const hex = "ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89";
    const sdp = `v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=fingerprint:SHA-256 ${hex}\r\n`;
    expect(extractFingerprint(sdp)).toBe(`sha-256 ${hex.toUpperCase()}`);
    const two = `${sdp}a=fingerprint:sha-1 ${"0a:".repeat(19)}0a\r\n`;
    expect(extractFingerprint(two)).toBe(`sha-1 ${"0A:".repeat(19)}0A,sha-256 ${hex.toUpperCase()}`);
    expect(extractFingerprint("v=0\r\n")).toBeNull();
    expect(extractFingerprint("a=fingerprint:sha-256 AB:CD\r\n")).toBeNull(); // too short
  });

  it("derives room ids the server accepts and binds the room secret", () => {
    const secret = randomBytes(16);
    const room = roomIdFromSecret(secret);
    expect(room).toMatch(/^r:[A-Za-z0-9_-]{22}$/);
    expect(isValidRoomId(room)).toBe(true);
    expect(roomIdFromSecret(secret)).toBe(room);
    expect(roomIdFromSecret(randomBytes(16))).not.toBe(room);

    const t = transcriptHash(parts());
    const key = deriveRoomKey(secret);
    const mac = roomMac(key, t);
    expect(verifyRoomMac(key, t, mac)).toBe(true);
    expect(verifyRoomMac(deriveRoomKey(randomBytes(16)), t, mac)).toBe(false);
    expect(verifyRoomMac(key, transcriptHash({ ...parts(), sessionId: "x" }), mac)).toBe(false);
    expect(verifyRoomMac(key, t, "garbage!")).toBe(false);
  });
});

describe("session handshake", () => {
  it("authenticates both peers with the right keys", async () => {
    const { a, b, idA, idB } = await createSessionPair();
    const [peerOfA, peerOfB] = await Promise.all([a.ready, b.ready]);
    expect(peerOfA.key).toBe(await exportPublicKey(idB));
    expect(peerOfB.key).toBe(await exportPublicKey(idA));
    expect(peerOfA.device).toEqual({ alias: "Bob", deviceType: "desktop", platform: "test" });
    expect(peerOfA.transcript).toEqual(peerOfB.transcript);
    expect(peerOfA.shortCode).toBe(peerOfB.shortCode);
    expect(peerOfA.shortCode).toMatch(/^\d{6}$/);
    expect(peerOfA.roomVerified).toBe(false);
    expect(a.state).toBe("ready");
    expect(a.peer).toBe(peerOfA);
    a.close();
  });

  it("fails when the peers observed different DTLS fingerprints (signaling MITM)", async () => {
    // A MITM terminates DTLS on both legs: each side sees the attacker's certificate.
    const { a, b } = await createSessionPair({ a: { remoteFingerprint: fakeFingerprint() } });
    const closedA = nextEvent(a, "closed");
    const closedB = nextEvent(b, "closed");
    expect((await rejection(a.ready)).code).toBe("auth");
    expect((await rejection(b.ready)).code).toBe("auth");
    expect((await closedA).reason).toBe("auth");
    expect((await closedB).reason).toBe("auth");
  });

  it("fails on a wrong signature", async () => {
    const { a, b, chA, chB } = await createSessionPair({ manualOpen: true });
    chB.transform = (frame) => {
      if (typeof frame !== "string" || !frame.includes('"t":"auth"')) return frame;
      const msg = JSON.parse(frame) as { sig: string };
      const sig = b64urlDecode(msg.sig);
      sig[10]! ^= 1;
      return JSON.stringify({ ...msg, sig: b64urlEncode(sig) });
    };
    const errorsB = nextEvent(b, "error");
    const closedB = nextEvent(b, "closed");
    FakeDataChannel.openPair(chA, chB);
    const err = await rejection(a.ready);
    expect(err.code).toBe("auth");
    expect(err.message).toMatch(/authentication failed/);
    // B verified A (A's auth was intact) but learns why A hung up.
    expect(await errorsB).toMatchObject({ code: "auth", remote: true });
    expect((await closedB).reason).toBe("auth");
  });

  it("binds the room secret (link/QR rooms)", async () => {
    const secret = randomBytes(16);
    const ok = await createSessionPair({ a: { roomSecret: secret }, b: { roomSecret: secret } });
    const [pa, pb] = await Promise.all([ok.a.ready, ok.b.ready]);
    expect(pa.roomVerified && pb.roomVerified).toBe(true);
    ok.a.close();

    const bad = await createSessionPair({ a: { roomSecret: secret }, b: { roomSecret: randomBytes(16) } });
    expect((await rejection(bad.a.ready)).code).toBe("auth");
    expect((await rejection(bad.b.ready)).code).toBe("auth");

    const missing = await createSessionPair({ a: { roomSecret: secret } });
    expect((await rejection(missing.a.ready)).code).toBe("auth");
  });

  it("pins an expected peer key", async () => {
    const stranger = await exportPublicKey(await generateIdentity("ed25519"));
    const { a } = await createSessionPair({ a: { expectedPeerKey: stranger } });
    const err = await rejection(a.ready);
    expect(err.code).toBe("auth");
    expect(err.message).toMatch(/unexpected peer key/);
  });

  it("interoperates between Ed25519 and P-256 identities", async () => {
    const identities = [await generateIdentity("p256"), await generateIdentity("ed25519")] as const;
    const { a, b } = await createSessionPair({ identities: [...identities] });
    const [pa, pb] = await Promise.all([a.ready, b.ready]);
    expect(pa.alg).toBe("ed25519");
    expect(pb.alg).toBe("p256");
    a.close();
  });

  it("times out a stalled handshake", async () => {
    const { a, chB } = await createSessionPair({ a: { handshakeTimeoutMs: 80, timeoutMs: 10_000 } });
    chB.blackhole = true; // B's hello never arrives
    const closed = nextEvent(a, "closed");
    const err = await rejection(a.ready);
    expect(err.code).toBe("timeout");
    expect(await closed).toEqual({ reason: "timeout", message: "handshake timed out" });
  });

  it("rejects a reflected handshake", async () => {
    const [ch] = createChannelPair();
    ch.peer = ch; // everything A sends comes back to A
    const s = new PeerSession({
      channel: ch,
      role: "offerer",
      sessionId: "s",
      localFingerprint: fakeFingerprint(),
      remoteFingerprint: fakeFingerprint(),
      identity: await generateIdentity("ed25519"),
      device: { alias: "A", deviceType: "web", platform: "test" },
    });
    FakeDataChannel.openPair(ch, ch);
    const err = await rejection(s.ready);
    expect(err.code).toBe("auth");
    expect(err.message).toMatch(/reflected/);
  });

  it("rejects transfer messages before authentication", async () => {
    const { a, b, chA, chB } = await createSessionPair({ manualOpen: true });
    FakeDataChannel.openPair(chA, chB);
    chA.send(JSON.stringify({ t: "done", transferId: "t1" })); // before A's auth
    expect((await rejection(b.ready)).code).toBe("protocol");
    await rejection(a.ready);
  });
});
