import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { checkForApplicationUpdate } from "./updater";

type RecordCounts = {
  customers: number;
  contacts: number;
  sales: number;
  installments: number;
  payments: number;
};

type FinancialTotals = {
  sales_kurus: number;
  installments_kurus: number;
  payments_kurus: number;
};

type ImportSummary = {
  replaced: boolean;
  counts: RecordCounts;
  totals: FinancialTotals;
  sha256: string;
};

type DatabaseStatus = {
  path: string;
  database_id: string;
  schema_version: number;
  journal_mode: string;
  integrity_check: string;
  last_modified_at: string | null;
  onboarding_complete: boolean;
  import_verification_pending: boolean;
  last_import: ImportSummary | null;
  counts: RecordCounts;
  totals: FinancialTotals;
};

type BackupRunReport = {
  encryptedSnapshotCreated: true;
  safeToContinue: true;
  retentionClass: "rolling" | "daily" | "monthly";
  createdAt: string;
  uploadedCount: number;
  pendingCount: number;
  localRecoveryCount: number;
  queueHealthy: boolean;
  quarantinedFileCount: number;
  remoteResult: "uploaded" | "queued_offline" | "not_enrolled" | "local_recovery";
};

type BackupStatusReport = {
  enrolled: boolean;
  deviceId: string | null;
  pendingCount: number;
  localRecoveryCount: number;
  queueHealthy: boolean;
  quarantinedFileCount: number;
  lastAttemptAt: string | null;
  lastSnapshotAt: string | null;
  lastRemoteSuccessAt: string | null;
  nextScheduledAt: string | null;
  remote: {
    deviceId: string;
    serverTime: string;
    activePendingUploads: number;
    expiredPendingUploads: number;
    latestCompleted: {
      backupId: string;
      sizeBytes: number;
      sha256: string;
      completedAt: string;
    } | null;
  } | null;
  gatewayReachable: boolean | null;
};

type ModalMode = "first-run" | "maintenance";
const IMPORT_STATUS_ATTEMPTS = 3;
let applicationVersion = "Bilinmiyor";

const byId = <T extends HTMLElement>(id: string): T => {
  const element = document.getElementById(id);
  if (!element) throw new Error(`Required interface element is missing: ${id}`);
  return element as T;
};

function totalRecords(counts: RecordCounts): number {
  return Object.values(counts).reduce((total, count) => total + count, 0);
}

function isEmpty(status: DatabaseStatus): boolean {
  return totalRecords(status.counts) === 0;
}

function formatTimestamp(value: string | null): string {
  if (!value) return "Henüz değişiklik yok";
  const timestamp = new Date(value);
  return Number.isNaN(timestamp.valueOf()) ? value : timestamp.toLocaleString("tr-TR");
}

function formatKurus(value: number): string {
  const sign = value < 0 ? "-" : "";
  const absolute = Math.abs(value);
  const lira = Math.floor(absolute / 100);
  const kurus = String(absolute % 100).padStart(2, "0");
  return `${sign}${lira.toLocaleString("tr-TR")},${kurus} TL`;
}

function suggestedExportName(): string {
  const date = new Date();
  const part = (value: number): string => String(value).padStart(2, "0");
  const stamp = `${date.getFullYear()}-${part(date.getMonth() + 1)}-${part(date.getDate())}_${part(date.getHours())}-${part(date.getMinutes())}`;
  return `pusula-veri-${stamp}.json`;
}

async function readStatus(): Promise<DatabaseStatus> {
  return invoke<DatabaseStatus>("database_status");
}

async function readStatusWithRetry(): Promise<DatabaseStatus> {
  let lastError: unknown = new Error("Veritabanı durumu okunamadı.");
  for (let attempt = 1; attempt <= IMPORT_STATUS_ATTEMPTS; attempt += 1) {
    try {
      return await readStatus();
    } catch (error) {
      lastError = error;
      if (attempt < IMPORT_STATUS_ATTEMPTS) {
        await new Promise((resolve) => window.setTimeout(resolve, attempt * 250));
      }
    }
  }
  throw lastError;
}

function sameCounts(left: RecordCounts, right: RecordCounts): boolean {
  return (
    left.customers === right.customers &&
    left.contacts === right.contacts &&
    left.sales === right.sales &&
    left.installments === right.installments &&
    left.payments === right.payments
  );
}

