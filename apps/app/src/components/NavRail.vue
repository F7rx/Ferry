<script setup lang="ts">
import { computed } from "vue";
import { Download, History, House, Inbox, MonitorSmartphone, Send, Settings } from "@lucide/vue";
import { activeTransfers, store } from "../stores/engine";
import { platform } from "../platform";
import DeviceAvatar from "./DeviceAvatar.vue";
import FIcon from "./FIcon.vue";
import FerryMark from "./FerryMark.vue";

defineProps<{ collapsed?: boolean }>();

const items = [
  { to: "/", label: "Home", icon: House },
  { to: "/send", label: "Send", icon: Send },
  { to: "/receive", label: "Receive", icon: Download },
  { to: "/inbox", label: "Inbox", icon: Inbox },
  { to: "/devices", label: "Devices", icon: MonitorSmartphone },
  { to: "/history", label: "History", icon: History },
];
const receiving = computed(() => store.settings?.receiveEnabled !== false && store.server.running);
</script>

<template>
  <nav class="rail glass" :class="{ collapsed }" aria-label="Main">
    <RouterLink to="/" class="brand" aria-label="Ferry home">
      <FerryMark :size="30" />
      <span v-if="!collapsed">Ferry</span>
    </RouterLink>

    <ul>
      <li v-for="item in items" :key="item.to">
        <RouterLink :to="item.to" class="item" :title="collapsed ? item.label : undefined" :exact-active-class="item.to === '/' ? 'active' : ''" :active-class="item.to === '/' ? '' : 'active'">
          <FIcon :icon="item.icon" :size="19" />
          <span v-if="!collapsed" class="label">{{ item.label }}</span>
          <span v-if="item.to === '/' && activeTransfers.length" class="count tabular" :aria-label="`${activeTransfers.length} active transfers`">{{ activeTransfers.length }}</span>
        </RouterLink>
      </li>
    </ul>

    <div class="bottom">
      <RouterLink to="/settings" class="item" active-class="active" :title="collapsed ? 'Settings' : undefined">
        <FIcon :icon="Settings" :size="19" />
        <span v-if="!collapsed" class="label">Settings</span>
      </RouterLink>
      <RouterLink v-if="store.local" to="/receive" class="me" :title="collapsed ? store.local.alias : undefined">
        <DeviceAvatar :kind="store.local.deviceKind" :model="store.local.deviceModel" :size="34" />
        <span v-if="!collapsed" class="me-text">
          <strong>{{ store.local.alias }}</strong>
          <span :class="{ off: !receiving }"><i class="dot" />{{ receiving ? "Receiving" : "Not receiving" }}</span>
        </span>
      </RouterLink>
      <p v-if="platform.capabilities.kind === 'demo' && !collapsed" class="preview">Preview with simulated devices</p>
    </div>
  </nav>
</template>

<style scoped>
.rail {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
  width: 232px;
  height: 100%;
  padding: 18px 12px 14px;
  border-radius: var(--radius-xl);
}
.rail.collapsed {
  width: 72px;
  align-items: center;
}
.brand {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 2px 8px 10px;
  color: var(--text-1);
  text-decoration: none;
  font-size: var(--text-lg);
  font-weight: 680;
  letter-spacing: -0.02em;
}
.collapsed .brand {
  padding: 2px 0 10px;
}
ul {
  display: flex;
  flex-direction: column;
  gap: 2px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.item {
  position: relative;
  display: flex;
  align-items: center;
  gap: 12px;
  height: 42px;
  padding: 0 12px;
  border-radius: var(--radius-sm);
  color: var(--text-2);
  text-decoration: none;
  font-size: var(--text-md);
  font-weight: 540;
  transition:
    background-color var(--dur-control) var(--ease-out),
    color var(--dur-control) var(--ease-out);
}
.collapsed .item {
  width: 46px;
  justify-content: center;
  padding: 0;
}
@media (hover: hover) {
  .item:hover {
    background: var(--fill-hover);
    color: var(--text-1);
  }
}
.item.active {
  background: var(--solid-1);
  color: var(--text-1);
  box-shadow:
    var(--shadow-1),
    inset 0 0 0 1px var(--hairline);
}
.item.active :deep(svg) {
  color: var(--accent);
}
.count {
  margin-left: auto;
  min-width: 20px;
  height: 20px;
  padding: 0 6px;
  border-radius: var(--radius-pill);
  background: var(--accent);
  color: var(--text-on-accent);
  font-size: var(--text-2xs);
  font-weight: 650;
  line-height: 20px;
  text-align: center;
}
.collapsed .count {
  position: absolute;
  top: 3px;
  right: 3px;
  min-width: 16px;
  height: 16px;
  padding: 0 4px;
  line-height: 16px;
}
.bottom {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin-top: auto;
}
.me {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px;
  border-radius: var(--radius-md);
  background: var(--fill-1);
  color: var(--text-1);
  text-decoration: none;
}
@media (hover: hover) {
  .me:hover {
    background: var(--fill-2);
  }
}
.collapsed .me {
  padding: 6px;
  background: none;
}
.me-text {
  display: flex;
  flex-direction: column;
  min-width: 0;
  line-height: 1.3;
}
.me-text strong {
  overflow: hidden;
  font-size: var(--text-sm);
  font-weight: 600;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.me-text span {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: var(--text-xs);
  color: var(--ok);
}
.me-text span.off {
  color: var(--text-3);
}
.dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: currentColor;
  box-shadow: 0 0 0 3px color-mix(in srgb, currentColor 18%, transparent);
}
.preview {
  padding: 6px 10px;
  border-radius: var(--radius-sm);
  background: var(--warn-soft);
  color: var(--warn);
  font-size: var(--text-2xs);
  font-weight: 600;
  text-align: center;
}
</style>
