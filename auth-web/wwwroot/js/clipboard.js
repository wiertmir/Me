// The only page script (CSP forbids inline scripts): clipboard copy and the native <dialog> open/close.
window.meUi = {
  // Copies the text of #id; falls back to selecting it so the user can press Ctrl+C. Resolves true when copied.
  async copy(id) {
    const el = document.getElementById(id);
    if (!el) return false;
    try {
      if (navigator.clipboard && window.isSecureContext) {
        await navigator.clipboard.writeText(el.textContent);
        return true;
      }
    } catch { /* fall through to selection */ }
    const range = document.createRange();
    range.selectNodeContents(el);
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
    return false;
  },
  open(id) { const d = document.getElementById(id); if (d && !d.open) d.showModal(); },
  close(id) { const d = document.getElementById(id); if (d && d.open) d.close(); },
};
