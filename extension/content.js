// Content script (Fase 1): panel melayang "Download with ADM" yang muncul saat
// ada video/audio progresif terdeteksi di halaman. Klik item → kirim ke ADM.
(function () {
  if (window.__admVideoPanel) return;
  window.__admVideoPanel = true;

  let items = [];
  let dismissed = false;
  let panel = null;
  let listEl = null;

  function fmtSize(n) {
    if (!n) return "";
    const u = ["B", "KB", "MB", "GB"];
    let i = 0;
    let v = n;
    while (v >= 1024 && i < u.length - 1) {
      v /= 1024;
      i += 1;
    }
    return ` · ${v.toFixed(v < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
  }

  function ensurePanel() {
    if (panel) return;
    panel = document.createElement("div");
    panel.id = "adm-video-panel";
    Object.assign(panel.style, {
      position: "fixed",
      top: "16px",
      right: "16px",
      zIndex: "2147483647",
      width: "264px",
      background: "#1f2d27",
      color: "#e6ece8",
      font: "13px/1.4 'Segoe UI', system-ui, sans-serif",
      borderRadius: "8px",
      boxShadow: "0 4px 16px rgba(0,0,0,.35)",
      border: "1px solid #2e4339",
      overflow: "hidden",
    });

    const head = document.createElement("div");
    Object.assign(head.style, {
      display: "flex",
      alignItems: "center",
      justifyContent: "space-between",
      padding: "8px 10px",
      background: "#243430",
    });
    const title = document.createElement("span");
    title.textContent = "▶ Download with ADM";
    title.style.fontWeight = "600";
    const close = document.createElement("span");
    close.textContent = "✕";
    Object.assign(close.style, { cursor: "pointer", opacity: "0.7", paddingLeft: "8px" });
    close.addEventListener("click", () => {
      dismissed = true;
      hide();
    });
    head.appendChild(title);
    head.appendChild(close);

    listEl = document.createElement("div");
    Object.assign(listEl.style, { maxHeight: "40vh", overflowY: "auto" });

    panel.appendChild(head);
    panel.appendChild(listEl);
    document.documentElement.appendChild(panel);
  }

  function hide() {
    if (panel) panel.style.display = "none";
  }

  function flash(row, text) {
    const old = row.textContent;
    row.textContent = text;
    setTimeout(() => {
      row.textContent = old;
    }, 1200);
  }

  function render() {
    if (dismissed || !items.length) {
      hide();
      return;
    }
    ensurePanel();
    panel.style.display = "block";
    listEl.innerHTML = "";
    items.forEach((it, idx) => {
      const row = document.createElement("div");
      Object.assign(row.style, {
        padding: "8px 10px",
        borderTop: "1px solid #2e4339",
        cursor: "pointer",
        whiteSpace: "nowrap",
        overflow: "hidden",
        textOverflow: "ellipsis",
      });
      row.textContent = `${idx + 1}. ${it.type} file${fmtSize(it.size)}`;
      row.title = it.url;
      row.addEventListener("mouseenter", () => (row.style.background = "#2e4339"));
      row.addEventListener("mouseleave", () => (row.style.background = ""));
      row.addEventListener("click", () => {
        chrome.runtime.sendMessage(
          { type: "adm-download", url: it.url, filename: it.filename },
          () => void chrome.runtime.lastError
        );
        flash(row, "✓ Dikirim ke ADM");
      });
      listEl.appendChild(row);
    });
  }

  chrome.runtime.onMessage.addListener((msg) => {
    if (msg && msg.type === "adm-media") {
      items = msg.items || [];
      render();
    }
  });

  // Minta daftar terkini (mis. bila service worker sudah mendeteksi sebelumnya).
  chrome.runtime.sendMessage({ type: "adm-get-media" }, () => void chrome.runtime.lastError);
})();
