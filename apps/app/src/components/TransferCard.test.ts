// The transfer card's controls: Pause and Resume only where the platform
// supports them, "Try again" only for a browser send that lost its
// connection. The card is real; the platform is a stub.
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ErrorInfo, TransferState, TransferSummary } from "../platform/types";

type Kind = "web" | "native";

function summary(patch: Partial<TransferSummary> = {}): TransferSummary {
  return {
    id: "t1",
    direction: "send",
    dropId: null,
    peer: { id: "p", alias: "Phone", deviceKind: "mobile", deviceModel: null, verified: true },
    state: "transferring",
    fileCount: 1,
    filesDone: 0,
    totalBytes: 1000,
    bytesDone: 400,
    speedBps: 100,
    etaSecs: 6,
    startedAtMs: 1,
    finishedAtMs: null,
    connection: { transport: "webrtc", encrypted: true, ipVersion: null, relayed: false, address: null },
    resumable: true,
    canPause: false,
    canResume: false,
    title: "photo.png",
    text: null,
    error: null,
    saveDir: null,
    ...patch,
  };
}

const lost: ErrorInfo = { code: "connection_lost", message: "Couldn't reach Phone again.", hint: "Press Try again when it's back online; it continues where it stopped." };
const failedSend = (patch: Partial<TransferSummary> = {}) => summary({ state: "failed", error: lost, finishedAtMs: 2, ...patch });

const unmounts: (() => void)[] = [];
afterEach(() => {
  for (const fn of unmounts.splice(0)) fn();
  vi.doUnmock("../platform");
});