function sameTotals(left: FinancialTotals, right: FinancialTotals): boolean {
  return (
    left.sales_kurus === right.sales_kurus &&
    left.installments_kurus === right.installments_kurus &&
    left.payments_kurus === right.payments_kurus
  );
}

function importVerificationError(status: DatabaseStatus, summary: ImportSummary): string | null {
  if (status.integrity_check !== "ok") return `SQLite bütünlük sonucu: ${status.integrity_check}`;
  if (!status.onboarding_complete) return "içe aktarma tamamlanma işareti kaydedilmedi";
  if (!sameCounts(status.counts, summary.counts)) return "güncel satır sayıları içe aktarma özetiyle eşleşmiyor";
  if (!sameTotals(status.totals, summary.totals)) return "güncel mali toplamlar içe aktarma özetiyle eşleşmiyor";
  if (!status.last_import) return "kalıcı içe aktarma özeti bulunamadı";
  if (
    status.last_import.sha256 !== summary.sha256 ||
    status.last_import.replaced !== summary.replaced ||
    !sameCounts(status.last_import.counts, summary.counts) ||
    !sameTotals(status.last_import.totals, summary.totals)
  ) {
    return "kalıcı içe aktarma kanıtı işlem özetiyle eşleşmiyor";
  }
  return null;
}

function replaceDescriptionList(id: string, rows: Array<[string, string]>): void {
  const list = byId<HTMLDListElement>(id);
  const fragment = document.createDocumentFragment();
  list.replaceChildren();
  for (const [label, value] of rows) {
    const row = document.createElement("div");
    const term = document.createElement("dt");
    const description = document.createElement("dd");
    term.textContent = label;
    description.textContent = value;
    row.append(term, description);
    fragment.append(row);
  }
  list.append(fragment);
}

function renderStatus(status: DatabaseStatus, importSummary = status.last_import): void {
  const rows: Array<[string, string]> = [
    ["Pusula sürümü", applicationVersion],
    ["Müşteriler", String(status.counts.customers)],
    ["Yakınlar", String(status.counts.contacts)],
    ["Satışlar", String(status.counts.sales)],
    ["Taksitler", String(status.counts.installments)],
    ["Ödemeler", String(status.counts.payments)],
    ["Güncel satış toplamı", formatKurus(status.totals.sales_kurus)],
    ["Güncel taksit toplamı", formatKurus(status.totals.installments_kurus)],
    ["Güncel ödeme toplamı", formatKurus(status.totals.payments_kurus)],
    ["Son değişiklik", formatTimestamp(status.last_modified_at)],
    ["Veritabanı denetimi", status.integrity_check === "ok" ? "Sağlam" : status.integrity_check],
  ];
  if (importSummary) {
    rows.push(
      ["Son içe aktarma satış toplamı", formatKurus(importSummary.totals.sales_kurus)],
      ["Son içe aktarma taksit toplamı", formatKurus(importSummary.totals.installments_kurus)],
      ["Son içe aktarma ödeme toplamı", formatKurus(importSummary.totals.payments_kurus)],
      ["Son içe aktarma SHA-256", importSummary.sha256],
    );
  }
  replaceDescriptionList("pusula-data-status", rows);
}

function describeRemoteResult(value: string): string {
  if (value === "uploaded" || value === "ok") return "Uzak yedek doğrulandı";
  if (value === "queued_offline" || value === "unreachable") return "Şifreli yedek sırada bekliyor";
  if (value === "not_enrolled") return "Kurulum kodu bekleniyor";
  if (value === "local_recovery") return "Yerel şifreli kurtarma kopyası hazır";
  return value || "Henüz deneme yok";
}

function renderBackupStatus(status: BackupStatusReport): void {
  const rows: Array<[string, string]> = [
    ["Uzak yedek", status.enrolled ? "Etkin" : "Etkin değil"],
    ["Şifreli sırada", String(status.pendingCount)],
    ["Yerel kurtarma kopyaları", String(status.localRecoveryCount)],
    [
      "Yedek sırası denetimi",
      status.queueHealthy
        ? "Sağlam"
        : `Dikkat gerekiyor (${status.quarantinedFileCount} karantinaya alınmış dosya)`,
    ],
    ["Son yerel yedek", formatTimestamp(status.lastSnapshotAt)],
    ["Son uzak doğrulama", formatTimestamp(status.lastRemoteSuccessAt)],
    [
      "Ağ geçidi",
      status.gatewayReachable === true
        ? "Erişilebilir"
        : status.gatewayReachable === false
          ? "Şu anda erişilemiyor"
          : "Kurulum kodu bekleniyor",
    ],
    ["Sonraki otomatik deneme", formatTimestamp(status.nextScheduledAt)],
  ];
  if (status.remote?.latestCompleted) {
    rows.push(
      ["Son uzak yedek kimliği", status.remote.latestCompleted.backupId],
      ["Son uzak yedek SHA-256", status.remote.latestCompleted.sha256],
    );
  }
  replaceDescriptionList("pusula-backup-status", rows);
  byId("pusula-backup-enrollment").hidden = status.enrolled;
}

