// @vitest-environment node
//
// Cross-language test vectors for the native (Rust) ferry-dc/1 implementation
// in crates/ferry-core/src/rtc. Everything is computed by *this* library from
// fixed inputs, so the Rust side can check byte-for-byte compatibility.
//
//   FERRY_WRITE_VECTORS=1 npx vitest run src/lib/rtc/vectors.gen.test.ts   # (re)write
//   npx vitest run src/lib/rtc/vectors.gen.test.ts                          # check the file is current
//
// The only non-deterministic inputs are P-256 signatures (ECDSA is randomized
// in WebCrypto): they are generated once and, in check mode, re-verified from
// the file instead of being regenerated.

import { sha256 } from "@noble/hashes/sha2.js";
import { describe, expect, it } from "vitest";
import { b64urlDecode, b64urlEncode, concatBytes, toHex, tryB64urlDecode, u32be, utf8, utf8Length } from "./bytes";
import { isValidPublicKey, shortCode, sign, verify, type Identity } from "./identity";
import {
  chunkSizeFor,
  encodeAnswer,
  encodeControl,
  encodeOffer,
  errorFrame,
  fileNameProblem,
  LARGE_CHUNK,
  MAX_CONTROL_BYTES,
  parseControl,
  parseMaxMessageSize,
  RtcError,
  SMALL_CHUNK,
  type AnswerMsg,
  type ControlMessage,
  type FileMeta,
  type OfferMsg,
} from "./protocol";
import { decodeSdp, encodeSdp, isValidRoomId, SignalingClient } from "./signaling";
import { collect, FakeWebSocket, sleep } from "./test-fakes";
import {
  authPayload,
  deriveRoomKey,
  extractFingerprint,
  roomIdFromSecret,
  roomMac,
  transcriptHash,
  verifyRoomMac,
  type Role,
  type TranscriptParts,
} from "./transcript";

