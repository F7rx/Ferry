<script setup lang="ts">
import { ArrowUpDown, Inbox, Radio, Settings } from "@lucide/vue";
import { activeTransfers } from "../stores/engine";
import FIcon from "./FIcon.vue";

const tabs = [
  { to: "/", label: "Nearby", icon: Radio, exact: true },
  { to: "/transfers", label: "Transfers", icon: ArrowUpDown },
  { to: "/inbox", label: "Inbox", icon: Inbox },
  { to: "/settings", label: "Settings", icon: Settings },
];
</script>

<template>
  <nav class="tabbar glass-2" aria-label="Main">
    <RouterLink
      v-for="tab in tabs"
      :key="tab.to"
      :to="tab.to"
      class="tab"
      :exact-active-class="tab.exact ? 'active' : ''"
      :active-class="tab.exact ? '' : 'active'"
    >
      <span class="icon">
        <FIcon :icon="tab.icon" :size="22" />
        <span v-if="tab.to === '/transfers' && activeTransfers.length" class="badge tabular">{{ activeTransfers.length }}</span>
      </span>
      <span class="label">{{ tab.label }}</span>
    </RouterLink>
  </nav>
</template>

<style scoped>
.tabbar {
  position: fixed;
  left: 10px;
  right: 10px;
  bottom: calc(10px + env(safe-area-inset-bottom));
  z-index: 40;
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  height: 64px;
  padding: 6px;
  border-radius: var(--radius-xl);
}
.tab {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 2px;
  min-height: var(--hit);
  border-radius: var(--radius-md);
  color: var(--text-3);
  text-decoration: none;
  font-size: var(--text-2xs);
  font-weight: 600;
  transition: color var(--dur-control) var(--ease-out), background-color var(--dur-control) var(--ease-out);
}
.tab.active {
  color: var(--accent-text);
  background: var(--accent-softer);
}
.icon {
  position: relative;
}
.badge {
  position: absolute;
  top: -4px;
  right: -10px;
  min-width: 16px;
  height: 16px;
  padding: 0 4px;
  border-radius: var(--radius-pill);
  background: var(--accent);
  color: var(--text-on-accent);
  font-size: 10px;
  line-height: 16px;
  text-align: center;
}
</style>
