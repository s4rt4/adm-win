// Service worker (MV3): tangkap unduhan → kirim ke ADM via native messaging.
// Plan §11.1. Host: com.adm.bridge.

const HOST = "com.adm.bridge";

// Anti-duplikat: lewati URL yang sama yang baru dikirim < 5 detik lalu
// (onCreated bisa terpicu lebih dari sekali untuk satu unduhan).
// URL ditandai hanya SETELAH kirim ke bridge sukses, agar kegagalan
// transien tidak menelan percobaan ulang < 5 detik berikutnya.
const recentlySent = new Map();
function isDuplicate(url) {
  const now = Date.now();
  for (const [u, t] of recentlySent) if (now - t > 5000) recentlySent.delete(u);
  return recentlySent.has(url) && now - recentlySent.get(url) < 5000;
}
function markSent(url) {
  recentlySent.set(url, Date.now());
}

// `enabled` dibaca async dari storage; service worker MV3 bisa bangun dan
// menerima onCreated SEBELUM get() resolve — tunggu enabledReady dulu agar
// toggle OFF tidak diabaikan pada event pertama.
let enabled = true;
const enabledReady = chrome.storage.local
  .get({ enabled: true })
  .then((v) => { enabled = v.enabled; })
  .catch(() => {});
chrome.storage.onChanged.addListener((changes) => {
  if (changes.enabled) enabled = changes.enabled.newValue;
});

chrome.runtime.onInstalled.addListener(() => {
  // removeAll dulu: saat ekstensi di-update, create id yang sama melempar
  // "duplicate id".
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: "adm-download",
      title: "Download with ADM",
      contexts: ["link"],
    });
  });
});

// Klik kanan pada link → kirim ke ADM (selalu, walau toggle off).
chrome.contextMenus.onClicked.addListener((info) => {
  const url = info.linkUrl || info.srcUrl;
  if (url && !isDuplicate(url)) sendToAdm(url, undefined, info.pageUrl);
});

// URL yang baru saja di-POST (unduhan hasil form/XHR POST tidak boleh
// dicegat: ADM mengulanginya sebagai GET → konten salah). TTL 30 detik.
const recentPost = new Map();
chrome.webRequest.onBeforeRequest.addListener(
  (d) => {
    if (d.method !== "POST") return;
    const now = Date.now();
    for (const [u, t] of recentPost) if (now - t > 30000) recentPost.delete(u);
    recentPost.set(d.url, now);
  },
  { urls: ["<all_urls>"] }
);
function isRecentPost(url) {
  const t = recentPost.get(url);
  return t !== undefined && Date.now() - t < 30000;
}

// Hint nama file dari onDeterminingFilename (sudah termasuk Content-Disposition
// yang di-parse browser) — onCreated sering belum punya filename.
const nameWaiters = new Map(); // downloadId -> resolve(filename)
chrome.downloads.onDeterminingFilename.addListener((item) => {
  const w = nameWaiters.get(item.id);
  if (w) {
    nameWaiters.delete(item.id);
    w(item.filename);
  }
});
function filenameHint(downloadId, ms) {
  return new Promise((resolve) => {
    const t = setTimeout(() => {
      nameWaiters.delete(downloadId);
      resolve(undefined);
    }, ms);
    nameWaiters.set(downloadId, (name) => {
      clearTimeout(t);
      resolve(name);
    });
  });
}

async function downloadState(downloadId) {
  try {
    const items = await chrome.downloads.search({ id: downloadId });
    return items && items[0] ? items[0].state : undefined;
  } catch (e) {
    return undefined;
  }
}

