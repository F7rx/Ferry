// Clearing history and received files: one flow for every screen that offers
// it, honest about what each platform keeps. Native: clearing history never
// touches received files (they are on disk). Browser: received files live in
// this browser's storage and the Inbox is the only way back to them, so
// clearing history keeps them, and deleting them is a separate, explicit step.
import { platform } from "../platform";
import { attempt, store, toast } from "./engine";

/** True where received files are kept inside the app (the browser) and can be deleted from it. */
export const keepsReceivedFiles = platform.capabilities.kind === "web" && typeof platform.clearReceivedFiles === "function";

/** What clearing history does here, for a settings row. */
export const clearHistoryDetail = keepsReceivedFiles
  ? "Sent items and messages are removed. Received files stay in the Inbox."
  : "Received files stay where they are.";

const clearHistoryQuestion = keepsReceivedFiles
  ? "Clear the whole history? Sent items and messages are removed. Received files stay in the Inbox."
  : "Clear the whole history? Received files stay where they are.";

/** Asks, then clears the history. Returns true only when it was cleared; nothing changes when cancelled or failed. */
export async function clearHistory(): Promise<boolean> {
  if (!confirm(clearHistoryQuestion)) return false;
  const done = await attempt(async () => {
    await platform.clearHistory();
    return true;
  });
  if (!done) return false;
  store.history = [];
  // Received files stay listed; messages went with the history.
  store.inbox = store.inbox.filter((e) => e.kind === "file");
  store.historyRevision++;
  toast({ level: "success", title: "History cleared" }, 2400);
  return true;
}

/** Browser: asks, then deletes every file received in this browser. Reports files that couldn't be deleted. */
export async function deleteReceivedFiles(): Promise<boolean> {
  const clear = platform.clearReceivedFiles;
  if (!keepsReceivedFiles || !clear) return false;
  if (!confirm("Delete every file received in this browser? Save the ones you want to keep from the Inbox first. This can't be undone.")) return false;
  const result = await attempt(() => clear.call(platform));
  if (!result) return false;
  store.inbox = store.inbox.filter((e) => e.kind !== "file");
  store.history = store.history.map((e) => (e.path ? { ...e, path: null } : e));
  store.historyRevision++;
  const files = (n: number) => `${n} ${n === 1 ? "file" : "files"}`;
  if (result.failed) {
    toast(
      {
        level: "error",
        title: `${files(result.failed)} couldn't be deleted`,
        body: `${result.deleted ? `${files(result.deleted)} deleted. ` : ""}Try again. If it keeps failing, clear this site's data in your browser settings.`,
      },
      9000,
    );
  } else {
    toast({ level: "success", title: result.deleted ? `Deleted ${files(result.deleted)}` : "No received files to delete" }, 2400);
  }
  return result.failed === 0;
}
