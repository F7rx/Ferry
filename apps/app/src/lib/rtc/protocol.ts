// ferry-dc/1 data-channel messages (05-protocol.md §5.2). Text frames carry
// the JSON control messages below; binary frames carry the bytes of the file
// currently being streamed. Every inbound control frame is validated strictly
// (types, lengths, ranges, file names) before the session acts on it; unknown
// members are dropped, unknown message types are rejected.

import { tryB64urlDecode, utf8Length } from "./bytes";
import { isIdentityAlg, isValidPublicKey, SIGNATURE_LENGTH, type IdentityAlg } from "./identity";

export const DC_LABEL = "ferry/1";
export const DC_VERSION = 1;
export const DC_CAPS: readonly string[] = [];

/** Limits (05-protocol.md §5.2). */
export const MAX_CONTROL_BYTES = 64 * 1024;
export const MAX_FILES = 10_000;
/** All frames of one (split) offer together. */
export const MAX_OFFER_BYTES = 8 * 1024 * 1024;
export const MAX_NAME_LENGTH = 1024;
/** Folder nesting accepted in a file name (`a/b/c.txt` has depth 3). */
export const MAX_PATH_DEPTH = 32;
/** UTF-8 bytes per path component (the common file-system limit). */
export const MAX_COMPONENT_BYTES = 255;
export const MAX_ID_LENGTH = 256;
export const MAX_MIME_LENGTH = 255;
export const MAX_REASON_LENGTH = 1024;
/** Offer `text`, measured as its JSON string literal in UTF-8 (always fits the first offer frame). */
export const MAX_TEXT_BYTES = 60 * 1024;
export const MAX_CODE_LENGTH = 64;
export const MAX_ALIAS_LENGTH = 256;
export const MAX_DEVICE_TYPE_LENGTH = 32;
export const MAX_PLATFORM_LENGTH = 64;
export const MAX_CAPS = 64;
export const MAX_CAP_LENGTH = 64;
/** Largest binary frame accepted (Chrome's SCTP limit); senders use ≤ 64 KiB. */
export const MAX_BINARY_FRAME = 256 * 1024;
export const NONCE_LENGTH = 32;
export const MAC_LENGTH = 32;

/** Chunking and backpressure (05-protocol.md §5.2). */
export const LARGE_CHUNK = 64 * 1024;
export const SMALL_CHUNK = 16 * 1024;
export const BUFFER_HIGH_WATER = 1024 * 1024;
export const BUFFER_LOW_WATER = 256 * 1024;
/** Flow control: file bytes a sender may have in flight beyond the receiver's last `progress`. */
export const RECV_WINDOW = 16 * 1024 * 1024;
/** Receivers report `progress` at least every this many processed bytes. */
export const PROGRESS_STEP = 1024 * 1024;

export class RtcError extends Error {
  readonly code: string;
  constructor(code: string, message?: string) {
    super(message ?? code);
    this.name = "RtcError";
    this.code = code;
  }
}

export interface DeviceInfo {
  alias: string;
  deviceType: string;
  platform: string;
}

export interface FileMeta {
  id: string;
  /** Relative path, `/`-separated (`folder/sub/file.ext`); see `fileNameProblem`. */
  name: string;
  size: number;
  mime: string;
  /** Last modification time, milliseconds since the Unix epoch. */
  modified?: number;
}

