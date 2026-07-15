import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import {
  escapeHtml,
  formatIsoDateDashed,
  formatIsoDateSlashed,
} from "../src/html-safety.js";

const source = await readFile(new URL("../src/pusula-app.js", import.meta.url), "utf8");

function paymentStoreHarness({ initial = null, failWrites = false, uuid = "00000000-0000-4000-8000-000000000001" } = {}) {
  let stored = initial;
  let writesFail = failWrites;
  const window = {
    crypto: { randomUUID: () => uuid },
    localStorage: {
      getItem: () => stored,
      setItem: (_key, value) => {
        if (writesFail) throw new Error("storage unavailable");
        stored = value;
      },
    },
  };
  const state = { pendingPaymentRequests: new Map() };
  const start = source.indexOf("  function paymentOperationKey");
  const end = source.indexOf("  function beginPaymentOperation", start);
  assert.ok(start >= 0 && end > start, "payment request helpers must remain extractable");
  const helpers = new Function(
    "window",
    "state",
    "pendingPaymentStorageKey",
    "pendingPaymentStorageMaxChars",
    "pendingPaymentEntryLimit",
    "console",
    `${source.slice(start, end)}\nreturn { restorePendingPaymentRequests, paymentRequestKey, clearPaymentRequest };`,
  )(window, state, "pusula-payment-requests-v1", 64 * 1024, 256, { warn() {} });
  return { state, helpers, stored: () => stored, failWrites: (value) => { writesFail = value; } };
}

test("HTML escaping protects imported customer and business text", () => {
  assert.equal(
    escapeHtml(`<img src=x onerror="alert('x')"> & test`),
    "&lt;img src=x onerror=&quot;alert(&#039;x&#039;)&quot;&gt; &amp; test",
  );
  assert.equal(escapeHtml(null), "");
  assert.equal(escapeHtml(17), "17");
});

test("date renderers reject imported markup and impossible dates", () => {
  assert.equal(formatIsoDateDashed("2026-07-14"), "14-07-2026");
  assert.equal(formatIsoDateSlashed("2026-07-14"), "14/07/2026");
  for (const malicious of [
    `<img src=x onerror=alert(1)>`,
    `2026-07-14<script>alert(1)</script>`,
    `2026-02-31`,
    `2026-13-01`,
  ]) {
    assert.equal(formatIsoDateDashed(malicious), "");
    assert.equal(formatIsoDateSlashed(malicious), "");
  }
});

test("customer, description, address, and business fields are never interpolated raw", () => {
  const forbidden = [
    "${company.name}",
    "${company.address}",
    "${company.footerSub}",
    "${businessContactLine}",
    "${customerName}",
    "${customerAddress}",
    "${cust.name || ''}",
    "${cust.address || ''}",
    "${s.customer_name || ''}",
    "${row && row.customer_name ? row.customer_name : ''}",
    "${row && row.customer_phone ? row.customer_phone : ''}",
    "${address}",
    "${desc}",
  ];
  for (const expression of forbidden) {
    assert.equal(source.includes(expression), false, `raw HTML interpolation remains: ${expression}`);
  }
  assert.equal((source.match(/window\.open\('', '_blank'\)/g) || []).length, 3);
  assert.equal((source.match(/w\.opener = null/g) || []).length, 3);
});

