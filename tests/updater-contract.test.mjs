import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { isSafeBackupReport, updateFailureMessage } from "../src/update-policy.js";

const source = await readFile(new URL("../src/updater.ts", import.meta.url), "utf8");

test("installer is gated by an explicitly verified encrypted backup report", () => {
  const backup = source.indexOf('invoke<BackupRunReport>("prepare_for_update")');
  const safetyGate = source.indexOf("!isSafeBackupReport(backup)", backup);
  const install = source.indexOf("await update.install()", backup);
  assert.ok(backup >= 0 && safetyGate > backup && install > safetyGate);
  assert.equal(isSafeBackupReport(null), false);
  assert.equal(isSafeBackupReport({ encryptedSnapshotCreated: true, safeToContinue: false }), false);
  assert.equal(isSafeBackupReport({ encryptedSnapshotCreated: false, safeToContinue: true }), false);
  assert.equal(isSafeBackupReport({ encryptedSnapshotCreated: true, safeToContinue: true }), true);
});

test("update failures are reported according to their actual phase", () => {
  for (const phase of ["downloading", "backing-up", "installing", "relaunching"]) {
    assert.match(source, new RegExp(`phase = "${phase}"`));
  }
  assert.match(source, /updateFailureMessage\(phase, showCurrentStatus\)/);
  assert.match(source, /await invoke\("cancel_prepared_update"\)/);
  assert.equal(updateFailureMessage("checking", false), null);
  assert.equal(updateFailureMessage("downloading", false), null);
  assert.match(updateFailureMessage("downloading", true), /indirilemedi veya doğrulanamadı/);
  assert.match(updateFailureMessage("backing-up", false), /yedek doğrulanamadı/);
  assert.match(updateFailureMessage("installing", false), /kurulamadı/);
  assert.match(updateFailureMessage("relaunching", false), /kapatıp yeniden açın/);
});

test("declined and failed downloads always release their native update resources", () => {
  assert.match(source, /let update: Update \| null = null/);
  const decline = source.indexOf("if (!installNow) return");
  const finallyBlock = source.indexOf("} finally {");
  const close = source.indexOf("await update.close()", finallyBlock);
  assert.ok(decline >= 0 && finallyBlock > decline && close > finallyBlock);
  assert.equal((source.match(/await update\.close\(\)/g) || []).length, 1);
});
