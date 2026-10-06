import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import { VitePWA } from "vite-plugin-pwa";

// One UI, two targets:
//   `vite build`               → installable PWA (service worker, manifest)
//   `vite build --mode desktop` → bundle embedded by the Tauri shell (no SW)
export default defineConfig(({ mode }) => {
  const desktop = mode === "desktop" || !!process.env.TAURI_ENV_PLATFORM;
  return {
    clearScreen: false,
    define: {
      __FERRY_TARGET__: JSON.stringify(desktop ? "desktop" : "web"),
      __FERRY_VERSION__: JSON.stringify(JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")).version),
    },
    server: {
      port: 5173,
      strictPort: true,
      host: process.env.TAURI_DEV_HOST || false,
    },
    envPrefix: ["VITE_", "TAURI_ENV_"],
    build: {
      target: desktop ? "es2022" : "es2021",
      sourcemap: !!process.env.TAURI_ENV_DEBUG,
      chunkSizeWarningLimit: 900,
    },
    plugins: [
      vue(),
      // Web build: a strict CSP. Received files opened in a tab inherit it, so
      // nothing a peer sends can run script on Ferry's origin. (Tauri sets its own.)
      !desktop && {
        name: "ferry-csp",
        apply: "build" as const,
        transformIndexHtml: {
          order: "post" as const,
          handler(source: string) {
            // Hash exactly what ships: line endings normalised first (a CRLF
            // checkout would otherwise hash bytes the browser never sees).
            const html = source.replace(/\r\n?/g, "\n");
            const hashes = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map(
              (m) => `'sha256-${createHash("sha256").update(m[1]!).digest("base64")}'`,
            );
            const csp = [
              "default-src 'self'",
              `script-src 'self' ${hashes.join(" ")}`,
              "style-src 'self' 'unsafe-inline'",
              "img-src 'self' data: blob:",
              "media-src 'self' blob:",
              "font-src 'self' data:",
              "connect-src 'self' ws: wss: https:",
              "worker-src 'self'",
              "manifest-src 'self'",
              "object-src 'none'",
              "base-uri 'none'",
              "form-action 'self'",
            ].join("; ");
            return html.replace("<head>", `<head>
    <meta http-equiv="Content-Security-Policy" content="${csp}" />`);
          },
        },
      },
      !desktop &&
        VitePWA({
          registerType: "prompt",
          injectRegister: false,
          includeAssets: ["icon.svg", "icons/*.png"],
          manifest: {
            name: "Ferry",
            short_name: "Ferry",
            description: "Fast, private device-to-device sharing.",
            theme_color: "#0a0a0a",
            background_color: "#0a0a0a",
            display: "standalone",
            display_override: ["window-controls-overlay", "standalone"],
            start_url: "/",
            scope: "/",
            icons: [
              { src: "/icon.svg", sizes: "any", type: "image/svg+xml", purpose: "any" },
              { src: "/icons/icon-192.png", sizes: "192x192", type: "image/png" },
              { src: "/icons/icon-512.png", sizes: "512x512", type: "image/png" },
              { src: "/icons/maskable-512.png", sizes: "512x512", type: "image/png", purpose: "maskable" },
            ],
            share_target: {
              action: "/share-target",
              method: "POST",
              enctype: "multipart/form-data",
              params: {
                title: "title",
                text: "text",
                url: "url",
                files: [{ name: "files", accept: ["*/*"] }],
              },
            },
          },
          // Our own service worker (src/sw.ts): precache, offline navigation
          // fallback and the share target live there, not in `workbox` options.
          strategies: "injectManifest",
          srcDir: "src",
          filename: "sw.ts",
          injectManifest: {
            globPatterns: ["**/*.{js,css,html,svg,png,woff2}"],
          },
        }),
    ].filter(Boolean),
    test: {
      environment: "jsdom",
      include: ["src/**/*.test.ts"],
    },
  };
});