async function refreshBackupStatus(): Promise<BackupStatusReport> {
  const status = await invoke<BackupStatusReport>("backup_status");
  renderBackupStatus(status);
  return status;
}

function setMessage(message: string, isError = false): void {
  const element = byId("pusula-data-modal-message");
  element.textContent = message;
  element.classList.toggle("is-error", isError);
}

function setBusy(busy: boolean): void {
  for (const id of [
    "pusula-import-button",
    "pusula-export-button",
    "pusula-empty-start-button",
    "pusula-data-modal-close",
    "pusula-update-check-button",
    "pusula-backup-now-button",
    "pusula-enroll-button",
  ]) {
    byId<HTMLButtonElement>(id).disabled = busy;
  }
  byId<HTMLInputElement>("pusula-enrollment-code").disabled = busy;
  byId<HTMLInputElement>("pusula-device-name").disabled = busy;
}

function blockAfterCommittedImport(message: string): void {
  // The database transaction completed, but the renderer could not prove what
  // is now on disk. Keep both first-run and maintenance controls unavailable
  // so stale pre-import state can never be used over the replacement DB.
  const modal = byId("pusula-data-modal");
  modal.dataset.fatal = "committed-import-unverified";
  modal.setAttribute("aria-busy", "true");
  setBusy(true);
  setMessage(
    `${message} Pusula iş ekranı açılmadı. Uygulamayı tamamen kapatıp yeniden açın; açılış denetimi yine başarısız olursa destek alın.`,
    true,
  );
}

async function chooseImportPath(): Promise<string | null> {
  const path = await open({
    title: "Pusula veri dosyasını seçin",
    multiple: false,
    directory: false,
    filters: [{ name: "Pusula JSON verisi", extensions: ["json"] }],
  });
  return typeof path === "string" ? path : null;
}

async function importFile(mode: ModalMode): Promise<boolean> {
  const path = await chooseImportPath();
  if (!path) return false;

  if (mode === "maintenance") {
    const acknowledgement = window.prompt(
      "İçe aktarma mevcut Pusula verisinin tamamını değiştirecek. Devam etmek için DEĞİŞTİR yazın.",
    );
    if (acknowledgement !== "DEĞİŞTİR") {
      setMessage("İçe aktarma iptal edildi.");
      return false;
    }

    setMessage("Mevcut veri için yerel şifreli kurtarma kopyası hazırlanacak; ardından dosya doğrulanacak…");
  }

  setBusy(true);
  setMessage("Dosya doğrulanıyor ve veriler içe aktarılıyor…");
  let summary: ImportSummary;
  try {
    summary = await invoke<ImportSummary>("import_data_file", {
      path,
      replace: mode === "maintenance",
    });
  } catch (error) {
    setBusy(false);
    setMessage(`İçe aktarma başarısız. Mevcut veriler değiştirilmedi. ${String(error)}`, true);
    return false;
  }

  let status: DatabaseStatus;
  try {
    status = await readStatusWithRetry();
  } catch (error) {
    blockAfterCommittedImport(
      `İçe aktarma veritabanına kaydedildi (SHA-256: ${summary.sha256}), ancak üç durum okuması da başarısız oldu. ${String(error)}`,
    );
    return false;
  }

  const verificationError = importVerificationError(status, summary);
  if (verificationError) {
    blockAfterCommittedImport(
      `İçe aktarma veritabanına kaydedildi ancak doğrulama başarısız: ${verificationError}.`,
    );
    return false;
  }

  try {
    await invoke("acknowledge_import_verification", { summary });
  } catch (error) {
    blockAfterCommittedImport(
      `İçe aktarma yeniden okundu ancak kalıcı doğrulama işareti temizlenemedi. ${String(error)}`,
    );
    return false;
  }

  renderStatus(status, summary);
  setMessage(`İçe aktarma tamamlandı ve yeniden okunarak doğrulandı. SHA-256: ${summary.sha256}`);
  setBusy(false);
  return true;
}

