import { check, type DownloadEvent, type Update } from "@tauri-apps/plugin-updater";

export type UpdateDownloadProgress = {
  downloadedBytes: number;
  totalBytes: number | null;
};

const inTauri = () => "__TAURI_INTERNALS__" in window;

/**
 * Checks the configured, signed release feed. Browser development mode has no
 * native updater, so it intentionally behaves as if no update were available.
 */
export async function checkForAppUpdate(): Promise<Update | null> {
  // Release CI injects the maintainer-owned public key. Local development has
  // no updater configuration and must not request signing material.
  if (!inTauri() || import.meta.env.DEV) return null;
  return check({ timeout: 15_000 });
}

/** Downloads and starts the verified native installer. */
export async function downloadAndInstallAppUpdate(
  update: Update,
  onProgress: (progress: UpdateDownloadProgress) => void,
): Promise<void> {
  let downloadedBytes = 0;
  let totalBytes: number | null = null;

  await update.downloadAndInstall((event: DownloadEvent) => {
    if (event.event === "Started") {
      totalBytes = event.data.contentLength ?? null;
      onProgress({ downloadedBytes, totalBytes });
      return;
    }
    if (event.event === "Progress") {
      downloadedBytes += event.data.chunkLength;
      onProgress({ downloadedBytes, totalBytes });
    }
  }, { timeout: 120_000, restartAfterInstall: true });
}
