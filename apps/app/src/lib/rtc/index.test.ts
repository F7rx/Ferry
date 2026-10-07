// @vitest-environment node
import { describe, expect, it } from "vitest";
import * as rtc from "./index";

describe("public surface", () => {
  it("exports exactly the documented runtime values", () => {
    expect(Object.keys(rtc).sort()).toEqual(
      [
        "DC_LABEL",
        "MAX_FILES",
        "MAX_NAME_LENGTH",
        "MAX_PATH_DEPTH",
        "MAX_TEXT_BYTES",
        "PeerConnector",
        "PeerSession",
        "RtcError",
        "SIGNALING_CAPS",
        "SIGNALING_VERSION",
        "SignalingClient",
        "b64urlDecode",
        "b64urlEncode",
        "deserializeFromIdb",
        "exportPublicKey",
        "extractFingerprint",
        "fileNameProblem",
        "generateIdentity",
        "isValidFileName",
        "isValidId",
        "isValidRoomId",
        "parseMaxMessageSize",
        "randomBytes",
        "randomId",
        "relayedOf",
        "roomIdFromSecret",
        "serializeForIdb",
      ].sort(),
    );
  });

  it("stays portable: no Vue and no DOM globals in library sources", () => {
    const sources = import.meta.glob(["./*.ts", "!./*.test.ts", "!./test-fakes.ts"], {
      query: "?raw",
      import: "default",
      eager: true,
    }) as Record<string, string>;
    expect(Object.keys(sources).length).toBeGreaterThanOrEqual(9);
    for (const [path, text] of Object.entries(sources)) {
      expect(text, path).not.toMatch(/from\s+["'](?:vue|@vue\/|vue-router|@tauri-apps)/);
      expect(text, path).not.toMatch(/\b(?:window|document|navigator|localStorage|sessionStorage|indexedDB)\.[A-Za-z]/);
    }
  });
});