async function runBackupNow(): Promise<void> {
  setBusy(true);
  setMessage("Tutarlı veritabanı görüntüsü şifreleniyor…");
  let report: BackupRunReport;
  try {
    report = await invoke<BackupRunReport>("backup_now", { retentionClass: "rolling" });
  } catch (error) {
    setMessage(`Şifreli yerel yedek oluşturulamadı: ${String(error)}`, true);
    setBusy(false);
    return;
  }

  try {
    await refreshBackupStatus();
    setMessage(
      `Şifreli yedek hazır. ${describeRemoteResult(report.remoteResult)}; sırada ${report.pendingCount} dosya var.`,
    );
  } catch (error) {
    setMessage(
      `Şifreli yedek hazır (${describeRemoteResult(report.remoteResult)}), ancak durum ekranı yenilenemedi: ${String(error)}`,
      true,
    );
  }
  setBusy(false);
}

async function enrollBackup(): Promise<void> {
  const codeInput = byId<HTMLInputElement>("pusula-enrollment-code");
  const nameInput = byId<HTMLInputElement>("pusula-device-name");
  const enrollmentCode = codeInput.value.trim();
  const deviceName = nameInput.value.trim();
  if (enrollmentCode.length < 20 || !deviceName) {
    setMessage("Geçerli tek kullanımlık kurulum kodunu ve bilgisayar adını girin.", true);
    return;
  }

  setBusy(true);
  setMessage("Uzak yedek güvenli şekilde etkinleştiriliyor…");
  try {
    await invoke("backup_enroll", { enrollmentCode, deviceName });
  } catch (error) {
    setMessage(`Uzak yedek etkinleştirilemedi: ${String(error)}`, true);
    setBusy(false);
    return;
  }

  codeInput.value = "";
  setMessage("Uzak yedek etkin. İlk şifreli yedek hazırlanıyor…");
  let report: BackupRunReport;
  try {
    report = await invoke<BackupRunReport>("backup_now", { retentionClass: "rolling" });
  } catch (error) {
    setMessage(`Uzak yedek etkinleştirildi, ancak ilk yerel yedek oluşturulamadı: ${String(error)}`, true);
    setBusy(false);
    return;
  }

  try {
    await refreshBackupStatus();
    setMessage(`Uzak yedek etkin. ${describeRemoteResult(report.remoteResult)}.`);
  } catch (error) {
    setMessage(
      `Uzak yedek etkin ve ilk yedek hazır (${describeRemoteResult(report.remoteResult)}), ancak durum ekranı yenilenemedi: ${String(error)}`,
      true,
    );
  }
  setBusy(false);
}

async function exportFile(): Promise<void> {
  const path = await save({
    title: "Pusula verisini dışa aktarın",
    defaultPath: suggestedExportName(),
    filters: [{ name: "Pusula JSON verisi", extensions: ["json"] }],
  });
  if (!path) return;

  setBusy(true);
  setMessage("Dışa aktarma hazırlanıyor…");
  try {
    await invoke("export_data_file", { path, overwrite: true });
    setMessage(`Veriler doğrulanmış JSON dosyasına kaydedildi: ${path}`);
  } catch (error) {
    setMessage(`Dışa aktarma başarısız: ${String(error)}`, true);
  } finally {
    setBusy(false);
  }
}

function configureModal(mode: ModalMode): void {
  const modal = byId("pusula-data-modal");
  const closeButton = byId<HTMLButtonElement>("pusula-data-modal-close");
  const emptyButton = byId<HTMLButtonElement>("pusula-empty-start-button");
  const exportButton = byId<HTMLButtonElement>("pusula-export-button");
  const backupPanel = byId("pusula-backup-panel");
  modal.dataset.mode = mode;
  closeButton.hidden = mode === "first-run";
  emptyButton.hidden = mode !== "first-run";
  exportButton.hidden = mode === "first-run";
  byId<HTMLButtonElement>("pusula-update-check-button").hidden = mode === "first-run";
  backupPanel.hidden = mode === "first-run";
  byId("pusula-data-modal-title").textContent =
    mode === "first-run" ? "Pusula'yı İlk Kez Açın" : "Veri ve Yedek Yönetimi";
  byId("pusula-data-modal-description").textContent =
    mode === "first-run"
      ? "Eski WordPress Pusula verinizi içe aktarın veya yeni ve boş bir kayıt sistemiyle başlayın. İçe aktarma internet bağlantısı gerektirmez."
      : "Veritabanının durumunu denetleyin veya tamamını doğrulanabilir bir JSON dosyasına aktarın. İçe aktarma mevcut verinin tamamını değiştirir.";
  setMessage("");
  modal.hidden = false;
}