const OUT = new URL("../../../../../crates/ferry-core/tests/vectors/rtc.json", import.meta.url);
const WRITE = !!(globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env.FERRY_WRITE_VECTORS;

/** The few `node:fs` calls used here (the app's tsconfig has no Node types). */
interface NodeFs {
  existsSync(path: URL): boolean;
  mkdirSync(path: URL, options: { recursive: boolean }): void;
  readFileSync(path: URL, encoding: "utf8"): string;
  writeFileSync(path: URL, data: string): void;
}
const nodeFs = async () => (await import(/* @vite-ignore */ `node:${"fs"}`)) as NodeFs;

/** JSON values (`undefined` members are dropped by JSON.stringify). */
type Json = null | undefined | boolean | number | string | Json[] | { [k: string]: Json };

const fromHex = (s: string) => Uint8Array.from(s.match(/../g) ?? [], (b) => parseInt(b, 16));

const hex = (b: Uint8Array) => toHex(b);
const fill = (n: number, v: number) => new Uint8Array(n).fill(v);
/** Bytes `i % 251` (period coprime with every power of two, so offsets show). */
const pattern = (n: number, start = 0) => Uint8Array.from({ length: n }, (_, i) => (start + i) % 251);
const sha256Hex = (data: Uint8Array | string) => hex(sha256(typeof data === "string" ? utf8(data) : data));
const isWellFormed = (s: string) => !/[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/.test(s);
/** A string the Rust side can load: JSON-safe text, or its UTF-16 code units when it holds lone surrogates. */
const jsonString = (s: string): { [k: string]: Json } =>
  isWellFormed(s) ? { text: s } : { utf16: Array.from({ length: s.length }, (_, i) => s.charCodeAt(i)) };

function rtcError(fn: () => unknown): { code: string; message: string } | null {
  try {
    fn();
    return null;
  } catch (err) {
    if (err instanceof RtcError) return { code: err.code, message: err.message };
    throw err;
  }
}

// ── Identities ────────────────────────────────────────────────────────────

/** PKCS#8 v1 prefix for a raw 32-byte Ed25519 seed (RFC 8410). */
const ED25519_PKCS8_PREFIX = Uint8Array.from([0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20]);

async function ed25519FromSeed(seed: Uint8Array): Promise<{ identity: Identity; publicKey: string }> {
  const privateKey = await crypto.subtle.importKey("pkcs8", concatBytes(ED25519_PKCS8_PREFIX, seed), { name: "Ed25519" }, true, ["sign"]);
  const jwk = await crypto.subtle.exportKey("jwk", privateKey);
  const publicKeyRaw = b64urlDecode(jwk.x!);
  const publicKey = await crypto.subtle.importKey("raw", publicKeyRaw, { name: "Ed25519" }, true, ["verify"]);
  return { identity: { alg: "ed25519", privateKey, publicKey }, publicKey: jwk.x! };
}

const SEED_OFFERER = Uint8Array.from({ length: 32 }, (_, i) => i + 1);
const SEED_ANSWERER = Uint8Array.from({ length: 32 }, (_, i) => 0xa0 + i);

// ── SDPs ──────────────────────────────────────────────────────────────────

const CHROME_FP = "6B:8B:5D:EA:59:04:20:23:29:C8:87:1C:CC:87:32:BE:DD:8C:66:A5:8E:50:55:EA:8C:D3:B6:5C:09:5E:D6:BC";
const FIREFOX_FP = "1C:B2:0D:6A:3E:51:F4:94:20:7B:0E:9D:88:2A:76:31:C5:4E:4A:9D:7F:C2:65:09:6B:1A:E0:7D:52:33:AF:C1";
const WEBRTC_RS_FP = "4E:14:87:5C:27:FC:A3:AB:53:0C:95:F7:C3:DC:28:25:0D:F2:1B:0E:D8:90:F3:4E:EA:16:92:CC:64:DC:59:36";

const CHROME_OFFER = [
  "v=0",
  "o=- 4215775240449105457 2 IN IP4 127.0.0.1",
  "s=-",
  "t=0 0",
  "a=group:BUNDLE 0",
  "a=extmap-allow-mixed",
  "a=msid-semantic: WMS",
  "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
  "c=IN IP4 0.0.0.0",
  "a=ice-ufrag:Gm0W",
  "a=ice-pwd:4gXLgmW3fs1Z6R3Py1OnUQ9a",
  "a=ice-options:trickle",
  `a=fingerprint:sha-256 ${CHROME_FP}`,
  "a=setup:actpass",
  "a=mid:0",
  "a=sctp-port:5000",
  "a=max-message-size:262144",
  "",
].join("\r\n");

const CHROME_ANSWER_WITH_CANDIDATES = [
  "v=0",
  "o=- 7362119624153473095 2 IN IP4 127.0.0.1",
  "s=-",
  "t=0 0",
  "a=group:BUNDLE 0",
  "a=extmap-allow-mixed",
  "a=msid-semantic: WMS",
  "m=application 58226 UDP/DTLS/SCTP webrtc-datachannel",
  "c=IN IP4 192.0.2.10",
  "a=candidate:2953574423 1 udp 2113937151 0b6d3a7c-5d0e-4f43-9bd8-0f4bd8b2d0c1.local 58226 typ host generation 0 network-cost 999",
  "a=candidate:842163049 1 udp 1677729535 192.0.2.10 58226 typ srflx raddr 0.0.0.0 rport 0 generation 0 network-cost 999",
  "a=ice-ufrag:x9Qe",
  "a=ice-pwd:fVb8Pf0V2o1bXy5Yy9VhCwtA",
  "a=ice-options:trickle",
  `a=fingerprint:sha-256 ${CHROME_FP.toLowerCase()}`,
  "a=setup:active",
  "a=mid:0",
  "a=sctp-port:5000",
  "a=max-message-size:262144",
  "",
].join("\r\n");

const FIREFOX_OFFER = [
  "v=0",
  "o=mozilla...THIS_IS_SDPARTA-128.0 6385218418011094843 0 IN IP4 0.0.0.0",
  "s=-",
  "t=0 0",
  "a=sendrecv",
  `a=fingerprint:sha-256 ${FIREFOX_FP}`,
  "a=group:BUNDLE 0",
  "a=ice-options:trickle",
  "a=msid-semantic:WMS *",
  "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
  "c=IN IP4 0.0.0.0",
  "a=sendrecv",
  "a=ice-pwd:e3b1f2c0a9d84f6e8b7c6d5e4f3a2b1c",
  "a=ice-ufrag:7a1c2b3d",
  "a=mid:0",
  "a=setup:actpass",
  "a=sctp-port:5000",
  "a=max-message-size:1073741823",
  "",
].join("\r\n");

const WEBRTC_RS_OFFER = [
  "v=0",
  "o=- 9204322425069737702 61748700 IN IP4 0.0.0.0",
  "s=-",
  "t=0 0",
  `a=fingerprint:sha-256 ${WEBRTC_RS_FP}`,
  "a=group:BUNDLE 0",
  "a=extmap-allow-mixed",
  "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
  "c=IN IP4 0.0.0.0",
  "a=setup:actpass",
  "a=mid:0",
  "a=sendrecv",
  "a=sctp-port:5000",
  "a=ice-ufrag:wxzizlMDpCjRwJyC",
  "a=ice-pwd:aeOjoAkiDgtLWPIIcnwGPTLHcrDwrEXT",
  "a=end-of-candidates",
  "",
].join("\r\n");

const SHA1_FP = "0a:".repeat(19) + "0a";

const FINGERPRINT_CASES: [string, string][] = [
  ["chrome offer", CHROME_OFFER],
  ["chrome answer, lowercase hex, mDNS candidates", CHROME_ANSWER_WITH_CANDIDATES],
  ["firefox offer, session-level fingerprint", FIREFOX_OFFER],
  ["webrtc-rs offer", WEBRTC_RS_OFFER],
  ["LF line endings", CHROME_OFFER.replace(/\r\n/g, "\n")],
  ["two algorithms", `${FIREFOX_OFFER}a=fingerprint:sha-1 ${SHA1_FP}\r\n`],
  ["same fingerprint at session and media level", FIREFOX_OFFER.replace("a=sendrecv\r\na=ice-pwd", `a=fingerprint:SHA-256 ${FIREFOX_FP.toLowerCase()}\r\na=sendrecv\r\na=ice-pwd`)],
  ["two certificates", `${CHROME_OFFER}a=fingerprint:sha-256 ${FIREFOX_FP}\r\n`],
  ["tabs and trailing spaces", `v=0\r\na=fingerprint:SHA-256\t${CHROME_FP}  \t\r\n`],
  ["no colons", `v=0\r\na=fingerprint:sha-256 ${CHROME_FP.replace(/:/g, "")}\r\n`],
  ["odd colons", `v=0\r\na=fingerprint:sha-256 ${CHROME_FP.replace(/:/g, "").replace(/(....)/g, "$1:")}\r\n`],
  ["too short", "v=0\r\na=fingerprint:sha-256 AB:CD\r\n"],
  ["odd number of digits", `v=0\r\na=fingerprint:sha-256 ${CHROME_FP}A\r\n`],
  ["not hex", `v=0\r\na=fingerprint:sha-256 ${CHROME_FP.replace("6B", "6G")}\r\n`],
  ["space inside the value", `v=0\r\na=fingerprint:sha-256 ${CHROME_FP.replace(":20:", ": 20:")}\r\n`],
  ["no algorithm", `v=0\r\na=fingerprint: ${CHROME_FP}\r\n`],
  ["not at line start", `v=0\r\n a=fingerprint:sha-256 ${CHROME_FP}\r\n`],
  ["prefix case matters", `v=0\r\nA=FINGERPRINT:sha-256 ${CHROME_FP}\r\n`],
  ["no fingerprint", "v=0\r\ns=-\r\n"],
  ["empty", ""],
  ["CR-only line ends", `v=0\ra=fingerprint:sha-256 ${CHROME_FP}\ra=mid:0`],
  ["unicode line separator", `v=0\u2028a=fingerprint:sha-512 ${"ab:".repeat(63)}ab\u2029m=x`],
];

const MAX_MESSAGE_SIZE_CASES: [string, string][] = [
  ["chrome", CHROME_OFFER],
  ["firefox", FIREFOX_OFFER],
  ["absent (webrtc-rs)", WEBRTC_RS_OFFER],
  ["zero = unlimited", "v=0\r\na=max-message-size:0\r\n"],
  ["small", "v=0\r\na=max-message-size:16384\r\n"],
  ["just below 64 KiB", "v=0\r\na=max-message-size:65535\r\n"],
  ["exactly 64 KiB", "v=0\r\na=max-message-size:65536\n"],
  ["trailing spaces", "v=0\r\na=max-message-size:100000 \t \r\n"],
  ["first well-formed line wins", "v=0\r\na=max-message-size:12x\r\na=max-message-size:1000\r\na=max-message-size:70000\r\n"],
  ["not a number", "v=0\r\na=max-message-size:lots\r\n"],
  ["negative", "v=0\r\na=max-message-size:-5\r\n"],
  ["huge", "v=0\r\na=max-message-size:99999999999999999999999\r\n"],
];

// ── File names ────────────────────────────────────────────────────────────

const FILE_NAMES: string[] = [
  // hostile
  "../x",
  "a/../b",
  "a/..",
  "..",
  ".",
  "...",
  "a/./b",
  "/etc/passwd",
  "/",
  "C:\\Windows\\x.exe",
  "C:x",
  "c:/x",
  "z:",
  "a:b",
  "a\\b",
  "\\\\server\\share\\x",
  "a\u0000b",
  "nul\u0000.txt",
  "line\nbreak",
  "tab\there",
  "del\u007f",
  "c1\u0085x",
  "a//b",
  "a/",
  "./a",
  " ",
  "\u00a0",
  "\u3000",
  "folder/ /x",
  ".\u200b.",
  "\u202e.\u200d.",
  ".\u00ad.",
  "..\u2060",
  "\u2028",
  "a/\u2029/b",
  "\ufeff",
  "lone\ud800",
  "\udc00tail",
  "x".repeat(256),
  `ok/${"é".repeat(128)}`,
  Array(33).fill("d").join("/"),
  "x".repeat(1025),
  "",
  // acceptable
  "photo.jpg",
  "folder/sub/file.txt",
  "naïve résumé.pdf",
  "👨‍👩‍👧 family.png",
  "Ünïcödé/ファイル.txt",
  ".bashrc",
  "a..b",
  "file.",
  "notes: draft.txt",
  "x".repeat(255),
  `ok/${"é".repeat(127)}x`,
  Array(32).fill("d").join("/"),
  "CON",
  "aux.txt",
  "COM¹.txt",
  "trailing dot.",
  "trailing space ",
  " leading space",
  "a\u200bb.txt",
  "\u202etxt.exe",
  "e\u0301.txt",
  "😀".repeat(63),
  `${"a/".repeat(31)}${"😀".repeat(50)}`,
  "a<b>c|d?e*f\".txt",
  "%2e%2e/x",
  "...a",
  "a/.b/.c",
];

// ── Control messages ──────────────────────────────────────────────────────

const KEY = b64urlEncode(fill(32, 7));
const P256_KEY = b64urlEncode(concatBytes(Uint8Array.of(4), fill(64, 9)));
const NONCE = b64urlEncode(fill(32, 1));
const SIG = b64urlEncode(fill(64, 2));
const MAC = b64urlEncode(fill(32, 3));
const HASH = "ab".repeat(32);

const ENCODE_MESSAGES: ControlMessage[] = [
  { t: "hello", v: 1, alg: "ed25519", key: KEY, nonce: NONCE, device: { alias: "Maya's laptop", deviceType: "desktop", platform: "windows" }, caps: [] },
  { t: "hello", v: 1, alg: "p256", key: P256_KEY, nonce: NONCE, device: { alias: "Chrome on Android \u{1F4F1} \"quoted\" \\ back", deviceType: "web", platform: "browser" }, caps: ["x", "y"] },
  { t: "auth", sig: SIG },
  { t: "auth", sig: SIG, mac: MAC },
  { t: "offer", transferId: "t1", files: [{ id: "0", name: "Brief.pdf", size: 1_200_000, mime: "application/pdf", modified: 1_700_000_000_123 }] },
  { t: "offer", transferId: "8c1f7d2e-35e4-4f0b-9a1e-4a3c2b1d0e9f", files: [], text: "Hi \u00b7 \u00e9t\u00e9 \u{1F600}\n\"quotes\" <tag> & \\ \u2028 \u0001" },
  { t: "offer", transferId: "t1", files: [{ id: "a", name: "Album/2026/IMG 1.jpg", size: 0, mime: "" }], more: true },
  { t: "offer", transferId: "t1", files: [{ id: "b", name: "x", size: Number.MAX_SAFE_INTEGER, mime: "application/octet-stream", modified: -1 }], text: "", more: true },
  { t: "answer", transferId: "t1", accept: ["0", "1"], offsets: {} },
  { t: "answer", transferId: "t1", accept: ["0"], offsets: { "0": 1_048_576 }, more: true },
  { t: "answer", transferId: "t1", accept: [], offsets: {}, declined: true },
  { t: "file", id: "0", offset: 0 },
  { t: "file", id: "f-9", offset: 123_456_789 },
  { t: "file-end", id: "0", sha256: HASH },
  { t: "file-ack", id: "0", ok: true, sha256: HASH },
  { t: "file-ack", id: "0", ok: false, sha256: HASH, error: "sha256 mismatch" },
  { t: "progress", transferId: "t1", bytes: 16_777_216 },
  { t: "done", transferId: "t1" },
  { t: "cancel", transferId: "t1" },
  { t: "cancel", transferId: "t1", reason: "timeout" },
  { t: "ping" },
  { t: "pong" },
  { t: "error", code: "auth", message: "peer authentication failed" },
];

const offerFrame = (files: unknown[], extra: Record<string, unknown> = {}) => JSON.stringify({ t: "offer", transferId: "t1", files, ...extra });
const metaOf = (over: Record<string, unknown> = {}) => ({ id: "f1", name: "photo.jpg", size: 10, mime: "image/jpeg", ...over });
const hello = (over: Record<string, unknown> = {}) =>
  JSON.stringify({ t: "hello", v: 1, alg: "ed25519", key: KEY, nonce: NONCE, device: { alias: "A", deviceType: "web", platform: "x" }, caps: [], ...over });
const answerFrame = (over: Record<string, unknown>) => JSON.stringify({ t: "answer", transferId: "t1", accept: ["a"], offsets: {}, ...over });

const PARSE_FRAMES: string[] = [
  // accepted
  '{"t":"ping"}',
  '{"t":"ping","extra":{"nested":[1,2]}}',
  '{"t":"pong"}',
  '{ "t" : "done" , "transferId" : "t1" }',
  '{"t":"done","transferId":"a","transferId":"b"}',
  '{"t":"file","id":"f1","offset":1e3}',
  '{"t":"file","id":"f1","offset":10.0}',
  '{"t":"file","id":"f1","offset":-0}',
  '{"t":"file","id":"f1","offset":9007199254740991}',
  '{"t":"file","id":"~!#$%&()*+,-./:;<=>?@[]^_`{|}","offset":0}',
  `{"t":"file-end","id":"f1","sha256":"${HASH}"}`,
  `{"t":"file-ack","id":"f1","ok":true}`,
  `{"t":"file-ack","id":"f1","ok":false,"sha256":"${HASH}","error":""}`,
  '{"t":"progress","transferId":"t1","bytes":0}',
  '{"t":"cancel","transferId":"t1","reason":""}',
  '{"t":"error","code":"x","message":""}',
  `{"t":"auth","sig":"${SIG}"}`,
  `{"t":"auth","sig":"${SIG}","mac":"${MAC}"}`,
  hello(),
  hello({ alg: "p256", key: P256_KEY, caps: ["a", ""], device: { alias: "", deviceType: "", platform: "", extra: 1 } }),
  offerFrame([metaOf()]),
  offerFrame([metaOf({ modified: 1_700_000_000_000, evil: "x" })], { junk: true, text: "hi" }),
  offerFrame([metaOf({ mime: "" })], { more: true }),
  offerFrame([]),
  offerFrame([metaOf({ name: "a/b/c.txt", size: 0 })], { text: "x".repeat(MAX_CONTROL_BYTES - 200) }),
  answerFrame({}),
  answerFrame({ accept: ["a", "b"], offsets: { b: 7 } }),
  answerFrame({ accept: [], declined: true }),
  answerFrame({ more: true }),
  '{"t":"answer","transferId":"t1","accept":["__proto__"],"offsets":{"__proto__":3}}',
  // rejected
  "not json",
  "",
  "[]",
  "null",
  '"ping"',
  "{}",
  '{"t":"pwn"}',
  '{"t":5}',
  '{"t":"ping"} trailing',
  '{"t":"file","id":"f1","offset":"1"}',
  '{"t":"file","id":"f1","offset":-1}',
  '{"t":"file","id":"f1","offset":1.5}',
  '{"t":"file","id":"f1","offset":9007199254740992}',
  '{"t":"file","id":"f1","offset":1e400}',
  '{"t":"file","id":"f1"}',
  '{"t":"file","id":"","offset":0}',
  '{"t":"file","id":"has space","offset":0}',
  '{"t":"file","id":"ünïcode","offset":0}',
  `{"t":"file","id":"${"x".repeat(257)}","offset":0}`,
  `{"t":"file","id":"${"x".repeat(256)}","offset":0}`,
  `{"t":"file-end","id":"f1","sha256":"${HASH.toUpperCase()}"}`,
  '{"t":"file-end","id":"f1","sha256":"abc"}',
  '{"t":"file-ack","id":"f1","ok":"yes"}',
  '{"t":"file-ack","id":"f1"}',
  `{"t":"auth","sig":"${SIG.slice(4)}"}`,
  `{"t":"auth","sig":"${SIG}","mac":"short"}`,
  `{"t":"auth","sig":"${SIG}=="}`,
  `{"t":"auth","sig":"${SIG}","mac":null}`,
  `{"t":"cancel","transferId":"t1","reason":"${"r".repeat(1025)}"}`,
  `{"t":"cancel","transferId":"t1","reason":"${"r".repeat(1024)}"}`,
  '{"t":"cancel","transferId":"t1","reason":null}',
  '{"t":"error","code":"","message":"x"}',
  `{"t":"error","code":"${"c".repeat(65)}","message":"x"}`,
  '{"t":"error","code":"x"}',
  '{"t":"progress","transferId":"t1","bytes":-5}',
  offerFrame([metaOf()], { more: false }),
  offerFrame([metaOf()], { more: null }),
  offerFrame([metaOf()], { text: 5 }),
  offerFrame([metaOf(), metaOf()]),
  offerFrame([metaOf({ id: "a", size: 2 ** 52 }), metaOf({ id: "b", size: 2 ** 52 })]),
  offerFrame([metaOf({ mime: "text/plain\r\nX-Evil: 1" })]),
  offerFrame([metaOf({ mime: "m".repeat(256) })]),
  offerFrame([metaOf({ modified: 1.5 })]),
  offerFrame([metaOf({ modified: "1" })]),
  offerFrame([metaOf({ name: "../evil" })]),
  offerFrame([metaOf({ name: "lone\ud800" })]),
  offerFrame([metaOf({ size: null })]),
  offerFrame([5]),
  '{"t":"offer","transferId":"t1"}',
  '{"t":"offer","transferId":"t1","files":{}}',
  `{"t":"offer","transferId":"t1","files":[],"text":"${"é".repeat(MAX_CONTROL_BYTES / 2)}"}`,
  JSON.stringify({ t: "cancel", transferId: "t1", reason: "x".repeat(MAX_CONTROL_BYTES) }),
  hello({ v: 2 }),
  hello({ v: "1" }),
  hello({ alg: "rsa" }),
  hello({ key: b64urlEncode(fill(31, 7)) }),
  hello({ alg: "p256", key: b64urlEncode(concatBytes(Uint8Array.of(2), fill(64, 9))) }),
  hello({ alg: "p256" }),
  hello({ nonce: b64urlEncode(fill(16, 1)) }),
  hello({ caps: Array(65).fill("c") }),
  hello({ caps: ["c".repeat(65)] }),
  hello({ caps: [1] }),
  hello({ device: { alias: "a".repeat(257), deviceType: "web", platform: "x" } }),
  hello({ device: { alias: "a", deviceType: "w".repeat(33), platform: "x" } }),
  hello({ device: { alias: "a", deviceType: "web", platform: "p".repeat(65) } }),
  hello({ device: null }),
  answerFrame({ accept: ["a", "a"] }),
  answerFrame({ offsets: { b: 1 } }),
  answerFrame({ offsets: { a: -1 } }),
  answerFrame({ offsets: [] }),
  answerFrame({ declined: true }),
  answerFrame({ accept: [], declined: true, more: true }),
  answerFrame({ declined: false }),
  answerFrame({ accept: "a" }),
  answerFrame({ accept: [""] }),
];

// ── Split offers / answers ────────────────────────────────────────────────

const splitOfferFiles = (): FileMeta[] =>
  Array.from({ length: 3000 }, (_, i) => ({
    id: `file-${i}`,
    name: `Holiday ${i % 7}/IMG_${String(i).padStart(5, "0")} \u00b7 ${"\u00e4".repeat(40)}.jpg`,
    size: i * 1000,
    mime: "image/jpeg",
    ...(i % 3 === 0 ? { modified: 1_700_000_000_000 + i } : {}),
  }));

const splitAnswer = (): AnswerMsg => {
  const accept = Array.from({ length: 10_000 }, (_, i) => `file-${i}-${"x".repeat(20)}`);
  const offsets: Record<string, number> = {};
  for (let i = 0; i < accept.length; i += 7) offsets[accept[i]!] = i + 1;
  return { t: "answer", transferId: "t1", accept, offsets };
};

const frameDigest = (frames: string[]) => frames.map((f) => ({ bytes: utf8Length(f), sha256: sha256Hex(f) }));

// ── Signaling ─────────────────────────────────────────────────────────────

const SDP_FOR_ENCODING = [CHROME_OFFER, FIREFOX_OFFER, `${WEBRTC_RS_OFFER}a=x-unicode:\u00e9\u{1F600}\r\n`, ""];

const sigPeer = (id: string, extra: Record<string, unknown> = {}) => ({ id, alias: `Peer ${id}`, version: "2.2", token: `t-${id}`, ...extra });

async function signalingVectors(): Promise<Json> {
  FakeWebSocket.instances.length = 0;
  const c = new SignalingClient({
    url: "wss://signal.example/v1/ws",
    info: { alias: `Maya's laptop ${"\u{1F600}".repeat(70)}`, deviceType: "desktop", deviceModel: "Windows", token: "tok-1", publicKey: KEY },
    WebSocket: FakeWebSocket,
    random: () => 0,
  });
  const url = c.connectUrl();
  const d = new URL(url).searchParams.get("d")!;
  const nearbyOff = new SignalingClient({ url: "ws://127.0.0.1:39000/v1/ws?x=1", info: { alias: "Kiosk", token: "t", publicKey: P256_KEY, nearby: false }, WebSocket: FakeWebSocket });

  c.joinRoom(`r:${"A".repeat(22)}`);
  c.connect();
  const ws = FakeWebSocket.last;
  ws.serverOpen();
  const events: { type: string; payload: unknown }[] = [];
  for (const type of ["hello", "join", "update", "left", "offer", "answer", "ice", "cancel", "roomHello", "roomPeerJoined", "roomPeerLeft", "error"] as const) {
    c.on(type, (payload) => events.push({ type, payload }));
  }
  const inbound: string[] = [
    JSON.stringify({
      type: "HELLO",
      client: sigPeer("me", { ext: { v: 1, caps: ["rooms", "trickle", "ferry-dc"], key: KEY } }),
      peers: [sigPeer("p1", { deviceType: "DESKTOP", deviceModel: "Windows", ext: { v: 1, caps: ["ferry-dc"], key: KEY, nearby: true, extra: 1 } }), { junk: true }, sigPeer("p2", { ext: { v: 1, caps: [5], key: KEY } })],
      server: { v: 1, caps: ["rooms", 5, "turn"] },
    }),
    JSON.stringify({ type: "JOIN", peer: sigPeer("p3", { deviceType: "WEB", ext: { v: 1, caps: [], key: P256_KEY } }) }),
    JSON.stringify({ type: "UPDATE", peer: sigPeer("p3", { alias: "Renamed" }) }),
    JSON.stringify({ type: "LEFT", peerId: "p3" }),
    JSON.stringify({ type: "OFFER", peer: sigPeer("p1"), sessionId: "s1", sdp: await encodeSdp(CHROME_OFFER) }),
    JSON.stringify({ type: "ANSWER", peer: sigPeer("p1"), sessionId: "s1", sdp: (await encodeSdp(FIREFOX_OFFER)).replace(/-/g, "+").replace(/_/g, "/") + "==" }),
    JSON.stringify({ type: "OFFER", peer: sigPeer("p1"), sessionId: "s2", sdp: "!!!not-base64" }),
    JSON.stringify({ type: "ICE", peer: sigPeer("p1"), sessionId: "s1", candidate: { candidate: "candidate:1 1 udp 2122260223 192.0.2.1 50000 typ host", sdpMid: "0", sdpMLineIndex: 0, usernameFragment: "abcd" } }),
    JSON.stringify({ type: "ICE", peer: sigPeer("p1"), sessionId: "s1", candidate: { candidate: "candidate:3 1 udp 1 192.0.2.3 3 typ host", sdpMid: null, sdpMLineIndex: 1.5, usernameFragment: 7 } }),
    JSON.stringify({ type: "ICE", peer: sigPeer("p1"), sessionId: "s1", candidate: null }),
    JSON.stringify({ type: "ICE", peer: sigPeer("p1"), sessionId: "s1", candidate: "candidate:2 1 udp 1 192.0.2.9 9 typ host" }),
    JSON.stringify({ type: "ICE", peer: sigPeer("p1"), sessionId: "s1", candidate: "not a candidate" }),
    JSON.stringify({ type: "ICE", peer: sigPeer("p1"), sessionId: "s1", candidate: 42 }),
    JSON.stringify({ type: "ICE", peer: sigPeer("p1"), sessionId: "s1", candidate: { sdpMid: "0" } }),
    JSON.stringify({ type: "ICE", peer: sigPeer("p1"), sessionId: "s1" }),
    JSON.stringify({ type: "CANCEL", peer: sigPeer("p1"), sessionId: "s1" }),
    JSON.stringify({ type: "ROOM_HELLO", room: `r:${"A".repeat(22)}`, peers: [sigPeer("p4", { ext: { v: 1, caps: [], key: KEY } })] }),
    JSON.stringify({ type: "ROOM_PEER_JOINED", room: `r:${"A".repeat(22)}`, peer: sigPeer("p5") }),
    JSON.stringify({ type: "ROOM_PEER_LEFT", room: `r:${"A".repeat(22)}`, peerId: "p5" }),
    JSON.stringify({ type: "ERROR", code: 404, message: "peer not found", sessionId: "s1" }),
    JSON.stringify({ type: "ERROR", code: 409, message: "room is full", room: `r:${"A".repeat(22)}` }),
    JSON.stringify({ type: "ERROR", code: "x", message: 5 }),
    JSON.stringify({ type: "PONG" }),
    JSON.stringify({ type: "SOMETHING_NEW" }),
    JSON.stringify({ type: "JOIN" }),
    JSON.stringify({ type: "JOIN", peer: { id: 5 } }),
    JSON.stringify({ type: "LEFT", peerId: 5 }),
    "not json",
    '"bare string"',
    "[]",
  ];
  const results: { frame: string; events: Json }[] = [];
  for (const frame of inbound) {
    const before = events.length;
    ws.serverSend(frame);
    await sleep(5);
    results.push({ frame, events: JSON.parse(JSON.stringify(events.slice(before))) as Json });
  }

  // Outbound forms, in the order they reach the server.
  const sentBefore = ws.sent.length;
  await c.sendOffer("p1", "s1", CHROME_OFFER);
  await c.sendAnswer("p1", "s1", FIREFOX_OFFER);
  await c.sendIce("p1", "s1", { candidate: "candidate:1 1 udp 2122260223 192.0.2.1 50000 typ host", sdpMid: "0", sdpMLineIndex: 0, usernameFragment: "abcd" });
  await c.sendIce("p1", "s1", { candidate: "candidate:1 1 udp 2122260223 192.0.2.1 50000 typ host" });
  await c.sendIce("p1", "s1", null);
  await c.sendCancel("p1", "s1");
  c.joinRoom(`r:${"B".repeat(22)}`);
  c.leaveRoom(`r:${"B".repeat(22)}`);
  c.update({ alias: "Renamed" });
  await sleep(5);
  const outbound = ws.sent.slice(sentBefore).map((text) => {
    const msg = JSON.parse(text) as Record<string, unknown>;
    // Compressed SDP bytes depend on the zlib implementation: record the plain text instead.
    return typeof msg.sdp === "string" ? { text: JSON.stringify({ ...msg, sdp: "<sdp>" }), sdpDecodesTo: msg.type === "OFFER" ? "chrome offer" : "firefox offer" } : { text };
  });
  c.close();
  nearbyOff.close();
  void collect;

  const sdps: Json[] = [];
  for (const sdp of SDP_FOR_ENCODING) {
    const encoded = await encodeSdp(sdp);
    sdps.push({ sdp, encoded, zlibHeader: hex(b64urlDecode(encoded).subarray(0, 2)) });
  }
  const tooLarge = await encodeSdp("a".repeat(300 * 1024));
  let tooLargeRejected = false;
  try {
    await decodeSdp(tooLarge);
  } catch {
    tooLargeRejected = true;
  }

  const roomIds = [`r:${"A".repeat(22)}`, `r:${"a".repeat(16)}`, `r:${"_-".repeat(32)}`, "c:123456", `r:${"A".repeat(15)}`, `r:${"A".repeat(65)}`, "r:AAAAAAAAAAAAAAAAAAAAA=", "c:12345", "c:1234567", "c:12345a", "x:123456", "", "r:"];

  return {
    clientInfo: { url, d, json: new TextDecoder().decode(b64urlDecode(d)) },
    clientInfoNearbyOff: { url: nearbyOff.connectUrl(), json: new TextDecoder().decode(b64urlDecode(new URL(nearbyOff.connectUrl()).searchParams.get("d")!)) },
    inbound: results,
    outbound,
    sdp: sdps,
    sdpBombRejected: tooLargeRejected,
    sdpBomb: { decodedBytes: 300 * 1024, limit: 256 * 1024 },
    roomIds: roomIds.map((room) => ({ room, valid: isValidRoomId(room) })),
  };
}

// ── The generator ─────────────────────────────────────────────────────────

async function generate(stored: Record<string, Json> | null): Promise<Record<string, Json>> {
  const offerer = await ed25519FromSeed(SEED_OFFERER);
  const answerer = await ed25519FromSeed(SEED_ANSWERER);

  // Ed25519: deterministic signatures over a few messages.
  const messages = [new Uint8Array(0), utf8("ferry"), pattern(1000)];
  const ed25519: Json[] = [];
  for (const [seed, id] of [
    [SEED_OFFERER, offerer],
    [SEED_ANSWERER, answerer],
  ] as const) {
    const sigs: Json[] = [];
    for (const data of messages) {
      const sig = await sign(id.identity, data);
      expect(await verify("ed25519", id.publicKey, data, sig)).toBe(true);
      sigs.push({ data: hex(data), sig });
    }
    ed25519.push({ seed: hex(seed), publicKey: id.publicKey, pkcs8: hex(concatBytes(ED25519_PKCS8_PREFIX, seed)), signatures: sigs });
  }

  // P-256: WebCrypto signatures are randomized, so they are made once and re-verified.
  let p256: Json;
  if (stored && !WRITE) {
    p256 = stored.identity && (stored.identity as Record<string, Json>).p256;
    for (const v of p256 as { publicKey: string; data: string; sig: string; valid: boolean }[]) {
      expect(await verify("p256", v.publicKey, fromHex(v.data), v.sig), JSON.stringify(v)).toBe(v.valid);
    }
  } else {
    const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
    const identity: Identity = { alg: "p256", privateKey: pair.privateKey, publicKey: pair.publicKey };
    const publicKey = b64urlEncode(new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey)));
    const out: Json[] = [];
    for (const data of messages) {
      const sig = await sign(identity, data);
      out.push({ publicKey, data: hex(data), sig, valid: true });
      const bad = b64urlDecode(sig);
      bad[5]! ^= 0x40;
      out.push({ publicKey, data: hex(data), sig: b64urlEncode(bad), valid: false });
      out.push({ publicKey, data: hex(concatBytes(data, Uint8Array.of(0))), sig, valid: false });
    }
    // An auth signature over a real transcript, as a P-256 browser peer would send it.
    const t = transcriptHash({
      sessionId: "p256-session",
      fpOfferer: `sha-256 ${CHROME_FP}`,
      fpAnswerer: `sha-256 ${WEBRTC_RS_FP}`,
      nonceOfferer: fill(32, 0x11),
      nonceAnswerer: fill(32, 0x22),
      keyOfferer: b64urlDecode(publicKey),
      keyAnswerer: b64urlDecode(answerer.publicKey),
    });
    const payload = authPayload("offerer", t);
    out.push({ publicKey, data: hex(payload), sig: await sign(identity, payload), valid: true, note: "auth payload of an offerer" });
    p256 = out;
  }
  const verifyRejects: Json[] = [];
  for (const [alg, key, sig, note] of [
    ["ed25519", "!!", SIG, "key not base64url"],
    ["ed25519", offerer.publicKey, SIG.slice(0, -2), "short signature"],
    ["p256", offerer.publicKey, SIG, "Ed25519 key passed as P-256"],
    ["ed25519", P256_KEY, SIG, "P-256 key passed as Ed25519"],
    ["rsa", offerer.publicKey, SIG, "unknown algorithm"],
    ["p256", b64urlEncode(concatBytes(Uint8Array.of(4), fill(64, 0))), SIG, "P-256 point not on the curve"],
  ] as const) {
    verifyRejects.push({ alg, key, data: "00", sig, valid: await verify(alg as "ed25519", key, Uint8Array.of(0), sig), note });
  }
  const keyChecks: Json[] = [];
  for (const [alg, raw] of [
    ["ed25519", fill(32, 1)],
    ["ed25519", fill(33, 1)],
    ["p256", concatBytes(Uint8Array.of(4), fill(64, 1))],
    ["p256", concatBytes(Uint8Array.of(3), fill(64, 1))],
    ["p256", concatBytes(Uint8Array.of(4), fill(32, 1))],
  ] as const) {
    keyChecks.push({ alg, key: hex(raw), valid: isValidPublicKey(alg, raw) });
  }

  // Transcript for both roles.
  const fpO = extractFingerprint(CHROME_OFFER)!;
  const fpA = extractFingerprint(WEBRTC_RS_OFFER)!;
  const transcripts: Json[] = [];
  for (const [sessionId, roomSecret] of [
    ["8f3c2b1a-0d4e-4c6b-9a7f-1e2d3c4b5a69", Uint8Array.from({ length: 16 }, (_, i) => i * 17)],
    ["s\u00e9ssion-\u{1F600}", fill(32, 0xee)],
  ] as const) {
    const parts: TranscriptParts = {
      sessionId,
      fpOfferer: fpO,
      fpAnswerer: fpA,
      nonceOfferer: pattern(32, 1),
      nonceAnswerer: pattern(32, 100),
      keyOfferer: b64urlDecode(offerer.publicKey),
      keyAnswerer: b64urlDecode(answerer.publicKey),
    };
    const enc = (v: string | Uint8Array) => {
      const bytes = typeof v === "string" ? utf8(v) : v;
      return concatBytes(u32be(bytes.byteLength), bytes);
    };
    const input = concatBytes(
      enc("ferry-dc/1"),
      enc(parts.sessionId),
      enc(parts.fpOfferer),
      enc(parts.fpAnswerer),
      enc(parts.nonceOfferer),
      enc(parts.nonceAnswerer),
      enc(parts.keyOfferer),
      enc(parts.keyAnswerer),
    );
    const t = transcriptHash(parts);
    expect(hex(t)).toBe(sha256Hex(input));
    const roomKey = deriveRoomKey(roomSecret);
    const mac = roomMac(roomKey, t);
    expect(verifyRoomMac(roomKey, t, mac)).toBe(true);
    const roles: Role[] = ["offerer", "answerer"];
    const payload: Record<string, Json> = {};
    const sigs: Record<string, Json> = {};
    for (const role of roles) {
      const p = authPayload(role, t);
      payload[role] = hex(p);
      sigs[role] = await sign((role === "offerer" ? offerer : answerer).identity, p);
    }
    const helloOf = (role: Role) =>
      encodeControl({
        t: "hello",
        v: 1,
        alg: "ed25519",
        key: role === "offerer" ? offerer.publicKey : answerer.publicKey,
        nonce: b64urlEncode(role === "offerer" ? parts.nonceOfferer : parts.nonceAnswerer),
        device: role === "offerer" ? { alias: "Maya's laptop", deviceType: "desktop", platform: "windows" } : { alias: "Chrome on Windows", deviceType: "web", platform: "browser" },
        caps: [],
      });
    transcripts.push({
      sessionId,
      fpOfferer: fpO,
      fpAnswerer: fpA,
      nonceOfferer: hex(parts.nonceOfferer),
      nonceAnswerer: hex(parts.nonceAnswerer),
      keyOfferer: offerer.publicKey,
      keyAnswerer: answerer.publicKey,
      input: hex(input),
      transcript: hex(t),
      shortCode: shortCode(t),
      authPayload: payload,
      signature: sigs,
      helloOfferer: helloOf("offerer"),
      helloAnswerer: helloOf("answerer"),
      authOfferer: encodeControl({ t: "auth", sig: sigs.offerer as string }),
      authAnswererInRoom: encodeControl({ t: "auth", sig: sigs.answerer as string, mac }),
      roomSecret: hex(roomSecret),
      roomSecretB64: b64urlEncode(roomSecret),
      roomKey: hex(roomKey),
      mac,
      roomId: roomIdFromSecret(roomSecret),
      wrongSecretMac: roomMac(deriveRoomKey(fill(16, 1)), t),
    });
  }

  const shortCodes: Json[] = [];
  for (const t of [Uint8Array.of(0, 0, 0, 42, 9, 9), Uint8Array.of(0xff, 0xff, 0xff, 0xff), Uint8Array.of(0, 0x0f, 0x42, 0x40), fill(32, 0x80), sha256(utf8("ferry"))]) {
    shortCodes.push({ transcript: hex(t), code: shortCode(t) });
  }

  const rooms: Json[] = [];
  for (const secret of [fill(16, 0), Uint8Array.from({ length: 16 }, (_, i) => i), fill(32, 0xff), pattern(64, 3)]) {
    const id = roomIdFromSecret(secret);
    expect(isValidRoomId(id)).toBe(true);
    rooms.push({ secret: hex(secret), secretB64: b64urlEncode(secret), roomId: id, fragment: `#room=${b64urlEncode(secret)}`, roomKey: hex(deriveRoomKey(secret)) });
  }

  const base64url: Json = {
    encode: [new Uint8Array(0), Uint8Array.of(0), Uint8Array.of(0xfb, 0xff), Uint8Array.of(1, 2, 3), pattern(31)].map((b) => ({ hex: hex(b), b64: b64urlEncode(b) })),
    decode: ["", "AA", "AAA", "AAAA", "-_8", "_w", "AB", "AQ", "AQI", "AQJ", "A", "AA==", "a+b/", "é", "AA AA", "AAAAA"].map((s) => {
      const b = tryB64urlDecode(s);
      return { text: s, hex: b ? hex(b) : null };
    }),
  };

  const fingerprints = FINGERPRINT_CASES.map(([name, sdp]) => ({ name, sdp, fingerprint: extractFingerprint(sdp) }));
  const maxMessageSizes = MAX_MESSAGE_SIZE_CASES.map(([name, sdp]) => {
    const n = parseMaxMessageSize(sdp);
    return { name, sdp, maxMessageSize: Number.isFinite(n) ? n : null, unlimited: !Number.isFinite(n), chunkSize: chunkSizeFor(n) };
  });
  const chunkSizes = [undefined, 0, 1, 16384, 65535, 65536, 262144, 1073741823].map((n) => ({ maxMessageSize: n ?? null, chunkSize: chunkSizeFor(n) }));
  expect(chunkSizeFor(Infinity)).toBe(LARGE_CHUNK);
  expect(SMALL_CHUNK).toBe(16384);

  const encode = ENCODE_MESSAGES.map((msg) => ({ json: encodeControl(msg) }));
  const errorFrames = [
    ["auth", "peer authentication failed"],
    ["", "x"],
    ["c".repeat(100), "m".repeat(5000)],
    ["protocol", "\u{1F600}".repeat(600)],
  ].map(([code, message]) => ({ code: code!, message: message!, json: encodeControl(errorFrame(code!, message!)) }));

  const parse = PARSE_FRAMES.map((text) => {
    try {
      const msg = parseControl(text);
      return { frame: text, ok: true, parsed: JSON.parse(JSON.stringify(msg)) as Json, reencoded: encodeControl(msg) };
    } catch (err) {
      if (!(err instanceof RtcError)) throw err;
      return { frame: text, ok: false, code: err.code, error: err.message };
    }
  });

  const fileNames = FILE_NAMES.map((name) => ({ ...jsonString(name), problem: fileNameProblem(name) }));

  // Offers/answers: single frames, splits and the size limits.
  const answers = [
    { t: "answer", transferId: "t1", accept: ["b", "10", "a", "2", "01", "4294967294", "4294967295", "0"], offsets: { b: 1, "10": 2, a: 3, "2": 4, "01": 5, "4294967294": 6, "4294967295": 7, "0": 8 } },
    { t: "answer", transferId: "t1", accept: ["x", "y"], offsets: { x: 0, y: 9 } },
    { t: "answer", transferId: "t1", accept: ["x"], offsets: {}, declined: true },
  ] as AnswerMsg[];
  const smallAnswers = answers.map((a) => ({ input: { accept: a.accept, offsets: a.offsets, declined: !!a.declined }, frames: encodeAnswer(a) }));
  const smallOffer = { t: "offer", transferId: "t1", files: [metaOf(), metaOf({ id: "f2", name: "dir/x.bin", size: 5, mime: "application/octet-stream", modified: 42 })], text: "hello" } as OfferMsg;

  const bigOffer: OfferMsg = { t: "offer", transferId: "t1", files: splitOfferFiles(), text: "for you" };
  const offerFrames = encodeOffer(bigOffer);
  const answerFrames = encodeAnswer(splitAnswer());
  // A text that just fits one frame next to files, and one that is too long.
  const textLimits = [
    { text: "x".repeat(60 * 1024 - 2), error: rtcError(() => encodeOffer({ t: "offer", transferId: "t1", files: [], text: "x".repeat(60 * 1024 - 2) })) },
    { text: "x".repeat(70_000), error: rtcError(() => encodeOffer({ t: "offer", transferId: "t1", files: [], text: "x".repeat(70_000) })) },
  ];
  const tooBig = rtcError(() =>
    encodeOffer({ t: "offer", transferId: "t1", files: Array.from({ length: 10_000 }, (_, i) => ({ id: `f${i}`, name: `${"n".repeat(900)}${i}`, size: 1, mime: "" })) }),
  );

  // Binary framing of one file: `file` → chunks → `file-end` (and a resumed one).
  const binary: Json[] = [];
  for (const [size, offset, chunk] of [
    [200_000, 0, LARGE_CHUNK],
    [200_000, 0, SMALL_CHUNK],
    [200_000, 70_000, LARGE_CHUNK],
    [0, 0, LARGE_CHUNK],
    [3 * 1024 * 1024 + 5, 1024 * 1024, LARGE_CHUNK],
  ] as const) {
    const data = pattern(size);
    // Senders read 1 MiB blocks and cut each block into chunks.
    const chunks: number[] = [];
    for (let block = offset; block < size; block += 1024 * 1024) {
      const end = Math.min(size, block + 1024 * 1024);
      for (let o = block; o < end; o += chunk) chunks.push(Math.min(end, o + chunk) - o);
    }
    binary.push({
      size,
      offset,
      chunkSize: chunk,
      data: "byte i = i % 251",
      fileMsg: encodeControl({ t: "file", id: "0", offset }),
      chunks: chunks.length > 64 ? { count: chunks.length, first: chunks.slice(0, 3), last: chunks.slice(-3), total: chunks.reduce((a, b) => a + b, 0) } : chunks,
      firstChunkHead: hex(data.subarray(offset, Math.min(size, offset + 8))),
      sha256: sha256Hex(data),
      fileEnd: encodeControl({ t: "file-end", id: "0", sha256: sha256Hex(data) }),
    });
  }

  return {
    version: 1,
    generator: "apps/app/src/lib/rtc/vectors.gen.test.ts (FERRY_WRITE_VECTORS=1 npx vitest run src/lib/rtc/vectors.gen.test.ts)",
    limits: {
      maxControlBytes: MAX_CONTROL_BYTES,
      largeChunk: LARGE_CHUNK,
      smallChunk: SMALL_CHUNK,
    },
    base64url,
    identity: { ed25519, p256, verifyRejects, keyChecks },
    transcript: transcripts,
    shortCodes,
    rooms,
    fingerprints,
    maxMessageSizes,
    chunkSizes,
    control: { encode, errorFrames, parse },
    fileNames,
    split: {
      smallOffer: { input: smallOffer as unknown as Json, frames: encodeOffer(smallOffer) },
      smallAnswers,
      offer: {
        rule: "3000 files: id `file-${i}`, name `Holiday ${i % 7}/IMG_${pad5(i)} \u00b7 ${'\u00e4' x 40}.jpg`, size i*1000, mime image/jpeg, modified 1700000000000+i when i % 3 == 0; text 'for you'",
        firstFiles: bigOffer.files.slice(0, 4) as unknown as Json,
        frames: frameDigest(offerFrames),
        firstFrameHead: offerFrames[0]!.slice(0, 300),
      },
      answer: {
        rule: "accept 10000 ids `file-${i}-${'x' x 20}`; offsets[id] = i + 1 for every 7th id (i % 7 == 0)",
        frames: frameDigest(answerFrames),
        firstFrameHead: answerFrames[0]!.slice(0, 300),
      },
      textLimits: textLimits.map((t) => ({ textLength: t.text.length, error: t.error })),
      offerTooLarge: tooBig,
    },
    binary,
    signaling: await signalingVectors(),
  };
}

