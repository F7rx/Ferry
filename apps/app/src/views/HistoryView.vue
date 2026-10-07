<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { ArrowDownLeft, ArrowUpRight, CircleAlert, ShieldCheck } from "@lucide/vue";
import FButton from "../components/FButton.vue";
import FIcon from "../components/FIcon.vue";
import FSegmented from "../components/FSegmented.vue";
import { attempt, store } from "../stores/engine";
import { clearHistory } from "../stores/history";
import { platform, type Direction, type HistoryEntry } from "../platform";
import { clockTime, dayLabel, formatBytes } from "../lib/format";

const direction = ref<"all" | Direction>("all");
const entries = ref<HistoryEntry[]>([]);
const loading = ref(false);
const exhausted = ref(false);

async function load(more = false) {
  loading.value = true;
  const before = more ? entries.value.at(-1)?.id : undefined;
  const page = (await attempt(() => platform.history(100, before, direction.value === "all" ? undefined : direction.value))) ?? [];
  entries.value = more ? [...entries.value, ...page] : page;
  exhausted.value = page.length < 100;
  loading.value = false;
}
onMounted(() => load());

const merged = computed(() => {
  const seen = new Set(entries.value.map((e) => e.id));
  const fresh = store.history.filter((h) => !seen.has(h.id) && (direction.value === "all" || h.direction === direction.value));
  return [...fresh, ...entries.value];
});
const groups = computed(() => {
  const out: { day: string; items: HistoryEntry[] }[] = [];
  for (const e of merged.value) {
    const day = dayLabel(e.timestampMs);
    const last = out.at(-1);
    if (last?.day === day) last.items.push(e);
    else out.push({ day, items: [e] });
  }
  return out;
});
async function clearAll() {
  // Shared flow: confirms, and only changes what's shown once clearing worked.
  if (await clearHistory()) entries.value = [];
}
watch(() => store.historyRevision, () => load());
</script>

<template>
  <div class="page">
    <header class="page-head">
      <div>
        <h1>History</h1>
        <p>Names, sizes and devices only. Ferry keeps no copies of what you sent.</p>
      </div>
      <div class="page-actions">
        <FSegmented
          :model-value="direction"
          :options="[
            { value: 'all', label: 'All' },
            { value: 'send', label: 'Sent' },
            { value: 'receive', label: 'Received' },
          ]"
          label="Direction"
          @update:model-value="(v) => { direction = v; load(); }"
        />
        <FButton v-if="merged.length" variant="ghost" size="sm" @click="clearAll">Clear history</FButton>
      </div>
    </header>

    <div v-if="!merged.length && !loading" class="module glass empty">
      <strong>No history yet</strong>
      <span>Transfers show up here when they finish.</span>
    </div>

    <section v-for="g in groups" :key="g.day" class="module glass day" :aria-label="g.day">
      <h2 class="eyebrow">{{ g.day }}</h2>
      <ul class="row-list">
        <li v-for="e in g.items" :key="e.id" class="entry">
          <span class="dir" :class="e.direction">
            <FIcon :icon="e.direction === 'send' ? ArrowUpRight : ArrowDownLeft" :size="15" />
          </span>
          <span class="text">
            <strong>{{ e.name }}</strong>
            <span>{{ e.direction === "send" ? "To" : "From" }} {{ e.peerAlias }}</span>
          </span>
          <span class="size tabular">{{ e.kind === "text" ? "Message" : formatBytes(e.size) }}</span>
          <span class="status">
            <FIcon v-if="e.status !== 'completed'" :icon="CircleAlert" :size="14" class="bad" />
            <FIcon v-else-if="e.verified" :icon="ShieldCheck" :size="14" class="ok" />
            <span class="tabular">{{ e.status === "completed" ? clockTime(e.timestampMs) : e.status }}</span>
          </span>
        </li>
      </ul>
    </section>
    <FButton v-if="!exhausted && merged.length" class="more" variant="ghost" :disabled="loading" @click="load(true)">Load older</FButton>
  </div>
</template>

<style scoped>
.day .eyebrow {
  margin-bottom: var(--space-2);
}
.entry {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  min-height: 52px;
}
.dir {
  display: grid;
  place-items: center;
  flex: none;
  width: 30px;
  height: 30px;
  border-radius: 50%;
  background: var(--accent-softer);
  color: var(--accent-strong);
}
.dir.receive {
  background: var(--ok-soft);
  color: var(--ok);
}
.text {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
}
.text strong {
  overflow: hidden;
  font-size: var(--text-md);
  font-weight: 560;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.text span,
.size,
.status {
  font-size: var(--text-xs);
  color: var(--text-3);
}
.status {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  min-width: 72px;
  justify-content: flex-end;
}
.ok {
  color: var(--ok);
}
.bad {
  color: var(--danger);
}
.more {
  align-self: center;
}
</style>