async function mount(transfer: TransferSummary, kind: Kind = "web", stubs: { pause?: () => Promise<boolean>; resume?: () => Promise<boolean> } = {}) {
  vi.resetModules();
  const platform = {
    capabilities: { kind, revealInFolder: kind === "native" },
    pause: vi.fn(stubs.pause ?? (async () => true)),
    resume: vi.fn(stubs.resume ?? (async () => true)),
    cancel: vi.fn(async () => true),
    dismiss: vi.fn(async () => true),
    reveal: vi.fn(async () => {}),
    submitPin: vi.fn(async () => true),
    transferFiles: vi.fn(async () => []),
  };
  vi.doMock("../platform", () => ({ platform, isNative: kind === "native" }));
  const { createApp, h, nextTick } = await import("vue");
  const { store } = await import("../stores/engine");
  const TransferCard = (await import("./TransferCard.vue")).default;
  const el = document.createElement("div");
  document.body.append(el);
  const app = createApp({ render: () => h(TransferCard, { transfer }) });
  app.mount(el);
  unmounts.push(() => {
    app.unmount();
    el.remove();
  });
  const flush = async () => {
    for (let i = 0; i < 3; i++) {
      await nextTick();
      await new Promise((r) => setTimeout(r, 0));
    }
  };
  await flush();
  const button = (label: string) => el.querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`);
  return { el, platform, store, button, flush };
}

describe("TransferCard: try again", () => {
  it("is shown for a browser send that lost its connection, and is a focusable button", async () => {
    const { button } = await mount(failedSend());
    const b = button("Try again");
    expect(b).not.toBeNull();
    expect(b!.tagName).toBe("BUTTON");
    expect(b!.disabled).toBe(false);
    b!.focus();
    expect(document.activeElement).toBe(b);
    // The first mount compiles the card: allow for a busy machine.
  }, 20_000);

  const hidden: [string, TransferSummary, Kind][] = [
    ["a declined send", summary({ state: "declined", error: { code: "declined", message: "Phone declined." } }), "web"],
    ["a cancelled send", summary({ state: "cancelled", error: null }), "web"],
    ["a send cancelled by the peer", summary({ state: "cancelled", error: { code: "cancelled_by_peer", message: "Phone cancelled." } }), "web"],
    ["a send that failed for another reason", failedSend({ error: { code: "files_failed", message: "1 file(s) failed." } }), "web"],
    ["a refused send", failedSend({ error: { code: "protocol", message: "Protocol error" } }), "web"],
    ["a receive that lost its connection", failedSend({ direction: "receive" }), "web"],
    ["a send that is still reconnecting", summary({ state: "reconnecting", error: lost }), "web"],
    ["a native send that lost its connection", failedSend(), "native"],
  ];
  for (const [what, transfer, kind] of hidden) {
    it(`is not shown for ${what}`, async () => {
      const { button } = await mount(transfer, kind);
      expect(button("Try again")).toBeNull();
    });
  }

  it("calls resume once, and is disabled until that call returns", async () => {
    let finish!: (ok: boolean) => void;
    const { button, platform, flush } = await mount(failedSend(), "web", { resume: () => new Promise<boolean>((r) => (finish = r)) });
    const b = button("Try again")!;
    b.click();
    await flush();
    expect(platform.resume).toHaveBeenCalledTimes(1);
    expect(platform.resume).toHaveBeenCalledWith("t1");
    expect(b.disabled).toBe(true);
    // A second press while it starts does nothing.
    b.click();
    await flush();
    expect(platform.resume).toHaveBeenCalledTimes(1);
    finish(true);
    await flush();
    expect(b.disabled).toBe(false);
  });

  it("says so when the transfer couldn't be tried again", async () => {
    const { button, store, flush } = await mount(failedSend(), "web", { resume: async () => false });
    button("Try again")!.click();
    await flush();
    expect(store.toasts.map((t) => t.title)).toContain("Couldn't try again");
  });
});

describe("TransferCard: pause and resume", () => {
  const cases: [string, Partial<TransferSummary>, boolean, boolean][] = [
    ["transferring, pausable", { state: "transferring", canPause: true, canResume: true }, true, false],
    ["transferring, not pausable (still resumable after a lost connection)", { state: "transferring", resumable: true }, false, false],
    ["paused, resumable by the user", { state: "paused", canPause: true, canResume: true }, false, true],
    ["paused, not resumable by the user", { state: "paused", canPause: true }, false, false],
    ["reconnecting", { state: "reconnecting", canPause: true, canResume: true }, false, false],
  ];
  for (const [what, patch, pause, resume] of cases) {
    it(`${what}: ${pause ? "Pause" : "no Pause"}, ${resume ? "Resume" : "no Resume"}`, async () => {
      const { button } = await mount(summary(patch));
      expect(button("Pause") !== null).toBe(pause);
      expect(button("Resume") !== null).toBe(resume);
      expect(button("Cancel")).not.toBeNull();
    });
  }

  it("shows the connection line's Resumable separately from the controls", async () => {
    const { el, button } = await mount(summary({ resumable: true }));
    expect(el.querySelector(".conn")!.textContent).toContain("Resumable");
    expect(button("Pause")).toBeNull();
  });

  const refused: [string, TransferState, "pause" | "resume", string, string][] = [
    ["pause", "transferring", "pause", "Pause", "Couldn't pause the transfer"],
    ["resume", "paused", "resume", "Resume", "Couldn't resume the transfer"],
  ];
  for (const [what, state, method, label, message] of refused) {
    it(`a ${what} the platform refuses shows a toast`, async () => {
      const { button, platform, store, flush } = await mount(summary({ state, canPause: true, canResume: true }), "native", { [method]: async () => false });
      button(label)!.click();
      await flush();
      expect(platform[method]).toHaveBeenCalledWith("t1");
      expect(store.toasts.map((t) => t.title)).toContain(message);
    });
  }

  it("a pause that works shows no toast", async () => {
    const { button, platform, store, flush } = await mount(summary({ canPause: true, canResume: true }), "native");
    button("Pause")!.click();
    await flush();
    expect(platform.pause).toHaveBeenCalledTimes(1);
    expect(store.toasts).toHaveLength(0);
  });
});
