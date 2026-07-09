//! adm-bridge — native messaging host (plan §11.2).
//!
//! Mode:
//!   (default)            : loop stdio native-messaging (dipanggil browser).
//!   ping                 : uji koneksi ke adm-app.
//!   add <url>            : kirim download.add (uji).
//!   register <chrome-id> [firefox-id] : tulis manifest + registry host.
//!   unregister           : hapus registry host.
//!
//! Protokol stdio: 4-byte length LE + JSON UTF-8 (Chrome/Edge/Firefox).
//! Pesan dari extension: {"method":"download.add","url":..,"filename":..}.

use adm_ipc::{method, Request, Response, DownloadAddParams, PIPE_NAME};
use std::io::{Read, Write};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::windows::named_pipe::ClientOptions;

const HOST_NAME: &str = "com.adm.bridge";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("ping") => cli_ping(),
        Some("add") => cli_add(&args),
        Some("register") => register(&args),
        Some("unregister") => unregister(),
        // Browser meluncurkan host dengan arg path-manifest/origin → mode stdio.
        _ => run_host(),
    }
}

// ---------------- Native messaging stdio host ----------------

fn run_host() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    loop {
        let mut len_buf = [0u8; 4];
        if lock.read_exact(&mut len_buf).is_err() {
            break; // EOF → browser menutup
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > 4 * 1024 * 1024 {
            break;
        }
        let mut buf = vec![0u8; len];
        if lock.read_exact(&mut buf).is_err() {
            break;
        }
        let msg: serde_json::Value = serde_json::from_slice(&buf).unwrap_or_default();
        let resp = rt.block_on(handle(msg));
        if write_message(&resp).is_err() {
            break;
        }
    }
}

async fn handle(msg: serde_json::Value) -> serde_json::Value {
    let url = msg.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if url.is_empty() {
        return serde_json::json!({ "ok": false, "error": "url kosong" });
    }
    let params = DownloadAddParams {
        url,
        filename: msg.get("filename").and_then(|v| v.as_str()).map(String::from),
        referrer: msg.get("referrer").and_then(|v| v.as_str()).map(String::from),
        user_agent: msg.get("userAgent").and_then(|v| v.as_str()).map(String::from),
        cookies: msg.get("cookies").and_then(|v| v.as_str()).map(String::from),
        ..Default::default()
    };

    if !ensure_app().await {
        return serde_json::json!({ "ok": false, "error": "adm-app tak bisa dijalankan" });
    }

    match request(method::DOWNLOAD_ADD, Some(serde_json::to_value(&params).unwrap())).await {
        Ok(resp) => serde_json::json!({ "ok": resp.error.is_none(), "result": resp.result }),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    }
}

fn write_message(value: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut out = std::io::stdout().lock();
    out.write_all(&(body.len() as u32).to_le_bytes())?;
    out.write_all(&body)?;
    out.flush()
}

/// Pastikan adm-app hidup; jika tidak, spawn `adm-app --tray` lalu tunggu.
async fn ensure_app() -> bool {
    if request(method::PING, None).await.is_ok() {
        return true;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            use std::os::windows::process::CommandExt;
            use std::process::Stdio;
            // DETACHED_PROCESS | CREATE_NO_WINDOW: jangan wariskan pipe
            // native-messaging bridge ke adm-app, dan jangan munculkan console.
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let app = dir.join("adm-app.exe");
            let _ = std::process::Command::new(app)
                .arg("--tray")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
                .spawn();
        }
    }
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if request(method::PING, None).await.is_ok() {
            return true;
        }
    }
    false
}

// ---------------- Pipe client ----------------

async fn request(
    method: &str,
    params: Option<serde_json::Value>,
) -> std::io::Result<Response> {
    let client = ClientOptions::new().open(PIPE_NAME)?;
    let mut reader = BufReader::new(client);
    let req = Request::new(1, method, params);
    adm_ipc::write_message(reader.get_mut(), &req).await?;
    match adm_ipc::read_message(&mut reader).await? {
        Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "pipe ditutup sebelum balasan",
        )),
    }
}

// ---------------- CLI helpers ----------------

