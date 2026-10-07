// @vitest-environment node
import { describe, expect, it } from "vitest";
import { relayedOf } from "./route";

type Stat = Record<string, unknown>;
const pc = (stats: Stat[] | Error) => ({
  getStats: async () => {
    if (stats instanceof Error) throw stats;
    return new Map(stats.map((s) => [s.id as string, s]));
  },
});
const candidates = (local: string | undefined, remote: string | undefined): Stat[] => [
  { id: "L", type: "local-candidate", ...(local ? { candidateType: local } : {}) },
  { id: "R", type: "remote-candidate", ...(remote ? { candidateType: remote } : {}) },
];

describe("relayedOf", () => {
  it("reads the transport's selected pair", async () => {
    const report = (l: string, r: string) => [
      { id: "T", type: "transport", selectedCandidatePairId: "P" },
      { id: "P", type: "candidate-pair", localCandidateId: "L", remoteCandidateId: "R" },
      // A pair that isn't selected must not count.
      { id: "Q", type: "candidate-pair", localCandidateId: "X", remoteCandidateId: "R", nominated: true, state: "succeeded" },
      { id: "X", type: "local-candidate", candidateType: "relay" },
      ...candidates(l, r),
    ];
    expect(await relayedOf(pc(report("host", "srflx")))).toBe(false);
    expect(await relayedOf(pc(report("relay", "host")))).toBe(true);
    expect(await relayedOf(pc(report("host", "relay")))).toBe(true);
  });

  it("falls back to the nominated, succeeded pair, or Firefox's selected flag", async () => {
    const nominated = [{ id: "P", type: "candidate-pair", localCandidateId: "L", remoteCandidateId: "R", nominated: true, state: "succeeded" }, ...candidates("prflx", "host")];
    expect(await relayedOf(pc(nominated))).toBe(false);
    const firefox = [{ id: "P", type: "candidate-pair", localCandidateId: "L", remoteCandidateId: "R", selected: true }, ...candidates("relay", "host")];
    expect(await relayedOf(pc(firefox))).toBe(true);
  });

  it("is unknown when the browser doesn't say", async () => {
    expect(await relayedOf(null)).toBeNull();
    expect(await relayedOf(pc(new Error("closed")))).toBeNull();
    expect(await relayedOf(pc([]))).toBeNull();
    // A pair still checking isn't the route.
    expect(await relayedOf(pc([{ id: "P", type: "candidate-pair", localCandidateId: "L", remoteCandidateId: "R", nominated: true, state: "in-progress" }, ...candidates("host", "host")]))).toBeNull();
    // Candidate types missing.
    expect(await relayedOf(pc([{ id: "P", type: "candidate-pair", localCandidateId: "L", remoteCandidateId: "R", selected: true }, ...candidates(undefined, "host")]))).toBeNull();
  });
});
