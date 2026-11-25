//! # system-shell（Webターミナル）
//!
//! ブラウザから Windows の `cmd.exe` を操作できる小さな Web サーバーです。
//!
//! 全体の流れ:
//! 1. ブラウザが `/` にアクセスすると、ターミナル画面（index.html）を返す
//! 2. 画面の JavaScript が `/ws` に WebSocket で接続する
//! 3. サーバーは接続ごとに「疑似端末（PTY）」を作り、その中で `cmd.exe` を起動する
//! 4. ブラウザで打ったキー → WebSocket → PTY（cmd.exe へ入力）
//!    cmd.exe の出力   → PTY → WebSocket → ブラウザに表示
//!
//! 起動方法は2通りあります:
//! - `--console` 付き: 普通のコマンドラインアプリとして起動（開発・動作確認向け）
//! - 引数なし:        Windows サービスとして起動（サービスマネージャー=SCM から起動される）
//!
//! ※ `//!` で始まるコメントは「このファイル（モジュール）全体の説明」を書くための
//!    ドキュメントコメントです。`///` は直後の関数や型の説明、`//` は普通のコメントです。

// ---------------------------------------------------------------------------
// use 宣言: 他のクレート（ライブラリ）や標準ライブラリの型・関数を
// 短い名前で使えるように取り込みます。Python の import に近いものです。
// `{ ... }` でまとめて複数を取り込めます。
// ---------------------------------------------------------------------------

// axum: Web サーバーを作るためのフレームワーク
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade}, // WebSocket 関連の型
    response::{Html, IntoResponse},                      // HTTP レスポンスを作るための型・トレイト
    routing::get,                                        // GET リクエスト用のルート登録関数
    Router,                                              // URL と処理（ハンドラ）を結び付ける仕組み
};
// futures_util: 非同期ストリームを扱う便利機能。
// SinkExt は `.send()`、StreamExt は `.next()` や `.split()` を使えるようにする「トレイト」です。
// （トレイト = 型に後から機能（メソッド）を追加するための仕組み。use しないとメソッドが見えません）
use futures_util::{SinkExt, StreamExt};
// portable_pty: 疑似端末（PTY）を作るライブラリ。cmd.exe を「本物の端末のように」動かせます。
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
// serde: JSON などのデータを Rust の型に変換（デシリアライズ）するためのライブラリ
use serde::Deserialize;
// 標準ライブラリ（std）から必要なものを取り込み
use std::{
    ffi::OsString,          // OS が扱う文字列型（UTF-8 とは限らない文字列）
    io::{Read, Write},      // `.read()` / `.write_all()` を使うためのトレイト
    net::SocketAddr,        // "127.0.0.1:8080" のような IP アドレス + ポート番号
    sync::mpsc,             // スレッド間でデータを送るチャネル（同期版）
    time::Duration,         // 時間の長さを表す型
};
// tokio の非同期版チャネル。`mpsc as tokio_mpsc` は「別名を付けて取り込む」書き方です。
// （上の std::sync::mpsc と名前がぶつかるため、別名にしています）
// oneshot は「1回だけ値を送れる」チャネルで、停止の合図を送るのに使います。
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
// windows_service: Rust のプログラムを Windows サービスとして動かすためのライブラリ
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

// ---------------------------------------------------------------------------
// 定数: `const 名前: 型 = 値;` で定義します。プログラム中で変わらない値です。
// `&str` は「文字列への参照（借用）」で、文字列リテラル "..." の型です。
// `u16` は 0〜65535 の符号なし 16bit 整数で、ポート番号にちょうど良い型です。
// ---------------------------------------------------------------------------

/// Windows サービスとして登録するときの名前（`sc create Katteguchi ...` の名前と一致させる）
const SERVICE_NAME: &str = "Katteguchi";
/// このサービスは自分専用のプロセスで動く、という種類の指定
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
/// 待ち受けるホストの既定値（127.0.0.1 = 自分のPCからのみアクセス可能）
const DEFAULT_HOST: &str = "127.0.0.1";
/// 待ち受けるポート番号の既定値
const DEFAULT_PORT: u16 = 8080;

