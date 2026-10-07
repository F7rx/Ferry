// @vitest-environment node
// The shared store's signaling mirror follows the browser platform's events.
import { describe, expect, it, vi } from "vitest";
import { FakePeerConnection, FakeSignalServer, waitUntil } from "../lib/rtc/test-fakes";
import { loadWeb, newProfile } from "../platform/web-test-env";

describe("engine store: signaling status", () => {
  it("mirrors connecting, open, closed, reconnects and a changed server", async () => {
    const server = new FakeSignalServer();
    const { web } = await loadWeb(newProfile());
    const platform = web.createWebPlatform({
      WebSocket: server.WebSocket,
      RTCPeerConnection: FakePeerConnection as unknown as typeof RTCPeerConnection,
    });
    vi.doMock("../platform", () => ({ platform }));
    const { initEngine, saveSettings, store } = await import("./engine");
    const seen: string[] = [];
    platform.subscribe((e) => e.type === "signalingStatus" && seen.push(e.status.state));

    expect(await initEngine()).toBe(true);
    const key = store.local!.fingerprint;
    await waitUntil(() => store.signaling?.state === "open", 5000, "open");
    expect(store.signaling).toMatchObject({ url: "wss://ferry.test/v1/ws", error: null, identityKey: key });
    expect(store.server.running).toBe(true);

    const id = [...server.conns.values()].find((c) => c.info.ext?.key === key)!.info.id;
    server.kick(id);
    await waitUntil(() => store.signaling?.state === "closed", 5000, "closed");
    expect(store.signaling!.error).toMatch(/Can't reach the signaling server \(wss:\/\/ferry\.test\/v1\/ws\)/);
    expect(store.server.running).toBe(false);
    await waitUntil(() => store.signaling?.state === "open", 5000, "reconnected");

    await saveSettings({ signalingUrl: "wss://elsewhere.test/v1/ws" });
    expect(store.signaling!.url).toBe("wss://elsewhere.test/v1/ws");
    await waitUntil(() => store.signaling?.state === "open", 5000, "open on the new server");
    expect(seen).toContain("connecting");
    vi.doUnmock("../platform");
  });
});
