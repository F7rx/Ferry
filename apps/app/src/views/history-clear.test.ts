// Clearing history and received files from both screens that offer it
// (History, Settings › Privacy), in the browser and the native app. The views
// and the shared flow are real; the platform is a stub.
import { afterEach, describe, expect, it, vi } from "vitest";
import type { HistoryEntry } from "../platform/types";

type Kind = "web" | "native";

const entry = (id: number, patch: Partial<HistoryEntry> = {}): HistoryEntry => ({
  id,
  transferId: `t${id}`,
  direction: "receive",
  peerId: "p",
  peerAlias: "Phone",
  peerKind: "mobile",
  kind: "file",
  name: `file${id}.jpg`,
  size: 10,
  mime: "image/jpeg",
  path: `browser:${String(id).repeat(32).slice(0, 32)}`,
  text: null,
  timestampMs: 1_700_000_000_000 + id,
  status: "completed",
  verified: true,
  ...patch,
});

function stubPlatform(kind: Kind) {
  const history = [entry(1), entry(2, { kind: "text", name: "Message", path: null })];
  return {
    capabilities: { kind, lanDiscovery: kind === "native", revealInFolder: kind === "native", pickFolders: true, remoteLinks: kind === "web" },
    history: vi.fn(async () => history),
    clearHistory: vi.fn(async () => {}),
    pickSaveFolder: vi.fn(async () => null),
    previewUrl: () => null,
    ...(kind === "web"
      ? {
          inbox: vi.fn(async () => history),
          clearReceivedFiles: vi.fn(async () => ({ deleted: 1, failed: 0 })),
        }
      : {}),
  };
}

async function mount(kind: Kind, view: "history" | "settings") {
  vi.resetModules();
  const platform = stubPlatform(kind);
  vi.doMock("../platform", () => ({ platform, isNative: kind === "native" }));
  const { createApp, nextTick } = await import("vue");
  const { createMemoryHistory, createRouter } = await import("vue-router");
  const { store } = await import("../stores/engine");
  const HistoryView = (await import("./HistoryView.vue")).default;
  const SettingsView = (await import("./SettingsView.vue")).default;
  store.settings = { historyEnabled: true, keepMessageText: false, stunServers: [], signalingUrl: null, alias: "Me" } as never;
  store.history = await platform.history();
  store.inbox = [entry(1)];
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/history", component: HistoryView },
      { path: "/settings/:section?", component: SettingsView },
      { path: "/:any(.*)", component: { template: "<div />" } },
    ],
  });
  await router.push(view === "history" ? "/history" : "/settings/privacy");
  await router.isReady();
  const el = document.createElement("div");
  document.body.append(el);
  const root = createApp(view === "history" ? HistoryView : SettingsView).use(router);
  root.mount(el);
  const flush = async () => {
    for (let i = 0; i < 5; i++) {
      await nextTick();
      await new Promise((r) => setTimeout(r, 0));
    }
  };
  await flush();
  const button = (text: string) => [...el.querySelectorAll("button")].find((b) => b.textContent?.trim() === text) as HTMLButtonElement | undefined;
  const click = async (text: string) => {
    const b = button(text);
    if (!b) throw new Error(`no "${text}" button`);
    b.click();
    await flush();
  };
  return { el, platform, store, button, click, flush, unmount: () => root.unmount() };
}

let confirmAnswer = true;
const questions: string[] = [];
vi.stubGlobal("confirm", (q: string) => {
  questions.push(q);
  return confirmAnswer;
});

afterEach(() => {
  confirmAnswer = true;
  questions.length = 0;
  document.body.innerHTML = "";
  vi.doUnmock("../platform");
});

describe("clear history", () => {
  for (const view of ["history", "settings"] as const) {
    const label = view === "history" ? "Clear history" : "Clear";

    it(`${view}: in the browser it says received files stay, and clears only history`, async () => {
      const m = await mount("web", view);
      await m.click(label);
      expect(questions).toEqual(["Clear the whole history? Sent items and messages are removed. Received files stay in the Inbox."]);
      expect(m.platform.clearHistory).toHaveBeenCalledOnce();
      expect(m.platform.clearReceivedFiles).not.toHaveBeenCalled();
      expect(m.store.history).toEqual([]);
      expect(m.store.inbox.map((e) => e.id)).toEqual([1]); // the file stays listed
      expect(m.store.toasts.at(-1)).toMatchObject({ level: "success", title: "History cleared" });
      expect(m.store.historyRevision).toBe(1);
    });

    it(`${view}: in the native app it says received files stay where they are`, async () => {
      const m = await mount("native", view);
      await m.click(label);
      expect(questions).toEqual(["Clear the whole history? Received files stay where they are."]);
      expect(m.platform.clearHistory).toHaveBeenCalledOnce();
      expect(m.store.history).toEqual([]);
      if (view === "settings") expect(m.button("Delete files")).toBeUndefined();
    });

    it(`${view}: cancelling changes nothing`, async () => {
      const m = await mount("web", view);
      confirmAnswer = false;
      await m.click(label);
      expect(m.platform.clearHistory).not.toHaveBeenCalled();
      expect(m.store.history).toHaveLength(2);
      expect(m.store.historyRevision).toBe(0);
      if (view === "history") expect(m.el.textContent).toContain("file1.jpg");
    });

    it(`${view}: a failure is reported and nothing is removed from view`, async () => {
      const m = await mount("web", view);
      m.platform.clearHistory.mockRejectedValueOnce({ message: "Couldn't clear the history.", hint: "Try again." });
      await m.click(label);
      expect(m.store.history).toHaveLength(2);
      expect(m.store.toasts.at(-1)).toMatchObject({ level: "error", title: "Couldn't clear the history.", body: "Try again." });
      if (view === "history") expect(m.el.textContent).toContain("file1.jpg");
    });
  }
});

describe("delete received files (browser)", () => {
  it("warns that files are deleted, then deletes them", async () => {
    const m = await mount("web", "settings");
    await m.click("Delete files");
    expect(questions[0]).toMatch(/^Delete every file received in this browser\?.*can't be undone\.$/);
    expect(m.platform.clearReceivedFiles).toHaveBeenCalledOnce();
    expect(m.platform.clearHistory).not.toHaveBeenCalled();
    expect(m.store.inbox).toEqual([]);
    expect(m.store.toasts.at(-1)).toMatchObject({ level: "success", title: "Deleted 1 file" });
  });

  it("cancelling deletes nothing", async () => {
    const m = await mount("web", "settings");
    confirmAnswer = false;
    await m.click("Delete files");
    expect(m.platform.clearReceivedFiles).not.toHaveBeenCalled();
    expect(m.store.inbox).toHaveLength(1);
  });

  it("reports files that couldn't be deleted", async () => {
    const m = await mount("web", "settings");
    m.platform.clearReceivedFiles!.mockResolvedValueOnce({ deleted: 3, failed: 1 });
    await m.click("Delete files");
    expect(m.store.toasts.at(-1)).toMatchObject({ level: "error", title: "1 file couldn't be deleted", body: expect.stringMatching(/^3 files deleted\. Try again/) });
  });
});
