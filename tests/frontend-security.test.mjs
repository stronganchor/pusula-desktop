import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import {
  escapeHtml,
  formatIsoDateDashed,
  formatIsoDateSlashed,
} from "../src/html-safety.js";

const source = await readFile(new URL("../src/pusula-app.js", import.meta.url), "utf8");

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
