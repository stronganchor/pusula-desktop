export function escapeHtml(value) {
  return String(value ?? '').replace(/[&<>"']/g, (character) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#039;',
  })[character]);
}

function isoDateParts(value) {
  const match = String(value ?? '').match(/^(\d{4})-(\d{2})-(\d{2})$/);
  if (!match) return null;
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const date = new Date(0);
  date.setUTCHours(0, 0, 0, 0);
  date.setUTCFullYear(year, month - 1, day);
  if (
    date.getUTCFullYear() !== year ||
    date.getUTCMonth() !== month - 1 ||
    date.getUTCDate() !== day
  ) {
    return null;
  }
  return { year: match[1], month: match[2], day: match[3] };
}

export function formatIsoDateDashed(value) {
  const parts = isoDateParts(value);
  return parts ? `${parts.day}-${parts.month}-${parts.year}` : '';
}

export function formatIsoDateSlashed(value) {
  const parts = isoDateParts(value);
  return parts ? `${parts.day}/${parts.month}/${parts.year}` : '';
}