// Tangkap unduhan baru: KIRIM ke ADM dulu; batalkan di browser hanya setelah
// bridge mengonfirmasi sukses. Bila gagal, unduhan browser dilanjutkan —
// jangan pernah cancel+erase sebelum ADM menerima (download bisa hilang total).
chrome.downloads.onCreated.addListener(async (item) => {
  await enabledReady;
  if (!enabled) return;
  const url = item.finalUrl || item.url;
  if (!url || !/^https?:/i.test(url)) return;
  if (isRecentPost(url)) return; // hasil POST → biarkan browser
  if (isDuplicate(url)) {
    // Sudah dikirim ke ADM < 5 dtk lalu → ini duplikat event browser; buang.
    try {
      await chrome.downloads.cancel(item.id);
      await chrome.downloads.erase({ id: item.id });
    } catch (e) {
      /* abaikan */
    }
    return;
  }
  // Tahan (pause) selama menunggu ADM, bukan cancel — masih bisa dilanjutkan.
  let paused = true;
  try {
    await chrome.downloads.pause(item.id);
  } catch (e) {
    paused = false; // mungkin sudah selesai (file kecil) / tak bisa di-pause
  }
  let filename = item.filename ? item.filename.split(/[\\/]/).pop() : undefined;
  if (!filename) {
    const hinted = await filenameHint(item.id, 1500);
    if (hinted) filename = hinted.split(/[\\/]/).pop() || undefined;
  }
  // File kecil bisa keburu selesai — cancel/erase pada unduhan complete hanya
  // menghapus riwayat, file tetap ada → jadi dobel dengan ADM. Biarkan browser.
  if ((await downloadState(item.id)) === "complete") return;
  const ok = await sendToAdm(url, filename, item.referrer);
  if (ok) {
    try {
      await chrome.downloads.cancel(item.id);
      await chrome.downloads.erase({ id: item.id });
    } catch (e) {
      /* sudah selesai/tak bisa dibatalkan — abaikan */
    }
  } else if (paused) {
    try {
      await chrome.downloads.resume(item.id);
    } catch (e) {
      /* abaikan */
    }
  }
});

// Kumpulkan Cookie header untuk URL agar unduhan ber-autentikasi (mis. lampiran
// Gmail) bisa diunduh ADM. Mengembalikan string "k=v; k2=v2" atau "".
async function cookieHeaderFor(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    if (cookies && cookies.length) {
      return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
    }
  } catch (e) {
    /* tak ada izin / gagal — abaikan, unduh tanpa cookie */
  }
  return "";
}

// Kirim ke ADM via bridge; resolve true hanya bila bridge menjawab {ok:true}.
// URL ditandai recentlySent hanya saat sukses agar retry cepat tak tertelan.
async function sendToAdm(url, filename, referrer) {
  const msg = { method: "download.add", url, userAgent: navigator.userAgent };
  if (filename) msg.filename = filename;
  if (referrer) msg.referrer = referrer;
  const cookie = await cookieHeaderFor(url);
  if (cookie) msg.cookies = cookie;
  return new Promise((resolve) => {
    chrome.runtime.sendNativeMessage(HOST, msg, (resp) => {
      if (chrome.runtime.lastError) {
        console.warn("ADM bridge:", chrome.runtime.lastError.message);
        resolve(false);
        return;
      }
      const ok = !!(resp && resp.ok);
      if (ok) markSent(url);
      else console.warn("ADM bridge menolak:", resp && resp.error);
      resolve(ok);
    });
  });
}

// ===== Fase 1: deteksi video/audio progresif yang sedang diputar =====
// Pantau respons jaringan; bila bertipe media progresif (mp4/webm/flv/mp3 dst),
// catat per-tab & tampilkan panel "Download with ADM" via content script.
const MEDIA_EXT = /\.(mp4|webm|flv|m4v|mov|mkv|mp3|m4a|aac|ogg|wav|3gp)(\?|$)/i;
const MEDIA_CT = /^(video|audio)\//i;
const MANIFEST_CT = /(mpegurl|dash\+xml)/i; // HLS/DASH = Fase 2, dilewati
const MIN_SIZE = 200 * 1024; // lewati klip kecil/iklan

// tabId -> Map(url -> {url, type, size, filename})
const mediaByTab = new Map();

function headerVal(headers, name) {
  const h = (headers || []).find((x) => x.name.toLowerCase() === name);
  return h ? h.value : undefined;
}

function extFromCt(ct) {
  if (!ct) return null;
  if (ct.includes("mp4")) return "mp4";
  if (ct.includes("webm")) return "webm";
  if (ct.includes("flv")) return "flv";
  if (ct.includes("audio/mpeg")) return "mp3";
  if (ct.includes("mpeg")) return "mpg"; // video/mpeg, bukan mp3
  if (ct.includes("3gpp")) return "3gp";
  if (ct.includes("ogg")) return "ogg";
  return null;
}