// マクロ（名前の最後に `!` が付くもの）は、コンパイル時にコードを自動生成する仕組みです。
// これは「Windows が呼び出せる形式の関数 `ffi_service_main` を作り、
// その中から Rust の関数 `my_service_main` を呼ぶ」コードを生成します。
define_windows_service!(ffi_service_main, my_service_main);

/// ブラウザ（クライアント）から WebSocket で送られてくるメッセージの種類。
///
/// `enum`（列挙型）は「いくつかの種類のうちどれか1つ」を表す型です。
/// 各バリアント（Input / Resize）はそれぞれ別のデータを持てます。
///
/// `#[derive(Deserialize)]` を付けると、serde が JSON → この型 への変換コードを自動生成します。
/// `#[serde(tag = "type", rename_all = "lowercase")]` により、次のような JSON を受け取れます:
///
/// ```json
/// { "type": "input",  "data": "dir\r" }      // → ClientMsg::Input { data: "dir\r" }
/// { "type": "resize", "cols": 120, "rows": 30 } // → ClientMsg::Resize { cols: 120, rows: 30 }
/// ```
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMsg {
    /// キー入力。`data` に入力された文字列が入る
    Input { data: String },
    /// ブラウザ側のターミナル画面のサイズ変更（列数・行数）
    Resize { cols: u16, rows: u16 },
}

/// サーバーの設定をまとめた構造体。
///
/// `struct`（構造体）は複数の値をひとまとめにした型です。
/// `String` は「自分で所有する、伸び縮みできる文字列」、`bool` は true / false です。
struct ServerConfig {
    host: String,
    port: u16,
    /// true ならコンソールアプリとして起動、false なら Windows サービスとして起動
    console: bool,
}

