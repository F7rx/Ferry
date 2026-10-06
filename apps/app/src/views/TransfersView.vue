<script setup lang="ts">
import { computed } from "vue";
import FButton from "../components/FButton.vue";
import TransferCard from "../components/TransferCard.vue";
import { activeTransfers, attempt, transfers } from "../stores/engine";
import { platform } from "../platform";

const finished = computed(() => transfers.value.filter((t) => !activeTransfers.value.includes(t)));
async function clearFinished() {
  for (const t of finished.value) await attempt(() => platform.dismiss(t.id));
}
</script>

<template>
  <div class="page">
    <header class="page-head">
      <div>
        <h1>Transfers</h1>
        <p>What's moving now, and what just finished.</p>
      </div>
      <div class="page-actions">
        <FButton v-if="finished.length" variant="ghost" size="sm" @click="clearFinished">Clear finished</FButton>
      </div>
    </header>
    <section v-if="activeTransfers.length" class="module glass" aria-labelledby="active">
      <div class="module-head"><h2 id="active">In progress</h2></div>
      <TransitionGroup tag="div" name="stack" class="stack">
        <TransferCard v-for="t in activeTransfers" :key="t.id" :transfer="t" />
      </TransitionGroup>
    </section>
    <section v-if="finished.length" class="module glass" aria-labelledby="done">
      <div class="module-head"><h2 id="done">Finished</h2></div>
      <TransitionGroup tag="div" name="stack" class="stack">
        <TransferCard v-for="t in finished" :key="t.id" :transfer="t" />
      </TransitionGroup>
    </section>
    <div v-if="!transfers.length" class="module glass empty">
      <strong>No transfers yet</strong>
      <span>Drop files on a nearby device to send them.</span>
    </div>
  </div>
</template>

<style scoped>
.stack {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.stack-enter-active {
  transition:
    opacity var(--dur-panel) var(--ease-out),
    transform var(--spring-soft-dur) var(--spring-soft);
}
.stack-leave-active {
  transition: opacity var(--dur-small) var(--ease-in);
}
.stack-enter-from {
  opacity: 0;
  transform: translateY(calc(-8px * var(--motion-scale)));
}
.stack-leave-to {
  opacity: 0;
}
.stack-move {
  transition: transform var(--dur-panel) var(--ease-out);
}
</style>
