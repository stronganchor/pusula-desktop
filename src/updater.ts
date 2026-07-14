import { invoke } from "@tauri-apps/api/core";
import { relaunch } from "@tauri-apps/plugin-process";
import { check } from "@tauri-apps/plugin-updater";

const SIX_HOURS_MS = 6 * 60 * 60 * 1000;
let checkInProgress = false;

function setSystemStatus(message: string, isError = false): void {
  const element = document.getElementById("pusula-system-status");
  if (!element) return;
  element.textContent = message;
  element.classList.toggle("is-error", isError);
}

export async function checkForApplicationUpdate(): Promise<void> {
  if (!import.meta.env.PROD || !("__TAURI_INTERNALS__" in window) || checkInProgress) return;
  checkInProgress = true;

  try {
    const update = await check();
    if (!update) {
      setSystemStatus("");
      return;
    }

    setSystemStatus(`Pusula ${update.version} indiriliyor…`);
    await update.download((event) => {
      if (event.event === "Progress" && event.data.chunkLength) {
        setSystemStatus(`Pusula ${update.version} indiriliyor…`);
      }
    });

    setSystemStatus(`Pusula ${update.version} kurulmaya hazır.`);
    const installNow = window.confirm(
      `Pusula ${update.version} indirildi. Veriler yedeklenip uygulama şimdi yeniden başlatılsın mı?`,
    );
    if (!installNow) return;

    setSystemStatus("Güncelleme öncesi yedek hazırlanıyor…");
    await invoke("prepare_for_update");
    setSystemStatus("Güncelleme kuruluyor…");
    await update.install();
    await relaunch();
  } catch (error) {
    console.warn("Pusula update check failed", error);
    setSystemStatus("Güncelleme daha sonra yeniden denenecek.", true);
  } finally {
    checkInProgress = false;
  }
}

export function startApplicationUpdater(): void {
  window.setTimeout(() => void checkForApplicationUpdate(), 15_000);
  window.setInterval(() => void checkForApplicationUpdate(), SIX_HOURS_MS);
}
