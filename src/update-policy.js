export function isSafeBackupReport(report) {
  return Boolean(
    report &&
      typeof report === 'object' &&
      report.encryptedSnapshotCreated === true &&
      report.safeToContinue === true,
  );
}

export function updateFailureMessage(phase, showCurrentStatus) {
  if ((phase === 'checking' || phase === 'downloading') && !showCurrentStatus) return null;
  if (phase === 'checking') return 'Güncelleme sunucusuna şu anda ulaşılamıyor.';
  if (phase === 'downloading') {
    return 'Güncelleme paketi indirilemedi veya doğrulanamadı; daha sonra yeniden deneyin.';
  }
  if (phase === 'backing-up') {
    return 'Güncelleme durduruldu: güvenli şifreli yedek doğrulanamadı. Veriler değiştirilmedi.';
  }
  if (phase === 'installing') {
    return 'Güncelleme kurulamadı. Mevcut Pusula ve veritabanı kullanılmaya devam edebilir.';
  }
  if (phase === 'relaunching') {
    return "Güncelleme kuruldu; tamamlamak için Pusula'yı kapatıp yeniden açın.";
  }
  return 'Güncelleme işlemi tamamlanamadı.';
}