export interface HelloMsg {
  t: "hello";
  v: 1;
  alg: IdentityAlg;
  key: string;
  nonce: string;
  device: DeviceInfo;
  caps: string[];
}
export interface AuthMsg {
  t: "auth";
  sig: string;
  mac?: string;
}
/** One frame of an offer. All frames but the last carry `more: true`; `text` only travels in the first. */
export interface OfferMsg {
  t: "offer";
  transferId: string;
  files: FileMeta[];
  text?: string;
  more?: true;
}
/** One frame of an answer (split like offers). `offsets` only name ids accepted in the same frame. */
export interface AnswerMsg {
  t: "answer";
  transferId: string;
  accept: string[];
  offsets: Record<string, number>;
  declined?: true;
  more?: true;
}
export interface FileMsg {
  t: "file";
  id: string;
  offset: number;
}
export interface FileEndMsg {
  t: "file-end";
  id: string;
  /** Lowercase hex SHA-256 of the whole file. */
  sha256: string;
}
export interface FileAckMsg {
  t: "file-ack";
  id: string;
  ok: boolean;
  sha256?: string;
  error?: string;
}
/** Receiver → sender: file bytes of this transfer received and processed so far (flow control). */
export interface ProgressMsg {
  t: "progress";
  transferId: string;
  bytes: number;
}
export interface DoneMsg {
  t: "done";
  transferId: string;
}
export interface CancelMsg {
  t: "cancel";
  transferId: string;
  reason?: string;
}
export interface PingMsg {
  t: "ping";
}
export interface PongMsg {
  t: "pong";
}
/** Sent right before closing the channel because of a fatal error. */
export interface ErrorMsg {
  t: "error";
  code: string;
  message: string;
}

export type ControlMessage =
  | HelloMsg
  | AuthMsg
  | OfferMsg
  | AnswerMsg
  | FileMsg
  | FileEndMsg
  | FileAckMsg
  | ProgressMsg
  | DoneMsg
  | CancelMsg
  | PingMsg
  | PongMsg
  | ErrorMsg;

/** Serializes a control message, enforcing the 64 KiB frame limit. */
export function encodeControl(msg: ControlMessage): string {
  const text = JSON.stringify(msg);
  if (utf8Length(text) > MAX_CONTROL_BYTES) {
    throw new RtcError("too-large", `"${msg.t}" message exceeds ${MAX_CONTROL_BYTES} bytes`);
  }
  return text;
}

/** An `error` message with code and message clipped to the limits the peer enforces. */
export function errorFrame(code: string, message: string): ErrorMsg {
  return { t: "error", code: code.slice(0, MAX_CODE_LENGTH) || "internal", message: message.slice(0, MAX_REASON_LENGTH) };
}

/**
 * Encodes an offer as one or more frames of at most 64 KiB each: files are
 * packed greedily, `text` goes into the first frame, every frame but the last
 * carries `more: true`. Throws `RtcError("too-large")` beyond `MAX_OFFER_BYTES`.
 */
export function encodeOffer(offer: OfferMsg): string[] {
  const head = (first: boolean, more: boolean): OfferMsg => {
    const msg: OfferMsg = { t: "offer", transferId: offer.transferId, files: [] };
    if (first && offer.text !== undefined) msg.text = offer.text;
    if (more) msg.more = true;
    return msg;
  };
  const frames = pack(offer.files, (f) => JSON.stringify(f), (first, more, items) => {
    const msg = head(first, more);
    msg.files = items;
    return msg;
  }, (first) => head(first, true));
  checkTotal(frames, "offer");
  return frames;
}

/** Encodes an answer as one or more frames (see `encodeOffer`); each accepted id keeps its offset in its frame. */
export function encodeAnswer(answer: AnswerMsg): string[] {
  if (answer.declined) return [encodeControl({ t: "answer", transferId: answer.transferId, accept: [], offsets: {}, declined: true })];
  const offsetOf = (id: string) => (Object.hasOwn(answer.offsets, id) ? answer.offsets[id]! : 0);
  const build = (more: boolean, ids: string[]): AnswerMsg => {
    const offsets = Object.create(null) as Record<string, number>;
    for (const id of ids) if (offsetOf(id) > 0) offsets[id] = offsetOf(id);
    const msg: AnswerMsg = { t: "answer", transferId: answer.transferId, accept: ids, offsets };
    if (more) msg.more = true;
    return msg;
  };
  // Cost of one id: `"id"` in `accept` plus `"id":n` in `offsets` (plus separators).
  const frames = pack(
    answer.accept,
    (id) => JSON.stringify(id) + (offsetOf(id) > 0 ? "," + JSON.stringify(id) + ":" + offsetOf(id) : ""),
    (_first, more, ids) => build(more, ids),
    () => build(true, []),
  );
  checkTotal(frames, "answer");
  return frames;
}

