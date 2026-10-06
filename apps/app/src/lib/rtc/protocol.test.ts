// @vitest-environment node
import { describe, expect, it } from "vitest";
import { b64urlDecode, b64urlEncode, randomBytes, tryB64urlDecode, utf8, utf8Length } from "./bytes";
import {
  chunkSizeFor,
  encodeAnswer,
  encodeControl,
  encodeOffer,
  errorFrame,
  fileNameProblem,
  LARGE_CHUNK,
  MAX_CONTROL_BYTES,
  MAX_FILES,
  parseControl,
  parseMaxMessageSize,
  RtcError,
  SMALL_CHUNK,
  validateFiles,
  type AnswerMsg,
  type ControlMessage,
  type FileMeta,
  type OfferMsg,
} from "./protocol";

const KEY = b64urlEncode(new Uint8Array(32).fill(7));
const NONCE = b64urlEncode(new Uint8Array(32).fill(1));
const SIG = b64urlEncode(new Uint8Array(64).fill(2));
const HASH = "ab".repeat(32);

const file = (over: Partial<FileMeta> & Record<string, unknown> = {}) => ({ id: "f1", name: "photo.jpg", size: 10, mime: "image/jpeg", ...over });
const offerText = (files: unknown[], extra: Record<string, unknown> = {}) => JSON.stringify({ t: "offer", transferId: "t1", files, ...extra });

function rejects(text: string, match?: RegExp): void {
  let error: unknown;
  try {
    parseControl(text);
  } catch (err) {
    error = err;
  }
  expect(error, text.slice(0, 120)).toBeInstanceOf(RtcError);
  expect((error as RtcError).code).toBe("protocol");
  if (match) expect((error as RtcError).message).toMatch(match);
}

describe("control messages", () => {
  it("round-trip every message type", () => {
    const messages: ControlMessage[] = [
      { t: "hello", v: 1, alg: "ed25519", key: KEY, nonce: NONCE, device: { alias: "Alice", deviceType: "web", platform: "browser" }, caps: ["x"] },
      { t: "auth", sig: SIG, mac: b64urlEncode(new Uint8Array(32)) },
      { t: "auth", sig: SIG },
      { t: "offer", transferId: "t1", files: [file({ modified: 1_700_000_000_000 })], text: "hi" },
      { t: "offer", transferId: "t1", files: [file()], more: true },
      { t: "answer", transferId: "t1", accept: ["f1"], offsets: { f1: 5 } },
      { t: "answer", transferId: "t1", accept: ["f1"], offsets: {}, more: true },
      { t: "answer", transferId: "t1", accept: [], offsets: {}, declined: true },
      { t: "file", id: "f1", offset: 0 },
      { t: "file-end", id: "f1", sha256: HASH },
      { t: "file-ack", id: "f1", ok: true, sha256: HASH },
      { t: "file-ack", id: "f1", ok: false, error: "disk full" },
      { t: "progress", transferId: "t1", bytes: 1_048_576 },
      { t: "done", transferId: "t1" },
      { t: "cancel", transferId: "t1", reason: "user" },
      { t: "ping" },
      { t: "pong" },
      { t: "error", code: "auth", message: "peer authentication failed" },
    ];
    for (const msg of messages) expect(parseControl(encodeControl(msg))).toEqual(msg);
  });

  it("drop unknown members", () => {
    expect(parseControl('{"t":"ping","extra":1}')).toEqual({ t: "ping" });
    const parsed = parseControl(offerText([file({ evil: "x" })], { junk: true })) as OfferMsg;
    expect(parsed).toEqual({ t: "offer", transferId: "t1", files: [file()] });
  });

  it("reject malformed frames", () => {
    rejects("not json", /not JSON/);
    rejects("[]");
    rejects("null");
    rejects('"ping"');
    rejects("{}", /unknown message type/);
    rejects('{"t":"pwn"}', /unknown message type/);
    rejects('{"t":"file","id":"f1","offset":"1"}');
    rejects('{"t":"file","id":"f1","offset":-1}');
    rejects('{"t":"file","id":"f1","offset":1.5}');
    rejects('{"t":"file","id":"f1","offset":9007199254740992}');
    rejects('{"t":"file","id":"","offset":0}');
    rejects('{"t":"file","id":"has space","offset":0}');
    rejects('{"t":"file","id":"ünïcode","offset":0}');
    rejects(`{"t":"file","id":"${"x".repeat(257)}","offset":0}`);
    rejects(`{"t":"file-end","id":"f1","sha256":"${HASH.toUpperCase()}"}`);
    rejects('{"t":"file-end","id":"f1","sha256":"abc"}');
    rejects('{"t":"file-ack","id":"f1","ok":"yes"}');
    rejects(`{"t":"auth","sig":"${SIG.slice(4)}"}`);
    rejects(`{"t":"auth","sig":"${SIG}","mac":"short"}`);
    rejects(`{"t":"cancel","transferId":"t1","reason":"${"r".repeat(1025)}"}`);
    rejects('{"t":"error","code":"","message":"x"}');
    rejects('{"t":"progress","transferId":"t1","bytes":-5}');
    rejects(offerText([file()], { more: false }), /more must be true/);
    const hello = { t: "hello", v: 1, alg: "ed25519", key: KEY, nonce: NONCE, device: { alias: "A", deviceType: "web", platform: "x" }, caps: [] };
    rejects(JSON.stringify({ ...hello, v: 2 }), /version/);
    rejects(JSON.stringify({ ...hello, alg: "rsa" }), /algorithm/);
    rejects(JSON.stringify({ ...hello, key: b64urlEncode(new Uint8Array(31)) }), /public key/);
    rejects(JSON.stringify({ ...hello, nonce: b64urlEncode(new Uint8Array(16)) }), /nonce/);
    rejects(JSON.stringify({ ...hello, caps: Array(65).fill("c") }), /caps/);
    rejects(JSON.stringify({ ...hello, device: { alias: "a".repeat(257), deviceType: "web", platform: "x" } }), /alias/);
  });

  it("reject oversized frames", () => {
    rejects(JSON.stringify({ t: "cancel", transferId: "t1", reason: "x".repeat(MAX_CONTROL_BYTES) }), /too large/);
    // Multi-byte characters count in UTF-8 bytes, not UTF-16 units.
    rejects(JSON.stringify({ t: "offer", transferId: "t1", files: [], text: "é".repeat(MAX_CONTROL_BYTES / 2) }), /too large/);
    expect(() => encodeControl({ t: "cancel", transferId: "t1", reason: "x".repeat(MAX_CONTROL_BYTES) })).toThrow(/exceeds/);
  });

  it("clip outgoing error frames to what the peer accepts", () => {
    const msg = errorFrame("c".repeat(100), "m".repeat(5000));
    expect(parseControl(encodeControl(msg))).toEqual({ t: "error", code: "c".repeat(64), message: "m".repeat(1024) });
    expect(errorFrame("", "x").code).toBe("internal");
  });
});

