//! Named Pipe server — jalur bridge -> app (plan §6). WM2: `download.add`
//! memanggil engine in-process; `app.activate` memunculkan jendela.

use crate::engine::EngineHandle;
use crate::state;
use adm_ipc::{method, DownloadAddParams, Request, Response, METHOD_NOT_FOUND, PIPE_NAME};
use serde_json::json;
use tokio::io::BufReader;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

pub async fn serve(engine: EngineHandle) -> std::io::Result<()> {
    eprintln!("[ipc] listen di {PIPE_NAME}");
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(PIPE_NAME)?;

    loop {
        // Error transien (klien mati saat handshake, dsb.) tidak boleh
        // mematikan server selamanya: buat ulang instance pipe & lanjut.
        if let Err(e) = server.connect().await {
            eprintln!("[ipc] accept gagal: {e}");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            server = recreate_pipe().await;
            continue;
        }
        let connected = server;
        server = recreate_pipe().await;

        let engine = engine.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(connected, engine).await {
                eprintln!("[ipc] koneksi berakhir: {e}");
            }
        });
    }
}

/// Buat instance pipe berikutnya; coba ulang dengan backoff bila gagal
/// (mis. kehabisan handle sesaat) — jangan pernah menyerah.
async fn recreate_pipe() -> NamedPipeServer {
    let mut delay = std::time::Duration::from_millis(100);
    loop {
        match ServerOptions::new().create(PIPE_NAME) {
            Ok(s) => return s,
            Err(e) => {
                eprintln!("[ipc] buat pipe gagal: {e}");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(5));
            }
        }
    }
}

async fn handle_conn(conn: NamedPipeServer, engine: EngineHandle) -> std::io::Result<()> {
    let mut reader = BufReader::new(conn);
    while let Some(bytes) = adm_ipc::read_message(&mut reader).await? {
        let resp = match serde_json::from_slice::<Request>(&bytes) {
            Ok(req) => dispatch(req, &engine),
            Err(e) => Response::err(None, adm_ipc::INTERNAL_ERROR, format!("parse: {e}")),
        };
        adm_ipc::write_message(reader.get_mut(), &resp).await?;
    }
    Ok(())
}

fn dispatch(req: Request, engine: &EngineHandle) -> Response {
    match req.method.as_str() {
        method::PING => Response::ok(
            req.id,
            json!({
                "pong": true,
                "app": "adm-app",
                "version": env!("CARGO_PKG_VERSION"),
                "engine": adm_core::version(),
                "active": engine.active_count(),
            }),
        ),
        method::DOWNLOAD_ADD => match req.params {
            Some(p) => match serde_json::from_value::<DownloadAddParams>(p) {
                Ok(params) if !params.url.is_empty() => {
                    // Jangan langsung mulai: serahkan ke UI untuk dialog
                    // "Download File Info" (user yang memutuskan mulai/queue).
                    crate::gui::request_add(params);
                    Response::ok(req.id, json!({ "accepted": true }))
                }
                Ok(_) => Response::err(req.id, adm_ipc::INVALID_PARAMS, "url kosong"),
                Err(e) => Response::err(req.id, adm_ipc::INVALID_PARAMS, format!("params: {e}")),
            },
            None => Response::err(req.id, adm_ipc::INVALID_PARAMS, "params wajib"),
        },
        method::APP_ACTIVATE => {
            state::post_to_ui(state::WM_ACTIVATE_APP);
            Response::ok(req.id, json!({ "ok": true }))
        }
        other => Response::err(req.id, METHOD_NOT_FOUND, format!("metode tidak dikenal: {other}")),
    }
}
