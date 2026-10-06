import { createApp } from "vue";
import "./styles/base.css";
import App from "./App.vue";
import { router } from "./router";
import { initAppearance } from "./lib/appearance";
import { initTruncationTitles } from "./lib/truncation";
import { initEngine, toast } from "./stores/engine";
import { registerServiceWorker } from "./lib/pwa";
import { stage } from "./stores/compose";
import { demo, native, web } from "./platform";

initAppearance();
initTruncationTitles();
registerServiceWorker((apply) =>
  toast({ level: "info", title: "A new version of Ferry is ready", body: "Reload when no transfer is running.", action: { label: "Reload", run: apply } }, 0),
);
createApp(App).use(router).mount("#app");

initEngine()
  .then(async (snapshot) => {
    // Files handed over on launch (e.g. Explorer "Send with Ferry").
    if (native && snapshot.pendingPaths?.length) stage(await native.pathItems(snapshot.pendingPaths));
    // Shared into the installed PWA (Web Share Target).
    if (web) {
      const shared = await web.takeShared();
      if (shared.length) stage(shared);
    }
    // Visual QA: ?demo&scene=incoming|message|transfers
    const scene = new URLSearchParams(location.search).get("scene");
    const d = demo;
    if (d && scene) setTimeout(() => scene.split(",").forEach((s) => d.trigger(s)), 1200);
  })
  .then(() => {
    // Development builds only: lets automated end-to-end tests stage OS paths
    // (native file dialogs can't be driven by a test runner).
    const n = native;
    if (import.meta.env.DEV && n) {
      (window as unknown as { __ferryDev: object }).__ferryDev = {
        stagePaths: async (paths: string[]) => stage(await n.pathItems(paths)),
      };
    }
  })
  .catch((err) => {
    console.error("Engine failed to start", err);
  });