describe("file names", () => {
  const hostile = [
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
    "folder/ /x",
    ".\u200b.",
    "\u202e.\u200d.",
    "lone\ud800",
    "x".repeat(256),
    `ok/${"é".repeat(128)}`,
    Array(33).fill("d").join("/"),
    "x".repeat(1025),
    "",
  ];
  const fine = [
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
    Array(32).fill("d").join("/"),
  ];

  it("reject traversal, absolute paths, control characters and junk components", () => {
    for (const name of hostile) {
      expect(fileNameProblem(name), JSON.stringify(name)).not.toBeNull();
      rejects(offerText([file({ name })]), /file name/);
    }
  });

  it("accept ordinary names and folders", () => {
    for (const name of fine) {
      expect(fileNameProblem(name), JSON.stringify(name)).toBeNull();
      expect((parseControl(offerText([file({ name })])) as OfferMsg).files[0]!.name).toBe(name);
    }
  });
});

describe("offer limits", () => {
  it("cap the number of files", () => {
    const files = Array.from({ length: MAX_FILES + 1 }, (_, i) => file({ id: `f${i}`, name: `f${i}` }));
    expect(() => validateFiles(files)).toThrow(/too many files/);
    expect(validateFiles(files.slice(0, MAX_FILES))).toHaveLength(MAX_FILES);
  });

  it("reject sizes beyond the limits", () => {
    for (const size of [-1, 1.5, "10", 2 ** 53, Number.MAX_VALUE, null]) {
      expect(() => validateFiles([file({ size: size as number })]), String(size)).toThrow(/file size/);
    }
    expect(validateFiles([file({ size: Number.MAX_SAFE_INTEGER })])[0]!.size).toBe(Number.MAX_SAFE_INTEGER);
    expect(() => validateFiles([file({ id: "a", size: 2 ** 52 }), file({ id: "b", size: 2 ** 52 })])).toThrow(/total size/);
  });

  it("reject duplicate ids, bad mime types and bad timestamps", () => {
    expect(() => validateFiles([file(), file()])).toThrow(/duplicate/);
    expect(() => validateFiles([file({ mime: "text/plain\r\nX-Evil: 1" })])).toThrow(/mime/);
    expect(() => validateFiles([file({ mime: "m".repeat(256) })])).toThrow(/mime/);
    expect(() => validateFiles([file({ modified: 1.5 })])).toThrow(/modified/);
  });

  it("validate answers", () => {
    const answer = (o: Record<string, unknown>) => JSON.stringify({ t: "answer", transferId: "t1", accept: ["a"], offsets: {}, ...o });
    rejects(answer({ accept: ["a", "a"] }), /duplicate/);
    rejects(answer({ offsets: { b: 1 } }), /not accepted/);
    rejects(answer({ offsets: { a: -1 } }));
    rejects(answer({ declined: true }), /declining/);
    rejects(answer({ accept: [], declined: true, more: true }), /declining/);
    rejects(answer({ declined: false }));
    rejects(answer({ accept: "a" }));
    // Ids such as "__proto__" stay plain keys.
    const parsed = parseControl('{"t":"answer","transferId":"t1","accept":["__proto__"],"offsets":{"__proto__":3}}') as AnswerMsg;
    expect(Object.getPrototypeOf(parsed.offsets)).toBeNull();
    expect(Object.hasOwn(parsed.offsets, "__proto__")).toBe(true);
    expect(parsed.offsets["__proto__"]).toBe(3);
  });
});

