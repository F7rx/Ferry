// How a WebRTC connection travels: directly, or through a TURN relay. Read
// from the selected ICE candidate pair in `getStats()`; unknown when the
// browser doesn't say (null), never guessed.

interface StatsLike {
  get(id: string): Record<string, unknown> | undefined;
  forEach(fn: (stat: Record<string, unknown>) => void): void;
}

/** The subset of RTCPeerConnection used here. */
export interface StatsSource {
  getStats(): Promise<unknown>;
}

/** True when either end of the selected candidate pair is a relay candidate, false when neither is, null when unknown. */
export async function relayedOf(pc: StatsSource | null | undefined): Promise<boolean | null> {
  if (!pc || typeof pc.getStats !== "function") return null;
  let report: StatsLike;
  try {
    report = (await pc.getStats()) as StatsLike;
  } catch {
    return null;
  }
  if (!report || typeof report.forEach !== "function" || typeof report.get !== "function") return null;
  let pair: Record<string, unknown> | undefined;
  let selectedId: string | undefined;
  report.forEach((stat) => {
    if (stat.type === "transport" && typeof stat.selectedCandidatePairId === "string") selectedId = stat.selectedCandidatePairId;
  });
  if (selectedId) pair = report.get(selectedId);
  if (!pair) {
    // Firefox marks the pair itself; others only nominate it once it succeeded.
    report.forEach((stat) => {
      if (pair || stat.type !== "candidate-pair") return;
      if (stat.selected === true || (stat.nominated === true && stat.state === "succeeded")) pair = stat;
    });
  }
  if (!pair) return null;
  const local = typeof pair.localCandidateId === "string" ? report.get(pair.localCandidateId) : undefined;
  const remote = typeof pair.remoteCandidateId === "string" ? report.get(pair.remoteCandidateId) : undefined;
  const types = [local?.candidateType, remote?.candidateType];
  if (types.includes("relay")) return true;
  return types.every((t) => typeof t === "string") ? false : null;
}