fn cli_ping() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    match rt.block_on(request(method::PING, None)) {
        Ok(resp) => println!("[bridge] ping OK: {}", serde_json::to_string(&resp).unwrap()),
        Err(e) => {
            eprintln!("[bridge] ping GAGAL: {e}");
            std::process::exit(1);
        }
    }
}

fn cli_add(args: &[String]) {
    let Some(url) = args.get(1) else {
        eprintln!("usage: adm-bridge add <url>");
        std::process::exit(2);
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let params = serde_json::json!({ "url": url });
    match rt.block_on(request(method::DOWNLOAD_ADD, Some(params))) {
        Ok(resp) => println!("[bridge] add OK: {}", serde_json::to_string(&resp).unwrap()),
        Err(e) => {
            eprintln!("[bridge] add GAGAL: {e}");
            std::process::exit(1);
        }
    }
}

// ---------------- Registrasi native messaging host (§11.2) ----------------

fn register(args: &[String]) {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().expect("dir").to_path_buf();
    let chrome_id = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("usage: adm-bridge register <chrome/edge-extension-id> [firefox-extension-id]");
        std::process::exit(2);
    });
    // ID Chrome/Edge selalu 32 huruf a-p; tolak selain itu (typo/salah tempel
    // menghasilkan manifest yang diam-diam tak pernah cocok).
    if chrome_id.len() != 32 || !chrome_id.bytes().all(|b| (b'a'..=b'p').contains(&b)) {
        eprintln!("[bridge] extension id tidak valid: {chrome_id} (harus 32 huruf a-p)");
        std::process::exit(2);
    }
    let firefox_id = args.get(2).cloned();

    // Manifest dibangun via serde_json agar path/ID ter-escape benar
    // (backslash, kutip, dsb.) — bukan format string manual.
    let manifest_json = |origins_key: &str, origins_val: serde_json::Value| {
        let v = serde_json::json!({
            "name": HOST_NAME,
            "description": "Alpha Download Manager host",
            "path": exe.to_string_lossy(),
            "type": "stdio",
            origins_key: origins_val,
        });
        let mut body = serde_json::to_vec_pretty(&v).expect("serialisasi manifest");
        body.push(b'\n');
        body
    };

    // Manifest Chrome/Edge (allowed_origins).
    let chrome_manifest = dir.join("com.adm.bridge.json");
    let chrome_json = manifest_json(
        "allowed_origins",
        serde_json::json!([format!("chrome-extension://{chrome_id}/")]),
    );
    std::fs::write(&chrome_manifest, chrome_json).expect("tulis manifest chrome");
    reg_add("HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts", &chrome_manifest);
    reg_add("HKCU\\Software\\Microsoft\\Edge\\NativeMessagingHosts", &chrome_manifest);

    // Manifest Firefox (allowed_extensions).
    if let Some(fid) = firefox_id {
        let ff_manifest = dir.join("com.adm.bridge.firefox.json");
        let ff_json = manifest_json("allowed_extensions", serde_json::json!([fid]));
        std::fs::write(&ff_manifest, ff_json).expect("tulis manifest firefox");
        reg_add("HKCU\\Software\\Mozilla\\NativeMessagingHosts", &ff_manifest);
    }

    println!("[bridge] terdaftar. manifest: {}", chrome_manifest.display());
}

fn unregister() {
    for base in [
        "HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts",
        "HKCU\\Software\\Microsoft\\Edge\\NativeMessagingHosts",
        "HKCU\\Software\\Mozilla\\NativeMessagingHosts",
    ] {
        let key = format!("{base}\\{HOST_NAME}");
        let _ = std::process::Command::new("reg")
            .args(["delete", &key, "/f"])
            .status();
    }
    println!("[bridge] registry host dihapus.");
}

fn reg_add(base: &str, manifest: &std::path::Path) {
    let key = format!("{base}\\{HOST_NAME}");
    let status = std::process::Command::new("reg")
        .args([
            "add",
            &key,
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            &manifest.to_string_lossy(),
            "/f",
        ])
        .status();
    match status {
        Ok(s) if s.success() => println!("  + {key}"),
        _ => eprintln!("  ! gagal menulis {key}"),
    }
}