function guessName(url, ct) {
  try {
    const base = decodeURIComponent(new URL(url).pathname.split("/").pop() || "");
    if (base && /\.[a-z0-9]{2,4}$/i.test(base)) return base;
  } catch (e) {
    /* abaikan */
  }
  return `video.${extFromCt(ct) || "mp4"}`;
}

function containerOf(url, ct) {
  const m = MEDIA_EXT.exec(url);
  if (m) return m[1].toUpperCase();
  return (extFromCt(ct) || "MEDIA").toUpperCase();
}

// Mirror daftar media per-tab ke storage.session agar bertahan bila service
// worker MV3 di-evict (Map in-memory hilang saat SW mati).
function persistTab(tabId) {
  const m = mediaByTab.get(tabId);
  const items = m ? [...m.values()] : [];
  chrome.storage.session.set({ ["m" + tabId]: items }).catch(() => {});
}

function pushToTab(tabId) {
  const m = mediaByTab.get(tabId);
  const items = m ? [...m.values()] : [];
  chrome.tabs.sendMessage(tabId, { type: "adm-media", items }, () => void chrome.runtime.lastError);
}

function clearTab(tabId) {
  mediaByTab.delete(tabId);
  chrome.storage.session.remove("m" + tabId).catch(() => {});
}

function addMedia(tabId, item) {
  let m = mediaByTab.get(tabId);
  if (!m) {
    m = new Map();
    mediaByTab.set(tabId, m);
  }
  if (m.has(item.url)) return;
  m.set(item.url, item);
  persistTab(tabId);
  pushToTab(tabId);
}

chrome.webRequest.onHeadersReceived.addListener(
  (d) => {
    if (!enabled || d.tabId < 0) return;
    const ct = (headerVal(d.responseHeaders, "content-type") || "").toLowerCase();
    if (MANIFEST_CT.test(ct)) return; // streaming adaptif → belum didukung
    const isMedia = (MEDIA_CT.test(ct) && !ct.includes("mp2t")) || MEDIA_EXT.test(d.url);
    if (!isMedia) return;
    let size = parseInt(headerVal(d.responseHeaders, "content-length") || "0", 10) || 0;
    const cr = headerVal(d.responseHeaders, "content-range"); // total dari 206
    if (cr) {
      const mm = /\/(\d+)\s*$/.exec(cr);
      if (mm) size = parseInt(mm[1], 10);
    }
    if (size && size < MIN_SIZE) return;
    addMedia(d.tabId, {
      url: d.url,
      type: containerOf(d.url, ct),
      size,
      filename: guessName(d.url, ct),
    });
  },
  { urls: ["<all_urls>"] },
  ["responseHeaders"]
);

// Bersihkan daftar saat tab navigasi/ditutup.
chrome.tabs.onUpdated.addListener((tabId, info) => {
  if (info.status === "loading") clearTab(tabId);
});
chrome.tabs.onRemoved.addListener((tabId) => clearTab(tabId));

// Pesan dari content script: minta daftar / minta unduh item terpilih.
chrome.runtime.onMessage.addListener((msg, sender) => {
  if (!msg || !sender.tab) return;
  if (msg.type === "adm-get-media") {
    const tabId = sender.tab.id;
    if (mediaByTab.has(tabId)) {
      pushToTab(tabId);
    } else {
      // SW mungkin baru bangun → pulihkan dari storage.session.
      chrome.storage.session
        .get("m" + tabId)
        .then((res) => {
          const items = res["m" + tabId] || [];
          if (items.length) {
            mediaByTab.set(tabId, new Map(items.map((it) => [it.url, it])));
          }
          chrome.tabs.sendMessage(tabId, { type: "adm-media", items }, () => void chrome.runtime.lastError);
        })
        .catch(() => {});
    }
  } else if (msg.type === "adm-download" && msg.url) {
    if (!isDuplicate(msg.url)) sendToAdm(msg.url, msg.filename, sender.tab.url);
  }
});