/** Greedy packing of `items` into frames ≤ MAX_CONTROL_BYTES. `cost` approximates an item's encoded size from above. */
function pack<T>(
  items: readonly T[],
  cost: (item: T) => string,
  build: (first: boolean, more: boolean, items: T[]) => ControlMessage,
  empty: (first: boolean) => ControlMessage,
): string[] {
  const frames: string[] = [];
  let batch: T[] = [];
  let first = true;
  let budget = MAX_CONTROL_BYTES - utf8Length(JSON.stringify(empty(true)));
  if (budget < 0) throw new RtcError("too-large", `"${empty(true).t}" message exceeds ${MAX_CONTROL_BYTES} bytes`);
  let used = 0;
  for (const item of items) {
    const size = utf8Length(cost(item)) + 1;
    if (batch.length > 0 && used + size > budget) {
      frames.push(encodeControl(build(first, true, batch)));
      first = false;
      batch = [];
      used = 0;
      budget = MAX_CONTROL_BYTES - utf8Length(JSON.stringify(empty(false)));
    }
    batch.push(item);
    used += size;
  }
  frames.push(encodeControl(build(first, false, batch)));
  return frames;
}

function checkTotal(frames: string[], what: string): void {
  let total = 0;
  for (const f of frames) total += utf8Length(f);
  if (total > MAX_OFFER_BYTES) throw new RtcError("too-large", `${what} exceeds ${MAX_OFFER_BYTES} bytes`);
}

/** Parses and validates an inbound control frame. Throws `RtcError("protocol")`. */
export function parseControl(text: string): ControlMessage {
  if (text.length > MAX_CONTROL_BYTES || utf8Length(text) > MAX_CONTROL_BYTES) {
    throw invalid("control message too large");
  }
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    throw invalid("control message is not JSON");
  }
  const o = obj(raw, "message");
  switch (o.t) {
    case "hello":
      return parseHello(o);
    case "auth": {
      const msg: AuthMsg = { t: "auth", sig: b64(o.sig, "sig", SIGNATURE_LENGTH) };
      if (o.mac !== undefined) msg.mac = b64(o.mac, "mac", MAC_LENGTH);
      return msg;
    }
    case "offer":
      return parseOffer(o);
    case "answer":
      return parseAnswer(o);
    case "file":
      return { t: "file", id: id(o.id, "id"), offset: size(o.offset, "offset") };
    case "file-end":
      return { t: "file-end", id: id(o.id, "id"), sha256: hash(o.sha256, "sha256") };
    case "file-ack": {
      if (typeof o.ok !== "boolean") throw invalid("ok must be a boolean");
      const msg: FileAckMsg = { t: "file-ack", id: id(o.id, "id"), ok: o.ok };
      if (o.sha256 !== undefined) msg.sha256 = hash(o.sha256, "sha256");
      if (o.error !== undefined) msg.error = str(o.error, "error", 0, MAX_REASON_LENGTH);
      return msg;
    }
    case "progress":
      return { t: "progress", transferId: id(o.transferId, "transferId"), bytes: size(o.bytes, "bytes") };
    case "done":
      return { t: "done", transferId: id(o.transferId, "transferId") };
    case "cancel": {
      const msg: CancelMsg = { t: "cancel", transferId: id(o.transferId, "transferId") };
      if (o.reason !== undefined) msg.reason = str(o.reason, "reason", 0, MAX_REASON_LENGTH);
      return msg;
    }
    case "ping":
      return { t: "ping" };
    case "pong":
      return { t: "pong" };
    case "error":
      return { t: "error", code: str(o.code, "code", 1, MAX_CODE_LENGTH), message: str(o.message, "message", 0, MAX_REASON_LENGTH) };
    default:
      throw invalid(`unknown message type ${JSON.stringify(String(o.t)).slice(0, 40)}`);
  }
}

