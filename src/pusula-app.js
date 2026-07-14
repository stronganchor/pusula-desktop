(() => {
  const state = {
    customers: [],
    customerBase: [],
    customerLoadSeq: 0,
    isLoadingCustomers: false,
    isSavingCustomer: false,
    isSavingSale: false,
    autoRefreshHandle: null,
    autoRefreshMs: 15000,
    snapshotSyncHandle: null,
    snapshotSyncMs: 300000,
    selected: null,
    selectionSeq: 0,
    pendingCustomerId: null,
    addFormCustomerId: null,
    currentSaleExplicit: false,
    sales: [],
    salesLoadSeq: 0,
    reportSales: [],
    expectedPayments: [],
    currentExpected: null,
    currentInstallment: null,
    currentInstallmentPayment: null,
    pendingInstallmentId: null,
    pendingPaymentId: null,
    offlineSnapshot: null,
    offlineLastSyncedAt: '',
    hasOfflineSnapshot: false,
    isOfflineMode: false,
    isSyncingOfflineSnapshot: false,
  };

  const apiBase = (window.PusulaApp && PusulaApp.apiBase) || '';
  const wpNonce = (window.PusulaApp && PusulaApp.nonce) || '';
  const desktopMode = Boolean(window.PusulaApp && PusulaApp.desktop);
  const offlineConfig = (window.PusulaApp && PusulaApp.offline && typeof PusulaApp.offline === 'object')
    ? PusulaApp.offline
    : {};
  const offlineEnabled = !desktopMode && false !== offlineConfig.enabled;
  const offlineServiceWorkerUrl = String(offlineConfig.swUrl || '').trim();
  const offlineAssetUrls = Array.isArray(offlineConfig.assetUrls) ? offlineConfig.assetUrls.filter(Boolean) : [];
  const offlineDbName = 'pusula-lite-offline';
  const offlineStoreName = 'kv';
  const offlineSnapshotKey = 'snapshot';

  function getBusinessProfile() {
    const raw = (state.offlineSnapshot && state.offlineSnapshot.business && typeof state.offlineSnapshot.business === 'object')
      ? state.offlineSnapshot.business
      : ((window.PusulaApp && PusulaApp.business && typeof PusulaApp.business === 'object')
        ? PusulaApp.business
        : {});

    return {
      name: String(raw.name || '').trim(),
      address: String(raw.address || '').trim(),
      phone: String(raw.phone || '').trim(),
      website: String(raw.website || '').trim(),
      footerSub: String(raw.footerSub || raw.footer_sub || '').trim(),
    };
  }

  function cloneData(value) {
    return value == null ? value : JSON.parse(JSON.stringify(value));
  }

  function createClientRequestKey(scope) {
    const prefix = String(scope || 'request').replace(/[^a-z0-9_-]/gi, '').toLowerCase() || 'request';
    if (window.crypto && typeof window.crypto.randomUUID === 'function') {
      return `${prefix}-${window.crypto.randomUUID()}`;
    }
    return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2)}`;
  }

  function normalizeUrl(url) {
    try {
      const parsed = new URL(url, window.location.origin);
      parsed.hash = '';
      return parsed.toString();
    } catch (error) {
      return '';
    }
  }

  function formatSyncDate(value) {
    if (!value) return '';
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) return String(value);
    return parsed.toLocaleString('tr-TR');
  }

  function buildBusinessContactLine(company) {
    const parts = [];
    if (company.phone) parts.push(`Telefon: ${company.phone}`);
    if (company.website) parts.push(`Web: ${company.website}`);
    return parts.join(' | ');
  }

  const todayStr = () => {
    const d = new Date();
    const pad = (n) => (n < 10 ? `0${n}` : n);
    return `${pad(d.getDate())}-${pad(d.getMonth() + 1)}-${d.getFullYear()}`;
  };
  const toISO = (value) => {
    const input = String(value || '').trim();
    if (!input) return '';
    if (/^\d{4}-\d{2}-\d{2}$/.test(input)) return input;
    let m = input.match(/^(\d{2})-(\d{2})-(\d{4})$/);
    if (m) return `${m[3]}-${m[2]}-${m[1]}`;
    m = input.match(/^(\d{2})\/(\d{2})\/(\d{4})$/);
    if (m) return `${m[3]}-${m[2]}-${m[1]}`;
    return '';
  };
  const fromISO = (iso) => {
    if (!iso) return '';
    const [y, m, d] = iso.split('-');
    return `${d}-${m}-${y}`;
  };

  const fromISOSlash = (iso) => {
    if (!iso) return '';
    const [y, m, d] = iso.split('-');
    return `${d}/${m}/${y}`;
  };

  function parseISODate(iso) {
    const s = String(iso || '').trim();
    const m = s.match(/^(\d{4})-(\d{2})-(\d{2})$/);
    if (!m) return null;
    const y = Number(m[1]);
    const mo = Number(m[2]);
    const d = Number(m[3]);
    if (!y || mo < 1 || mo > 12 || d < 1 || d > 31) return null;
    return new Date(y, mo - 1, d);
  }

  function formatISODate(date) {
    const d = date instanceof Date ? date : new Date(date);
    if (!d || Number.isNaN(d.getTime())) return '';
    const pad = (n) => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  }

  const formatMoney = (value) => {
    const num = Number(value);
    if (!Number.isFinite(num)) return '0,00₺';
    return `${num.toLocaleString('tr-TR', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}₺`;
  };

  const defaultDueDay = () => Math.min(new Date().getDate(), 28);
  const defaultFirstDueMonthOffset = () => 1;

  function parseFirstDueMonthOffset(value) {
    return String(value) === '0' ? 0 : 1;
  }

  function calculateInstallmentDueDate(saleBase, dueDay, installmentNumber, firstDueMonthOffset = 1) {
    if (!(saleBase instanceof Date) || Number.isNaN(saleBase.getTime())) return null;

    const day = Number(dueDay);
    const installmentIndex = Number(installmentNumber);
    if (!Number.isFinite(day) || !Number.isFinite(installmentIndex) || installmentIndex < 1) return null;

    const safeDueDay = Math.min(Math.max(Math.trunc(day), 1), 28);
    const monthOffset = parseFirstDueMonthOffset(firstDueMonthOffset);
    const dueDate = new Date(saleBase.getFullYear(), saleBase.getMonth() + monthOffset + Math.trunc(installmentIndex) - 1, 1);
    dueDate.setDate(safeDueDay);
    return dueDate;
  }

  const isPaid = (val) => Number(val) === 1;
  const moneyNumber = (value) => {
    const num = Number(value);
    return Number.isFinite(num) ? Math.round(num * 100) / 100 : 0;
  };
  const saleInstallments = (sale) => (sale && Array.isArray(sale.installments) ? sale.installments : []);
  const installmentAmount = (inst) => moneyNumber(inst && inst.amount ? inst.amount : 0);
  const installmentPaidAmount = (inst) => {
    if (!inst) return 0;
    if (Number.isFinite(Number(inst.paid_amount))) return Number(inst.paid_amount);
    const amount = installmentAmount(inst);
    return isPaid(inst.paid) ? amount : 0;
  };
  const installmentRemainingAmount = (inst) => {
    if (!inst) return 0;
    if (Number.isFinite(Number(inst.remaining_amount))) return Math.max(0, Number(inst.remaining_amount));
    const amount = installmentAmount(inst);
    const paid = installmentPaidAmount(inst);
    return Math.max(0, Math.round((amount - paid) * 100) / 100);
  };
  const hasUnpaidInstallments = (sale) => {
    const insts = sale && sale.installments;
    if (!Array.isArray(insts) || !insts.length) return false;
    return insts.some((inst) => installmentRemainingAmount(inst) > 0);
  };
  const saleReportCashAmount = (sale) => {
    const saleTotal = moneyNumber(sale && sale.total);
    const insts = saleInstallments(sale);
    if (!insts.length) return saleTotal;
    const totalInst = insts.reduce((sum, inst) => sum + installmentAmount(inst), 0);
    return moneyNumber(Math.max(0, saleTotal - totalInst));
  };

  function openOfflineDb() {
    return new Promise((resolve, reject) => {
      if (!window.indexedDB) {
        reject(new Error('Tarayıcı çevrimdışı veriyi desteklemiyor.'));
        return;
      }

      const request = window.indexedDB.open(offlineDbName, 1);
      request.onupgradeneeded = () => {
        const db = request.result;
        if (!db.objectStoreNames.contains(offlineStoreName)) {
          db.createObjectStore(offlineStoreName, { keyPath: 'key' });
        }
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error || new Error('Çevrimdışı veritabanı açılamadı.'));
    });
  }

  function offlineStoreGet(key) {
    return openOfflineDb().then((db) => new Promise((resolve, reject) => {
      const tx = db.transaction(offlineStoreName, 'readonly');
      const request = tx.objectStore(offlineStoreName).get(key);
      request.onsuccess = () => resolve(request.result ? request.result.value : null);
      request.onerror = () => reject(request.error || new Error('Çevrimdışı kayıt okunamadı.'));
      tx.oncomplete = () => db.close();
      tx.onerror = () => reject(tx.error || new Error('Çevrimdışı okuma tamamlanamadı.'));
    }));
  }

  function offlineStoreSet(key, value) {
    return openOfflineDb().then((db) => new Promise((resolve, reject) => {
      const tx = db.transaction(offlineStoreName, 'readwrite');
      tx.objectStore(offlineStoreName).put({ key, value });
      tx.oncomplete = () => {
        db.close();
        resolve();
      };
      tx.onerror = () => reject(tx.error || new Error('Çevrimdışı kayıt yazılamadı.'));
    }));
  }

  function offlineStoreDelete(key) {
    return openOfflineDb().then((db) => new Promise((resolve, reject) => {
      const tx = db.transaction(offlineStoreName, 'readwrite');
      tx.objectStore(offlineStoreName).delete(key);
      tx.oncomplete = () => {
        db.close();
        resolve();
      };
      tx.onerror = () => reject(tx.error || new Error('Çevrimdışı kayıt silinemedi.'));
    }));
  }

  function applyOfflineSnapshot(snapshot) {
    const next = snapshot && typeof snapshot === 'object' ? cloneData(snapshot) : null;
    state.offlineSnapshot = next;
    state.hasOfflineSnapshot = Boolean(next && Array.isArray(next.customers));
    state.offlineLastSyncedAt = next && next.synced_at ? String(next.synced_at) : '';
  }

  async function loadOfflineSnapshotFromStorage() {
    if (!offlineEnabled) return null;
    try {
      const snapshot = await offlineStoreGet(offlineSnapshotKey);
      applyOfflineSnapshot(snapshot);
      return state.offlineSnapshot;
    } catch (error) {
      applyOfflineSnapshot(null);
      return null;
    }
  }

  async function saveOfflineSnapshot(snapshot) {
    if (!offlineEnabled || !snapshot || typeof snapshot !== 'object') return;
    await offlineStoreSet(offlineSnapshotKey, snapshot);
    applyOfflineSnapshot(snapshot);
  }

  async function clearOfflineSnapshotStorage() {
    state.offlineSnapshot = null;
    state.offlineLastSyncedAt = '';
    state.hasOfflineSnapshot = false;
    try {
      await offlineStoreDelete(offlineSnapshotKey);
    } catch (error) {
      // ignore cleanup failures
    }
  }

  async function registerOfflineShell() {
    if (!offlineEnabled || !offlineServiceWorkerUrl || !('serviceWorker' in navigator)) {
      return;
    }

    const shellUrl = normalizeUrl(`${window.location.origin}${window.location.pathname}`);
    const payload = {
      type: 'CONFIGURE_OFFLINE',
      shellUrl,
      assetUrls: offlineAssetUrls.map((url) => normalizeUrl(url)).filter(Boolean),
    };

    try {
      const registration = await navigator.serviceWorker.register(offlineServiceWorkerUrl, { scope: '/' });
      const activeWorker = registration.active || registration.waiting || registration.installing;
      if (activeWorker) {
        activeWorker.postMessage(payload);
      }
      const ready = await navigator.serviceWorker.ready;
      const readyWorker = ready.active || navigator.serviceWorker.controller;
      if (readyWorker) {
        readyWorker.postMessage(payload);
      }
    } catch (error) {
      // Best effort only.
    }
  }

  function getOfflineSnapshot() {
    return state.offlineSnapshot && typeof state.offlineSnapshot === 'object' ? state.offlineSnapshot : null;
  }

  function ensureWriteAllowed() {
    if (!state.isOfflineMode) return true;
    setStatus('Çevrimdışı modda veriler salt okunur gösterilir.', true);
    return false;
  }

  function updateOfflineBanner() {
    const banner = document.getElementById('pusula-offline-banner');
    if (!banner) return;

    if (!state.isOfflineMode) {
      banner.hidden = true;
      banner.textContent = '';
      return;
    }

    const suffix = state.offlineLastSyncedAt
      ? ` Son senkronizasyon: ${formatSyncDate(state.offlineLastSyncedAt)}.`
      : '';

    banner.hidden = false;
    banner.textContent = state.hasOfflineSnapshot
      ? `Çevrimdışı mod açık. Veriler yalnızca görüntülenebilir.${suffix}`
      : 'Çevrimdışı mod etkin, ancak tarayıcıda kayıtlı veri bulunamadı.';
  }

  function updateReadOnlyUi() {
    const app = document.getElementById('pusula-lite-app');
    if (app) {
      app.classList.toggle('read-only', state.isOfflineMode);
    }

    const addTab = document.querySelector('.pusula-tabs button[data-tab="add"]');
    const saleTab = document.querySelector('.pusula-tabs button[data-tab="sale"]');
    if (addTab) addTab.disabled = state.isOfflineMode;
    if (saleTab) saleTab.disabled = state.isOfflineMode;

    const forceDisabledIds = [
      'cust-save',
      'cust-clear',
      'sale-save',
      'sale-clear',
      'nav-edit',
      'nav-sale',
      'nav-delete',
      'btn-edit-sale',
      'btn-delete-sale',
      'btn-add-payment',
      'btn-mark-paid',
      'btn-mark-unpaid',
      'exp-mark-paid',
      'exp-mark-unpaid',
    ];
    forceDisabledIds.forEach((id) => {
      const el = document.getElementById(id);
      if (el) el.disabled = state.isOfflineMode;
    });

    document.querySelectorAll('#pusula-tab-add input, #pusula-tab-add textarea').forEach((el) => {
      el.disabled = state.isOfflineMode;
    });
    document.querySelectorAll('#pusula-tab-sale input, #pusula-tab-sale textarea, #pusula-tab-sale select').forEach((el) => {
      el.disabled = state.isOfflineMode;
    });

    const instPayInput = document.getElementById('inst-pay-amount');
    const expPayInput = document.getElementById('exp-pay-amount');
    if (instPayInput) instPayInput.disabled = state.isOfflineMode;
    if (expPayInput) expPayInput.disabled = state.isOfflineMode;

    if (state.isOfflineMode && ['add', 'sale'].includes(getActiveTabName())) {
      activateTab('search');
    }

    updateOfflineBanner();
  }

  function stopAutoRefresh() {
    if (state.autoRefreshHandle) {
      clearInterval(state.autoRefreshHandle);
      state.autoRefreshHandle = null;
    }
  }

  function setOfflineMode(isOffline) {
    const nextValue = desktopMode ? false : Boolean(isOffline);
    const changed = state.isOfflineMode !== nextValue;
    state.isOfflineMode = nextValue;
    if (changed) {
      if (state.isOfflineMode) {
        stopAutoRefresh();
      } else {
        startAutoRefresh();
      }
    }
    updateReadOnlyUi();
  }

  function extractSnapshotArray(key) {
    const snapshot = getOfflineSnapshot();
    if (!snapshot || !Array.isArray(snapshot[key])) return [];
    return cloneData(snapshot[key]);
  }

  function filterOfflineCustomers(searchParams) {
    const rows = extractSnapshotArray('customers');
    const limit = Math.max(1, Math.min(5000, Number(searchParams.get('limit') || 100)));
    const offset = Math.max(0, Number(searchParams.get('offset') || 0));
    const search = String(searchParams.get('search') || '').trim().toLowerCase();
    const id = String(searchParams.get('id') || '').trim();
    const name = String(searchParams.get('name') || '').trim().toLowerCase();
    const phone = String(searchParams.get('phone') || '').trim().toLowerCase();
    const address = String(searchParams.get('address') || '').trim().toLowerCase();

    const filtered = rows.filter((row) => {
      if (id && String(row.id || '') !== id && !String(row.id || '').includes(id)) return false;
      if (search) {
        const haystack = [row.name, row.phone, row.address, row.work_address].join(' ').toLowerCase();
        if (!haystack.includes(search)) return false;
      }
      if (name && !String(row.name || '').toLowerCase().includes(name)) return false;
      if (phone && !String(row.phone || '').toLowerCase().includes(phone)) return false;
      if (address) {
        const home = String(row.address || '').toLowerCase();
        const work = String(row.work_address || '').toLowerCase();
        if (!home.includes(address) && !work.includes(address)) return false;
      }
      return true;
    });

    return filtered.slice(offset, offset + limit);
  }

  function filterOfflineSales(searchParams) {
    const rows = extractSnapshotArray('sales');
    const customerId = String(searchParams.get('customer_id') || '').trim();
    const start = String(searchParams.get('start') || '').trim();
    const end = String(searchParams.get('end') || '').trim();
    const withInstallments = String(searchParams.get('with') || '').includes('installments');

    return rows
      .filter((row) => {
        if (customerId && String(row.customer_id || '') !== customerId) return false;
        if (start && String(row.date || '') < start) return false;
        if (end && String(row.date || '') > end) return false;
        return true;
      })
      .map((row) => {
        if (withInstallments) return row;
        const next = { ...row };
        delete next.installments;
        delete next.installments_total;
        delete next.installments_paid_total;
        delete next.installments_remaining_total;
        return next;
      });
  }

  function filterOfflineDailyReport(searchParams) {
    const rows = extractSnapshotArray('sales');
    const start = String(searchParams.get('start') || '').trim();
    const end = String(searchParams.get('end') || '').trim();
    const reportRows = [];

    rows.forEach((sale) => {
      const saleDate = String(sale.date || '');
      const saleId = Number(sale.id || 0);
      const saleAmount = saleReportCashAmount(sale);
      const insts = saleInstallments(sale);

      if ((!start || saleDate >= start) && (!end || saleDate <= end) && saleAmount > 0) {
        reportRows.push({
          event_id: `sale-${saleId}`,
          event_type: insts.length ? 'down_payment' : 'sale',
          id: saleId,
          sale_id: saleId,
          payment_id: null,
          installment_id: null,
          customer_id: Number(sale.customer_id || 0),
          customer_name: sale.customer_name || '',
          date: saleDate,
          total: saleAmount,
          amount: saleAmount,
          sale_total: moneyNumber(sale.total),
          description: sale.description || '',
        });
      }

      insts.forEach((inst) => {
        const payments = Array.isArray(inst.payments) ? inst.payments : [];
        payments.forEach((payment) => {
          const paymentDate = String(payment.payment_date || '');
          const amount = moneyNumber(payment.amount);
          if ((!start || paymentDate >= start) && (!end || paymentDate <= end) && amount > 0) {
            reportRows.push({
              event_id: `payment-${payment.id || ''}`,
              event_type: 'installment_payment',
              id: saleId,
              sale_id: saleId,
              payment_id: payment.id || null,
              installment_id: inst.id || inst.installment_id || null,
              customer_id: Number(sale.customer_id || 0),
              customer_name: sale.customer_name || '',
              date: paymentDate,
              total: amount,
              amount,
              sale_total: moneyNumber(sale.total),
              description: sale.description || '',
            });
          }
        });
      });
    });

    return reportRows.sort((a, b) => {
      const dateA = String(a.date || '');
      const dateB = String(b.date || '');
      if (dateA !== dateB) return dateB.localeCompare(dateA);
      return String(b.event_id || '').localeCompare(String(a.event_id || ''));
    });
  }

  function filterOfflineExpectedPayments(searchParams) {
    const rows = extractSnapshotArray('expected_payments');
    const start = String(searchParams.get('start') || '').trim();
    const end = String(searchParams.get('end') || '').trim();
    return rows.filter((row) => {
      if (start && String(row.due_date || '') < start) return false;
      if (end && String(row.due_date || '') > end) return false;
      return true;
    });
  }

  async function offlineApi(path) {
    const snapshot = getOfflineSnapshot() || await loadOfflineSnapshotFromStorage();
    if (!snapshot) {
      throw new Error('Çevrimdışı kullanım için önceden eşitlenmiş veri bulunamadı.');
    }

    const url = new URL(path, 'https://pusula.local');
    const pathname = url.pathname.replace(/\/+$/, '') || '/';

    if (pathname === '/customers') {
      return filterOfflineCustomers(url.searchParams);
    }

    if (pathname === '/sales') {
      return filterOfflineSales(url.searchParams);
    }

    if (pathname === '/daily-report') {
      return filterOfflineDailyReport(url.searchParams);
    }

    if (pathname === '/expected-payments') {
      return filterOfflineExpectedPayments(url.searchParams);
    }

    const customerMatch = pathname.match(/^\/customers\/(\d+)$/);
    if (customerMatch) {
      const customerId = String(customerMatch[1]);
      const row = extractSnapshotArray('customers').find((customer) => String(customer.id || '') === customerId);
      if (!row) throw new Error('Müşteri bulunamadı.');
      return row;
    }

    const paymentsMatch = pathname.match(/^\/installments\/(\d+)\/payments$/);
    if (paymentsMatch) {
      const installmentId = String(paymentsMatch[1]);
      const sales = extractSnapshotArray('sales');
      for (let i = 0; i < sales.length; i += 1) {
        const sale = sales[i];
        const installment = Array.isArray(sale.installments)
          ? sale.installments.find((row) => String(row.id || row.installment_id || '') === installmentId)
          : null;
        if (installment) {
          return {
            installment,
            payments: Array.isArray(installment.payments) ? installment.payments : [],
          };
        }
      }
      throw new Error('Taksit bulunamadı.');
    }

    throw new Error('Bu veri çevrimdışı kullanılamıyor.');
  }

  async function syncOfflineSnapshot({ silent = true } = {}) {
    if (!offlineEnabled || state.isSyncingOfflineSnapshot || !navigator.onLine) {
      return false;
    }

    state.isSyncingOfflineSnapshot = true;
    try {
      const snapshot = await api('/offline-snapshot', { allowOfflineFallback: false });
      await saveOfflineSnapshot(snapshot);
      await registerOfflineShell();
      if (!silent && state.offlineLastSyncedAt) {
        setStatus(`Çevrimdışı kopya güncellendi. (${formatSyncDate(state.offlineLastSyncedAt)})`);
      }
      return true;
    } catch (error) {
      return false;
    } finally {
      state.isSyncingOfflineSnapshot = false;
      updateOfflineBanner();
    }
  }

  function startOfflineSnapshotSync() {
    if (desktopMode) return;
    if (state.snapshotSyncHandle) clearInterval(state.snapshotSyncHandle);
    state.snapshotSyncHandle = setInterval(() => {
      if (!state.isOfflineMode && navigator.onLine) {
        syncOfflineSnapshot({ silent: true });
      }
    }, state.snapshotSyncMs);
  }

  function scheduleOfflineSnapshotSync() {
    if (desktopMode) return;
    if (!state.isOfflineMode && navigator.onLine) {
      syncOfflineSnapshot({ silent: true });
    }
  }

  async function handleConnectionRestore() {
    if (desktopMode) return;
    const synced = await syncOfflineSnapshot({ silent: true });
    if (!synced) return;
    setOfflineMode(false);
    autoRefreshTick();
  }

  function handleConnectionLoss() {
    if (desktopMode) return;
    setOfflineMode(true);
  }

  function ensureLayoutFits() {
    const app = document.getElementById('pusula-lite-app');
    if (!app) return;
    const needsScroll = app.scrollHeight > app.clientHeight + 1;
    app.classList.toggle('allow-scroll', needsScroll);
  }

  function setCustomersLoading(isLoading) {
    const wrapper = document.querySelector('#pusula-tab-search .pusula-table-wrapper');
    if (!wrapper) return;
    wrapper.classList.toggle('loading', Boolean(isLoading));
  }

  function setInstallmentsLoading(isLoading) {
    const table = document.getElementById('pusula-inst-table');
    if (!table) return;
    const wrapper = table.closest('.pusula-table-wrapper');
    if (!wrapper) return;
    wrapper.classList.toggle('loading', Boolean(isLoading));
  }

  function setExpectedLoading(isLoading) {
    const table = document.getElementById('exp-table');
    if (!table) return;
    const wrapper = table.closest('.pusula-table-wrapper');
    if (!wrapper) return;
    wrapper.classList.toggle('loading', Boolean(isLoading));
  }

  function getCustomerSearchFields() {
    const root = document.getElementById('pusula-tab-search');
    if (!root) return {};
    const fields = {};
    root.querySelectorAll('input[data-field]').forEach((input) => {
      const val = input.value.trim();
      if (val) fields[input.getAttribute('data-field')] = val;
    });
    return fields;
  }

  function normalizeText(s) {
    return String(s || '').toLowerCase();
  }

  function truncateDescription(text, maxLen = 70) {
    const raw = String(text || '').replace(/\r\n/g, '\n');
    const firstLine = raw.split('\n')[0] || '';
    const cleaned = firstLine.trim();
    if (!cleaned) return '';
    if (cleaned.length <= maxLen) return cleaned;
    return `${cleaned.slice(0, Math.max(0, maxLen - 1))}…`;
  }

  function filterCustomers(rows, fields) {
    const id = String(fields.id || '').trim();
    const name = normalizeText(fields.name);
    const phone = normalizeText(fields.phone);
    const address = normalizeText(fields.address);
    if (!id && !name && !phone && !address) return rows;
    return (rows || []).filter((c) => {
      if (id && !String(c.id || '').includes(id)) return false;
      if (name && !normalizeText(c.name).includes(name)) return false;
      if (phone && !normalizeText(c.phone).includes(phone)) return false;
      if (address) {
        const addr = normalizeText(c.address);
        const work = normalizeText(c.work_address);
        if (!addr.includes(address) && !work.includes(address)) return false;
      }
      return true;
    });
  }

  function applyCustomerFilterLocal() {
    const fields = getCustomerSearchFields();
    const base = state.customerBase && state.customerBase.length ? state.customerBase : state.customers;
    renderTable(filterCustomers(base || [], fields));
  }

  function customerIdOf(customer) {
    return String(customer && customer.id != null ? customer.id : '').trim();
  }

  function setSelectedCustomer(customer) {
    const next = customer || null;
    const prevId = customerIdOf(state.selected);
    const nextId = customerIdOf(next);
    state.selected = next;
    state.pendingCustomerId = null;
    if (prevId !== nextId) state.selectionSeq += 1;
    return state.selected;
  }

  function selectedCustomerSnapshot() {
    if (!state.selected || !customerIdOf(state.selected)) return null;
    return { ...state.selected };
  }

  function isSelectedCustomerStillCurrent(customerId, selectionSeq) {
    return selectionSeq === state.selectionSeq && customerIdOf(state.selected) === String(customerId || '').trim();
  }

  const trErrorMessage = (msg) => {
    if (!msg) return 'Bilinmeyen hata.';
    const normalized = String(msg).toLowerCase();
    if (normalized.includes('<!doctype') || normalized.includes('<html')) return 'Sunucuya ulaşılamadı.';
    if (normalized.includes('customer not found')) return 'Müşteri bulunamadı.';
    if (normalized.includes('not found')) return 'Kayıt bulunamadı.';
    if (normalized.includes('forbidden') || normalized.includes('unauthorized')) return 'Yetki hatası. Lütfen oturum açın.';
    return msg;
  };

  function setStatus(msg, isError = false) {
    const el = document.getElementById('pusula-status');
    if (el) {
      el.textContent = msg || '';
      el.style.color = isError ? '#f0a7a7' : '#88b7c9';
    }
  }

  function shouldFallbackToOfflineFromResponse(response) {
    if (!response) return true;
    const status = Number(response.status || 0);
    return status >= 500;
  }

  async function api(path, opts = {}) {
    if (!apiBase) throw new Error('API ayarları eksik.');
    const method = String(opts.method || 'GET').toUpperCase();
    if (desktopMode) {
      if (typeof window.pusulaDesktopApi !== 'function') {
        throw new Error('Masaüstü veri bağlantısı hazır değil.');
      }
      try {
        return await window.pusulaDesktopApi(path, { ...opts, method });
      } catch (error) {
        const message = error && error.message ? error.message : String(error || 'İstek başarısız.');
        throw new Error(message);
      }
    }
    const allowOfflineFallback = false !== opts.allowOfflineFallback;
    const requestOptions = {
      ...opts,
      headers: {
        'Content-Type': 'application/json',
        'X-WP-Nonce': wpNonce,
        ...(opts.headers || {}),
      },
      credentials: 'same-origin',
    };

    let res;
    try {
      res = await fetch(`${apiBase}${path}`, requestOptions);
    } catch (networkError) {
      if (allowOfflineFallback && method === 'GET') {
        try {
          const offlineResponse = await offlineApi(path);
          setOfflineMode(true);
          return offlineResponse;
        } catch (offlineError) {
          throw networkError;
        }
      }
      throw networkError;
    }
    if (!res.ok) {
      if (allowOfflineFallback && method === 'GET' && shouldFallbackToOfflineFromResponse(res)) {
        try {
          const offlineResponse = await offlineApi(path);
          setOfflineMode(true);
          return offlineResponse;
        } catch (offlineError) {
          // Fall back to the original server error if there is no cached snapshot.
        }
      }
      const text = await res.text();
      let msg = text;
      try {
        msg = JSON.parse(text).message || msg;
      } catch (e) { /* ignore */ }
      throw new Error(msg || 'İstek başarısız.');
    }
    if (method === 'GET') {
      setOfflineMode(false);
    }
    if (res.status === 204) return null;
    return res.json();
  }

  const debounce = (fn, wait = 200) => {
    let t;
    return (...args) => {
      clearTimeout(t);
      t = setTimeout(() => fn(...args), wait);
    };
  };

  function activateTab(tab) {
    const tabName = String(tab || '');
    document.querySelectorAll('.pusula-tabs button').forEach((b) => {
      b.classList.toggle('active', b.getAttribute('data-tab') === tabName);
    });
    document.querySelectorAll('.pusula-tab-content').forEach((c) => (c.style.display = 'none'));
    const target = document.getElementById(`pusula-tab-${tabName}`);
    if (target) target.style.display = '';

    if (tabName === 'add') {
      if (state.selected) {
        const selectedId = customerIdOf(state.selected);
        const formMatchesSelected = selectedId && state.addFormCustomerId === selectedId;
        if (!formMatchesSelected || !isAddFormDirty()) fillCustomerForm(state.selected);
      } else if (!isAddFormDirty()) {
        fillCustomerForm();
      }
    }
    if (tabName === 'sale') {
      if (state.selected) fillSaleCustomer(state.selected);
      else syncSaleCustomerFromPending();
    }
    if (tabName === 'detail') {
      if (state.selected) {
        updateDetail(state.selected);
        loadSales(state.selected.id, state.currentSale ? state.currentSale.id : null);
      } else {
        syncDetailFromPending();
      }
    }
    if (tabName === 'report') {
      loadReport();
    }
    if (tabName === 'expected') {
      loadExpectedPayments();
    }
    setTimeout(ensureLayoutFits, 0);
  }

  function renderTabs() {
    document.querySelectorAll('.pusula-tabs button').forEach((btn) => {
      btn.addEventListener('click', () => activateTab(btn.getAttribute('data-tab')));
    });
  }

  function getActiveTabName() {
    const btn = document.querySelector('.pusula-tabs button.active');
    return btn ? btn.getAttribute('data-tab') : 'search';
  }

  function isUserInteracting() {
    const app = document.getElementById('pusula-lite-app');
    if (!app) return false;
    const active = document.activeElement;
    if (!active || !app.contains(active)) return false;
    return ['INPUT', 'TEXTAREA', 'SELECT'].includes(active.tagName);
  }

  function isAddFormDirty() {
    const ids = [
      'cust-name',
      'cust-phone',
      'cust-address',
      'cust-work',
      'cust-notes',
      'c1-name',
      'c1-phone',
      'c1-home',
      'c1-work',
      'c2-name',
      'c2-phone',
      'c2-home',
      'c2-work',
    ];
    return ids.some((id) => {
      const el = document.getElementById(id);
      if (!el) return false;
      return String(el.value || '').trim().length > 0;
    });
  }

  async function autoRefreshTick() {
    if (state.isOfflineMode) return;
    if (document.hidden) return;
    if (state.isSavingCustomer || state.isSavingSale || state.isLoadingCustomers) return;
    if (document.querySelector('.pusula-modal-backdrop')) return;
    if (isUserInteracting()) return;

    const tab = getActiveTabName();
    try {
      await loadCustomers({ silent: true, showLoading: false });
      if (tab === 'detail' && state.selected) {
        await loadSales(state.selected.id, state.currentSale ? state.currentSale.id : null);
      }
      if (tab === 'report') {
        await loadReport({ silent: true });
      }
      if (tab === 'expected') {
        await loadExpectedPayments({ silent: true, showLoading: false });
      }
      if (tab === 'add' && !state.selected && !isAddFormDirty()) {
        const localNext = nextCustomerId();
        refreshNewCustomerIdFromServer(localNext);
      }
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    }
  }

  function startAutoRefresh() {
    if (state.isOfflineMode) return;
    if (state.autoRefreshHandle) clearInterval(state.autoRefreshHandle);
    state.autoRefreshHandle = setInterval(autoRefreshTick, state.autoRefreshMs);
  }

  function openTextModal({
    title,
    hideTitle = false,
    infoRows = [],
    textareaLabel,
    value,
    saveLabel,
    onSave,
    deleteLabel,
    onDelete,
  }) {
    const existing = document.querySelector('.pusula-modal-backdrop');
    if (existing) existing.remove();

    const hasInfo = Array.isArray(infoRows) && infoRows.length > 0;
    const hasDelete = typeof onDelete === 'function';
	    const showTitle = !hideTitle && title;
	    const backdrop = document.createElement('div');
	    backdrop.className = 'pusula-modal-backdrop';
	    backdrop.innerHTML = `
	      <div class="pusula-modal" role="dialog" aria-modal="true">
	        <div class="pusula-modal-header ${showTitle ? '' : 'no-title'}">
	          ${showTitle ? `<div class="pusula-modal-title">${title}</div>` : ''}
	          <button class="pusula-modal-close" type="button" data-action="close" aria-label="Kapat">✕</button>
	        </div>
	        <div class="pusula-modal-body">
	          <div class="pusula-modal-main ${hasInfo ? 'two-col' : 'one-col'}">
	            ${hasInfo ? '<div class="pusula-modal-info"></div>' : ''}
	            <div class="pusula-modal-editor">
	              ${textareaLabel ? `<div class="pusula-modal-label">${textareaLabel}</div>` : ''}
	              <textarea class="pusula-modal-textarea" rows="10"></textarea>
	              <div class="pusula-modal-status" aria-live="polite"></div>
	            </div>
	          </div>
	        </div>
	        <div class="pusula-modal-actions">
	          ${hasDelete ? `<button type="button" class="pusula-modal-btn danger" data-action="delete">${deleteLabel || 'SİL'}</button>` : ''}
	          <button type="button" class="pusula-modal-btn" data-action="cancel">VAZGEÇ</button>
	          <button type="button" class="pusula-modal-btn primary" data-action="save">${saveLabel || 'KAYDET'}</button>
	        </div>
	      </div>
	    `;

    document.body.appendChild(backdrop);

    const modal = backdrop.querySelector('.pusula-modal');
    const infoEl = backdrop.querySelector('.pusula-modal-info');
    const textarea = backdrop.querySelector('.pusula-modal-textarea');
    const statusEl = backdrop.querySelector('.pusula-modal-status');
    const deleteBtn = backdrop.querySelector('[data-action="delete"]');
    const saveBtn = backdrop.querySelector('[data-action="save"]');
    const cancelBtn = backdrop.querySelector('[data-action="cancel"]');
    const closeBtn = backdrop.querySelector('[data-action="close"]');

    if (infoEl && hasInfo) {
      infoRows.forEach((row) => {
        const label = row && row.label ? String(row.label) : '';
        const val = row && row.value !== undefined && row.value !== null ? String(row.value) : '';
        if (!label && !val) return;
        const rowEl = document.createElement('div');
        rowEl.className = 'pusula-modal-info-row';
        const labelEl = document.createElement('div');
        labelEl.className = 'pusula-modal-info-label';
        labelEl.textContent = label;
        const valueEl = document.createElement('div');
        valueEl.className = 'pusula-modal-info-value';
        valueEl.textContent = val;
        rowEl.appendChild(labelEl);
        rowEl.appendChild(valueEl);
        infoEl.appendChild(rowEl);
      });
    }

    if (textarea) {
      textarea.value = value || '';
      textarea.focus();
      textarea.setSelectionRange(textarea.value.length, textarea.value.length);
    }

    const close = () => {
      document.removeEventListener('keydown', onKeyDown);
      backdrop.remove();
      setTimeout(ensureLayoutFits, 0);
    };

    const setModalStatus = (msg, isError = false) => {
      if (!statusEl) return;
      statusEl.textContent = msg || '';
      statusEl.style.color = isError ? '#f0a7a7' : '#88b7c9';
    };

    const onKeyDown = (e) => {
      if (e.key === 'Escape') close();
    };
    document.addEventListener('keydown', onKeyDown);

    backdrop.addEventListener('click', (e) => {
      if (e.target === backdrop) close();
    });
    if (cancelBtn) cancelBtn.addEventListener('click', close);
    if (closeBtn) closeBtn.addEventListener('click', close);

    if (saveBtn) {
      saveBtn.addEventListener('click', async () => {
        if (!ensureWriteAllowed()) return;
        if (!onSave) return close();
        const nextVal = textarea ? textarea.value : '';
        if (deleteBtn) deleteBtn.disabled = true;
        saveBtn.disabled = true;
        if (cancelBtn) cancelBtn.disabled = true;
        if (closeBtn) closeBtn.disabled = true;
        setModalStatus('Kaydediliyor...');
        try {
          await onSave(nextVal);
          close();
        } catch (err) {
          setModalStatus(`Hata: ${trErrorMessage(err.message)}`, true);
          if (deleteBtn) deleteBtn.disabled = false;
          saveBtn.disabled = false;
          if (cancelBtn) cancelBtn.disabled = false;
          if (closeBtn) closeBtn.disabled = false;
        }
      });
    }

    if (deleteBtn) {
      deleteBtn.addEventListener('click', async () => {
        if (!ensureWriteAllowed()) return;
        if (!onDelete) return close();
        deleteBtn.disabled = true;
        if (saveBtn) saveBtn.disabled = true;
        if (cancelBtn) cancelBtn.disabled = true;
        if (closeBtn) closeBtn.disabled = true;
        setModalStatus('Siliniyor...');
        try {
          const deleted = await onDelete();
          if (deleted === false) {
            setModalStatus('');
            deleteBtn.disabled = false;
            if (saveBtn) saveBtn.disabled = false;
            if (cancelBtn) cancelBtn.disabled = false;
            if (closeBtn) closeBtn.disabled = false;
            return;
          }
          close();
        } catch (err) {
          setModalStatus(`Hata: ${trErrorMessage(err.message)}`, true);
          deleteBtn.disabled = false;
          if (saveBtn) saveBtn.disabled = false;
          if (cancelBtn) cancelBtn.disabled = false;
          if (closeBtn) closeBtn.disabled = false;
        }
      });
    }

    if (modal) modal.addEventListener('click', (e) => e.stopPropagation());
  }

  function openSaleDescriptionEditor(sale, { customer, onUpdated } = {}) {
    if (!ensureWriteAllowed()) return;
    if (!sale || !sale.id) return;
    const custId = customer && customer.id ? customer.id : sale.customer_id;
    const custName = customer && customer.name ? customer.name : sale.customer_name;
    const infoRows = [
      { label: 'Satış No', value: sale.id },
      { label: 'Tarih', value: fromISO(sale.date) },
      { label: 'Tutar', value: formatMoney(sale.total || 0) },
      { label: 'Müşteri No', value: custId || '' },
      { label: 'Adı Soyadı', value: custName || '' },
    ];
    openTextModal({
      hideTitle: true,
      infoRows,
      textareaLabel: 'Açıklama',
      value: sale.description || '',
      saveLabel: 'KAYDET',
      deleteLabel: 'SİL',
      onDelete: () => deleteSale(sale),
      onSave: async (nextText) => {
        const next = String(nextText ?? '');
        await api(`/sales/${sale.id}`, {
          method: 'PUT',
          body: JSON.stringify({
            description: next,
          }),
        });
        sale.description = next;
        setStatus('Açıklama güncellendi.');
        if (onUpdated) onUpdated(next);
      },
    });
  }

  // ------------------- Search -------------------
  function renderSearchTab() {
    const root = document.getElementById('pusula-tab-search');
    if (!root) return;
    root.innerHTML = `
      <div class="pusula-grid">
        ${['Müşteri No', 'İsim', 'Telefon', 'Adres'].map((label, idx) => `
          <label class="pusula-input">
            <span>${label}</span>
            <input type="text" data-field="${['id','name','phone','address'][idx]}">
          </label>
        `).join('')}
      </div>
      <div class="pusula-table-wrapper">
        <table class="pusula-table" id="pusula-search-table">
          <thead><tr><th>No</th><th>Kayıt Tarihi</th><th>İsim</th><th>Telefon</th><th>Adres</th><th>Toplam Borç</th></tr></thead>
          <tbody></tbody>
        </table>
      </div>
      <div class="pusula-actions" id="pusula-nav-actions">
        <button id="nav-edit" disabled>MÜŞTERİ BİLGİLERİNİ DÜZELT</button>
        <button id="nav-sale" disabled>SATIŞ KAYDET</button>
        <button id="nav-detail" disabled>TAKSİTLİ SATIŞ KAYIT BİLGİSİ</button>
        <button id="nav-delete" disabled>MÜŞTERİ SİL</button>
      </div>
    `;
    const debouncedRemote = debounce(loadCustomers, 150);
    root.querySelectorAll('input[data-field]').forEach((inp) => {
      inp.addEventListener('input', () => {
        applyCustomerFilterLocal();
        setCustomersLoading(true);
        debouncedRemote();
      });
    });
    document.getElementById('nav-edit').addEventListener('click', async () => {
      const selected = selectedCustomerSnapshot();
      if (!selected) return;
      const selectedId = customerIdOf(selected);
      const selectionSeq = state.selectionSeq;
      if (selected) {
        try {
          const full = await fetchCustomerById(selectedId);
          if (!isSelectedCustomerStillCurrent(selectedId, selectionSeq)) return;
          if (full) setSelectedCustomer(full);
        } catch (err) {
          if (!isSelectedCustomerStillCurrent(selectedId, selectionSeq)) return;
          setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
        }
        if (!Array.isArray(state.selected.contacts)) {
          setStatus('Müşteri bilgileri tam yüklenemedi. Lütfen listeyi yenileyip tekrar deneyin.', true);
          return;
        }
        fillCustomerForm(state.selected);
        activateTab('add');
      }
    });
    document.getElementById('nav-sale').addEventListener('click', () => {
      const selected = selectedCustomerSnapshot();
      if (!selected) return;
      setSelectedCustomer(selected);
      fillSaleCustomer(selected);
      activateTab('sale');
    });
    document.getElementById('nav-detail').addEventListener('click', () => {
      const selected = selectedCustomerSnapshot();
      if (!selected) return;
      setSelectedCustomer(selected);
      updateDetail(selected);
      activateTab('detail');
      loadSales(selected.id);
    });
    document.getElementById('nav-delete').addEventListener('click', deleteCustomer);
    updateReadOnlyUi();
  }

  function renderTable(rows) {
    const tbody = document.querySelector('#pusula-search-table tbody');
    if (!tbody) return;
    tbody.innerHTML = '';
    let hasSelection = false;
    const list = Array.isArray(rows) ? rows : [];
    list.forEach((r) => {
      const tr = document.createElement('tr');
      const reg = r.registration_date ? fromISO(r.registration_date) : '';
      tr.innerHTML = `<td>${r.id}</td><td>${reg}</td><td>${r.name || ''}</td><td>${r.phone || ''}</td><td>${r.address || ''}</td><td>${formatMoney(r.debt_total || 0)}</td>`;
      if (Number(r.late_unpaid) === 1) tr.classList.add('late');
      const isSelected = state.selected && String(state.selected.id) === String(r.id);
      tr.addEventListener('click', () => {
        setSelectedCustomer(r);
        fillSaleCustomer(r);
        tbody.querySelectorAll('tr').forEach((row) => row.classList.remove('selected'));
        tr.classList.add('selected');
        enableNav(true);
      });
      tr.addEventListener('dblclick', () => {
        setSelectedCustomer(r);
        fillSaleCustomer(r);
        updateDetail(r);
        activateTab('detail');
        loadSales(r.id);
        tbody.querySelectorAll('tr').forEach((row) => row.classList.remove('selected'));
        tr.classList.add('selected');
        enableNav(true);
      });
      if (isSelected) {
        tr.classList.add('selected');
        hasSelection = true;
      }
      tbody.appendChild(tr);
    });
    if (!list.length) {
      const fields = getCustomerSearchFields();
      const hasFilters = Boolean(fields.id || fields.name || fields.phone || fields.address);
      const msg = hasFilters ? 'Arama kriterlerine uygun müşteri bulunamadı.' : 'Henüz müşteri kaydı yok.';
      const tr = document.createElement('tr');
      tr.className = 'pusula-empty-row';
      tr.innerHTML = `<td colspan="6" class="pusula-empty-cell">${msg}</td>`;
      tbody.appendChild(tr);
    }

    if (!list.length || !hasSelection) {
      enableNav(false);
    }
    updateReadOnlyUi();
  }

  async function loadCustomers({ silent = false, showLoading = true } = {}) {
    const fields = getCustomerSearchFields();
    const seq = ++state.customerLoadSeq;
    state.isLoadingCustomers = true;
    if (showLoading) setCustomersLoading(true);
    try {
      const query = new URLSearchParams({ limit: 5000, with: 'late_unpaid,contacts' });
      ['id','name','phone','address'].forEach((f) => {
        if (fields[f]) query.append(f, fields[f]);
      });
      const rows = await api(`/customers?${query.toString()}`);
      if (seq !== state.customerLoadSeq) return;
      state.customers = rows || [];
      if (!fields.id && !fields.name && !fields.phone && !fields.address) {
        state.customerBase = state.customers;
      }
      renderTable(state.customers);
      if (!silent) setStatus(`${state.customers.length} kayıt yüklendi.`);
      if (getActiveTabName() === 'add' && !state.selected && !isAddFormDirty()) {
        refreshNewCustomerIdFromServer(nextCustomerId());
      }
    } catch (err) {
      if (seq !== state.customerLoadSeq) return;
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
      enableNav(false);
    } finally {
      if (seq === state.customerLoadSeq) {
        state.isLoadingCustomers = false;
        if (showLoading) setCustomersLoading(false);
      }
    }
    setTimeout(ensureLayoutFits, 0);
  }

  async function fetchCustomerById(customerId) {
    const id = String(customerId || '').trim();
    if (!/^\d+$/.test(id)) return null;
    const local = state.customers.find((c) => String(c.id) === id);
    if (local && Array.isArray(local.contacts)) return local;
    const full = await api(`/customers/${encodeURIComponent(id)}`);
    if (full && full.id) return full;
    const rows = await api(`/customers?id=${encodeURIComponent(id)}&limit=1&with=contacts`);
    if (rows && rows[0]) return rows[0];
    return null;
  }

  async function fetchSalesForCustomer(customerId) {
    const id = Number(customerId);
    if (!Number.isFinite(id) || id < 1) return [];
    const rows = await api(`/sales?customer_id=${encodeURIComponent(id)}&with=installments`);
    return rows || [];
  }

  function enableNav(enabled) {
    ['nav-edit', 'nav-sale', 'nav-detail', 'nav-delete'].forEach((id) => {
      const btn = document.getElementById(id);
      if (btn) btn.disabled = !enabled;
    });
    updateReadOnlyUi();
  }

  async function deleteCustomer() {
    if (!ensureWriteAllowed()) return;
    if (!state.selected) return;
    if (!window.confirm('Bu müşteriyi silmek istediğinize emin misiniz?')) return;
    if (!window.confirm('Dikkat: Bu işlem geri alınamaz. Müşteriye ait tüm satış ve taksit kayıtları silinecek. Devam etmek istiyor musunuz?')) return;
    try {
      await api(`/customers/${state.selected.id}`, { method: 'DELETE' });
      setStatus('Müşteri silindi.');
      setSelectedCustomer(null);
      updateDetail(null);
      fillCustomerForm();
      enableNav(false);
      await loadCustomers();
      scheduleOfflineSnapshotSync();
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    }
  }

  // ------------------- Add/Edit Customer -------------------
  function renderAddTab() {
    const root = document.getElementById('pusula-tab-add');
    if (!root) return;
    root.innerHTML = `
      <div class="add-top-bar">
        <div class="form-row">
          <label>Müşteri No</label>
          <input type="text" id="cust-id">
        </div>
        <div class="form-row readonly">
          <label>Kayıt Tarihi</label>
          <div class="form-static" id="cust-date-label">${todayStr()}</div>
          <input type="hidden" id="cust-date-hidden" value="${todayStr()}">
        </div>
      </div>
      <div class="add-columns">
        <div class="add-left">
          <div class="form-row">
            <label>Adı Soyadı *</label>
            <input type="text" id="cust-name">
          </div>
          <div class="form-row">
            <label>Telefon</label>
            <input type="text" id="cust-phone">
          </div>
          <div class="form-row">
            <label>Ev Adresi</label>
            <input type="text" id="cust-address">
          </div>
          <div class="form-row">
            <label>İş Adresi</label>
            <input type="text" id="cust-work">
          </div>
          <div class="form-row textarea-row">
            <label>Notlar</label>
            <textarea id="cust-notes" rows="6"></textarea>
          </div>
        </div>
        <div class="add-right">
          <h4>Ek Kişi 1</h4>
          <div class="form-row">
            <label>Adı Soyadı</label>
            <input type="text" id="c1-name">
          </div>
          <div class="form-row">
            <label>Telefon</label>
            <input type="text" id="c1-phone">
          </div>
          <div class="form-row">
            <label>Ev Adresi</label>
            <input type="text" id="c1-home">
          </div>
          <div class="form-row">
            <label>İş Adresi</label>
            <input type="text" id="c1-work">
          </div>
          <h4>Ek Kişi 2</h4>
          <div class="form-row">
            <label>Adı Soyadı</label>
            <input type="text" id="c2-name">
          </div>
          <div class="form-row">
            <label>Telefon</label>
            <input type="text" id="c2-phone">
          </div>
          <div class="form-row">
            <label>Ev Adresi</label>
            <input type="text" id="c2-home">
          </div>
          <div class="form-row">
            <label>İş Adresi</label>
            <input type="text" id="c2-work">
          </div>
        </div>
      </div>
      <div class="pusula-actions add-actions">
        <button id="cust-save">KAYDET</button>
        <button id="cust-clear">TEMİZLE</button>
      </div>
    `;
    root.querySelector('#cust-save').addEventListener('click', saveCustomer);
    root.querySelector('#cust-clear').addEventListener('click', () => {
      setSelectedCustomer(null);
      fillCustomerForm();
    });
    const custIdInput = root.querySelector('#cust-id');
    if (custIdInput) {
      const syncPending = () => {
        if (state.selected) return;
        setPendingCustomerId(custIdInput.value.trim());
      };
      custIdInput.addEventListener('input', syncPending);
      custIdInput.addEventListener('blur', syncPending);
    }
    fillCustomerForm();
    updateReadOnlyUi();
  }

  function collectContacts() {
    const contacts = [];
    const c1 = {
      name: document.getElementById('c1-name').value.trim(),
      phone: document.getElementById('c1-phone').value.trim(),
      home_address: document.getElementById('c1-home').value.trim(),
      work_address: document.getElementById('c1-work').value.trim(),
    };
    const c2 = {
      name: document.getElementById('c2-name').value.trim(),
      phone: document.getElementById('c2-phone').value.trim(),
      home_address: document.getElementById('c2-home').value.trim(),
      work_address: document.getElementById('c2-work').value.trim(),
    };
    [c1, c2].forEach((c) => {
      if (c.name || c.phone || c.home_address || c.work_address) contacts.push(c);
    });
    return contacts;
  }

  function fillCustomerForm(cust = null) {
    const set = (id, val) => {
      const el = document.getElementById(id);
      if (!el) return;
      if ('value' in el) el.value = val || '';
      else el.textContent = val || '';
    };
    if (!cust) {
      setSelectedCustomer(null);
      state.addFormCustomerId = null;
      ['cust-id','cust-name','cust-phone','cust-address','cust-work','cust-notes','c1-name','c1-phone','c1-home','c1-work','c2-name','c2-phone','c2-home','c2-work'].forEach((id) => set(id, ''));
      const localNext = nextCustomerId();
      set('cust-id', String(localNext));
      setPendingCustomerId(localNext);
      set('cust-date-label', todayStr());
      set('cust-date-hidden', todayStr());
      refreshNewCustomerIdFromServer(localNext);
      return;
    }
    setSelectedCustomer(cust);
    state.addFormCustomerId = customerIdOf(cust);
    set('cust-id', cust.id || '');
    set('cust-date-label', cust.registration_date ? fromISO(cust.registration_date) : todayStr());
    set('cust-date-hidden', cust.registration_date ? fromISO(cust.registration_date) : todayStr());
    set('cust-name', cust.name || '');
    set('cust-phone', cust.phone || '');
    set('cust-address', cust.address || '');
    set('cust-work', cust.work_address || '');
    set('cust-notes', cust.notes || '');
    const contacts = cust.contacts || [];
    const c1 = contacts[0] || {};
    const c2 = contacts[1] || {};
    set('c1-name', c1.name || '');
    set('c1-phone', c1.phone || '');
    set('c1-home', c1.home_address || '');
    set('c1-work', c1.work_address || '');
    set('c2-name', c2.name || '');
    set('c2-phone', c2.phone || '');
    set('c2-home', c2.home_address || '');
    set('c2-work', c2.work_address || '');
  }

  function nextCustomerId() {
    const base = state.customerBase && state.customerBase.length ? state.customerBase : state.customers;
    if (!base || !base.length) return 1;
    const ids = new Set(
      base
        .map((c) => parseInt(c.id, 10))
        .filter((n) => Number.isFinite(n) && n > 0)
    );
    let nextId = 1;
    while (ids.has(nextId)) nextId += 1;
    return nextId;
  }

  function setPendingCustomerId(nextId) {
    if (state.selected) return;
    const val = String(nextId || '').trim();
    state.pendingCustomerId = val || null;
    syncSaleCustomerFromPending();
    syncDetailFromPending();
  }

  function syncSaleCustomerFromPending() {
    if (state.selected) return;
    const pending = state.pendingCustomerId;
    if (!pending) return;
    const idEl = document.getElementById('sale-cust-id');
    if (idEl) idEl.value = pending;
    const nameEl = document.getElementById('sale-cust-name');
    if (nameEl) nameEl.value = '';
  }

  function syncDetailFromPending() {
    if (state.selected) return;
    const pending = state.pendingCustomerId;
    if (!pending) return;
    updateDetail({ id: pending });
    if (getActiveTabName() === 'detail') {
      renderSalesTable([], { selectedSaleId: null });
    }
  }

  async function refreshNewCustomerIdFromServer(expectedId) {
    if (state.isOfflineMode) return;
    const idEl = document.getElementById('cust-id');
    if (!idEl) return;
    const before = idEl.value.trim();
    if (before && expectedId && before !== String(expectedId)) return;
    try {
      const res = await api('/customers/next-id');
      const nextId = Number(res && res.next_id);
      if (!Number.isFinite(nextId) || nextId < 1) return;
      const nowEl = document.getElementById('cust-id');
      if (!nowEl) return;
      if (nowEl.value.trim() !== before) return;
      nowEl.value = String(nextId);
      setPendingCustomerId(nextId);
    } catch (e) {
      // ignore (fallback already set)
    }
  }

  async function saveCustomer() {
    if (!ensureWriteAllowed()) return;
    if (state.isSavingCustomer) return;
    const addRoot = document.getElementById('pusula-tab-add');
    const saveBtn = document.getElementById('cust-save');
    const clearBtn = document.getElementById('cust-clear');
    const tabButtons = Array.from(document.querySelectorAll('.pusula-tabs button'));
    const prevSaveHtml = saveBtn ? saveBtn.innerHTML : '';

    const name = document.getElementById('cust-name').value.trim();
    if (!name) {
      setStatus('Adı Soyadı zorunlu.', true);
      return;
    }
    const payload = {
      id: document.getElementById('cust-id').value.trim() || undefined,
      name,
      phone: document.getElementById('cust-phone').value.trim(),
      address: document.getElementById('cust-address').value.trim(),
      work_address: document.getElementById('cust-work').value.trim(),
      notes: document.getElementById('cust-notes').value.trim(),
      registration_date: toISO(document.getElementById('cust-date-hidden').value.trim() || todayStr()),
      contacts: collectContacts(),
    };
    const targetId = payload.id;
    const exists = targetId && state.customers.some((c) => String(c.id) === String(targetId));
    const method = exists ? 'PUT' : 'POST';
    const url = exists ? `/customers/${targetId}` : '/customers';
    try {
      state.isSavingCustomer = true;
      setStatus('Kaydediliyor...');
      if (addRoot) {
        addRoot.querySelectorAll('input, textarea, button').forEach((el) => {
          if (el.type === 'hidden') return;
          el.disabled = true;
        });
      }
      tabButtons.forEach((b) => { b.disabled = true; });
      if (saveBtn) saveBtn.innerHTML = `<span class="pusula-spinner" aria-hidden="true"></span>KAYDEDİLİYOR...`;
      if (clearBtn) clearBtn.disabled = true;

      const res = await api(url, {
        method,
        body: JSON.stringify(payload),
      });
      if (res && res.id) payload.id = res.id;
      setStatus('Müşteri kaydedildi.');
      const newCust = {
        id: payload.id,
        name: payload.name,
        phone: payload.phone,
        address: payload.address,
        work_address: payload.work_address,
        notes: payload.notes,
        registration_date: payload.registration_date,
        contacts: payload.contacts || [],
        debt_total: 0,
      };
      const existingIdx = state.customers.findIndex((c) => String(c.id) === String(newCust.id));
      if (existingIdx >= 0) {
        state.customers[existingIdx] = newCust;
      } else {
        state.customers.unshift(newCust);
      }
      if (state.customerBase && state.customerBase.length) {
        const baseIdx = state.customerBase.findIndex((c) => String(c.id) === String(newCust.id));
        if (baseIdx >= 0) state.customerBase[baseIdx] = newCust;
        else state.customerBase.unshift(newCust);
      }
      setSelectedCustomer(newCust);
      renderTable(state.customers);
      enableNav(true);
      updateDetail(newCust);
      fillCustomerForm(newCust);
      resetSaleForm();
      fillSaleCustomer(newCust);
      activateTab('sale');
      scheduleOfflineSnapshotSync();
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    } finally {
      state.isSavingCustomer = false;
      if (saveBtn) saveBtn.innerHTML = prevSaveHtml || 'KAYDET';
      if (addRoot) {
        addRoot.querySelectorAll('input, textarea, button').forEach((el) => {
          if (el.type === 'hidden') return;
          el.disabled = false;
        });
      }
      tabButtons.forEach((b) => { b.disabled = false; });
      setTimeout(ensureLayoutFits, 0);
    }
  }

  // ------------------- Sale -------------------
  function resetSaleForm() {
    const setVal = (id, val) => {
      const el = document.getElementById(id);
      if (el) el.value = val;
    };
    setVal('sale-date', todayStr());
    setVal('sale-total', '');
    setVal('sale-down', '0');
    setVal('sale-n', '1');
    setVal('sale-due', String(defaultDueDay()));
    setVal('sale-first-month', String(defaultFirstDueMonthOffset()));
    setVal('sale-desc', '');
    updateSalePreview();
  }

  function renderSaleTab() {
    const root = document.getElementById('pusula-tab-sale');
    if (!root) return;
    root.innerHTML = `
      <div class="sale-container">
        <div class="sale-left">
          <div class="form-row">
            <label>Müşteri No *</label>
            <input type="text" id="sale-cust-id">
          </div>
          <div class="form-row">
            <label>Adı Soyadı</label>
            <input type="text" id="sale-cust-name" disabled>
          </div>
          <div class="form-row">
            <label>Tarih (GG-AA-YYYY)</label>
            <input type="text" id="sale-date" value="${todayStr()}">
          </div>
          <div class="form-row">
            <label>Toplam Tutar *</label>
            <input type="number" step="0.01" id="sale-total">
          </div>
          <div class="form-row">
            <label>Peşinat</label>
            <input type="number" step="0.01" id="sale-down" value="0">
          </div>
          <div class="form-row">
            <label>Taksit Sayısı *</label>
            <input type="number" id="sale-n" value="1">
          </div>
          <div class="form-row">
            <label>Ödeme Günü (1-28)</label>
            <input type="number" id="sale-due" value="${defaultDueDay()}">
          </div>
          <div class="form-row">
            <label>İlk Taksit Ayı</label>
            <select id="sale-first-month">
              <option value="1" selected>Sonraki ay</option>
              <option value="0">Satış ayı</option>
            </select>
          </div>
          <div class="sale-preview">
            <strong>Önizleme</strong>
            <div><span>Her Taksit Tutarı:</span> <span id="sale-preview-amt">—</span></div>
            <div><span>Son Taksit Tarihi:</span> <span id="sale-preview-date">—</span></div>
          </div>
        </div>
        <div class="sale-right">
          <label>Açıklama</label>
          <textarea id="sale-desc" rows="8"></textarea>
        </div>
      </div>
      <div class="pusula-actions sale-actions">
        <button id="sale-save">KAYDET</button>
        <button id="sale-clear">TEMİZLE</button>
      </div>
    `;
    root.querySelector('#sale-save').addEventListener('click', saveSale);
    root.querySelector('#sale-clear').addEventListener('click', () => {
      ['sale-total','sale-down','sale-n','sale-due','sale-first-month','sale-desc'].forEach((id) => {
        const el = document.getElementById(id);
        if (el) {
          el.value = id === 'sale-down'
            ? '0'
            : id === 'sale-n'
              ? '1'
              : id === 'sale-due'
                ? String(defaultDueDay())
                : id === 'sale-first-month'
                  ? String(defaultFirstDueMonthOffset())
                  : '';
        }
      });
      document.getElementById('sale-date').value = todayStr();
      updateSalePreview();
    });
    ['sale-date','sale-total','sale-down','sale-n','sale-due','sale-first-month'].forEach((id) => {
      const el = document.getElementById(id);
      if (el) el.addEventListener('input', updateSalePreview);
      if (el) el.addEventListener('change', updateSalePreview);
    });
    const custIdEl = document.getElementById('sale-cust-id');
    if (custIdEl) {
      const debouncedLookup = debounce(async () => {
        const id = custIdEl.value.trim();
        const nameEl = document.getElementById('sale-cust-name');
        if (!id) {
          if (nameEl) nameEl.value = '';
          return;
        }
        const cust = await fetchCustomerById(id);
        if (custIdEl.value.trim() !== id) return;
        if (cust) {
          setSelectedCustomer(cust);
          fillSaleCustomer(cust);
        } else if (nameEl) {
          nameEl.value = '';
        }
      }, 200);
      custIdEl.addEventListener('input', debouncedLookup);
      custIdEl.addEventListener('blur', debouncedLookup);
    }
    updateSalePreview();
    syncSaleCustomerFromPending();
    updateReadOnlyUi();
    setTimeout(ensureLayoutFits, 0);
  }

  function fillSaleCustomer(cust) {
    if (!cust) return;
    const idEl = document.getElementById('sale-cust-id');
    const nameEl = document.getElementById('sale-cust-name');
    if (idEl) idEl.value = cust.id || '';
    if (nameEl) nameEl.value = cust.name || '';
  }

  async function saveSale() {
    if (!ensureWriteAllowed()) return;
    if (state.isSavingSale) return;

    const saleRoot = document.getElementById('pusula-tab-sale');
    const saveBtn = document.getElementById('sale-save');
    const clearBtn = document.getElementById('sale-clear');
    const tabButtons = Array.from(document.querySelectorAll('.pusula-tabs button'));
    const prevSaveHtml = saveBtn ? saveBtn.innerHTML : '';
    state.isSavingSale = true;
    try {
      if (saleRoot) {
        saleRoot.querySelectorAll('input, textarea, select, button').forEach((el) => {
          el.disabled = true;
        });
      }
      tabButtons.forEach((b) => { b.disabled = true; });
      if (saveBtn) saveBtn.innerHTML = `<span class="pusula-spinner" aria-hidden="true"></span>KAYDEDİLİYOR...`;
      if (clearBtn) clearBtn.disabled = true;

      const custId = document.getElementById('sale-cust-id').value.trim();
      if (!custId) {
        setStatus('Müşteri seçiniz.', true);
        return;
      }
      if (!state.selected || String(state.selected.id) !== String(custId)) {
        const cust = await fetchCustomerById(custId);
        if (!cust) {
          setStatus('Hata: Müşteri bulunamadı.', true);
          return;
        }
        setSelectedCustomer(cust);
        fillSaleCustomer(cust);
      }
      const total = parseFloat(document.getElementById('sale-total').value);
      const downRaw = parseFloat(document.getElementById('sale-down').value || '0');
      const down = Number.isFinite(downRaw) ? Math.max(0, downRaw) : 0;
      const nInstRaw = parseInt(document.getElementById('sale-n').value || '1', 10);
      const dueDay = parseInt(document.getElementById('sale-due').value || '1', 10);
      const firstDueMonthOffset = parseFirstDueMonthOffset(document.getElementById('sale-first-month')?.value || '1');
      const totalCents = Math.round(Math.max(0, total) * 100);
      const downCents = Math.round(Math.max(0, down) * 100);
      const isFullyPaid = totalCents > 0 && downCents >= totalCents;
      const nInst = isFullyPaid ? 0 : nInstRaw;
      const effectiveDown = isFullyPaid ? total : down;
      if (!total || total <= 0 || (!isFullyPaid && (nInst < 1 || dueDay < 1 || dueDay > 28))) {
        setStatus('Satış alanlarını kontrol edin.', true);
        return;
      }
      const saleDateISO = toISO(document.getElementById('sale-date').value || todayStr());
      const saleBase = parseISODate(saleDateISO);
      if (!saleBase) {
        setStatus('Satış tarihi geçersiz. (GG-AA-YYYY)', true);
        return;
      }
      const desc = document.getElementById('sale-desc').value.trim();
      const instList = [];
      const remainingCents = Math.max(0, totalCents - Math.round(Math.max(0, effectiveDown) * 100));
      if (nInst > 0 && remainingCents > 0) {
        const baseInstallmentCents = Math.floor(remainingCents / nInst);
        for (let i = 1; i <= nInst; i++) {
          const d = calculateInstallmentDueDate(saleBase, dueDay, i, firstDueMonthOffset);
          const amountCents = i === nInst
            ? remainingCents - (baseInstallmentCents * (nInst - 1))
            : baseInstallmentCents;
          instList.push({
            due_date: formatISODate(d),
            amount: amountCents / 100,
            paid: 0,
          });
        }
      }
      setStatus('Kaydediliyor...');
      const sale = await api('/sales', {
        method: 'POST',
        body: JSON.stringify({
          customer_id: parseInt(custId, 10),
          date: saleDateISO,
          total,
          description: desc,
          request_key: createClientRequestKey('sale'),
          down_payment: effectiveDown,
          installments: instList,
        }),
      });
      const saleId = sale.id;
      const installmentIds = Array.isArray(sale.installment_ids) ? sale.installment_ids : [];
      instList.forEach((inst, index) => {
        inst.sale_id = saleId;
        if (installmentIds[index]) inst.id = installmentIds[index];
      });
      setStatus(nInst > 0 ? 'Satış ve taksitler kaydedildi.' : 'Satış kaydedildi.');
      const newSale = {
        id: saleId,
        customer_id: Number(custId),
        customer_name: (state.selected && state.selected.name) || '',
        date: saleDateISO,
        total,
        description: desc,
        installments: instList,
      };
      const withoutDup = (state.sales || []).filter((s) => String(s.id) !== String(saleId));
      state.sales = [newSale, ...withoutDup].sort((a, b) => {
        const da = String(a.date || '');
        const db = String(b.date || '');
        if (da !== db) return db.localeCompare(da);
        return Number(b.id || 0) - Number(a.id || 0);
      });
      updateDetail(state.selected);
      renderSalesTable(state.sales, { selectedSaleId: saleId });
      activateTab('detail');
      if (state.selected) loadSales(state.selected.id, saleId);
      fillSaleCustomer(state.selected);
      updateSalePreview();
      const saleData = {
        id: saleId,
        date: saleDateISO,
        total,
        description: desc,
        down: effectiveDown,
        installments: instList,
      };
      if (window.confirm('Satış makbuzu yazdırılsın mı?')) {
        printReceiptDetailed(saleData, state.selected || { id: custId });
      }
      scheduleOfflineSnapshotSync();
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    } finally {
      state.isSavingSale = false;
      if (saveBtn) saveBtn.innerHTML = prevSaveHtml || 'KAYDET';
      if (saleRoot) {
        saleRoot.querySelectorAll('input, textarea, select, button').forEach((el) => {
          el.disabled = false;
        });
      }
      const saleCustomerName = document.getElementById('sale-cust-name');
      if (saleCustomerName) saleCustomerName.disabled = true;
      tabButtons.forEach((b) => { b.disabled = false; });
      updateReadOnlyUi();
      setTimeout(ensureLayoutFits, 0);
    }
  }

  function updateSalePreview() {
    const totalEl = document.getElementById('sale-total');
    const downEl = document.getElementById('sale-down');
    const nEl = document.getElementById('sale-n');
    const dueEl = document.getElementById('sale-due');
    const firstMonthEl = document.getElementById('sale-first-month');

    const total = parseFloat(totalEl?.value || '0');
    const down = parseFloat(downEl?.value || '0');
    const totalCents = Math.round(Math.max(0, total) * 100);
    const downCents = Math.round(Math.max(0, down) * 100);
    const isFullyPaid = totalCents > 0 && downCents >= totalCents;

    if (nEl) {
      if (isFullyPaid) {
        if (!nEl.disabled) nEl.dataset.prevValue = nEl.value;
        nEl.value = '0';
        nEl.disabled = true;
      } else if (nEl.disabled) {
        nEl.disabled = false;
        if (String(nEl.value) === '0') {
          const prev = nEl.dataset.prevValue;
          nEl.value = prev && prev !== '0' ? prev : '1';
        }
      }
    }

    if (dueEl) dueEl.disabled = isFullyPaid;
    if (firstMonthEl) firstMonthEl.disabled = isFullyPaid;

    const nInst = parseInt(nEl?.value || '1', 10);
    const dueDay = parseInt(dueEl?.value || '1', 10);
    const firstDueMonthOffset = parseFirstDueMonthOffset(firstMonthEl?.value || '1');
    const saleDate = toISO(document.getElementById('sale-date')?.value || todayStr());
    const base = parseISODate(saleDate);
    const remainingCents = isFullyPaid ? 0 : Math.max(0, totalCents - downCents);
    const remain = remainingCents / 100;
    const hasInstallments = !isFullyPaid && nInst > 0 && remainingCents > 0;
    const per = hasInstallments ? (remain / nInst) : 0;
    const amtEl = document.getElementById('sale-preview-amt');
    const dateEl = document.getElementById('sale-preview-date');
    if (amtEl) amtEl.textContent = hasInstallments && isFinite(per) ? formatMoney(per) : '—';
    if (dateEl) {
      if (!hasInstallments || !base || !isFinite(dueDay)) {
        dateEl.textContent = '—';
      } else {
        const d = calculateInstallmentDueDate(base, dueDay, nInst, firstDueMonthOffset);
        dateEl.textContent = fromISO(formatISODate(d));
      }
    }
  }

  // ------------------- Detail -------------------
  function renderDetailTab() {
    const root = document.getElementById('pusula-tab-detail');
    if (!root) return;
    root.innerHTML = `
      <div id="pusula-detail-summary" class="detail-summary">
        <div class="detail-item">
          <span class="detail-label">No:</span>
          <span class="detail-value" data-field="id">-</span>
        </div>
        <div class="detail-item">
          <span class="detail-label">İsim:</span>
          <span class="detail-value" data-field="name">-</span>
        </div>
        <div class="detail-item">
          <span class="detail-label">Telefon:</span>
          <span class="detail-value" data-field="phone">-</span>
        </div>
        <div class="detail-item detail-item-wide">
          <span class="detail-label">Adres:</span>
          <span class="detail-value" data-field="address">-</span>
        </div>
        <div class="detail-item">
          <span class="detail-label">Toplam Borç:</span>
          <span class="detail-value" data-field="debt_total">0,00₺</span>
        </div>
      </div>
      <div class="detail-stack">
        <div class="detail-section">
          <div class="detail-section-header">
            <div class="detail-section-title">Satışlar</div>
            <div class="pusula-actions detail-actions">
              <button id="btn-print-sale" disabled>MAKBUZ YAZDIR</button>
              <button id="btn-edit-sale" disabled>DÜZELT</button>
              <button id="btn-delete-sale" disabled>SİL</button>
            </div>
          </div>
          <div class="pusula-table-wrapper">
            <table class="pusula-table" id="pusula-sales-table">
              <thead><tr><th>No</th><th>Tarih</th><th>Tutar</th><th>Açıklama</th></tr></thead>
              <tbody></tbody>
            </table>
          </div>
        </div>
        <div class="detail-section">
          <div class="detail-section-header">
            <div class="detail-section-title">Taksitler</div>
            <div class="pusula-actions detail-actions">
              <input type="number" step="0.01" min="0" id="inst-pay-amount" class="detail-inline-input" placeholder="Tahsilat Tutarı">
              <button id="btn-add-payment" disabled>TAHSİLAT AL</button>
              <button id="btn-mark-paid" disabled>KALANI TAHSİL ET</button>
              <button id="btn-mark-unpaid" disabled>ÖDEMELERİ SIFIRLA</button>
              <button id="btn-print-inst" disabled>Makbuz Yazdır</button>
            </div>
          </div>
          <div class="pusula-table-wrapper">
            <table class="pusula-table" id="pusula-inst-table">
              <thead><tr><th>Vade</th><th>Taksit</th><th>Ödenen</th><th>Kalan</th><th>Durum</th></tr></thead>
              <tbody></tbody>
            </table>
          </div>
        </div>
        <div class="detail-section">
          <div class="detail-section-header">
            <div class="detail-section-title">Tahsilat Geçmişi</div>
          </div>
          <div class="pusula-table-wrapper">
            <table class="pusula-table" id="pusula-inst-payments-table">
              <thead><tr><th>Tarih</th><th>Tutar</th><th>Kalan</th></tr></thead>
              <tbody></tbody>
            </table>
          </div>
        </div>
      </div>
    `;
    const editBtn = document.getElementById('btn-edit-sale');
    if (editBtn) {
      editBtn.addEventListener('click', () => {
        if (!state.currentSale) return;
        openSaleDescriptionEditor(state.currentSale, {
          customer: state.selected,
          onUpdated: () => renderSalesTable(state.sales || [], { selectedSaleId: state.currentSale.id }),
        });
      });
    }
    const salePrintBtn = document.getElementById('btn-print-sale');
    if (salePrintBtn) salePrintBtn.addEventListener('click', printReceipt);
    const deleteBtn = document.getElementById('btn-delete-sale');
    if (deleteBtn) deleteBtn.addEventListener('click', () => deleteSale(state.currentSale));
    const addPaymentBtn = document.getElementById('btn-add-payment');
    const paidBtn = document.getElementById('btn-mark-paid');
    const unpaidBtn = document.getElementById('btn-mark-unpaid');
    if (addPaymentBtn) addPaymentBtn.addEventListener('click', () => takeInstallmentPayment(false));
    if (paidBtn) paidBtn.addEventListener('click', () => toggleInstallmentPaid(true));
    if (unpaidBtn) unpaidBtn.addEventListener('click', () => toggleInstallmentPaid(false));
    const instPrintBtn = document.getElementById('btn-print-inst');
    if (instPrintBtn) instPrintBtn.addEventListener('click', printSelectedInstallmentReceipt);
    updateReadOnlyUi();
    setTimeout(ensureLayoutFits, 0);
  }

  function updateDetail(cust) {
    const root = document.getElementById('pusula-detail-summary');
    if (!root) return;
    ['id','name','phone','address'].forEach((field) => {
      const el = root.querySelector(`[data-field="${field}"]`);
      if (!el) return;
      el.textContent = cust ? (cust[field] || '—') : '—';
    });
    const debtEl = root.querySelector('[data-field="debt_total"]');
    if (debtEl) debtEl.textContent = cust ? formatMoney(cust.debt_total || 0) : formatMoney(0);
  }

  function computeCustomerDebtFromSales(sales) {
    const rows = Array.isArray(sales) ? sales : [];
    let total = 0;
    rows.forEach((sale) => {
      (sale.installments || []).forEach((inst) => {
        total += installmentRemainingAmount(inst);
      });
    });
    return Math.round(total * 100) / 100;
  }

  function syncSelectedCustomerDebt() {
    if (!state.selected) return;
    const debtTotal = computeCustomerDebtFromSales(state.sales || []);
    state.selected.debt_total = debtTotal;
    const selectedId = String(state.selected.id || '');
    state.customers = (state.customers || []).map((c) => (
      String(c.id || '') === selectedId ? { ...c, debt_total: debtTotal } : c
    ));
    state.customerBase = (state.customerBase || []).map((c) => (
      String(c.id || '') === selectedId ? { ...c, debt_total: debtTotal } : c
    ));
    updateDetail(state.selected);
    if (getActiveTabName() === 'search') {
      renderTable(state.customers || []);
    }
  }

  async function loadSales(customerId, selectedSaleId = null) {
    const selectedId = String(customerId || '').trim();
    const seq = ++state.salesLoadSeq;
    try {
      const sales = await api(`/sales?customer_id=${customerId}&with=installments`);
      if (seq !== state.salesLoadSeq || (state.selected && customerIdOf(state.selected) !== selectedId)) return;
      state.sales = sales;
      renderSalesTable(state.sales, { selectedSaleId });
      syncSelectedCustomerDebt();
    } catch (err) {
      if (seq !== state.salesLoadSeq || (state.selected && customerIdOf(state.selected) !== selectedId)) return;
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
      state.sales = [];
      renderSalesTable([], { selectedSaleId });
      syncSelectedCustomerDebt();
    }
  }

  function renderSalesTable(rows, { selectedSaleId = null } = {}) {
    const tbody = document.querySelector('#pusula-sales-table tbody');
    if (!tbody) return;
    tbody.innerHTML = '';
    const prevSaleId = state.currentSale ? String(state.currentSale.id) : null;
    const prevExplicit = state.currentSaleExplicit;
    state.currentSale = null;
    state.currentSaleExplicit = false;
    const editBtn = document.getElementById('btn-edit-sale');
    const deleteBtn = document.getElementById('btn-delete-sale');
    const printBtn = document.getElementById('btn-print-sale');
    if (editBtn) editBtn.disabled = true;
    if (deleteBtn) deleteBtn.disabled = true;
    if (printBtn) printBtn.disabled = true;
    const list = Array.isArray(rows) ? rows : [];
    const hasSingleSale = list.length === 1;
    let preferredSale = null;
    if (selectedSaleId) {
      preferredSale = list.find((s) => String(s.id) === String(selectedSaleId));
    }
    if (!preferredSale) {
      preferredSale = list.find((s) => hasUnpaidInstallments(s));
    }
    if (!preferredSale && list.length) {
      preferredSale = list[0];
    }
    const preferredId = preferredSale ? String(preferredSale.id) : null;
    let preferredRow = null;
    list.forEach((s) => {
      const tr = document.createElement('tr');
      tr.innerHTML = `<td>${s.id}</td><td>${fromISO(s.date)}</td><td>${formatMoney(s.total || 0)}</td><td>${truncateDescription(s.description || '')}</td>`;
      tr.addEventListener('click', () => {
        state.currentSale = s;
        state.currentSaleExplicit = true;
        renderInstallments(s.installments || []);
        const editBtn = document.getElementById('btn-edit-sale');
        const deleteBtn = document.getElementById('btn-delete-sale');
        const printBtn = document.getElementById('btn-print-sale');
        if (editBtn) editBtn.disabled = false;
        if (deleteBtn) deleteBtn.disabled = false;
        if (printBtn) printBtn.disabled = false;
        tbody.querySelectorAll('tr').forEach((row) => row.classList.remove('selected'));
        tr.classList.add('selected');
      });
      tr.addEventListener('dblclick', (e) => {
        e.preventDefault();
        const descCell = tr.querySelector('td:last-child');
        openSaleDescriptionEditor(s, {
          customer: state.selected,
          onUpdated: (next) => {
            if (descCell) descCell.textContent = truncateDescription(next || '');
            if (state.currentSale && String(state.currentSale.id) === String(s.id)) {
              state.currentSale.description = next;
            }
          },
        });
      });
      if (preferredId && String(s.id) === preferredId) {
        preferredRow = tr;
        preferredSale = s;
        tr.classList.add('selected');
      }
      tbody.appendChild(tr);
    });
    if (preferredSale) {
      if (prevExplicit && prevSaleId && preferredId && prevSaleId === preferredId) {
        state.currentSaleExplicit = true;
      }
      state.currentSale = preferredSale;
      renderInstallments(preferredSale.installments || []);
      const allowSaleActions = hasSingleSale || state.currentSaleExplicit;
      if (editBtn) editBtn.disabled = !allowSaleActions;
      if (deleteBtn) deleteBtn.disabled = !allowSaleActions;
      if (printBtn) printBtn.disabled = !allowSaleActions;
      if (preferredRow && selectedSaleId) {
        try {
          preferredRow.scrollIntoView({ block: 'nearest' });
        } catch (e) { /* ignore */ }
      }
    } else if (!rows.length) {
      renderInstallments([]);
    }
    updateReadOnlyUi();
  }

  function renderInstallments(insts) {
    const tbody = document.querySelector('#pusula-inst-table tbody');
    if (!tbody) return;
    tbody.innerHTML = '';
    state.currentInstallment = null;
    state.currentInstallmentPayment = null;
    renderInstallmentPayments([]);
    const addBtn = document.getElementById('btn-add-payment');
    const btnPaid = document.getElementById('btn-mark-paid');
    const btnUnpaid = document.getElementById('btn-mark-unpaid');
    const instPrintBtn = document.getElementById('btn-print-inst');
    const payInput = document.getElementById('inst-pay-amount');
    if (addBtn) addBtn.disabled = true;
    if (btnPaid) btnPaid.disabled = true;
    if (btnUnpaid) btnUnpaid.disabled = true;
    if (payInput) payInput.value = '';
    if (instPrintBtn) instPrintBtn.disabled = true;
    const todayISO = formatISODate(new Date());
    const list = Array.isArray(insts) ? insts : [];
    const preferredId = state.pendingInstallmentId ? String(state.pendingInstallmentId) : '';
    let preferredRow = null;
    let preferredInst = null;

    const selectInstallment = (inst, tr) => {
      tbody.querySelectorAll('tr').forEach((row) => row.classList.remove('selected'));
      tr.classList.add('selected');
      tr.dataset.instId = inst.id;
      tr.dataset.saleId = inst.sale_id || (state.currentSale && state.currentSale.id);
      state.currentInstallment = inst;
      const remaining = installmentRemainingAmount(inst);
      const paidAmount = installmentPaidAmount(inst);
      const canPay = remaining > 0;
      if (addBtn) addBtn.disabled = !canPay;
      if (btnPaid) btnPaid.disabled = !canPay;
      if (btnUnpaid) btnUnpaid.disabled = paidAmount <= 0;
      if (payInput) payInput.value = canPay ? String(remaining) : '';
      renderInstallmentPayments(inst.payments || [], inst);
    };

    list.forEach((i) => {
      const tr = document.createElement('tr');
      const amount = installmentAmount(i);
      const paidAmount = installmentPaidAmount(i);
      const remaining = installmentRemainingAmount(i);
      const isFullyPaid = remaining <= 0;
      const status = isFullyPaid ? 'Tamamlandı' : (paidAmount > 0 ? 'Kısmi Ödendi' : 'Ödenmedi');
      tr.innerHTML = `<td>${fromISO(i.due_date)}</td><td>${formatMoney(amount)}</td><td>${formatMoney(paidAmount)}</td><td>${formatMoney(remaining)}</td><td>${status}</td>`;
      if (isFullyPaid) tr.classList.add('paid');
      if (!isFullyPaid && paidAmount > 0) tr.classList.add('partial');
      if (!isFullyPaid && i.due_date && i.due_date < todayISO) tr.classList.add('late');
      tr.addEventListener('click', () => {
        selectInstallment(i, tr);
      });
      if (preferredId && String(i.id) === preferredId) {
        preferredRow = tr;
        preferredInst = i;
      } else if (!preferredInst && remaining > 0) {
        preferredRow = tr;
        preferredInst = i;
      } else if (!preferredInst) {
        preferredRow = tr;
        preferredInst = i;
      }
      tbody.appendChild(tr);
    });

    if (!list.length) {
      const tr = document.createElement('tr');
      tr.className = 'pusula-empty-row';
      tr.innerHTML = '<td colspan="5" class="pusula-empty-cell">Taksit bulunamadı.</td>';
      tbody.appendChild(tr);
      renderInstallmentPayments([]);
    } else if (preferredInst && preferredRow) {
      selectInstallment(preferredInst, preferredRow);
      if (state.pendingInstallmentId) {
        try {
          preferredRow.scrollIntoView({ block: 'nearest' });
        } catch (e) { /* ignore */ }
      }
    }

    state.pendingInstallmentId = null;
    state.pendingPaymentId = null;
    updateReadOnlyUi();
  }

  function renderInstallmentPayments(payments, installment = null) {
    const tbody = document.querySelector('#pusula-inst-payments-table tbody');
    const printBtn = document.getElementById('btn-print-inst');
    if (!tbody) return;
    tbody.innerHTML = '';
    state.currentInstallmentPayment = null;
    const list = Array.isArray(payments) ? payments : [];
    const pendingPaymentId = state.pendingPaymentId ? String(state.pendingPaymentId) : '';
    let preferredPayment = null;
    let preferredRow = null;

    const selectPayment = (payment, tr) => {
      tbody.querySelectorAll('tr').forEach((row) => row.classList.remove('selected'));
      tr.classList.add('selected');
      state.currentInstallmentPayment = payment;
      if (printBtn) printBtn.disabled = false;
    };

    list.forEach((payment) => {
      const tr = document.createElement('tr');
      tr.innerHTML = `
        <td>${fromISO(payment.payment_date || '')}</td>
        <td>${formatMoney(payment.amount || 0)}</td>
        <td>${formatMoney(payment.remaining_after_payment || 0)}</td>
      `;
      tr.addEventListener('click', () => selectPayment(payment, tr));
      if (pendingPaymentId && String(payment.id) === pendingPaymentId) {
        preferredPayment = payment;
        preferredRow = tr;
      }
      tbody.appendChild(tr);
    });

    if (!list.length) {
      const tr = document.createElement('tr');
      tr.className = 'pusula-empty-row';
      tr.innerHTML = '<td colspan="3" class="pusula-empty-cell">Tahsilat kaydı yok.</td>';
      tbody.appendChild(tr);
      if (printBtn) printBtn.disabled = true;
      updateReadOnlyUi();
      return;
    }

    if (!preferredPayment && list.length) {
      preferredPayment = list[list.length - 1];
      preferredRow = tbody.querySelector('tr:last-child');
    }
    if (preferredPayment && preferredRow) {
      selectPayment(preferredPayment, preferredRow);
      if (pendingPaymentId) {
        try {
          preferredRow.scrollIntoView({ block: 'nearest' });
        } catch (e) { /* ignore */ }
      }
    } else if (printBtn) {
      const selectedInstallment = installment || state.currentInstallment;
      const hasPayments = selectedInstallment && Array.isArray(selectedInstallment.payments) && selectedInstallment.payments.length > 0;
      printBtn.disabled = !hasPayments;
    }
    updateReadOnlyUi();
  }

  async function toggleInstallmentPaid(paid) {
    if (!ensureWriteAllowed()) return;
    const inst = state.currentInstallment;
    if (!inst || !inst.id) return;
    const instId = inst.id;
    if (paid) {
      await takeInstallmentPayment(true);
      return;
    }
    if (installmentPaidAmount(inst) <= 0) return;
    if (!window.confirm('Bu taksite ait tüm tahsilatlar silinecek. Devam edilsin mi?')) return;
    try {
      setInstallmentsLoading(true);
      await api(`/installments/${instId}`, {
        method: 'PUT',
        body: JSON.stringify({ paid: 0 }),
      });
      setStatus('Taksit güncellendi.');
      state.pendingInstallmentId = instId;
      state.pendingPaymentId = null;
      if (state.selected) await loadSales(state.selected.id, state.currentSale ? state.currentSale.id : null);
      scheduleOfflineSnapshotSync();
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    } finally {
      setInstallmentsLoading(false);
    }
  }

  async function takeInstallmentPayment(useRemainingAmount = false) {
    if (!ensureWriteAllowed()) return;
    const inst = state.currentInstallment;
    if (!inst || !inst.id) return;
    const remaining = installmentRemainingAmount(inst);
    if (!(remaining > 0)) return;

    const inputEl = document.getElementById('inst-pay-amount');
    const entered = Number(inputEl && inputEl.value !== '' ? inputEl.value : NaN);
    const amount = useRemainingAmount
      ? remaining
      : (Number.isFinite(entered) && entered > 0 ? entered : remaining);
    const normalizedAmount = Math.round(amount * 100) / 100;
    const maxAmount = Math.round(remaining * 100) / 100;
    if (!(normalizedAmount > 0) || normalizedAmount - maxAmount > 0.00001) {
      setStatus('Tahsilat tutarı kalan borçtan büyük olamaz.', true);
      return;
    }

    try {
      setInstallmentsLoading(true);
      const result = await api(`/installments/${inst.id}/payments`, {
        method: 'POST',
        body: JSON.stringify({ amount: normalizedAmount }),
      });
      const payment = result && result.payment ? result.payment : null;
      setStatus('Tahsilat kaydedildi.');
      state.pendingInstallmentId = inst.id;
      state.pendingPaymentId = payment && payment.id ? payment.id : null;

      if (state.selected && payment && window.confirm('Makbuz yazdırılsın mı?')) {
        const sale = findSaleByInstallmentId(inst.id);
        printInstallmentPaymentReceipt(state.selected, result.installment || inst, payment, sale);
      }

      if (state.selected) await loadSales(state.selected.id, state.currentSale ? state.currentSale.id : null);
      scheduleOfflineSnapshotSync();
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    } finally {
      setInstallmentsLoading(false);
    }
  }

  function printSelectedInstallmentReceipt() {
    if (!state.selected || !state.currentInstallment) return;
    const inst = state.currentInstallment;
    const selectedPayment = state.currentInstallmentPayment;
    const fallbackPayment = Array.isArray(inst.payments) && inst.payments.length
      ? inst.payments[inst.payments.length - 1]
      : null;
    const payment = selectedPayment || fallbackPayment;
    if (!payment) {
      setStatus('Hata: Yazdırılacak tahsilat bulunamadı.', true);
      return;
    }
    const sale = findSaleByInstallmentId(inst.id);
    printInstallmentPaymentReceipt(state.selected, inst, payment, sale);
  }

  function findSaleByInstallmentId(installmentId, salesList = null) {
    const targetId = String(installmentId || '');
    if (!targetId) return null;
    const sales = Array.isArray(salesList) ? salesList : (state.sales || []);
    for (let i = 0; i < sales.length; i += 1) {
      const sale = sales[i];
      const found = (sale.installments || []).find((inst) => String(inst.id || '') === targetId);
      if (found) return sale;
    }
    return null;
  }

  function printInstallmentPaymentReceipt(customer, installment, payment, sale = null) {
    if (!customer || !installment || !payment) return;
    const company = getBusinessProfile();
    const businessContactLine = buildBusinessContactLine(company);

    const todayISO = formatISODate(new Date());
    const paymentDate = payment.payment_date || todayISO;
    const dueDate = installment.due_date || '';
    const installmentAmountVal = installmentAmount(installment);
    const paymentAmountVal = Number(payment.amount || 0);
    const paidAfterVal = Number.isFinite(Number(payment.paid_after))
      ? Number(payment.paid_after)
      : (Number.isFinite(Number(payment.running_paid_amount)) ? Number(payment.running_paid_amount) : installmentPaidAmount(installment));
    const remainingAfterVal = Number.isFinite(Number(payment.remaining_after_payment))
      ? Number(payment.remaining_after_payment)
      : Math.max(0, installmentAmountVal - paidAfterVal);
    const now = new Date();
    const hh = String(now.getHours()).padStart(2, '0');
    const mm = String(now.getMinutes()).padStart(2, '0');
    const timeStr = `${hh}:${mm}`;

    const html = `
      <html><head><title> </title>
      <style>
        @page { size: A4; margin: 0; }
        @media print {
          .no-print { display:none; }
          html, body { margin:0; padding:0; }
        }
        body { font-family: Arial, sans-serif; color:#000; margin:0; -webkit-print-color-adjust: exact; print-color-adjust: exact; }
        .receipt { padding: 14mm; }
        .no-print { font-size:12px; color:#000; margin:10px 0 14px; }
        .brand { text-align:center; }
        .brand .name { font-weight:700; font-size:18px; letter-spacing:1px; margin:0; }
        .brand .line { font-size:11px; margin:3px 0 0; }
        .rule { border-top:2px solid #000; margin:10px 0 12px; }
        .title { text-align:center; font-weight:700; font-size:14px; margin:0 0 10px; }
        .meta-row { display:flex; justify-content:space-between; font-size:12px; margin:6px 0 10px; }
        .info { font-size:12px; margin:6px 0 12px; }
        .info .row { display:flex; gap:10px; margin:4px 0; }
        .info .label { width:120px; }
        table { width:100%; border-collapse: collapse; margin-top: 8px; }
        th, td { border:1px solid #000; padding:6px 8px; font-size:12px; text-align:left; }
        th { font-weight:700; }
        .totals { margin-top: 12px; font-size:12px; }
        .totals .row { display:flex; justify-content:space-between; margin:6px 0; }
        .totals .grand { font-weight:700; border-top:2px solid #000; padding-top:8px; margin-top:8px; }
        .footer-rule { border-top:2px solid #000; margin:18px 0 10px; }
        .footer { text-align:center; font-size:11px; }
        .footer .thanks { margin:0 0 10px; }
        .footer .name { font-weight:700; letter-spacing:1px; margin:0; }
        .footer .sub { margin:4px 0 0; color:#000; }
      </style></head><body>
      <div class="receipt">
        <div class="no-print">Not: Yazdırma ekranında “Üstbilgi ve altbilgiler” seçeneğini kapatın.</div>
        <div class="brand">
          ${company.name ? `<p class="name">${company.name}</p>` : ''}
          ${company.address ? `<p class="line">${company.address}</p>` : ''}
          ${businessContactLine ? `<p class="line">${businessContactLine}</p>` : ''}
        </div>
        <div class="rule"></div>
        <p class="title">Taksit Tahsilat Makbuzu</p>
        <div class="meta-row">
          <div><strong>Tarih:</strong> ${fromISOSlash(paymentDate)}</div>
          <div><strong>Saat:</strong> ${timeStr}</div>
        </div>
        <div class="info">
          <div class="row"><div class="label"><strong>Makbuz No:</strong></div><div>${payment.id || ''}</div></div>
          <div class="row"><div class="label"><strong>Hesap No:</strong></div><div>${customer.id || ''}</div></div>
          <div class="row"><div class="label"><strong>Müşteri:</strong></div><div>${customer.name || ''}</div></div>
          <div class="row"><div class="label"><strong>Adres:</strong></div><div>${customer.address || ''}</div></div>
        </div>
        <table>
          <thead>
            <tr>
              <th>Satış No</th>
              <th>Vade</th>
              <th>Taksit Tutarı</th>
              <th>Ödeme Tutarı</th>
              <th>Kalan</th>
            </tr>
          </thead>
          <tbody>
            <tr>
              <td>${sale && sale.id ? sale.id : (installment.sale_id || '')}</td>
              <td>${fromISOSlash(dueDate)}</td>
              <td>${formatMoney(installmentAmountVal)}</td>
              <td>${formatMoney(paymentAmountVal)}</td>
              <td>${formatMoney(remainingAfterVal)}</td>
            </tr>
          </tbody>
        </table>
        <div class="totals">
          <div class="row"><div><strong>Toplam Ödenen:</strong></div><div>${formatMoney(paidAfterVal)}</div></div>
          <div class="row grand"><div><strong>Kalan Borç:</strong></div><div>${formatMoney(remainingAfterVal)}</div></div>
        </div>
        <div class="footer-rule"></div>
        <div class="footer">
          <p class="thanks">Mağazamızdan yapmış olduğunuz ödeme için teşekkür ederiz</p>
          ${company.name ? `<p class="name">${company.name}</p>` : ''}
          ${company.footerSub ? `<p class="sub">${company.footerSub}</p>` : ''}
        </div>
      </div>
      </body></html>`;

    const w = window.open('', '_blank');
    if (w) {
      try { w.document.title = ''; } catch (e) { /* ignore */ }
      w.document.write(html);
      w.document.close();
      w.focus();
      w.print();
    }
  }

  function trMonthName(month1to12) {
    const months = [
      'Ocak',
      'Şubat',
      'Mart',
      'Nisan',
      'Mayıs',
      'Haziran',
      'Temmuz',
      'Ağustos',
      'Eylül',
      'Ekim',
      'Kasım',
      'Aralık',
    ];
    const idx = Number(month1to12) - 1;
    return months[idx] || '';
  }

  function collectPaidInstallmentsForMonth(year, month1to12, salesList = null) {
    const items = [];
    const sales = Array.isArray(salesList) ? salesList : (state.sales || []);
    sales.forEach((sale) => {
      (sale.installments || []).forEach((inst) => {
        if (!isPaid(inst.paid)) return;
        const dt = parseISODate(inst.due_date);
        if (!dt) return;
        if (dt.getFullYear() !== Number(year) || dt.getMonth() + 1 !== Number(month1to12)) return;
        items.push({
          due_date: inst.due_date,
          amount: Number(inst.amount || 0),
          sale_id: sale.id,
        });
      });
    });
    const normalizeDueDate = (value) => {
      const raw = String(value || '').trim();
      if (!raw) return null;
      const compact = raw.length >= 10 ? raw.slice(0, 10) : raw;
      const iso = toISO(compact) || compact;
      const dt = parseISODate(iso);
      return dt ? dt.getTime() : null;
    };
    items.sort((a, b) => {
      const da = normalizeDueDate(a.due_date);
      const db = normalizeDueDate(b.due_date);
      if (da !== null && db !== null && da !== db) return da - db;
      if (da !== null && db === null) return -1;
      if (db !== null && da === null) return 1;
      return Number(a.sale_id || 0) - Number(b.sale_id || 0);
    });
    return items;
  }

  function printPaymentReceiptForMonth(customer, year, month1to12, salesList = null, fallbackInst = null) {
    if (!customer) return;
    const company = getBusinessProfile();
    const businessContactLine = buildBusinessContactLine(company);

    const items = collectPaidInstallmentsForMonth(year, month1to12, salesList);
    if (!items.length && fallbackInst && isPaid(fallbackInst.paid)) {
      items.push({
        due_date: fallbackInst.due_date,
        amount: Number(fallbackInst.amount || 0),
        sale_id: fallbackInst.sale_id || (state.currentSale && state.currentSale.id),
      });
    }
    if (!items.length && state.currentInstallment && isPaid(state.currentInstallment.paid)) {
      items.push({
        due_date: state.currentInstallment.due_date,
        amount: Number(state.currentInstallment.amount || 0),
        sale_id: state.currentInstallment.sale_id || (state.currentSale && state.currentSale.id),
      });
    }
    if (!items.length) {
      setStatus('Bu ay için ödenmiş taksit bulunamadı.', true);
      return;
    }

    const todayISO = formatISODate(new Date());
    const totalPaid = items.reduce((sum, i) => sum + Number(i.amount || 0), 0);
    const now = new Date();
    const hh = String(now.getHours()).padStart(2, '0');
    const mm = String(now.getMinutes()).padStart(2, '0');
    const timeStr = `${hh}:${mm}`;

    const monthName = trMonthName(month1to12);
    const customerId = customer.id || '';
    const customerName = customer.name || '';
    const customerAddress = customer.address || '';
    const anyLate = items.some((i) => i.due_date && i.due_date < todayISO);

    let html = `
      <html><head><title> </title>
      <style>
        @page { size: A4; margin: 0; }
        @media print {
          .no-print { display:none; }
          html, body { margin:0; padding:0; }
        }
        body { font-family: Arial, sans-serif; color:#000; margin:0; -webkit-print-color-adjust: exact; print-color-adjust: exact; }
        .receipt { padding: 14mm; }
        .no-print { font-size:12px; color:#000; margin:10px 0 14px; }
        .brand { text-align:center; }
        .brand .name { font-weight:700; font-size:18px; letter-spacing:1px; margin:0; }
        .brand .line { font-size:11px; margin:3px 0 0; }
        .rule { border-top:2px solid #000; margin:10px 0 12px; }
        .title { text-align:center; font-weight:700; font-size:14px; margin:0 0 10px; }
        .meta-row { display:flex; justify-content:space-between; font-size:12px; margin:6px 0 10px; }
        .info { font-size:12px; margin:6px 0 12px; }
        .info .row { display:flex; gap:10px; margin:4px 0; }
        .info .label { width:110px; }
        table { width:100%; border-collapse: collapse; margin-top: 8px; }
        th, td { border:1px solid #000; padding:6px 8px; font-size:12px; text-align:left; }
        th { font-weight:700; }
        .totals { margin-top: 12px; font-size:12px; }
        .totals .row { display:flex; justify-content:space-between; margin:6px 0; }
        .totals .grand { font-weight:700; border-top:2px solid #000; padding-top:8px; margin-top:8px; }
        .note { margin-top: 8px; font-size: 12px; font-weight: 700; }
        .footer-rule { border-top:2px solid #000; margin:18px 0 10px; }
        .footer { text-align:center; font-size:11px; }
        .footer .thanks { margin:0 0 10px; }
        .footer .name { font-weight:700; letter-spacing:1px; margin:0; }
        .footer .sub { margin:4px 0 0; color:#000; }
      </style></head><body>
      <div class="receipt">
        <div class="no-print">Not: Yazdırma ekranında “Üstbilgi ve altbilgiler” seçeneğini kapatın.</div>
        <div class="brand">
          ${company.name ? `<p class="name">${company.name}</p>` : ''}
          ${company.address ? `<p class="line">${company.address}</p>` : ''}
          ${businessContactLine ? `<p class="line">${businessContactLine}</p>` : ''}
        </div>
        <div class="rule"></div>
        <p class="title">${monthName} ${year} Taksit Ödemesi</p>
        <div class="meta-row">
          <div><strong>Tarih:</strong> ${fromISOSlash(todayISO)}</div>
          <div><strong>Saat:</strong> ${timeStr}</div>
        </div>
        <div class="info">
          <div class="row"><div class="label"><strong>Hesap No:</strong></div><div>${customerId}</div></div>
          <div class="row"><div class="label"><strong>Müşteri:</strong></div><div>${customerName}</div></div>
          <div class="row"><div class="label"><strong>Adres:</strong></div><div>${customerAddress}</div></div>
        </div>
        <table>
          <thead>
            <tr>
              <th>Taksit Tarihi</th>
              <th>Tutar</th>
              <th>Satış No</th>
              <th>Durum</th>
            </tr>
          </thead>
          <tbody>`;

    items.forEach((i) => {
      const due = i.due_date ? String(i.due_date) : '';
      const status = due && due < todayISO ? 'Geç Ödeme' : 'Vadesinde';
      html += `
            <tr>
              <td>${fromISOSlash(due)}</td>
              <td>${formatMoney(i.amount)}</td>
              <td>${i.sale_id || ''}</td>
              <td>${status}</td>
            </tr>`;
    });

    html += `
          </tbody>
        </table>
        <div class="totals">
          <div class="row grand"><div><strong>Toplam Ödenen:</strong></div><div><strong>${formatMoney(totalPaid)}</strong></div></div>
        </div>
        ${anyLate ? `<div class="note">Bu ödeme vadesinden sonra yapılmıştır.</div>` : ''}
        <div class="footer-rule"></div>
        <div class="footer">
          <p class="thanks">Mağazamızdan yapmış olduğunuz ödeme için teşekkür ederiz</p>
          ${company.name ? `<p class="name">${company.name}</p>` : ''}
          ${company.footerSub ? `<p class="sub">${company.footerSub}</p>` : ''}
        </div>
      </div>
      </body></html>`;

    const w = window.open('', '_blank');
    if (w) {
      try { w.document.title = ''; } catch (e) { /* ignore */ }
      w.document.write(html);
      w.document.close();
      w.focus();
      w.print();
    }
  }

  async function deleteSale(sale) {
    if (!ensureWriteAllowed()) return false;
    const s = sale && sale.id ? sale : null;
    if (!s) return false;
    if (!window.confirm('Bu satışı silmek istediğinize emin misiniz?')) return false;
    if (!window.confirm('Dikkat: Bu işlem geri alınamaz. Bu satışa bağlı tüm taksitler de silinecek. Devam etmek istiyor musunuz?')) return false;
    try {
      setStatus('Siliniyor...');
      await api(`/sales/${s.id}`, { method: 'DELETE' });
      setStatus('Satış silindi.');
      const keepSelectedId = state.currentSale && String(state.currentSale.id) !== String(s.id) ? state.currentSale.id : null;
      state.sales = (state.sales || []).filter((row) => String(row.id) !== String(s.id));
      renderSalesTable(state.sales || [], { selectedSaleId: keepSelectedId });
      scheduleOfflineSnapshotSync();
      return true;
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
      return false;
    }
  }

  function printReceipt() {
    if (!state.currentSale || !state.selected) return;
    const sale = state.currentSale;
    const cust = state.selected;
    const insts = sale.installments || [];
    const totalInst = insts.reduce((s, i) => s + Number(i.amount || 0), 0);
    const down = Math.max(0, Number(sale.total || 0) - totalInst);
    printReceiptDetailed(
      {
        id: sale.id,
        date: sale.date,
        total: Number(sale.total || 0),
        down,
        installments: insts,
      },
      cust
    );
  }

  function printReceiptDetailed(sale, cust) {
    if (!sale || !cust) return;
    const company = getBusinessProfile();
    const businessContactLine = buildBusinessContactLine(company);

    const normalizeDueDate = (value) => {
      const raw = String(value || '').trim();
      if (!raw) return null;
      const compact = raw.length >= 10 ? raw.slice(0, 10) : raw;
      const iso = toISO(compact) || compact;
      const dt = parseISODate(iso);
      return dt ? dt.getTime() : null;
    };
    const compareByDueDate = (a, b) => {
      const da = normalizeDueDate(a?.due_date);
      const db = normalizeDueDate(b?.due_date);
      if (da !== null && db !== null) {
        if (da !== db) return da - db;
      } else if (da !== null) {
        return -1;
      } else if (db !== null) {
        return 1;
      }
      return Number(a?.id || 0) - Number(b?.id || 0);
    };
    const insts = (sale.installments || [])
      .map((i) => ({ ...i, paid: isPaid(i.paid) ? 1 : 0 }))
      .sort(compareByDueDate);
    const todayISO = formatISODate(new Date());
    const todayStamp = normalizeDueDate(todayISO);
    const unpaid = insts.filter((i) => !isPaid(i.paid));
    const overdue = unpaid.filter((i) => {
      if (!i.due_date) return false;
      const stamp = normalizeDueDate(i.due_date);
      if (stamp !== null && todayStamp !== null) return stamp < todayStamp;
      return String(i.due_date) < todayISO;
    });
    const upcoming = unpaid.filter((i) => {
      if (!i.due_date) return false;
      const stamp = normalizeDueDate(i.due_date);
      if (stamp !== null && todayStamp !== null) return stamp >= todayStamp;
      return String(i.due_date) >= todayISO;
    });
    overdue.sort(compareByDueDate);
    upcoming.sort(compareByDueDate);

    const formatTL = (n) => formatMoney(n || 0);
    const fmtDate = (iso) => fromISOSlash(iso || '');
    const now = new Date();
    const hh = String(now.getHours()).padStart(2, '0');
    const mm = String(now.getMinutes()).padStart(2, '0');
    const timeStr = `${hh}:${mm}`;
    const overdueTotal = overdue.reduce((s, i) => s + Number(i.amount || 0), 0);
    const totalInst = insts.reduce((s, i) => s + Number(i.amount || 0), 0);
    const saleTotal = Number(sale.total || 0);
    const downVal = Number(sale.down);
    const down = Number.isFinite(downVal) ? downVal : Math.max(0, saleTotal - totalInst);
    const isInstallmentSale = insts.length > 0;
    const receiptTitle = isInstallmentSale ? 'Taksitli Alışveriş - Satış Makbuzu' : 'Satış Makbuzu';

    let html = `
      <html><head><title> </title>
      <style>
        @page { size: A4; margin: 0; }
        @media print {
          .no-print { display:none; }
          html, body { margin:0; padding:0; }
        }
        body { font-family: Arial, sans-serif; color:#000; margin:0; -webkit-print-color-adjust: exact; print-color-adjust: exact; }
        .receipt { padding: 14mm; }
        .no-print { font-size:12px; color:#000; margin:10px 0 14px; }
        .brand { text-align:center; }
        .brand .name { font-weight:700; font-size:18px; letter-spacing:1px; margin:0; }
        .brand .line { font-size:11px; margin:3px 0 0; }
        .rule { border-top:2px solid #000; margin:10px 0 12px; }
        .title { text-align:center; font-weight:700; font-size:14px; margin:0 0 10px; }
        .meta-row { display:flex; justify-content:space-between; font-size:12px; margin:6px 0 12px; }
        .info { font-size:12px; margin:8px 0 12px; }
        .info .row { display:flex; gap:10px; margin:4px 0; }
        .info .label { width:90px; }
        .box { border:1px solid #000; border-radius:4px; padding:10px 12px; font-size:12px; margin:10px 0 14px; }
        .box .line { margin:3px 0; }
        .cols { display:flex; gap:30px; }
        .col { flex:1; }
        .col h3 { font-size:12px; margin:0; font-weight:700; }
        .col .under { border-bottom:1px solid #000; margin:6px 0 8px; }
        .item { font-size:12px; margin:6px 0; }
        .money { color:#000; font-weight:700; }
        .totals { margin-top:16px; font-size:12px; }
        .totals .row { display:flex; justify-content:space-between; margin:6px 0; }
        .totals .grand { font-weight:700; border-top:2px solid #000; padding-top:8px; margin-top:8px; }
        .footer-rule { border-top:2px solid #000; margin:18px 0 10px; }
        .footer { text-align:center; font-size:11px; }
        .footer .thanks { margin:0 0 10px; }
        .footer .name { font-weight:700; letter-spacing:1px; margin:0; }
        .footer .sub { margin:4px 0 0; color:#000; }
      </style></head><body>
      <div class="receipt">
        <div class="no-print">Not: Yazdırma ekranında “Üstbilgi ve altbilgiler” seçeneğini kapatın.</div>
        <div class="brand">
          ${company.name ? `<p class="name">${company.name}</p>` : ''}
          ${company.address ? `<p class="line">${company.address}</p>` : ''}
          ${businessContactLine ? `<p class="line">${businessContactLine}</p>` : ''}
        </div>
        <div class="rule"></div>
        <p class="title">${receiptTitle}</p>
        <div class="meta-row">
          <div><strong>Tarih:</strong> ${fmtDate(sale.date)}</div>
          <div><strong>Saat:</strong> ${timeStr}</div>
        </div>
        <div class="info">
          <div class="row"><div class="label"><strong>Hesap No:</strong></div><div>${cust.id || ''}</div></div>
          <div class="row"><div class="label"><strong>Sayın:</strong></div><div>${cust.name || ''}</div></div>
          <div class="row"><div class="label"><strong>Adres:</strong></div><div>${cust.address || ''}</div></div>
        </div>
        <div class="box">
          <div class="line"><strong>${fmtDate(sale.date)}</strong> Tarihinde <span class="money">${formatTL(sale.total)}</span> Alışveriş Yapılıp</div>
          <div class="line"><span class="money">${formatTL(isInstallmentSale ? down : saleTotal)}</span> ${isInstallmentSale ? 'Peşinat Alınmıştır' : 'Tahsil Edilmiştir'}</div>
        </div>`;

    if (isInstallmentSale) {
      html += `
        <div class="cols">
          <div class="col">
            <h3>Geciken Taksitler</h3>
            <div class="under"></div>`;
      if (!overdue.length) {
        html += `<div class="item">Yok</div>`;
      } else {
        overdue.forEach((i) => {
          html += `<div class="item">${fmtDate(i.due_date)} - <span class="money">${formatTL(i.amount)}</span></div>`;
        });
      }
      html += `
          </div>
          <div class="col">
            <h3>Yaklaşan Taksitler</h3>
            <div class="under"></div>`;
      upcoming.forEach((i) => {
        html += `<div class="item">${fmtDate(i.due_date)} - <span class="money">${formatTL(i.amount)}</span></div>`;
      });
      html += `
          </div>
        </div>
        <div class="totals">
          <div class="row"><div><strong>Geciken Toplam:</strong></div><div class="money">${formatTL(overdueTotal)}</div></div>
          <div class="row"><div><strong>Taksitler Toplamı:</strong></div><div class="money">${formatTL(totalInst)}</div></div>
          <div class="row grand"><div><strong>Genel Toplam:</strong></div><div class="money">${formatTL(saleTotal)}</div></div>
        </div>`;
    } else {
      html += `
        <div class="totals">
          <div class="row grand"><div><strong>Genel Toplam:</strong></div><div class="money">${formatTL(saleTotal)}</div></div>
        </div>`;
    }

    html += `
        <div class="footer-rule"></div>
        <div class="footer">
          <p class="thanks">Mağazamızdan yapmış olduğunuz alış verişten dolayı teşekkür ederiz</p>
          ${company.name ? `<p class="name">${company.name}</p>` : ''}
          ${company.footerSub ? `<p class="sub">${company.footerSub}</p>` : ''}
        </div>
      </div>
      </body></html>`;
    const w = window.open('', '_blank');
    if (w) {
      try { w.document.title = ''; } catch (e) { /* ignore */ }
      w.document.write(html);
      w.document.close();
      w.focus();
      w.print();
    }
  }

  // ------------------- Report -------------------
  function startOfWeekMonday(date) {
    const d = date instanceof Date ? new Date(date) : new Date(date);
    const day = d.getDay(); // 0=Sun, 1=Mon...
    const diff = day === 0 ? -6 : 1 - day;
    d.setDate(d.getDate() + diff);
    return new Date(d.getFullYear(), d.getMonth(), d.getDate());
  }

  function setReportRange(startDate, endDate) {
    const startEl = document.getElementById('rep-start');
    const endEl = document.getElementById('rep-end');
    if (!startEl || !endEl) return;
    startEl.value = fromISO(formatISODate(startDate));
    endEl.value = fromISO(formatISODate(endDate));
    loadReport();
  }

  function formatReportDescription(row) {
    const base = String(row && row.description ? row.description : '').trim();
    if (row && row.event_type === 'installment_payment') {
      const label = row.installment_id ? `Taksit tahsilatı #${row.installment_id}` : 'Taksit tahsilatı';
      return base ? `${label} - ${base}` : label;
    }
    if (row && row.event_type === 'down_payment') {
      return base ? `Peşinat - ${base}` : 'Peşinat';
    }
    return base;
  }

  function renderReportTab() {
    const root = document.getElementById('pusula-tab-report');
    if (!root) return;
    const today = todayStr();
    root.innerHTML = `
      <div class="pusula-grid">
        <label class="pusula-input"><span>Başlangıç</span><input type="text" id="rep-start" value="${today}"></label>
        <label class="pusula-input"><span>Bitiş</span><input type="text" id="rep-end" value="${today}"></label>
        <div class="pusula-actions"><button id="rep-run">Raporla</button></div>
      </div>
      <div class="pusula-actions report-quick-actions" id="rep-quick">
        <button type="button" class="secondary" data-range="today">BUGÜN</button>
        <button type="button" class="secondary" data-range="yesterday">DÜN</button>
        <button type="button" class="secondary" data-range="this-week">BU HAFTA</button>
        <button type="button" class="secondary" data-range="last-week">GEÇEN HAFTA</button>
        <button type="button" class="secondary" data-range="this-month">BU AY</button>
        <button type="button" class="secondary" data-range="last-month">GEÇEN AY</button>
      </div>
      <div class="pusula-summary"><span id="rep-total">0,00₺</span> — <span id="rep-count">0</span> tahsilat</div>
      <div class="pusula-table-wrapper">
        <table class="pusula-table" id="rep-table">
          <thead><tr><th>Satış No</th><th>Müşteri</th><th>Tarih</th><th>Tutar</th><th>Açıklama</th></tr></thead>
          <tbody></tbody>
        </table>
      </div>
    `;
    root.querySelector('#rep-run').addEventListener('click', () => loadReport());
    root.querySelectorAll('#rep-quick [data-range]').forEach((btn) => {
      btn.addEventListener('click', () => {
        const range = btn.getAttribute('data-range');
        const now = new Date();
        const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
        if (range === 'today') {
          setReportRange(today, today);
          return;
        }
        if (range === 'yesterday') {
          const y = new Date(today);
          y.setDate(y.getDate() - 1);
          setReportRange(y, y);
          return;
        }
        if (range === 'this-week') {
          const start = startOfWeekMonday(today);
          setReportRange(start, today);
          return;
        }
        if (range === 'last-week') {
          const thisWeekStart = startOfWeekMonday(today);
          const start = new Date(thisWeekStart);
          start.setDate(start.getDate() - 7);
          const end = new Date(start);
          end.setDate(end.getDate() + 6);
          setReportRange(start, end);
          return;
        }
        if (range === 'this-month') {
          const start = new Date(today.getFullYear(), today.getMonth(), 1);
          setReportRange(start, today);
          return;
        }
        if (range === 'last-month') {
          const start = new Date(today.getFullYear(), today.getMonth() - 1, 1);
          const end = new Date(today.getFullYear(), today.getMonth(), 0);
          setReportRange(start, end);
        }
      });
    });
    setTimeout(ensureLayoutFits, 0);
  }

  async function loadReport({ silent = false } = {}) {
    const startEl = document.getElementById('rep-start');
    const endEl = document.getElementById('rep-end');
    if (!startEl || !endEl) return;
    const start = startEl.value || todayStr();
    const end = endEl.value || todayStr();
    try {
      const rows = await api(`/daily-report?start=${toISO(start)}&end=${toISO(end)}`);
      state.reportSales = rows || [];
      const total = rows.reduce((sum, s) => sum + Number(s.amount ?? s.total ?? 0), 0);
      document.getElementById('rep-total').textContent = formatMoney(total);
      document.getElementById('rep-count').textContent = rows.length;
      const tbody = document.querySelector('#rep-table tbody');
      tbody.innerHTML = '';
      rows.forEach((s) => {
        const tr = document.createElement('tr');
        const saleId = s.sale_id || s.id || '';
        const amount = Number(s.amount ?? s.total ?? 0);
        tr.innerHTML = `<td>${saleId}</td><td>${s.customer_name || ''}</td><td>${fromISO(s.date)}</td><td>${formatMoney(amount)}</td><td>${truncateDescription(formatReportDescription(s))}</td>`;
        tr.addEventListener('dblclick', (e) => {
          e.preventDefault();
          if (!saleId) return;
          const descCell = tr.querySelector('td:last-child');
          openSaleDescriptionEditor({ ...s, id: saleId }, {
            customer: { id: s.customer_id, name: s.customer_name },
            onUpdated: (next) => {
              s.description = next || '';
              if (descCell) descCell.textContent = truncateDescription(formatReportDescription(s));
            },
          });
        });
        tbody.appendChild(tr);
      });
      if (!silent) setStatus('Rapor yüklendi.');
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    }
    setTimeout(ensureLayoutFits, 0);
  }

  // ------------------- Expected Payments -------------------
  function renderExpectedTab() {
    const root = document.getElementById('pusula-tab-expected');
    if (!root) return;
    root.innerHTML = `
      <div class="pusula-grid">
        <label class="pusula-input"><span>Vade Başlangıç</span><input type="text" id="exp-start" placeholder="GG-AA-YYYY"></label>
        <label class="pusula-input"><span>Vade Bitiş</span><input type="text" id="exp-end" placeholder="GG-AA-YYYY"></label>
        <div class="pusula-actions"><button id="exp-run">LİSTELE</button></div>
      </div>
      <div class="pusula-actions report-quick-actions" id="exp-quick">
        <button type="button" class="secondary" data-range="all">TÜMÜ</button>
        <button type="button" class="secondary" data-range="this-month">BU AY</button>
        <button type="button" class="secondary" data-range="next-month">GELECEK AY</button>
        <label class="pusula-check"><input type="checkbox" id="exp-hide-late"><span>Gecikmiş ödemeleri gizle</span></label>
      </div>
      <div class="pusula-summary"><span id="exp-total">0,00₺</span> — <span id="exp-count">0</span> taksit</div>
      <div class="pusula-actions expected-actions">
        <input type="number" step="0.01" min="0" id="exp-pay-amount" class="detail-inline-input" placeholder="Tahsilat Tutarı">
        <button id="exp-mark-paid" disabled>TAHSİLAT AL</button>
        <button id="exp-mark-unpaid" disabled>SON ÖDEMEYİ GERİ AL</button>
        <button id="exp-print" disabled>Makbuz Yazdır</button>
      </div>
      <div class="pusula-table-wrapper">
        <table class="pusula-table" id="exp-table">
          <thead>
            <tr>
              <th>Vade</th>
              <th>Taksit</th>
              <th>Ödenen</th>
              <th>Kalan</th>
              <th>Müşteri No</th>
              <th>Müşteri</th>
              <th>Telefon</th>
              <th>Adres</th>
              <th>Satış No</th>
              <th>Satış Tarihi</th>
              <th>Satış Tutarı</th>
              <th>Açıklama</th>
            </tr>
          </thead>
          <tbody></tbody>
        </table>
      </div>
    `;
    root.querySelector('#exp-run').addEventListener('click', () => loadExpectedPayments());
    root.querySelector('#exp-hide-late').addEventListener('change', () => renderExpectedPaymentsTable());
    root.querySelector('#exp-start').addEventListener('input', () => {
      const changed = updateExpectedHideLateState();
      if (changed) renderExpectedPaymentsTable();
    });
    root.querySelector('#exp-mark-paid').addEventListener('click', markExpectedPaymentPaid);
    root.querySelector('#exp-mark-unpaid').addEventListener('click', markExpectedPaymentUnpaid);
    root.querySelector('#exp-print').addEventListener('click', () => printExpectedPaymentReceipt());
    root.querySelectorAll('#exp-quick [data-range]').forEach((btn) => {
      btn.addEventListener('click', () => {
        const range = btn.getAttribute('data-range');
        const now = new Date();
        const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
        if (range === 'all') {
          const startEl = document.getElementById('exp-start');
          const endEl = document.getElementById('exp-end');
          if (startEl) startEl.value = '';
          if (endEl) endEl.value = '';
          const hideEl = document.getElementById('exp-hide-late');
          if (hideEl) hideEl.checked = false;
          updateExpectedHideLateState();
          loadExpectedPayments();
          return;
        }
        if (range === 'this-month') {
          const start = new Date(today.getFullYear(), today.getMonth(), 1);
          const end = new Date(today.getFullYear(), today.getMonth() + 1, 0);
          setExpectedRange(start, end);
          return;
        }
        if (range === 'next-month') {
          const start = new Date(today.getFullYear(), today.getMonth() + 1, 1);
          const end = new Date(today.getFullYear(), today.getMonth() + 2, 0);
          setExpectedRange(start, end);
        }
      });
    });
    updateExpectedHideLateState();
    updateReadOnlyUi();
    setTimeout(ensureLayoutFits, 0);
  }

  function setExpectedRange(startDate, endDate) {
    const startEl = document.getElementById('exp-start');
    const endEl = document.getElementById('exp-end');
    if (!startEl || !endEl) return;
    startEl.value = fromISO(formatISODate(startDate));
    endEl.value = fromISO(formatISODate(endDate));
    updateExpectedHideLateState();
    loadExpectedPayments();
  }

  function updateExpectedHideLateState() {
    const hideEl = document.getElementById('exp-hide-late');
    const startEl = document.getElementById('exp-start');
    if (!hideEl || !startEl) return false;
    const todayISO = formatISODate(new Date());
    const startIso = toISO(startEl.value.trim());
    const shouldDisable = Boolean(startIso) && startIso >= todayISO;
    let changed = false;
    if (shouldDisable && !hideEl.disabled) {
      hideEl.disabled = true;
      changed = true;
    } else if (!shouldDisable && hideEl.disabled) {
      hideEl.disabled = false;
    }
    if (shouldDisable && hideEl.checked) {
      hideEl.checked = false;
      changed = true;
    }
    return changed;
  }

  function renderExpectedPaymentsTable() {
    const tbody = document.querySelector('#exp-table tbody');
    if (!tbody) return;
    tbody.innerHTML = '';
    state.currentExpected = null;
    const markPaidBtn = document.getElementById('exp-mark-paid');
    const markUnpaidBtn = document.getElementById('exp-mark-unpaid');
    const printBtn = document.getElementById('exp-print');
    const payInput = document.getElementById('exp-pay-amount');
    if (markPaidBtn) markPaidBtn.disabled = true;
    if (markUnpaidBtn) markUnpaidBtn.disabled = true;
    if (printBtn) printBtn.disabled = true;
    if (payInput) payInput.value = '';
    const hideLate = Boolean(document.getElementById('exp-hide-late')?.checked);
    const todayISO = formatISODate(new Date());
    const lateCustomers = new Set();
    state.expectedPayments.forEach((row) => {
      const remaining = installmentRemainingAmount(row);
      if (row && row.due_date && row.due_date < todayISO && remaining > 0) {
        lateCustomers.add(String(row.customer_id || ''));
      }
    });
    const visibleRows = state.expectedPayments.filter((row) => {
      const isLatePayment = row && row.due_date && row.due_date < todayISO && installmentRemainingAmount(row) > 0;
      return !(hideLate && isLatePayment);
    });
    const total = visibleRows.reduce((sum, r) => sum + installmentRemainingAmount(r), 0);
    const totalEl = document.getElementById('exp-total');
    const countEl = document.getElementById('exp-count');
    if (totalEl) totalEl.textContent = formatMoney(total);
    if (countEl) countEl.textContent = visibleRows.length;
    visibleRows.forEach((row) => {
      const tr = document.createElement('tr');
      const due = row && row.due_date ? fromISO(row.due_date) : '';
      const saleDate = row && row.sale_date ? fromISO(row.sale_date) : '';
      const amount = formatMoney(row && (row.installment_amount ?? row.amount) ? (row.installment_amount ?? row.amount) : 0);
      const paidAmount = formatMoney(installmentPaidAmount(row));
      const remainingAmount = formatMoney(installmentRemainingAmount(row));
      const saleTotal = formatMoney(row && row.sale_total ? row.sale_total : 0);
      const desc = truncateDescription(row && row.sale_description ? row.sale_description : '');
      const address = [row && row.customer_address ? row.customer_address : '', row && row.customer_work_address ? row.customer_work_address : '']
        .filter(Boolean)
        .join(' / ');
      const remaining = installmentRemainingAmount(row);
      const isPaidRow = remaining <= 0;
      const lastPaymentDate = row && row.last_payment_date ? String(row.last_payment_date) : '';
      const canUndoToday = Boolean(row && row.last_payment_id) && lastPaymentDate === todayISO;
      const isPaidToday = isPaidRow && canUndoToday;
      tr.innerHTML = `
        <td>${due}</td>
        <td>${amount}</td>
        <td>${paidAmount}</td>
        <td>${remainingAmount}</td>
        <td>${row && row.customer_id ? row.customer_id : ''}</td>
        <td>${row && row.customer_name ? row.customer_name : ''}</td>
        <td>${row && row.customer_phone ? row.customer_phone : ''}</td>
        <td>${address}</td>
        <td>${row && row.sale_id ? row.sale_id : ''}</td>
        <td>${saleDate}</td>
        <td>${saleTotal}</td>
        <td>${desc}</td>
      `;
      const isLatePayment = row && row.due_date && row.due_date < todayISO && remaining > 0;
      if (isPaidToday) {
        tr.classList.add('paid-today');
      } else if (isLatePayment || lateCustomers.has(String(row && row.customer_id ? row.customer_id : ''))) {
        tr.classList.add('late');
      } else if (!isPaidRow && installmentPaidAmount(row) > 0) {
        tr.classList.add('partial');
      }
      tr.addEventListener('click', () => {
        state.currentExpected = row;
        tbody.querySelectorAll('tr').forEach((r) => r.classList.remove('selected'));
        tr.classList.add('selected');
        if (payInput) payInput.value = remaining > 0 ? String(Math.round(remaining * 100) / 100) : '';
        if (markPaidBtn) markPaidBtn.disabled = isPaidRow;
        if (markUnpaidBtn) markUnpaidBtn.disabled = !canUndoToday;
        if (printBtn) printBtn.disabled = !Boolean(row && row.last_payment_id);
      });
      tr.addEventListener('dblclick', (e) => {
        e.preventDefault();
        openExpectedCustomerDetail(row);
      });
      tbody.appendChild(tr);
    });
    if (!visibleRows.length) {
      const tr = document.createElement('tr');
      tr.className = 'pusula-empty-row';
      tr.innerHTML = '<td colspan="12" class="pusula-empty-cell">Beklenen ödeme bulunamadı.</td>';
      tbody.appendChild(tr);
    }
    updateReadOnlyUi();
  }

  function openExpectedCustomerDetail(row) {
    if (!row || !row.customer_id) return;
    const cust = {
      id: row.customer_id,
      name: row.customer_name || '',
      phone: row.customer_phone || '',
      address: row.customer_address || '',
      work_address: row.customer_work_address || '',
      debt_total: 0,
    };
    setSelectedCustomer(cust);
    updateDetail(cust);
    fillSaleCustomer(cust);
    activateTab('detail');
    loadSales(row.customer_id, row.sale_id || null);
  }

  async function printExpectedPaymentReceipt(row) {
    const target = row || state.currentExpected;
    if (!target || !target.installment_id) return;
    if (!target.last_payment_id) {
      setStatus('Hata: Yazdırılacak tahsilat bulunamadı.', true);
      return;
    }
    try {
      const payload = await api(`/installments/${target.installment_id}/payments`);
      const installment = payload && payload.installment ? payload.installment : target;
      const payments = payload && Array.isArray(payload.payments) ? payload.payments : [];
      const payment = payments.find((p) => String(p.id) === String(target.last_payment_id)) || payments[payments.length - 1];
      if (!payment) {
        setStatus('Hata: Tahsilat kaydı bulunamadı.', true);
        return;
      }
      const cust = {
        id: target.customer_id,
        name: target.customer_name || '',
        address: target.customer_address || '',
      };
      const sale = findSaleByInstallmentId(installment.id) || { id: target.sale_id };
      printInstallmentPaymentReceipt(cust, installment, payment, sale);
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    }
  }

  async function markExpectedPaymentPaid() {
    if (!ensureWriteAllowed()) return;
    if (!state.currentExpected || !state.currentExpected.installment_id) return;
    const target = state.currentExpected;
    const instId = target.installment_id;
    const remaining = installmentRemainingAmount(target);
    if (!(remaining > 0)) return;
    const inputEl = document.getElementById('exp-pay-amount');
    const entered = Number(inputEl && inputEl.value !== '' ? inputEl.value : NaN);
    const amount = Number.isFinite(entered) && entered > 0 ? entered : remaining;
    const normalizedAmount = Math.round(amount * 100) / 100;
    if (!(normalizedAmount > 0) || normalizedAmount - remaining > 0.00001) {
      setStatus('Tahsilat tutarı kalan borçtan büyük olamaz.', true);
      return;
    }
    try {
      setExpectedLoading(true);
      const result = await api(`/installments/${instId}/payments`, {
        method: 'POST',
        body: JSON.stringify({ amount: normalizedAmount }),
      });
      setStatus('Tahsilat kaydedildi.');
      const payment = result && result.payment ? result.payment : null;
      const updatedInstallment = result && result.installment ? result.installment : null;
      const updatedRow = {
        ...target,
        ...(updatedInstallment || {}),
        last_payment_id: payment && payment.id ? payment.id : target.last_payment_id,
        last_payment_amount: payment && payment.amount ? payment.amount : target.last_payment_amount,
        last_payment_date: payment && payment.payment_date ? payment.payment_date : target.last_payment_date,
      };
      state.currentExpected = null;
      await loadExpectedPayments({ silent: true, showLoading: false });
      if (state.selected && String(state.selected.id || '') === String(target.customer_id || '')) {
        await loadSales(target.customer_id, target.sale_id || null);
      }
      if (window.confirm('Makbuz yazdırılsın mı?')) {
        const cust = {
          id: target.customer_id,
          name: target.customer_name || '',
          address: target.customer_address || '',
        };
        const sale = findSaleByInstallmentId(instId) || { id: target.sale_id };
        if (payment) {
          printInstallmentPaymentReceipt(cust, updatedInstallment || target, payment, sale);
        } else {
          await printExpectedPaymentReceipt(updatedRow);
        }
      }
      scheduleOfflineSnapshotSync();
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    } finally {
      setExpectedLoading(false);
    }
  }

  async function markExpectedPaymentUnpaid() {
    if (!ensureWriteAllowed()) return;
    if (!state.currentExpected || !state.currentExpected.installment_id) return;
    const target = state.currentExpected;
    const instId = target.installment_id;
    const todayISO = formatISODate(new Date());
    const lastPaymentDate = target.last_payment_date ? String(target.last_payment_date) : '';
    const paymentId = target.last_payment_id;
    if (!paymentId || lastPaymentDate !== todayISO) return;
    try {
      setExpectedLoading(true);
      await api(`/installments/${instId}/payments/${paymentId}`, {
        method: 'DELETE',
      });
      setStatus('Ödeme güncellendi.');
      state.currentExpected = null;
      await loadExpectedPayments({ silent: true, showLoading: false });
      if (state.selected && String(state.selected.id || '') === String(target.customer_id || '')) {
        await loadSales(target.customer_id, target.sale_id || null);
      }
      scheduleOfflineSnapshotSync();
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    } finally {
      setExpectedLoading(false);
    }
  }

  async function loadExpectedPayments({ silent = false, showLoading = true } = {}) {
    const startEl = document.getElementById('exp-start');
    const endEl = document.getElementById('exp-end');
    const query = new URLSearchParams();
    const startRaw = startEl ? startEl.value.trim() : '';
    const endRaw = endEl ? endEl.value.trim() : '';
    const startIso = toISO(startRaw);
    const endIso = toISO(endRaw);
    if (startIso) query.append('start', startIso);
    if (endIso) query.append('end', endIso);
    try {
      if (showLoading) setExpectedLoading(true);
      const rows = await api(`/expected-payments${query.toString() ? `?${query.toString()}` : ''}`);
      state.expectedPayments = rows || [];
      renderExpectedPaymentsTable();
      if (!silent) setStatus('Beklenen ödemeler yüklendi.');
    } catch (err) {
      setStatus(`Hata: ${trErrorMessage(err.message)}`, true);
    } finally {
      if (showLoading) setExpectedLoading(false);
    }
    setTimeout(ensureLayoutFits, 0);
  }

  // ------------------- Settings -------------------
  function renderSettingsTab() {
    const root = document.getElementById('pusula-tab-settings');
    if (!root) return;
    root.innerHTML = `
      <p>Bu sayfa, eklentide kayıtlı API anahtarını kullanır.</p>
      <p><strong>API Uç Noktası:</strong> ${apiBase}</p>
      <p>Pusula Kullanıcısı rolüne sahip kullanıcılar bu arayüze erişebilir.</p>
    `;
  }

  // ------------------- Init -------------------
  async function init() {
    if (!desktopMode) {
      await loadOfflineSnapshotFromStorage();
      await registerOfflineShell();
    }
    renderTabs();
    renderSearchTab();
    renderAddTab();
    renderSaleTab();
    renderDetailTab();
    renderReportTab();
    renderExpectedTab();
    if (desktopMode) {
      setOfflineMode(false);
    } else if (!navigator.onLine) {
      setOfflineMode(true);
    } else {
      updateReadOnlyUi();
    }
    await loadCustomers();
    startAutoRefresh();
    if (!desktopMode) startOfflineSnapshotSync();
    if (!desktopMode && navigator.onLine) {
      syncOfflineSnapshot({ silent: true });
    }
    window.addEventListener('resize', () => setTimeout(ensureLayoutFits, 0));
    window.addEventListener('focus', () => autoRefreshTick());
    if (!desktopMode) {
      window.addEventListener('online', handleConnectionRestore);
      window.addEventListener('offline', handleConnectionLoss);
    }
    document.addEventListener('visibilitychange', () => {
      if (!document.hidden) autoRefreshTick();
    });
    setTimeout(ensureLayoutFits, 0);
  }

  const bootstrapApp = () => {
    init().catch(() => {
      // Ignore bootstrap errors; the app will show request failures in-place.
    });
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', bootstrapApp, { once: true });
  } else {
    bootstrapApp();
  }
})();

export {};
