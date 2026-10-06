// Byte helpers shared by the WebRTC library: base64url (no padding), UTF-8,
// framing primitives and secure randomness. Everything returns arrays backed
// by a plain ArrayBuffer so the results can be handed straight to WebCrypto.

export type Bytes = Uint8Array<ArrayBuffer>;

const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const LOOKUP = new Int16Array(128).fill(-1);
for (let i = 0; i < ALPHABET.length; i++) LOOKUP[ALPHABET.charCodeAt(i)] = i;

/** base64url without padding (RFC 4648 §5). */
export function b64urlEncode(bytes: Uint8Array): string {
  let out = "";
  let i = 0;
  for (; i + 2 < bytes.length; i += 3) {
    const n = (bytes[i]! << 16) | (bytes[i + 1]! << 8) | bytes[i + 2]!;
    out += ALPHABET[n >>> 18]! + ALPHABET[(n >>> 12) & 63]! + ALPHABET[(n >>> 6) & 63]! + ALPHABET[n & 63]!;
  }
  const rest = bytes.length - i;
  if (rest === 1) {
    const n = bytes[i]! << 16;
    out += ALPHABET[n >>> 18]! + ALPHABET[(n >>> 12) & 63]!;
  } else if (rest === 2) {
    const n = (bytes[i]! << 16) | (bytes[i + 1]! << 8);
    out += ALPHABET[n >>> 18]! + ALPHABET[(n >>> 12) & 63]! + ALPHABET[(n >>> 6) & 63]!;
  }
  return out;
}

/** Strict base64url decoding: no padding, no foreign characters, canonical trailing bits. */
export function b64urlDecode(text: string): Bytes {
  const bytes = tryB64urlDecode(text);
  if (!bytes) throw new TypeError("invalid base64url");
  return bytes;
}

export function tryB64urlDecode(text: string): Bytes | null {
  if (typeof text !== "string" || text.length % 4 === 1) return null;
  const out = new Uint8Array(Math.floor((text.length * 3) / 4));
  let bits = 0;
  let acc = 0;
  let o = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    const v = c < 128 ? LOOKUP[c]! : -1;
    if (v < 0) return null;
    acc = (acc << 6) | v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out[o++] = (acc >>> bits) & 0xff;
    }
    acc &= (1 << bits) - 1;
  }
  // Leftover bits must be zero, otherwise two strings would decode to the same bytes.
  if (acc !== 0) return null;
  return out;
}

const encoder = new TextEncoder();

/** UTF-8 encode into an array owned by this realm (TextEncoder may hand back a foreign-realm array). */
export function utf8(text: string): Bytes {
  return new Uint8Array(encoder.encode(text));
}

export function fromUtf8(bytes: Uint8Array): string {
  return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
}

/** Exact UTF-8 byte length without allocating (lone surrogates count as U+FFFD, 3 bytes). */
export function utf8Length(text: string): number {
  let n = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c < 0x80) n += 1;
    else if (c < 0x800) n += 2;
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < text.length && (text.charCodeAt(i + 1) & 0xfc00) === 0xdc00) {
      n += 4;
      i++;
    } else n += 3;
  }
  return n;
}

export function concatBytes(...parts: Uint8Array[]): Bytes {
  let total = 0;
  for (const p of parts) total += p.byteLength;
  const out = new Uint8Array(total);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.byteLength;
  }
  return out;
}

export function u32be(n: number): Bytes {
  const out = new Uint8Array(4);
  new DataView(out.buffer).setUint32(0, n >>> 0, false);
  return out;
}

export function toHex(bytes: Uint8Array): string {
  let out = "";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

/** Constant-time comparison for MACs and other secrets. */
export function equalBytes(a: Uint8Array, b: Uint8Array): boolean {
  if (a.byteLength !== b.byteLength) return false;
  let diff = 0;
  for (let i = 0; i < a.byteLength; i++) diff |= a[i]! ^ b[i]!;
  return diff === 0;
}

/** Cryptographically secure random bytes (never Math.random; threat model W5). */
export function randomBytes(length: number): Bytes {
  const out = new Uint8Array(length);
  for (let o = 0; o < length; o += 65536) crypto.getRandomValues(out.subarray(o, Math.min(length, o + 65536)));
  return out;
}

/** Random 128-bit identifier (UUID v4 when available). */
export function randomId(): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  return b64urlEncode(randomBytes(16));
}

export function isArrayBuffer(value: unknown): value is ArrayBuffer {
  return value instanceof ArrayBuffer || Object.prototype.toString.call(value) === "[object ArrayBuffer]";
}