describe("split offers and answers", () => {
  it("keep small messages in one frame", () => {
    const offer: OfferMsg = { t: "offer", transferId: "t1", files: [file()], text: "hello" };
    expect(encodeOffer(offer)).toEqual([encodeControl(offer)]);
    const answer: AnswerMsg = { t: "answer", transferId: "t1", accept: ["f1"], offsets: { f1: 3 } };
    expect(encodeAnswer(answer)).toEqual([encodeControl(answer)]);
  });

  it("split large offers into ≤ 64 KiB frames that reassemble exactly", () => {
    const files = Array.from({ length: 3000 }, (_, i) =>
      file({ id: `file-${i}`, name: `Holiday ${i}/IMG_${String(i).padStart(5, "0")} · ${"ä".repeat(40)}.jpg`, size: i * 1000 }),
    );
    const frames = encodeOffer({ t: "offer", transferId: "t1", files, text: "for you" });
    expect(frames.length).toBeGreaterThan(3);
    const parts = frames.map((f) => {
      expect(utf8Length(f)).toBeLessThanOrEqual(MAX_CONTROL_BYTES);
      return parseControl(f) as OfferMsg;
    });
    parts.forEach((p, i) => {
      expect(p.transferId).toBe("t1");
      expect(p.more).toBe(i < parts.length - 1 ? true : undefined);
      expect(p.text).toBe(i === 0 ? "for you" : undefined);
    });
    expect(parts.flatMap((p) => p.files)).toEqual(files);
  });

  it("split large answers, keeping each offset next to its id", () => {
    const accept = Array.from({ length: MAX_FILES }, (_, i) => `file-${i}-${"x".repeat(20)}`);
    const offsets: Record<string, number> = {};
    for (let i = 0; i < accept.length; i += 7) offsets[accept[i]!] = i + 1;
    const frames = encodeAnswer({ t: "answer", transferId: "t1", accept, offsets });
    expect(frames.length).toBeGreaterThan(1);
    const parts = frames.map((f) => {
      expect(utf8Length(f)).toBeLessThanOrEqual(MAX_CONTROL_BYTES);
      return parseControl(f) as AnswerMsg; // parse checks offsets ⊆ accept per frame
    });
    expect(parts.flatMap((p) => p.accept)).toEqual(accept);
    expect(Object.assign({}, ...parts.map((p) => p.offsets))).toEqual(offsets);
    expect(parts.at(-1)!.more).toBeUndefined();
    expect(encodeAnswer({ t: "answer", transferId: "t1", accept: [], offsets: {}, declined: true })).toHaveLength(1);
  });

  it("refuse offers beyond the total budget and texts beyond one frame", () => {
    const files = Array.from({ length: MAX_FILES }, (_, i) => file({ id: `f${i}`, name: `${"n".repeat(900)}${i}` }));
    expect(() => encodeOffer({ t: "offer", transferId: "t1", files })).toThrow(RtcError);
    expect(() => encodeOffer({ t: "offer", transferId: "t1", files: [], text: "x".repeat(70_000) })).toThrow(/exceeds/);
  });
});

describe("chunking", () => {
  it("picks the chunk size from max-message-size", () => {
    expect(chunkSizeFor(undefined)).toBe(SMALL_CHUNK);
    expect(chunkSizeFor(16384)).toBe(SMALL_CHUNK);
    expect(chunkSizeFor(65536)).toBe(LARGE_CHUNK);
    expect(chunkSizeFor(262144)).toBe(LARGE_CHUNK);
    expect(chunkSizeFor(0)).toBe(LARGE_CHUNK);
    expect(chunkSizeFor(Infinity)).toBe(LARGE_CHUNK);
    expect(parseMaxMessageSize("v=0\r\na=max-message-size:262144\r\n")).toBe(262144);
    expect(parseMaxMessageSize("v=0\r\n")).toBe(65536);
    expect(parseMaxMessageSize("a=max-message-size:0\r\n")).toBe(Infinity);
  });
});

describe("bytes", () => {
  it("base64url round-trips and decodes strictly", () => {
    for (let n = 0; n < 40; n++) {
      const data = randomBytes(n);
      expect(b64urlDecode(b64urlEncode(data))).toEqual(data);
    }
    expect(b64urlEncode(new Uint8Array([0xfb, 0xff]))).toBe("-_8");
    for (const bad of ["AA==", "A", "a+b/", "AB", "é"]) expect(tryB64urlDecode(bad), bad).toBeNull();
  });

  it("counts UTF-8 bytes like TextEncoder", () => {
    for (const s of ["", "abc", "é", "€", "😀", "a😀b€", "ファイル"]) expect(utf8Length(s)).toBe(utf8(s).byteLength);
    expect(utf8Length("\ud800")).toBe(3); // lone surrogate → U+FFFD
  });
});
