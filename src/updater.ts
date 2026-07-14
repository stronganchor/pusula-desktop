import { invoke } from "@tauri-apps/api/core";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { isSafeBackupReport, updateFailureMessage } from "./update-policy.js";

const SIX_HOURS_MS = 6 * 60 * 60 * 1000;
let checkInProgress = false;

type BackupRunReport = {
  encryptedSnapshotCreated: boolean;
  safeToContinue: boolean;
};

type UpdatePhase =
  | "checking"
  | "downloading"
  | "awaiting-confirmation"
  | "backing-up"
  | "installing"
  | "relaunching";

function setSystemStatus(message: string, isError = false): void {
  const element = document.getElementById("pusula-system-status");
  if (!element) return;
  element.textContent = message;
  element.classList.toggle("is-error", isError);
}

export async function checkForApplicationUpdate(showCurrentStatus = false): Promise<void> {
  if (!import.meta.env.PROD || !("__TAURI_INTERNALS__" in window) || checkInProgress) return;
  checkInProgress = true;
  let phase: UpdatePhase = "checking";
  let update: Update | null = null;

  try {
    update = await check();
    if (!update) {
      setSystemStatus(showCurrentStatus ? "Pusula güncel." : "");
      return;
    }
    const version = update.version;

    phase = "downloading";
    setSystemStatus(`Pusula ${version} indiriliyor…`);
    await update.download((event) => {
      if (event.event === "Progress" && event.data.chunkLength) {
        setSystemStatus(`Pusula ${version} indiriliyor…`);
      }
    });

    phase = "awaiting-confirmation";
    setSystemStatus(`Pusula ${version} kurulmaya hazır.`);
    const installNow = window.confirm(
      `Pusula ${version} indirildi. Veriler yedeklenip uygulama şimdi yeniden başlatılsın mı?`,
    );
    if (!installNow) return;

    phase = "backing-up";
    setSystemStatus("Güncelleme öncesi yedek hazırlanıyor…");
    const backup = await invoke<BackupRunReport>("prepare_for_update");
    if (!isSafeBackupReport(backup)) {
      throw new Error("Güncelleme öncesi şifreli yedek doğrulanamadı.");
    }

    phase = "installing";
    setSystemStatus("Güncelleme kuruluyor…");
    await update.install();

    phase = "relaunching";
    await relaunch();
  } catch (error) {
    console.warn(`Pusula update failed during ${phase}`, error);
    if (phase === "backing-up" || phase === "installing" || phase === "relaunching") {
      try {
        await invoke("cancel_prepared_update");
      } catch (releaseError) {
        console.warn("Pusula update maintenance lock could not be released", releaseError);
      }
    }
    // Being offline is normal. Only background check/download failures remain
    // quiet; every user-approved local phase reports its real failure.
    const message = updateFailureMessage(phase, showCurrentStatus);
    setSystemStatus(message ?? "", message !== null);
  } finally {
    // check() and download() allocate native resources. install() consumes the
    // byte resource, but every decline and failure path must still close the
    // Update handle or six-hour retries can retain installer-sized buffers.
    if (update) {
      try {
        await update.close();
      } catch (closeError) {
        console.warn("Pusula update resources could not be released", closeError);
      }
    }
    checkInProgress = false;
  }
}

export function startApplicationUpdater(): void {
  window.setTimeout(() => void checkForApplicationUpdate(), 15_000);
  window.setInterval(() => void checkForApplicationUpdate(), SIX_HOURS_MS);
}
