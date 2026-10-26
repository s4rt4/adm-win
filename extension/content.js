// Content script (Fase 1): ikon melayang kecil + badge jumlah video/audio
// progresif yang terdeteksi. Klik ikon → dropdown daftar (nama asli video);
// klik item → kirim ke ADM. UI di Shadow DOM agar CSS halaman tak mengganggu.
(function () {
  if (window.__admVideoPanel) return;
  window.__admVideoPanel = true;

  let items = [];
  let dismissed = false;
  let open = false;
  let host = null;
  let btn = null;
  let badge = null;
  let menu = null;
  let listEl = null;

  const CSS = `
    :host { all: initial; }
    .wrap { position: fixed; top: 16px; right: 16px; z-index: 2147483647;
      font: 13px/1.4 'Segoe UI', system-ui, sans-serif; color: #e6ece8; }
    .btn { position: relative; width: 36px; height: 36px; border-radius: 50%;
      background: #1f2d27; border: 1px solid #2e4339; cursor: pointer;
      display: flex; align-items: center; justify-content: center;
      box-shadow: 0 2px 8px rgba(0,0,0,.35); opacity: .85; transition: opacity .15s; }
    .btn:hover, .btn.on { opacity: 1; }
    .btn svg { width: 18px; height: 18px; fill: none; stroke: #7fd1a8;
      stroke-width: 2.2; stroke-linecap: round; stroke-linejoin: round; }
    .badge { position: absolute; top: -5px; right: -5px; min-width: 18px; height: 18px;
      padding: 0 4px; box-sizing: border-box; border-radius: 9px; background: #e5484d;
      color: #fff; font: 600 11px/18px 'Segoe UI', system-ui, sans-serif; text-align: center; }
    .menu { position: absolute; top: 44px; right: 0; width: 320px; max-width: calc(100vw - 32px);
      background: #1f2d27; border: 1px solid #2e4339; border-radius: 8px;
      box-shadow: 0 4px 16px rgba(0,0,0,.35); overflow: hidden; display: none; }
    .menu.on { display: block; }
    .head { display: flex; align-items: center; justify-content: space-between;
      padding: 8px 10px; background: #243430; font-weight: 600; }
    .x { cursor: pointer; opacity: .7; padding-left: 8px; }
    .x:hover { opacity: 1; }
    .list { max-height: 50vh; overflow-y: auto; }
    .row { padding: 7px 10px; border-top: 1px solid #2e4339; cursor: pointer; }
    .row:hover { background: #2e4339; }
    .name, .meta { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .meta { font-size: 11px; opacity: .65; }
  `;

  function fmtSize(n) {
    if (!n) return "";
    const u = ["B", "KB", "MB", "GB"];
    let i = 0;
    let v = n;
    while (v >= 1024 && i < u.length - 1) {
      v /= 1024;
      i += 1;
    }
    return `${v.toFixed(v < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
  }

  // Nama file dari URL yang tak bermakna (video.mp4, hash/ID acak).
  function isGenericName(base) {
    const stem = base.replace(/\.[a-z0-9]{2,4}$/i, "");
    return (
      /^(video|audio|media|file|download|index|playback|stream|master|chunk|seg(ment)?)[-_\d]*$/i.test(stem) ||
      /^[0-9a-f_-]{12,}$/i.test(stem) ||
      /^\d+$/.test(stem)
    );
  }

  function pageTitle() {
    const og = document.querySelector('meta[property="og:title"], meta[name="twitter:title"]');
    const t = (og && og.content) || document.title || "";
    return t.trim();
  }

  // Elemen <video>/<audio> yang memutar URL ini (src/currentSrc/<source>).
  function mediaElementFor(url) {
    for (const el of document.querySelectorAll("video, audio")) {
      if (el.currentSrc === url || el.src === url) return el;
      for (const s of el.querySelectorAll("source")) if (s.src === url) return el;
    }
    return null;
  }

  // Nama tampilan (tanpa ekstensi): atribut elemen media → nama file URL yang
  // bermakna → judul halaman.
  function displayName(it) {
    const el = mediaElementFor(it.url);
    if (el) {
      const fig = el.closest("figure");
      const cap = fig && fig.querySelector("figcaption");
      const t = el.title || el.getAttribute("aria-label") || (cap && cap.textContent);
      if (t && t.trim()) return t.trim();
    }
    const base = it.filename || "";
    if (base && !isGenericName(base)) return base.replace(/\.[a-z0-9]{2,4}$/i, "");
    return pageTitle() || base || "Video";
  }

  function extOf(it) {
    const m = /\.([a-z0-9]{2,4})$/i.exec(it.filename || "");
    return m ? m[1].toLowerCase() : (it.type || "mp4").toLowerCase();
  }

  // Nama berkas aman untuk Windows.
  function safeFilename(name, ext) {
    const s = name.replace(/[\\/:*?"<>|\u0000-\u001f]+/g, " ").replace(/\s+/g, " ").trim().slice(0, 180);
    return `${s.replace(/[. ]+$/, "") || "video"}.${ext}`;
  }

  function ensureUi() {
    if (host) return;
    host = document.createElement("div");
    host.id = "adm-video-panel";
    const root = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = CSS;
    const wrap = document.createElement("div");
    wrap.className = "wrap";

    btn = document.createElement("div");
    btn.className = "btn";
    btn.title = "Download with ADM";
    btn.innerHTML =
      '<svg viewBox="0 0 24 24"><path d="M12 4v11"/><path d="M7 10l5 5 5-5"/><path d="M5 20h14"/></svg>';
    badge = document.createElement("span");
    badge.className = "badge";
    btn.appendChild(badge);
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      setOpen(!open);
    });

    menu = document.createElement("div");
    menu.className = "menu";
    const head = document.createElement("div");
    head.className = "head";
    const title = document.createElement("span");
    title.textContent = "Download with ADM";
    const close = document.createElement("span");
    close.className = "x";
    close.textContent = "✕";
    close.title = "Sembunyikan di halaman ini";
    close.addEventListener("click", () => {
      dismissed = true;
      hide();
    });
    head.appendChild(title);
    head.appendChild(close);
    listEl = document.createElement("div");
    listEl.className = "list";
    menu.appendChild(head);
    menu.appendChild(listEl);
    menu.addEventListener("click", (e) => e.stopPropagation());

    wrap.appendChild(btn);
    wrap.appendChild(menu);
    root.appendChild(style);
    root.appendChild(wrap);
    document.documentElement.appendChild(host);

    // Klik di luar / Esc → tutup dropdown.
    document.addEventListener("click", () => setOpen(false));
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") setOpen(false);
    });
  }

  function setOpen(v) {
    if (!host || open === v) return;
    open = v;
    menu.classList.toggle("on", open);
    btn.classList.toggle("on", open);
    if (open) renderList(); // nama dihitung ulang: judul/elemen bisa berubah
  }

  function hide() {
    if (!host) return;
    setOpen(false);
    host.style.display = "none";
  }

  function flash(row, text) {
    if (row.dataset.flash) return;
    row.dataset.flash = "1";
    const nameEl = row.firstChild;
    const old = nameEl.textContent;
    nameEl.textContent = text;
    setTimeout(() => {
      nameEl.textContent = old;
      delete row.dataset.flash;
    }, 1200);
  }

  function renderList() {
    listEl.innerHTML = "";
    const names = items.map(displayName);
    // Nama kembar (mis. semua jatuh ke judul halaman) → beri nomor.
    const count = {};
    names.forEach((n) => (count[n] = (count[n] || 0) + 1));
    const seen = {};
    items.forEach((it, idx) => {
      let name = names[idx];
      if (count[name] > 1) {
        seen[name] = (seen[name] || 0) + 1;
        name = `${name} (${seen[name]})`;
      }
      const row = document.createElement("div");
      row.className = "row";
      row.title = `${name}\n${it.url}`;
      const nameEl = document.createElement("div");
      nameEl.className = "name";
      nameEl.textContent = name;
      const meta = document.createElement("div");
      meta.className = "meta";
      meta.textContent = [it.type, fmtSize(it.size)].filter(Boolean).join(" · ");
      row.appendChild(nameEl);
      row.appendChild(meta);
      row.addEventListener("click", () => {
        chrome.runtime.sendMessage(
          { type: "adm-download", url: it.url, filename: safeFilename(name, extOf(it)) },
          () => void chrome.runtime.lastError
        );
        flash(row, "✓ Dikirim ke ADM");
      });
      listEl.appendChild(row);
    });
  }

  function render() {
    if (dismissed || !items.length) {
      hide();
      return;
    }
    ensureUi();
    host.style.display = "";
    badge.textContent = items.length > 99 ? "99+" : String(items.length);
    if (open) renderList();
  }

  chrome.runtime.onMessage.addListener((msg) => {
    if (msg && msg.type === "adm-media") {
      items = msg.items || [];
      render();
    }
  });

  function requestMedia() {
    chrome.runtime.sendMessage({ type: "adm-get-media" }, () => void chrome.runtime.lastError);
  }

  // Minta daftar terkini (mis. bila service worker sudah mendeteksi sebelumnya).
  requestMedia();
  // Saat tab kembali terlihat, minta lagi — service worker bisa sempat di-evict
  // (daftar jadi kosong) lalu dipulihkan dari storage.session.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") requestMedia();
  });
})();