/** Validates the files of an outgoing or incoming offer (count, ids, names, sizes). */
export function validateFiles(files: readonly unknown[]): FileMeta[] {
  if (files.length > MAX_FILES) throw invalid(`too many files (max ${MAX_FILES})`);
  const seen = new Set<string>();
  let total = 0;
  return files.map((f) => {
    const o = obj(f, "file");
    const name = str(o.name, "file name", 1, MAX_NAME_LENGTH);
    const problem = fileNameProblem(name);
    if (problem) throw invalid(`file name ${problem}`);
    const mime = str(o.mime, "mime", 0, MAX_MIME_LENGTH);
    if (CONTROL.test(mime)) throw invalid("mime contains control characters");
    const meta: FileMeta = { id: id(o.id, "file id"), name, size: size(o.size, "file size"), mime };
    if (o.modified !== undefined) {
      if (!Number.isSafeInteger(o.modified)) throw invalid("modified must be an integer");
      meta.modified = o.modified as number;
    }
    if (seen.has(meta.id)) throw invalid(`duplicate file id ${JSON.stringify(meta.id)}`);
    seen.add(meta.id);
    total += meta.size;
    if (!Number.isSafeInteger(total)) throw invalid("total size too large");
    return meta;
  });
}

const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;
const LONE_SURROGATE = /[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/;
/** Characters a user cannot see: they must not smuggle a `..` past the check (`.​.`). */
const INVISIBLE = /[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu;
const DRIVE = /^[A-Za-z]:/;
const ID = /^[\x21-\x7e]+$/;

/**
 * Why `name` is not an acceptable relative path, or null when it is. Rejects
 * (threat model F1/F2): control characters including NUL, lone surrogates,
 * backslashes, absolute paths, drive prefixes, empty / `.` / `..` / all-dot
 * components (judged after removing invisible characters), more than
 * `MAX_PATH_DEPTH` components and components over 255 UTF-8 bytes.
 * Receivers still sanitize each component for their file system.
 */
export function fileNameProblem(name: string): string | null {
  if (typeof name !== "string" || name.length < 1 || name.length > MAX_NAME_LENGTH) return "length out of range";
  if (CONTROL.test(name)) return "contains control characters";
  if (LONE_SURROGATE.test(name)) return "is not valid Unicode";
  if (name.includes("\\")) return "contains a backslash";
  if (name.startsWith("/")) return "is an absolute path";
  if (DRIVE.test(name)) return "has a drive prefix";
  const parts = name.split("/");
  if (parts.length > MAX_PATH_DEPTH) return "is nested too deeply";
  for (const part of parts) {
    const visible = part.replace(INVISIBLE, "").trim();
    if (visible === "") return "has an empty path component";
    if (/^\.+$/.test(visible)) return "has a '.' or '..' component";
    if (utf8Length(part) > MAX_COMPONENT_BYTES) return "has a component longer than 255 bytes";
  }
  return null;
}

export function isValidFileName(name: string): boolean {
  return fileNameProblem(name) === null;
}

/** Transfer and file ids: 1 to 256 printable ASCII characters, no spaces. */
export function isValidId(value: unknown): value is string {
  return typeof value === "string" && value.length >= 1 && value.length <= MAX_ID_LENGTH && ID.test(value);
}

/**
 * Chunk size for binary frames: 64 KiB when the remote SDP's
 * `a=max-message-size` allows it (0 = unlimited), otherwise 16 KiB.
 */
export function chunkSizeFor(maxMessageSize: number | undefined): number {
  if (maxMessageSize === undefined || Number.isNaN(maxMessageSize)) return SMALL_CHUNK;
  return maxMessageSize === 0 || maxMessageSize >= LARGE_CHUNK ? LARGE_CHUNK : SMALL_CHUNK;
}

/**
 * Reads `a=max-message-size` from an SDP. Per RFC 8841 an absent attribute
 * means 65536 and 0 means "no limit".
 */
export function parseMaxMessageSize(sdp: string): number {
  const m = /^a=max-message-size:(\d+)\s*$/m.exec(sdp);
  if (!m) return 65536;
  const n = Number(m[1]);
  return n === 0 ? Infinity : n;
}

// ── validation helpers ────────────────────────────────────────────────────

type Obj = Record<string, unknown>;

function invalid(message: string): RtcError {
  return new RtcError("protocol", message);
}

function obj(value: unknown, what: string): Obj {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw invalid(`${what} must be an object`);
  return value as Obj;
}

function str(value: unknown, what: string, min: number, max: number): string {
  if (typeof value !== "string") throw invalid(`${what} must be a string`);
  if (value.length < min || value.length > max) throw invalid(`${what} length out of range`);
  return value;
}

function id(value: unknown, what: string): string {
  if (!isValidId(value)) throw invalid(`${what} must be 1-${MAX_ID_LENGTH} printable ASCII characters`);
  return value;
}

function size(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw invalid(`${what} must be a non-negative safe integer`);
  }
  return value;
}

function hash(value: unknown, what: string): string {
  if (typeof value !== "string" || !/^[0-9a-f]{64}$/.test(value)) throw invalid(`${what} must be lowercase hex SHA-256`);
  return value;
}

function b64(value: unknown, what: string, length: number): string {
  const bytes = typeof value === "string" ? tryB64urlDecode(value) : null;
  if (!bytes || bytes.byteLength !== length) throw invalid(`${what} must be ${length} bytes of base64url`);
  return value as string;
}

function strings(value: unknown, what: string, maxItems: number, maxLength: number): string[] {
  if (!Array.isArray(value) || value.length > maxItems) throw invalid(`${what} must be an array of at most ${maxItems} items`);
  return value.map((v) => str(v, what, 0, maxLength));
}

function more(o: Obj): boolean {
  if (o.more === undefined) return false;
  if (o.more !== true) throw invalid("more must be true when present");
  return true;
}

function parseHello(o: Obj): HelloMsg {
  if (o.v !== DC_VERSION) throw invalid("unsupported protocol version");
  if (!isIdentityAlg(o.alg)) throw invalid("unsupported key algorithm");
  const key = typeof o.key === "string" ? tryB64urlDecode(o.key) : null;
  if (!key || !isValidPublicKey(o.alg, key)) throw invalid("invalid public key");
  const d = obj(o.device, "device");
  return {
    t: "hello",
    v: 1,
    alg: o.alg,
    key: o.key as string,
    nonce: b64(o.nonce, "nonce", NONCE_LENGTH),
    device: {
      alias: str(d.alias, "alias", 0, MAX_ALIAS_LENGTH),
      deviceType: str(d.deviceType, "deviceType", 0, MAX_DEVICE_TYPE_LENGTH),
      platform: str(d.platform, "platform", 0, MAX_PLATFORM_LENGTH),
    },
    caps: strings(o.caps, "caps", MAX_CAPS, MAX_CAP_LENGTH),
  };
}

function parseOffer(o: Obj): OfferMsg {
  if (!Array.isArray(o.files)) throw invalid("files must be an array");
  const msg: OfferMsg = { t: "offer", transferId: id(o.transferId, "transferId"), files: validateFiles(o.files) };
  if (o.text !== undefined) msg.text = str(o.text, "text", 0, MAX_CONTROL_BYTES);
  if (more(o)) msg.more = true;
  return msg;
}

function parseAnswer(o: Obj): AnswerMsg {
  if (!Array.isArray(o.accept) || o.accept.length > MAX_FILES) throw invalid(`accept must be an array of at most ${MAX_FILES} ids`);
  const accept = o.accept.map((v) => id(v, "accepted id"));
  const ids = new Set(accept);
  if (ids.size !== accept.length) throw invalid("duplicate id in accept");
  const rawOffsets = obj(o.offsets, "offsets");
  // Null prototype: ids such as "__proto__" or "constructor" stay plain keys.
  const offsets = Object.create(null) as Record<string, number>;
  for (const k of Object.keys(rawOffsets)) {
    if (!ids.has(k)) throw invalid("offset for an id that is not accepted in this frame");
    offsets[k] = size(rawOffsets[k], "offset");
  }
  const msg: AnswerMsg = { t: "answer", transferId: id(o.transferId, "transferId"), accept, offsets };
  if (o.declined !== undefined) {
    if (o.declined !== true) throw invalid("declined must be true when present");
    if (accept.length > 0 || o.more !== undefined) throw invalid("a declining answer accepts nothing and is a single frame");
    msg.declined = true;
  }
  if (more(o)) msg.more = true;
  return msg;
}