test("payment submits share a per-installment guard and retain an idempotency key for retries", () => {
  assert.match(source, /paymentOperations: new Set\(\)/);
  assert.match(source, /pendingPaymentRequests: new Map\(\)/);
  assert.match(source, /if \(!beginPaymentOperation\(inst\.id\)\) return;/);
  assert.match(source, /if \(!beginPaymentOperation\(instId\)\) return;/);
  assert.equal((source.match(/request_key: requestKey/g) || []).length, 2);
  assert.equal((source.match(/clearPaymentRequest\(/g) || []).length, 5);
  assert.equal((source.match(/endPaymentOperation\(/g) || []).length, 5);
  assert.match(source, /pending && pending\.fingerprint === fingerprint/);
});

test("payment request intent survives restart, rotates only for changed data, and clears definitively", () => {
  const first = paymentStoreHarness();
  const firstKey = first.helpers.paymentRequestKey(17, 12.34, "2026-07-15");
  assert.equal(firstKey, "payment:00000000-0000-4000-8000-000000000001");
  const persisted = first.stored();
  assert.match(persisted, /1234:2026-07-15/);

  const restarted = paymentStoreHarness({
    initial: persisted,
    uuid: "00000000-0000-4000-8000-000000000002",
  });
  restarted.helpers.restorePendingPaymentRequests();
  assert.equal(
    restarted.helpers.paymentRequestKey(17, 12.34, "2026-07-15"),
    firstKey,
    "the exact retry must reuse the durable key",
  );
  const changedKey = restarted.helpers.paymentRequestKey(17, 12.35, "2026-07-15");
  assert.equal(changedKey, "payment:00000000-0000-4000-8000-000000000002");
  assert.notEqual(changedKey, firstKey);
  restarted.helpers.clearPaymentRequest(17, changedKey);
  assert.equal(restarted.state.pendingPaymentRequests.size, 0);
  assert.equal(restarted.stored(), "{}");

  assert.equal(
    (source.match(/includes\('istek anahtarı farklı bir tahsilat'\)[\s\S]{0,120}clearPaymentRequest/g) || []).length,
    2,
    "both payment screens must clear a definitive backend key conflict",
  );
});

test("malformed persisted payment intents are rejected and storage failure blocks submission", () => {
  const malformed = paymentStoreHarness({
    initial: JSON.stringify({
      bad: { fingerprint: "1234:2026-07-15", requestKey: "payment:valid" },
      17: { fingerprint: "not-a-fingerprint", requestKey: "payment:valid" },
      18: { fingerprint: "1234:2026-07-15", requestKey: "../../invalid" },
      19: { fingerprint: "1234:2026-07-15", requestKey: `payment:${"a".repeat(65)}` },
    }),
  });
  malformed.helpers.restorePendingPaymentRequests();
  assert.equal(malformed.state.pendingPaymentRequests.size, 0);

  const unavailable = paymentStoreHarness({ failWrites: true });
  assert.equal(unavailable.helpers.paymentRequestKey(17, 12.34, "2026-07-15"), null);
  assert.equal(unavailable.state.pendingPaymentRequests.size, 0);
  const submitGuard = source.slice(
    source.indexOf("const requestKey = paymentRequestKey(inst.id"),
    source.indexOf("try {", source.indexOf("const requestKey = paymentRequestKey(inst.id")),
  );
  assert.match(submitGuard, /if \(!requestKey\)/);
  assert.match(submitGuard, /endPaymentOperation\(inst\.id\)/);
  assert.match(submitGuard, /return;/);
});

test("payment intents require secure randomness and retain durable replay state when clearing fails", () => {
  const secureFallback = paymentStoreHarness();
  secureFallback.state.pendingPaymentRequests.clear();
  secureFallback.helpers.clearPaymentRequest(17, "unused");
  const fallbackBytes = Array.from({ length: 16 }, (_, index) => index + 1);
  const fallbackWindow = {
    crypto: {
      getRandomValues(bytes) {
        bytes.set(fallbackBytes);
        return bytes;
      },
    },
    localStorage: {
      getItem: () => null,
      setItem() {},
    },
  };
  const fallbackState = { pendingPaymentRequests: new Map() };
  const start = source.indexOf("  function paymentOperationKey");
  const end = source.indexOf("  function beginPaymentOperation", start);
  const fallbackHelpers = new Function(
    "window",
    "state",
    "pendingPaymentStorageKey",
    "pendingPaymentStorageMaxChars",
    "pendingPaymentEntryLimit",
    "console",
    `${source.slice(start, end)}\nreturn { paymentRequestKey };`,
  )(fallbackWindow, fallbackState, "pusula-payment-requests-v1", 64 * 1024, 256, { warn() {} });
  assert.equal(
    fallbackHelpers.paymentRequestKey(17, 12.34, "2026-07-15"),
    "payment:0102030405060708090a0b0c0d0e0f10",
  );

  fallbackWindow.crypto = {};
  assert.equal(fallbackHelpers.paymentRequestKey(18, 12.34, "2026-07-15"), null);
  assert.doesNotMatch(source, /Math\.random/);

  const durable = paymentStoreHarness();
  const key = durable.helpers.paymentRequestKey(19, 45.67, "2026-07-15");
  const persisted = durable.stored();
  durable.failWrites(true);
  assert.equal(durable.helpers.clearPaymentRequest(19, key), false);
  assert.equal(durable.state.pendingPaymentRequests.get("19").requestKey, key);
  assert.equal(durable.stored(), persisted);
});

test("payment intent restore rejects oversized strings and excessive entry maps", () => {
  const oversized = paymentStoreHarness({ initial: "x".repeat((64 * 1024) + 1) });
  oversized.helpers.restorePendingPaymentRequests();
  assert.equal(oversized.state.pendingPaymentRequests.size, 0);

  const entries = {};
  for (let index = 1; index <= 257; index += 1) {
    entries[index] = {
      fingerprint: "1234:2026-07-15",
      requestKey: `payment:${String(index).padStart(8, "0")}`,
    };
  }
  const excessive = paymentStoreHarness({ initial: JSON.stringify(entries) });
  excessive.helpers.restorePendingPaymentRequests();
  assert.equal(excessive.state.pendingPaymentRequests.size, 0);
});