async function showFirstRun(status: DatabaseStatus): Promise<void> {
  configureModal("first-run");
  renderStatus(status);

  await new Promise<void>((resolve) => {
    const importButton = byId<HTMLButtonElement>("pusula-import-button");
    const emptyButton = byId<HTMLButtonElement>("pusula-empty-start-button");
    let confirmEmpty = false;

    importButton.onclick = async () => {
      if (await importFile("first-run")) {
        byId("pusula-data-modal").hidden = true;
        resolve();
      }
    };

    emptyButton.onclick = () => {
      if (!confirmEmpty) {
        confirmEmpty = true;
        emptyButton.textContent = "EVET, BOŞ BAŞLA";
        setMessage("Eski kayıtları daha sonra da içe aktarabilirsiniz. Boş başlamayı onaylamak için düğmeye tekrar basın.");
        return;
      }
      setBusy(true);
      setMessage("Boş veritabanı bu Pusula kurulumu için hazırlanıyor…");
      void invoke("acknowledge_empty_start")
        .then(() => {
          byId("pusula-data-modal").hidden = true;
          resolve();
        })
        .catch((error) => {
          setMessage(`Boş başlangıç kaydedilemedi: ${String(error)}`, true);
        })
        .finally(() => setBusy(false));
    };
  });
}

async function showMaintenance(): Promise<void> {
  configureModal("maintenance");
  const modal = byId("pusula-data-modal");
  const importButton = byId<HTMLButtonElement>("pusula-import-button");
  const exportButton = byId<HTMLButtonElement>("pusula-export-button");

  // Bind controls before any gateway request. An enrolled machine may be
  // offline, and its status probe is intentionally allowed to time out without
  // making the local maintenance dialog feel frozen.
  importButton.onclick = async () => {
    if (await importFile("maintenance")) window.location.reload();
  };
  exportButton.onclick = () => void exportFile();
  byId<HTMLButtonElement>("pusula-update-check-button").onclick = () =>
    void checkForApplicationUpdate(true);
  byId<HTMLButtonElement>("pusula-backup-now-button").onclick = () => void runBackupNow();
  byId<HTMLButtonElement>("pusula-enroll-button").onclick = () => void enrollBackup();
  byId<HTMLButtonElement>("pusula-data-modal-close").onclick = () => {
    modal.hidden = true;
  };

  try {
    renderStatus(await readStatus());
  } catch (error) {
    setMessage(`Veritabanı durumu okunamadı: ${String(error)}`, true);
  }

  try {
    await refreshBackupStatus();
  } catch (error) {
    setMessage(`Yedek durumu okunamadı: ${String(error)}`, true);
  }
}

export async function initializeDataManagement(): Promise<void> {
  try {
    applicationVersion = await getVersion();
  } catch (error) {
    console.warn("Pusula application version could not be read", error);
  }
  byId<HTMLButtonElement>("pusula-data-tools-button").onclick = () => void showMaintenance();
  const status = await readStatus();
  if (status.integrity_check !== "ok") {
    throw new Error(
      `SQLite bütünlük denetimi başarısız (${status.integrity_check}). Veri girişi engellendi; kurtarma runbook'unu kullanın.`,
    );
  }
  if (status.import_verification_pending) {
    if (!status.last_import) {
      throw new Error(
        "Doğrulama bekleyen içe aktarma için kalıcı özet bulunamadı. Veri girişi engellendi; destek alın.",
      );
    }
    const verificationError = importVerificationError(status, status.last_import);
    if (verificationError) {
      throw new Error(
        `Son içe aktarma açılış denetimini geçemedi (${verificationError}). Veri girişi engellendi; destek alın.`,
      );
    }
    await invoke("acknowledge_import_verification", { summary: status.last_import });
  }
  if (isEmpty(status) && !status.onboarding_complete) {
    await showFirstRun(status);
  }
}
