import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const source = await readFile(new URL("../src/data-management.ts", import.meta.url), "utf8");

test("first-run state belongs to SQLite and integrity failure blocks startup", () => {
  assert.equal(source.includes("localStorage"), false);
  assert.match(source, /status\.integrity_check !== "ok"/);
  assert.match(source, /invoke\("acknowledge_empty_start"\)/);
  assert.match(source, /!status\.onboarding_complete/);
});

test("destructive import delegates its atomic recovery gate to the backend", () => {
  assert.doesNotMatch(source, /invoke<BackupRunReport>\("prepare_for_destructive_import"\)/);
  assert.match(source, /invoke<ImportSummary>\("import_data_file"/);
  assert.match(source, /yerel şifreli kurtarma kopyası hazırlanacak/);
});

test("import completion evidence is retained and rendered", () => {
  assert.match(source, /summary = await invoke<ImportSummary>/);
  assert.match(source, /status\.last_import/);
  assert.match(source, /Son içe aktarma SHA-256/);
  assert.match(source, /importSummary\.totals\.sales_kurus/);
  assert.match(source, /status\.totals\.sales_kurus/);
  assert.match(source, /status\.integrity_check !== "ok"/);
  assert.match(source, /status\.last_import\.sha256 !== summary\.sha256/);
  assert.match(source, /sameCounts\(status\.counts, summary\.counts\)/);
  assert.match(source, /sameTotals\(status\.totals, summary\.totals\)/);
  assert.match(source, /await readStatusWithRetry\(\)/);
  assert.match(source, /return false;[\s\S]*const verificationError/);
});

test("backup queue degradation and retained local recovery are visible", () => {
  assert.match(source, /status\.localRecoveryCount/);
  assert.match(source, /status\.queueHealthy/);
  assert.match(source, /status\.quarantinedFileCount/);
});

test("maintenance controls are bound before an offline gateway status probe", () => {
  const maintenance = source.slice(source.indexOf("async function showMaintenance"));
  const closeHandler = maintenance.indexOf('"pusula-data-modal-close").onclick');
  const statusProbe = maintenance.indexOf("await refreshBackupStatus()");
  assert.ok(closeHandler >= 0 && statusProbe > closeHandler);
});
