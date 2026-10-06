import { createRouter, createWebHistory } from "vue-router";

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: "/", component: () => import("./views/HomeView.vue") },
    { path: "/send", component: () => import("./views/SendView.vue") },
    { path: "/receive", component: () => import("./views/ReceiveView.vue") },
    { path: "/transfers", component: () => import("./views/TransfersView.vue") },
    { path: "/inbox", component: () => import("./views/InboxView.vue") },
    { path: "/devices", component: () => import("./views/DevicesView.vue") },
    { path: "/history", component: () => import("./views/HistoryView.vue") },
    { path: "/settings/:section?", component: () => import("./views/SettingsView.vue") },
    { path: "/diagnostics", component: () => import("./views/DiagnosticsView.vue") },
    { path: "/:rest(.*)*", redirect: "/" },
  ],
  scrollBehavior: () => ({ top: 0 }),
});