describe("cross-language vectors (crates/ferry-core/tests/vectors/rtc.json)", () => {
  it(WRITE ? "writes the vectors" : "are up to date", async () => {
    const fs = await nodeFs();
    const stored = fs.existsSync(OUT) ? (JSON.parse(fs.readFileSync(OUT, "utf8")) as Record<string, Json>) : null;
    const vectors = await generate(stored);
    const text = JSON.stringify(vectors, null, 1) + "\n";
    // Every string must survive a strict JSON parser (no lone surrogates).
    const strings: string[] = [];
    const walk = (v: unknown): void => {
      if (typeof v === "string") strings.push(v);
      else if (Array.isArray(v)) v.forEach(walk);
      else if (v && typeof v === "object") for (const [k, x] of Object.entries(v)) (strings.push(k), walk(x));
    };
    walk(JSON.parse(text));
    expect(strings.filter((s) => !isWellFormed(s))).toEqual([]);
    expect((vectors.fileNames as Json[]).length).toBeGreaterThanOrEqual(40);
    if (WRITE) {
      fs.mkdirSync(new URL(".", OUT), { recursive: true });
      fs.writeFileSync(OUT, text);
      return;
    }
    expect(stored, `missing ${OUT.pathname}: run with FERRY_WRITE_VECTORS=1`).not.toBeNull();
    expect(vectors).toEqual(stored);
  }, 60_000);
});
