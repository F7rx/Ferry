// The ferry-dc/1 handshake transcript (05-protocol.md §5.2, threat model W3).
// Each side hashes both DTLS fingerprints *as it observed them*, the session id,
// both nonces and both identity keys. A signaling server that swaps SDPs makes
// the two legs observe different fingerprints, so the signatures cannot verify.

import { hmac } from "@noble/hashes/hmac.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { b64urlEncode, concatBytes, equalBytes, tryB64urlDecode, u32be, utf8, type Bytes } from "./bytes";

export const DC_PROTOCOL = "ferry-dc/1";

export type Role = "offerer" | "answerer";

export interface TranscriptParts {
  sessionId: string;
  /** Normalized fingerprints (see `extractFingerprint`). */
  fpOfferer: string;
  fpAnswerer: string;
  /** Raw 32-byte nonces. */
  nonceOfferer: Uint8Array;
  nonceAnswerer: Uint8Array;
  /** Raw public key bytes. */
  keyOfferer: Uint8Array;
  keyAnswerer: Uint8Array;
}

/** `enc(x) = u32be(byteLength(x)) ‖ x`; strings are UTF-8. */
function enc(value: string | Uint8Array): Bytes {
  const bytes = typeof value === "string" ? utf8(value) : value;
  return concatBytes(u32be(bytes.byteLength), bytes);
}

/** `T = SHA-256(enc("ferry-dc/1") ‖ enc(sessionId) ‖ enc(fpO) ‖ enc(fpA) ‖ enc(nonceO) ‖ enc(nonceA) ‖ enc(keyO) ‖ enc(keyA))` */
export function transcriptHash(p: TranscriptParts): Bytes {
  const h = sha256.create();
  for (const part of [DC_PROTOCOL, p.sessionId, p.fpOfferer, p.fpAnswerer, p.nonceOfferer, p.nonceAnswerer, p.keyOfferer, p.keyAnswerer]) {
    h.update(enc(part));
  }
  return new Uint8Array(h.digest());
}

/** Signature input: UTF-8 `"ferry-dc/1 auth " + role` ‖ T. */
export function authPayload(role: Role, transcript: Uint8Array): Bytes {
  return concatBytes(utf8(`${DC_PROTOCOL} auth ${role}`), transcript);
}

/** `roomKey = SHA-256(UTF-8 "ferry-room-key/1" ‖ roomSecret)` */
export function deriveRoomKey(roomSecret: Uint8Array): Bytes {
  return new Uint8Array(sha256(concatBytes(utf8("ferry-room-key/1"), roomSecret)));
}

/** `mac = HMAC-SHA256(roomKey, T)`, base64url. */
export function roomMac(roomKey: Uint8Array, transcript: Uint8Array): string {
  return b64urlEncode(hmac(sha256, roomKey, transcript));
}

export function verifyRoomMac(roomKey: Uint8Array, transcript: Uint8Array, macB64: string): boolean {
  const mac = tryB64urlDecode(macB64);
  return !!mac && equalBytes(mac, hmac(sha256, roomKey, transcript));
}

/** Link/QR room id: `"r:" + base64url(SHA-256("ferry-room/1" ‖ secret))[0..22]`. */
export function roomIdFromSecret(roomSecret: Uint8Array): string {
  return "r:" + b64urlEncode(sha256(concatBytes(utf8("ferry-room/1"), roomSecret))).slice(0, 22);
}

const FINGERPRINT_LINE = /^a=fingerprint:([!-~]+)[ \t]+([0-9A-Fa-f:]+)[ \t]*\r?$/gm;

/**
 * Extracts the DTLS certificate fingerprint from an SDP and normalizes it to
 * `"<algo lowercase> <HEX:UPPER:WITH:COLONS>"`. When the SDP lists several
 * distinct fingerprints (e.g. one per hash algorithm or certificate) they are
 * all bound: deduplicated, sorted and joined with ",", so adding or swapping a
 * fingerprint line also changes the transcript. Returns null when none is found.
 */
export function extractFingerprint(sdp: string): string | null {
  const found = new Set<string>();
  for (const m of sdp.matchAll(FINGERPRINT_LINE)) {
    const algo = m[1]!.toLowerCase();
    const hex = m[2]!.replace(/:/g, "").toUpperCase();
    if (hex.length < 32 || hex.length % 2 !== 0) continue;
    found.add(`${algo} ${hex.match(/../g)!.join(":")}`);
  }
  if (found.size === 0) return null;
  return [...found].sort().join(",");
}
