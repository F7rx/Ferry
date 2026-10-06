<script setup lang="ts">
import { onMounted, ref } from "vue";
import { CircleAlert, CircleCheck, CircleHelp, RefreshCw, TriangleAlert } from "@lucide/vue";
import FButton from "../components/FButton.vue";
import FIcon from "../components/FIcon.vue";
import { attempt, store } from "../stores/engine";
import { platform, type DiagnosticCheck } from "../platform";

const checks = ref<DiagnosticCheck[]>([]);
const running = ref(false);
const ranAt = ref<number | null>(null);
const icons = { ok: CircleCheck, warning: TriangleAlert, error: CircleAlert, unknown: CircleHelp };

async function run() {
  running.value = true;
  await attempt(() => platform.refreshDevices());
  checks.value = (await attempt(() => platform.diagnostics())) ?? [];
  ranAt.value = Date.now();
  running.value = false;
}
onMounted(run);
</script>

<template>
  <div class="page">
    <header class="page-head">
      <div>
        <h1>Diagnostics</h1>
        <p>If devices don't show up or transfers fail, start here.</p>
      </div>
      <div class="page-actions">
        <FButton variant="primary" :icon="RefreshCw" :disabled="running" @click="run">{{ running ? "Testing…" : "Run network test" }}</FButton>
      </div>
    </header>

    <section class="module glass" aria-live="polite" :aria-busy="running">
      <ul class="row-list">
        <li v-for="c in checks" :key="c.id" class="check" :class="c.status">
          <FIcon :icon="icons[c.status]" :size="20" class="status-icon" />
          <div class="text">
            <div class="line">
              <strong>{{ c.label }}</strong>
              <span class="value tabular">{{ c.value }}</span>
            </div>
            <p v-if="c.detail" class="detail">{{ c.detail }}</p>
          </div>
        </li>
      </ul>
      <p v-if="!checks.length && !running" class="empty">No results yet.</p>
    </section>

    <section class="module glass">
      <div class="module-head"><h2>This device</h2></div>
      <dl class="facts">
        <div><dt>Name</dt><dd>{{ store.local?.alias }}</dd></div>
        <div><dt>Identity</dt><dd class="mono">{{ store.local?.shortId }}</dd></div>
        <template v-if="platform.capabilities.kind === 'web'">
          <div><dt>Browser</dt><dd>{{ store.local?.deviceModel }}</dd></div>
          <div><dt>Transport</dt><dd>WebRTC (encrypted)</dd></div>
        </template>
        <template v-else>
          <div><dt>Addresses</dt><dd class="mono">{{ store.local?.addresses.join(", ") || "None" }}</dd></div>
          <div><dt>Port</dt><dd class="mono">{{ store.local?.port }} ({{ store.local?.protocol }})</dd></div>
        </template>
        <div><dt>Version</dt><dd class="mono">{{ store.local?.appVersion }}</dd></div>
      </dl>
    </section>
  </div>
</template>

<style scoped>
.check {
  display: flex;
  align-items: flex-start;
  gap: var(--space-3);
  padding: var(--space-3) 0;
}
.status-icon {
  margin-top: 1px;
}
.ok .status-icon {
  color: var(--ok);
}
.warning .status-icon {
  color: var(--warn);
}
.error .status-icon {
  color: var(--danger);
}
.unknown .status-icon {
  color: var(--text-3);
}
.text {
  flex: 1;
  min-width: 0;
}
.line {
  display: flex;
  justify-content: space-between;
  gap: var(--space-4);
}
.line strong {
  font-size: var(--text-md);
  font-weight: 600;
}
.value {
  font-size: var(--text-sm);
  color: var(--text-2);
  text-align: right;
}
.detail {
  margin-top: 4px;
  font-size: var(--text-sm);
  color: var(--text-3);
  max-width: 70ch;
  text-wrap: pretty;
}
.facts {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
  gap: var(--space-3);
  margin: 0;
}
.facts div {
  padding: 10px 12px;
  border-radius: var(--radius-md);
  background: var(--fill-1);
}
.facts dt {
  font-size: var(--text-xs);
  color: var(--text-3);
}
.facts dd {
  margin: 2px 0 0;
  overflow-wrap: anywhere;
  font-size: var(--text-sm);
}
</style>
