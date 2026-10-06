// Persistent WebRTC device identity (threat model "Identity model"): an
// Ed25519 key pair, or ECDSA P-256 where the browser lacks Ed25519. Private
// keys are generated non-extractable; callers persist the CryptoKey objects in
// IndexedDB (structured clone keeps them non-extractable).

import { b64urlEncode, tryB64urlDecode, type Bytes } from "./bytes";

export type IdentityAlg = "ed25519" | "p256";

export interface Identity {
  readonly alg: IdentityAlg;
  readonly privateKey: CryptoKey;
  readonly publicKey: CryptoKey;
}

/** Shape stored in IndexedDB. CryptoKeys survive structured clone as-is. */
export interface StoredIdentity {
  v: 1;
  alg: IdentityAlg;
  privateKey: CryptoKey;
  publicKey: CryptoKey;
}

const ED25519 = { name: "Ed25519" } as const;
const P256: EcKeyGenParams & EcKeyImportParams = { name: "ECDSA", namedCurve: "P-256" };
const P256_SIGN: EcdsaParams = { name: "ECDSA", hash: "SHA-256" };

/** Raw public key length: 32 bytes (Ed25519) or 65 bytes (uncompressed P-256 point). */
export const PUBLIC_KEY_LENGTH: Record<IdentityAlg, number> = { ed25519: 32, p256: 65 };
/** Both algorithms produce 64-byte signatures (P-256 in IEEE-P1363 r‖s form). */
export const SIGNATURE_LENGTH = 64;

export function isIdentityAlg(value: unknown): value is IdentityAlg {
  return value === "ed25519" || value === "p256";
}

/** Checks the length (and point-format prefix) of a raw public key. */
export function isValidPublicKey(alg: IdentityAlg, raw: Uint8Array): boolean {
  if (raw.byteLength !== PUBLIC_KEY_LENGTH[alg]) return false;
  return alg !== "p256" || raw[0] === 0x04;
}

/**
 * Generates a new identity. Prefers Ed25519 and falls back to P-256 when the
 * browser rejects Ed25519. Passing `alg` forces one algorithm (no fallback).
 */
export async function generateIdentity(alg?: IdentityAlg): Promise<Identity> {
  if (alg !== "p256") {
    try {
      const pair = await crypto.subtle.generateKey(ED25519, false, ["sign", "verify"]);
      return { alg: "ed25519", privateKey: pair.privateKey, publicKey: pair.publicKey };
    } catch (err) {
      if (alg === "ed25519") throw err;
    }
  }
  const pair = await crypto.subtle.generateKey(P256, false, ["sign", "verify"]);
  return { alg: "p256", privateKey: pair.privateKey, publicKey: pair.publicKey };
}

/** Raw public key bytes (public keys stay exportable even when the pair is not). */
export async function exportPublicKeyRaw(identity: Identity): Promise<Bytes> {
  return new Uint8Array(await crypto.subtle.exportKey("raw", identity.publicKey));
}

/** Raw public key, base64url without padding (the wire format). */
export async function exportPublicKey(identity: Identity): Promise<string> {
  return b64urlEncode(await exportPublicKeyRaw(identity));
}

/** Signs `data`; returns the signature as base64url. */
export async function sign(identity: Identity, data: Uint8Array): Promise<string> {
  const params = identity.alg === "ed25519" ? ED25519 : P256_SIGN;
  const sig = await crypto.subtle.sign(params, identity.privateKey, new Uint8Array(data));
  return b64urlEncode(new Uint8Array(sig));
}

/** Verifies a base64url signature against a base64url raw public key. Never throws. */
export async function verify(alg: IdentityAlg, publicKeyB64: string, data: Uint8Array, sigB64: string): Promise<boolean> {
  if (!isIdentityAlg(alg)) return false;
  const raw = tryB64urlDecode(publicKeyB64);
  const sig = tryB64urlDecode(sigB64);
  if (!raw || !sig || !isValidPublicKey(alg, raw) || sig.byteLength !== SIGNATURE_LENGTH) return false;
  try {
    if (alg === "ed25519") {
      const key = await crypto.subtle.importKey("raw", raw, ED25519, false, ["verify"]);
      return await crypto.subtle.verify(ED25519, key, sig, new Uint8Array(data));
    }
    const key = await crypto.subtle.importKey("raw", raw, P256, false, ["verify"]);
    return await crypto.subtle.verify(P256_SIGN, key, sig, new Uint8Array(data));
  } catch {
    return false;
  }
}

/**
 * Six-digit first-contact verification code: the first four bytes of the
 * transcript hash as a big-endian u32, mod 1,000,000, zero-padded.
 */
export function shortCode(transcriptHash: Uint8Array): string {
  if (transcriptHash.byteLength < 4) throw new RangeError("transcript hash too short");
  const view = new DataView(transcriptHash.buffer, transcriptHash.byteOffset, 4);
  return String(view.getUint32(0, false) % 1_000_000).padStart(6, "0");
}

/** Value to put into IndexedDB. The keys are passed through untouched (structured clone). */
export function serializeForIdb(identity: Identity): StoredIdentity {
  return { v: 1, alg: identity.alg, privateKey: identity.privateKey, publicKey: identity.publicKey };
}

/** Reads a value written by `serializeForIdb`; returns null when it is missing or malformed. */
export function deserializeFromIdb(value: unknown): Identity | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Partial<StoredIdentity>;
  if (v.v !== 1 || !isIdentityAlg(v.alg)) return null;
  const algName = v.alg === "ed25519" ? "Ed25519" : "ECDSA";
  if (!isKey(v.privateKey, "private", algName) || !isKey(v.publicKey, "public", algName)) return null;
  return { alg: v.alg, privateKey: v.privateKey, publicKey: v.publicKey };
}

function isKey(key: unknown, type: KeyType, algName: string): key is CryptoKey {
  if (!key || typeof key !== "object") return false;
  const k = key as Partial<CryptoKey>;
  return k.type === type && k.algorithm?.name === algName;
}
