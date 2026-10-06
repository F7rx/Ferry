// Service worker registration for the browser build: offline app shell and
// the Web Share Target. A new version waits until the user chooses to reload,
// so a transfer in progress is never cut off by an update.

export function registerServiceWorker(onUpdate: (apply: () => void) => void) {
  if (__FERRY_TARGET__ !== "web" || !import.meta.env.PROD || !("serviceWorker" in navigator)) return;
  const register = async () => {
    try {
      const reg = await navigator.serviceWorker.register("/sw.js", { scope: "/" });
      const offer = (worker: ServiceWorker) =>
        onUpdate(() => {
          navigator.serviceWorker.addEventListener("controllerchange", () => location.reload(), { once: true });
          worker.postMessage({ type: "SKIP_WAITING" });
        });
      if (reg.waiting && navigator.serviceWorker.controller) offer(reg.waiting);
      reg.addEventListener("updatefound", () => {
        const worker = reg.installing;
        worker?.addEventListener("statechange", () => {
          if (worker.state === "installed" && navigator.serviceWorker.controller) offer(worker);
        });
      });
    } catch {
      // Blocked or unsupported (private mode, insecure origin): Ferry still works online.
    }
  };
  if (document.readyState === "complete") void register();
  else window.addEventListener("load", () => void register(), { once: true });
}
