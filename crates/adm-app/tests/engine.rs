//! Test WM2: engine in-process menerima `download.add` (lewat EngineHandle)
//! dan menyelesaikan unduhan, dengan event dialirkan ke sink. Server lokal
//! tiny_http (Range + ETag); tanpa GUI.

use adm_app::engine::{EngineEvent, EngineHandle, EventSink};
use adm_ipc::DownloadAddParams;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tiny_http::{Header, Response, Server, StatusCode};

fn make_payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i as u64).wrapping_mul(2_654_435_761) & 0xff) as u8).collect()
}

fn sha(d: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(d);
    format!("{:x}", h.finalize())
}

struct Guard {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn start_server(payload: Arc<Vec<u8>>) -> (String, Guard) {
    let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for _ in 0..6 {
        let server = server.clone();
        let payload = payload.clone();
        let stop = stop.clone();
        threads.push(std::thread::spawn(move || loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match server.recv_timeout(Duration::from_millis(100)) {
                Ok(Some(req)) => handle(req, &payload),
                Ok(None) => continue,
                Err(_) => break,
            }
        }));
    }
    (base, Guard { stop, threads })
}

fn handle(req: tiny_http::Request, payload: &[u8]) {
    let total = payload.len();
    let range = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string());
    let named = req.url().contains("named"); // → kirim Content-Disposition
    let etag = Header::from_bytes(&b"ETag"[..], &b"\"v1\""[..]).unwrap();

    let mut resp = match range.as_deref().and_then(parse_range) {
        Some((a, b)) => {
            let b = b.unwrap_or(total as u64 - 1).min(total as u64 - 1);
            let a = a.min(total as u64 - 1);
            let slice = payload[a as usize..=b as usize].to_vec();
            let cr = format!("bytes {}-{}/{}", a, b, total);
            Response::from_data(slice)
                .with_status_code(StatusCode(206))
                .with_header(Header::from_bytes(&b"Content-Range"[..], cr.as_bytes()).unwrap())
                .with_header(Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap())
                .with_header(etag)
        }
        None => Response::from_data(payload.to_vec())
            .with_header(Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap())
            .with_header(etag),
    };
    if named {
        resp = resp.with_header(
            Header::from_bytes(
                &b"Content-Disposition"[..],
                &b"attachment; filename=\"real-name.rar\""[..],
            )
            .unwrap(),
        );
    }
    let _ = req.respond(resp);
}

fn parse_range(v: &str) -> Option<(u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes=")?;
    let (a, b) = rest.split_once('-')?;
    let a = a.trim().parse().ok()?;
    let b = if b.trim().is_empty() { None } else { Some(b.trim().parse().ok()?) };
    Some((a, b))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_downloads_in_process() {
    let payload = Arc::new(make_payload(1024 * 1024)); // 1 MiB
    let (base, _srv) = start_server(payload.clone());
    let dir = tempfile::tempdir().unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();
    let sink: EventSink = Arc::new(move |ev| {
        let _ = tx.send(ev);
    });

    let engine = EngineHandle::new(
        tokio::runtime::Handle::current(),
        dir.path().to_path_buf(),
        sink,
    );

    let id = engine.add(DownloadAddParams {
        url: format!("{base}/f.bin"),
        filename: Some("f.bin".into()),
        ..Default::default()
    });
    assert_eq!(engine.active_count(), 1);

    // Tunggu event Completed (atau gagal).
    let mut done = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            EngineEvent::Completed { id: cid, bytes } => {
                assert_eq!(cid, id);
                assert_eq!(bytes as usize, payload.len());
                done = true;
                break;
            }
            EngineEvent::Failed { error, .. } => panic!("unduhan gagal: {error}"),
            _ => {}
        }
    }
    assert!(done, "harus menerima event Completed");

    let mut got = Vec::new();
    std::fs::File::open(dir.path().join("f.bin"))
        .unwrap()
        .read_to_end(&mut got)
        .unwrap();
    assert_eq!(sha(&got), sha(&payload), "checksum harus cocok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routes_by_category() {
    let payload = Arc::new(make_payload(256 * 1024));
    let (base, _srv) = start_server(payload.clone());
    let dir = tempfile::tempdir().unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();
    let sink: EventSink = Arc::new(move |ev| {
        let _ = tx.send(ev);
    });
    let engine = EngineHandle::new(tokio::runtime::Handle::current(), dir.path().to_path_buf(), sink);

    // .zip → harus masuk subfolder Compressed (plan §10).
    let id = engine.add(DownloadAddParams {
        url: format!("{base}/pkg.zip"),
        filename: Some("pkg.zip".into()),
        ..Default::default()
    });

    while let Some(ev) = rx.recv().await {
        match ev {
            EngineEvent::Completed { id: cid, .. } if cid == id => break,
            EngineEvent::Failed { error, .. } => panic!("gagal: {error}"),
            _ => {}
        }
    }

    let expected = dir.path().join("Compressed").join("pkg.zip");
    assert!(expected.exists(), "berkas .zip harus di subfolder Compressed: {}", expected.display());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_respects_max() {
    let payload = Arc::new(make_payload(128 * 1024));
    let (base, _srv) = start_server(payload.clone());
    let dir = tempfile::tempdir().unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();
    let sink: EventSink = Arc::new(move |ev| {
        let _ = tx.send(ev);
    });
    let engine = EngineHandle::new(tokio::runtime::Handle::current(), dir.path().to_path_buf(), sink);
    engine.set_queue_max(1); // strictly sequential

    for i in 0..3 {
        engine.enqueue(DownloadAddParams {
            url: format!("{base}/f{i}.bin"),
            filename: Some(format!("f{i}.bin")),
            ..Default::default()
        });
    }
    engine.start_queue();

    let mut active = 0i32;
    let mut peak = 0i32;
    let mut completed = 0;
    while completed < 3 {
        match rx.recv().await.unwrap() {
            EngineEvent::Started { .. } => {
                active += 1;
                peak = peak.max(active);
            }
            EngineEvent::Completed { .. } => {
                active -= 1;
                completed += 1;
            }
            EngineEvent::Failed { error, .. } => panic!("gagal: {error}"),
            _ => {}
        }
    }
    assert_eq!(completed, 3, "ketiga unduhan antrian harus selesai");
    assert!(peak <= 1, "konkuren melebihi batas (max=1): {peak}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn filename_from_content_disposition() {
    let payload = Arc::new(make_payload(64 * 1024));
    let (base, _srv) = start_server(payload.clone());
    let dir = tempfile::tempdir().unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();
    let sink: EventSink = Arc::new(move |ev| {
        let _ = tx.send(ev);
    });
    let engine = EngineHandle::new(tokio::runtime::Handle::current(), dir.path().to_path_buf(), sink);

    // URL tanpa nama berguna; server mengirim Content-Disposition real-name.rar.
    let id = engine.add(DownloadAddParams {
        url: format!("{base}/named?x=1"),
        filename: None,
        ..Default::default()
    });

    while let Some(ev) = rx.recv().await {
        match ev {
            EngineEvent::Completed { id: cid, .. } if cid == id => break,
            EngineEvent::Failed { error, .. } => panic!("gagal: {error}"),
            _ => {}
        }
    }

    // .rar → kategori Compressed.
    let expected = dir.path().join("Compressed").join("real-name.rar");
    assert!(expected.exists(), "nama dari Content-Disposition harus dipakai: {}", expected.display());
}
