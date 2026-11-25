use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Deserialize;
use std::{
    ffi::OsString,
    io::{Read, Write},
    net::SocketAddr,
    sync::mpsc,
    time::Duration,
};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

const SERVICE_NAME: &str = "Katteguchi";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;

define_windows_service!(ffi_service_main, my_service_main);

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMsg {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

struct ServerConfig {
    host: String,
    port: u16,
    console: bool,
}

/// コマンドライン引数をパース（--port / -p, --host, --console）
fn parse_args<I, S>(args: I) -> ServerConfig
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut host = DEFAULT_HOST.to_string();
    let mut port = DEFAULT_PORT;
    let mut console = false;

    let mut iter = args.into_iter().map(|s| s.into());
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--console" => console = true,
            "--port" | "-p" => {
                if let Some(val) = iter.next() {
                    if let Ok(p) = val.parse::<u16>() {
                        port = p;
                    }
                }
            }
            "--host" => {
                if let Some(val) = iter.next() {
                    host = val;
                }
            }
            _ => {
                // `--port=9000` 形式にも対応
                if let Some(val) = arg.strip_prefix("--port=") {
                    if let Ok(p) = val.parse::<u16>() {
                        port = p;
                    }
                } else if let Some(val) = arg.strip_prefix("--host=") {
                    host = val.to_string();
                }
            }
        }
    }

    ServerConfig {
        host,
        port,
        console,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 実行時のコマンドライン引数を解析（1番目の実行ファイルパスはスキップ）
    let config = parse_args(std::env::args().skip(1));

    if config.console {
        println!("Starting Web Terminal on http://{}:{}", config.host, config.port);
        let (_tx, rx) = oneshot::channel();
        run_web_server(config, rx);
        return Ok(());
    }

    // Windowsサービスとして起動
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn my_service_main(arguments: Vec<OsString>) {
    // SCMから渡された引数、またはプロセス起動引数の両方に対応
    let mut config = parse_args(std::env::args().skip(1));
    if arguments.len() > 1 {
        let scm_args = arguments.into_iter().skip(1).map(|s| s.to_string_lossy().into_owned());
        config = parse_args(scm_args);
    }

    let _ = run_service(config);
}

fn run_service(config: ServerConfig) -> windows_service::Result<()> {
    let (shutdown_tx, shutdown_rx) = mpsc::channel();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    let (web_shutdown_tx, web_shutdown_rx) = oneshot::channel();

    let server_handle = std::thread::spawn(move || {
        run_web_server(config, web_shutdown_rx);
    });

    let _ = shutdown_rx.recv();
    let _ = web_shutdown_tx.send(());
    let _ = server_handle.join();

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

fn run_web_server(config: ServerConfig, shutdown_rx: oneshot::Receiver<()>) {
    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    rt.block_on(async move {
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/ws", get(ws_handler));

        let addr: SocketAddr = format!("{}:{}", config.host, config.port)
            .parse()
            .expect("Invalid host or port");

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("Failed to bind port");

        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });
}

async fn index_handler() -> impl IntoResponse {
    Html(include_str!("index.html"))
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(socket: WebSocket) {
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => return,
    };

    let mut cmd = CommandBuilder::new("cmd.exe");
    cmd.cwd("C:\\Windows\\System32");

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(_) => return,
    };
    drop(pair.slave);

    let mut pty_reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut pty_writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(_) => return,
    };

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (out_tx, mut out_rx) = tokio_mpsc::channel::<Vec<u8>>(64);

    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let send_task = tokio::spawn(async move {
        while let Some(data) = out_rx.recv().await {
            if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = ws_receiver.next().await {
        if let Message::Text(text) = msg {
            if let Ok(parsed) = serde_json::from_str::<ClientMsg>(&text) {
                match parsed {
                    ClientMsg::Input { data } => {
                        let _ = pty_writer.write_all(data.as_bytes());
                        let _ = pty_writer.flush();
                    }
                    ClientMsg::Resize { cols, rows } => {
                        let _ = pair.master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
            }
        }
    }

    send_task.abort();
    let _ = child.kill();
}