/// コマンドライン引数をパース（--port / -p, --host, --console）
///
/// ## ジェネリクスについて
/// `<I, S>` は「型を後から決められる」ジェネリクスです。`where` 以下でその条件を書いています。
/// - `I: IntoIterator<Item = S>` … I は「S を順番に取り出せるもの」（Vec やイテレータなど）
/// - `S: Into<String>`          … S は「String に変換できるもの」（String や &str など）
///
/// こうしておくと、`std::env::args()`（String のイテレータ）でも、
/// テスト用の `vec!["--port", "9000"]`（&str の Vec）でも同じ関数で受け取れます。
///
/// 戻り値の型は `-> ServerConfig` で指定しています。
fn parse_args<I, S>(args: I) -> ServerConfig
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    // `let mut` で「後から変更できる変数」を作ります。
    // Rust の変数は `let` だけだと変更不可（イミュータブル）なので、変えたい場合は `mut` が必要です。
    let mut host = DEFAULT_HOST.to_string(); // &str → String に変換
    let mut port = DEFAULT_PORT;
    let mut console = false;

    // `.into_iter()` で引数を1つずつ取り出せるイテレータにし、
    // `.map(|s| s.into())` で各要素を String に変換しています。
    // `|s| s.into()` は「クロージャ」（その場で作る小さな無名関数）です。
    let mut iter = args.into_iter().map(|s| s.into());

    // `while let Some(arg) = iter.next()` は
    // 「次の要素がある（Some）間は arg に入れてループ、無くなったら（None）終了」という意味です。
    // Rust には null が無く、代わりに「値があるかもしれない」を Option 型（Some / None）で表します。
    while let Some(arg) = iter.next() {
        // `match` は値に応じて処理を分岐する構文です（switch 文の強力版）。
        // `arg.as_str()` で String を &str にして、文字列リテラルと比較できるようにしています。
        match arg.as_str() {
            "--console" => console = true,
            // `|` で「どちらかに一致したら」という意味になります
            "--port" | "-p" => {
                // `--port 9000` のように、次の引数が値になっている
                if let Some(val) = iter.next() {
                    // `.parse::<u16>()` で文字列を u16 の数値に変換します。
                    // 変換は失敗する可能性があるので Result 型（Ok / Err）が返ります。
                    // `if let Ok(p) = ...` で「成功したときだけ」処理します（失敗時は既定値のまま）。
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
            // `_` は「上のどれにも当てはまらない場合」（default）
            _ => {
                // `--port=9000` 形式にも対応
                // `.strip_prefix("--port=")` は、先頭が "--port=" なら残りの部分を Some で返し、
                // 違えば None を返します。
                if let Some(val) = arg.strip_prefix("--port=") {
                    if let Ok(p) = val.parse::<u16>() {
                        port = p;
                    }
                } else if let Some(val) = arg.strip_prefix("--host=") {
                    host = val.to_string();
                }
                // それ以外の未知の引数は無視します
            }
        }
    }

    // 最後の式に `;` を付けないと、それが関数の戻り値になります（`return` を省略できる）。
    // `host,` は `host: host,` の省略形です（変数名とフィールド名が同じ場合）。
    ServerConfig {
        host,
        port,
        console,
    }
}

/// プログラムのエントリーポイント（最初に実行される関数）。
///
/// 戻り値 `Result<(), Box<dyn std::error::Error>>` は
/// 「成功なら何も返さない `()`、失敗なら何らかのエラー」という意味です。
/// - `()` は「ユニット型」で、値が無いことを表します
/// - `Box<dyn Error>` は「Error トレイトを実装した、どんな型のエラーでも入れられる箱」です
///
/// main が Err を返すと、エラー内容が表示されてプログラムが終了します。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 実行時のコマンドライン引数を解析（1番目の実行ファイルパスはスキップ）
    let config = parse_args(std::env::args().skip(1));

    if config.console {
        // `println!` は標準出力に表示するマクロ。`{}` の部分に後ろの値が埋め込まれます。
        println!("Starting Web Terminal on http://{}:{}", config.host, config.port);

        // oneshot チャネルを作成。`tx` が送信側、`rx` が受信側です。
        // コンソールモードでは停止の合図を送らない（Ctrl+C で終了する）ので、
        // 送信側は使いません。変数名の先頭の `_` は「使わないけど警告を出さないで」という印です。
        // ※ `_tx` は main の最後まで生きているので、rx が「送信側が消えた」と誤解することはありません。
        let (_tx, rx) = oneshot::channel();
        run_web_server(config, rx);
        return Ok(());
    }

    // Windowsサービスとして起動
    // SCM（サービスコントロールマネージャー）に「このプロセスのサービス本体は
    // ffi_service_main です」と登録し、サービスが終わるまでここでブロックします。
    //
    // 末尾の `?` は「エラーならその場で関数から Err を返す、成功なら中身を取り出す」演算子です。
    // ※ サービスとしてではなく普通にダブルクリック等で起動すると、ここでエラーになります。
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

/// Windows サービスとして起動されたときに、SCM から呼ばれる関数。
///
/// `arguments` には SCM から渡された引数が入ります（1つ目はサービス名）。
/// `Vec<OsString>` は「OsString の可変長配列」です。
fn my_service_main(arguments: Vec<OsString>) {
    // SCMから渡された引数、またはプロセス起動引数の両方に対応
    // まずはプロセス起動時の引数（サービス登録時の binPath に書いた引数）で設定を作る
    let mut config = parse_args(std::env::args().skip(1));
    // SCM から開始パラメーター（`sc start Katteguchi --port 9000` など）が
    // 渡されていれば、そちらを優先する
    if arguments.len() > 1 {
        // OsString は UTF-8 とは限らないので、`.to_string_lossy()` で String に変換します
        // （変換できない文字は � に置き換えられます）。
        let scm_args = arguments.into_iter().skip(1).map(|s| s.to_string_lossy().into_owned());
        config = parse_args(scm_args);
    }

    // `let _ = ...;` は「戻り値（Result）を意図的に無視する」書き方です。
    // Rust は Result を無視すると警告を出すので、明示的に捨てています。
    let _ = run_service(config);
}

/// サービスとしての本体処理。
///
/// 1. SCM からの「停止して」等の通知を受け取るハンドラを登録
/// 2. 「実行中」状態を SCM に報告
/// 3. 別スレッドで Web サーバーを起動
/// 4. 停止の通知が来るまで待つ → Web サーバーを止める
/// 5. 「停止済み」状態を SCM に報告
fn run_service(config: ServerConfig) -> windows_service::Result<()> {
    // スレッド間通信用のチャネル。SCM から停止通知が来たら shutdown_tx で合図を送り、
    // メインの処理は shutdown_rx でその合図を待ちます。
    let (shutdown_tx, shutdown_rx) = mpsc::channel();

    // SCM から送られてくるイベント（停止・シャットダウン等）を処理するクロージャ。
    // `move` を付けると、クロージャが使う変数（ここでは shutdown_tx）の
    // 所有権をクロージャの中に移します。これにより別スレッドからでも安全に使えます。
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            // 「サービス停止」または「Windows のシャットダウン」が来たら停止の合図を送る
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            // Interrogate は「今の状態を教えて」という問い合わせ。特に何もせず OK を返す
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            // それ以外のイベントには対応していない
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    // イベントハンドラを SCM に登録。戻り値の status_handle を使って状態を報告します。
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    // SCM に「実行中（Running）」になったことを報告。
    // 受け付ける操作は「停止」と「シャットダウン」。
    // `A | B` はビットフラグの組み合わせ（両方受け付ける）を表しています。
    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Web サーバーに「止まって」と伝えるための oneshot チャネル
    let (web_shutdown_tx, web_shutdown_rx) = oneshot::channel();

    // `std::thread::spawn` で新しい OS スレッドを作り、その中で Web サーバーを動かします。
    // `move ||` は引数なしのクロージャで、config と web_shutdown_rx の所有権をスレッドに渡します。
    // 戻り値の server_handle は、後でスレッドの終了を待つ（join）ために使います。
    let server_handle = std::thread::spawn(move || {
        run_web_server(config, web_shutdown_rx);
    });

    // `.recv()` は値が届くまでここで待ち続けます（ブロック）。
    // → SCM から停止要求が来るまで、サービスはここで待機している状態です。
    let _ = shutdown_rx.recv();
    // Web サーバーに停止の合図を送る
    let _ = web_shutdown_tx.send(());
    // Web サーバーのスレッドが終わるまで待つ
    let _ = server_handle.join();

    // SCM に「停止済み（Stopped）」を報告。もう何の操作も受け付けないので empty()。
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

/// Web サーバーを起動し、`shutdown_rx` に合図が来るまで動かし続ける関数。
///
/// ## async / await について
/// Rust の非同期処理（async）は、それを実行する「ランタイム」が必要です。
/// ここでは tokio というランタイムを手動で作り、`block_on` で非同期処理が終わるまで待っています。
/// （main 関数に `#[tokio::main]` を付ける方法もありますが、
///   サービスとして動かす都合上、ここでは自分でランタイムを作っています）
fn run_web_server(config: ServerConfig, shutdown_rx: oneshot::Receiver<()>) {
    // tokio のランタイムを作成。
    // `.expect("...")` は「失敗したらこのメッセージを出してプログラムを停止（panic）する」という意味です。
    // 失敗したら続行しようがない場面で使います。
    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    // `async move { ... }` は非同期処理のブロック。`block_on` でそれが完了するまで待ちます。
    rt.block_on(async move {
        // ルーティング設定: URL ごとにどの関数で処理するかを登録
        // - "/"   → index_handler（ターミナル画面の HTML を返す）
        // - "/ws" → ws_handler（WebSocket 接続を受け付ける）
        // `.route(...).route(...)` のようにメソッドをつなげて書く方式を「メソッドチェーン」と呼びます。
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/ws", get(ws_handler));

        // "127.0.0.1:8080" のような文字列を作り、SocketAddr 型に変換します。
        // `format!` は println! と同じ書式で、表示する代わりに String を作るマクロです。
        let addr: SocketAddr = format!("{}:{}", config.host, config.port)
            .parse()
            .expect("Invalid host or port");

        // 指定したアドレスで TCP の接続待ち受けを開始。
        // `.await` は「非同期処理が終わるまで待つ（その間、他の処理に CPU を譲る）」という意味です。
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("Failed to bind port");

        // サーバーを起動。`with_graceful_shutdown` に渡した非同期処理が終わると、
        // サーバーは処理中のリクエストを片付けてから「穏やかに」停止します。
        // ここでは shutdown_rx に合図が届いたら（または送信側が消えたら）停止します。
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok(); // エラーは無視（Result を Option に変換して捨てている）
    });
}

/// `GET /` の処理: ターミナル画面の HTML を返す。
///
/// `include_str!` はコンパイル時にファイルの中身を文字列として埋め込むマクロです。
/// つまり index.html は実行ファイルの中に含まれるので、配布時に別ファイルは不要です。
///
/// 戻り値の `impl IntoResponse` は「IntoResponse トレイトを実装した何らかの型」という意味で、
/// 具体的な型名を書かずに済ませられます。
async fn index_handler() -> impl IntoResponse {
    Html(include_str!("index.html"))
}

/// `GET /ws` の処理: HTTP 接続を WebSocket 接続に切り替える（アップグレード）。
///
/// 引数の `ws: WebSocketUpgrade` は axum が自動でリクエストから取り出してくれます（エクストラクタ）。
/// 切り替えが完了すると `handle_socket` が呼ばれます。
async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

/// WebSocket 接続1つ分の処理。ブラウザのタブ1つにつき1回呼ばれます。
///
/// データの流れ:
/// ```text
///  [ブラウザ] --入力(JSON)--> ws_receiver --> pty_writer --> [cmd.exe]
///  [ブラウザ] <--出力(Binary)-- ws_sender <-- out_rx <-- (読み取りスレッド) <-- pty_reader <-- [cmd.exe]
/// ```
async fn handle_socket(socket: WebSocket) {
    // ----- 1. 疑似端末（PTY）を作る -----
    // PTY は「プログラムから見ると本物のターミナルに見える仮想の端末」です。
    // これを使うことで、cmd.exe が色付き表示やカーソル移動などを正しく行えます。
    let pty_system = native_pty_system();

    // 初期サイズ 80列 × 24行 で PTY を開く。
    // `match` で Result を分岐し、成功なら中身を取り出し、失敗なら `return` で接続処理を終了します。
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => return,
    };
    // pair には2つの端があります:
    // - pair.master: こちら（サーバー）側。ここに書くと cmd.exe への入力、ここから読むと cmd.exe の出力
    // - pair.slave:  cmd.exe 側。cmd.exe はこれを自分のターミナルだと思って使う

    // ----- 2. PTY の中で cmd.exe を起動する -----
    let mut cmd = CommandBuilder::new("cmd.exe");
    // 作業ディレクトリ（起動時のカレントディレクトリ）を指定。
    // 文字列中の `\\` はバックスラッシュ1文字を表します（`\` はエスケープ文字のため）。
    cmd.cwd("C:\\Windows\\System32");

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(_) => return,
    };
    // slave 側はもう cmd.exe に渡したので、こちらでは不要。
    // `drop` で明示的に破棄（解放）します。
    // Rust では値がスコープを抜けると自動で解放されますが、早く解放したい時は drop を使います。
    drop(pair.slave);

    // ----- 3. PTY の読み書き用オブジェクトを取り出す -----
    // 読み取り用（cmd.exe の出力を読む）
    let mut pty_reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return,
    };
    // 書き込み用（cmd.exe へ入力を送る）
    let mut pty_writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(_) => return,
    };

    // WebSocket を「送信側」と「受信側」に分割します。
    // こうすると送信と受信を別々のタスクで同時に行えます。
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // PTY の出力を WebSocket 送信タスクへ渡すためのチャネル。
    // `::<Vec<u8>>` はチャネルで送るデータの型（バイト列）を指定しています（ターボフィッシュ構文）。
    // 64 はバッファの大きさ（溜められるメッセージ数の上限）です。
    let (out_tx, mut out_rx) = tokio_mpsc::channel::<Vec<u8>>(64);

    // ----- 4. cmd.exe の出力を読み続けるスレッド -----
    // pty_reader.read() は「データが来るまで待つ（ブロッキング）」処理です。
    // 非同期タスクの中でブロッキング処理をすると他の処理が止まってしまうため、
    // 専用の OS スレッドを立てて、そこで読み取りを行います。
    std::thread::spawn(move || {
        // 4096 バイトの読み取り用バッファ。`[0u8; 4096]` は「0 (u8型) を 4096 個並べた配列」です。
        let mut buf = [0u8; 4096];
        loop {
            // `&mut buf` は「buf を書き換え可能な状態で貸し出す（可変借用）」という意味です。
            match pty_reader.read(&mut buf) {
                // 0 バイト = 終端（cmd.exe が終了した）、Err = 読み取り失敗 → どちらもループを抜ける
                Ok(0) | Err(_) => break,
                // n バイト読めた
                Ok(n) => {
                    // `buf[..n]` は配列の先頭から n バイトまでの部分（スライス）。
                    // `.to_vec()` でそれをコピーして Vec<u8> にし、チャネルで送ります。
                    // `blocking_send` はスレッド（非 async）から tokio のチャネルへ送るためのメソッドです。
                    // 送信に失敗 = 受信側（WebSocket 側）がもう無い → ループを抜ける
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // ----- 5. チャネルから受け取った出力をブラウザへ送るタスク -----
    // `tokio::spawn` は非同期タスクを作ってバックグラウンドで実行します（軽量なスレッドのようなもの）。
    let send_task = tokio::spawn(async move {
        // チャネルからデータが届くたびに、WebSocket のバイナリメッセージとしてブラウザへ送る。
        // `recv().await` はチャネルが閉じられると None を返すので、ループが終わります。
        while let Some(data) = out_rx.recv().await {
            if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                // 送信失敗 = ブラウザとの接続が切れた
                break;
            }
        }
    });

    // ----- 6. ブラウザからのメッセージを受け取るループ -----
    // `Some(Ok(msg))` に一致する間（= 正常にメッセージを受信できている間）ループします。
    // 接続が閉じられる（None）かエラー（Some(Err)）になるとループを抜けます。
    while let Some(Ok(msg)) = ws_receiver.next().await {
        // テキストメッセージだけを処理（それ以外の種類は無視）
        if let Message::Text(text) = msg {
            // JSON 文字列を ClientMsg 型に変換。形式が正しくなければ無視します。
            if let Ok(parsed) = serde_json::from_str::<ClientMsg>(&text) {
                match parsed {
                    // キー入力 → cmd.exe へそのまま書き込む
                    ClientMsg::Input { data } => {
                        // `.as_bytes()` で文字列をバイト列（&[u8]）として扱う
                        let _ = pty_writer.write_all(data.as_bytes());
                        // バッファに溜まっている分をすぐに送り出す
                        let _ = pty_writer.flush();
                    }
                    // 画面サイズ変更 → PTY のサイズも合わせる（cmd.exe の折り返し位置などが変わる）
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

    // ----- 7. 後片付け（ブラウザとの接続が切れた後） -----
    // 送信タスクを強制終了
    send_task.abort();
    // cmd.exe のプロセスを終了させる（残しておくとプロセスが溜まり続けてしまう）
    let _ = child.kill();
}
