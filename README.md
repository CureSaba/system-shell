# system-shell

ブラウザから Windows の `cmd.exe` を操作できる、Rust 製の Web ターミナルです。
Windows サービスとして登録すると、PC 起動時に **SYSTEM 権限**で自動起動します。

```
[ブラウザ (xterm.js)] ⇄ WebSocket (/ws) ⇄ [system-shell] ⇄ PTY ⇄ [cmd.exe]
```

- `http://<host>:<port>/` … ターミナル画面
- `ws://<host>:<port>/ws` … 入出力用の WebSocket
- サービス名: `Katteguchi`（コード内で固定）

> [!WARNING]
> **このツールには認証機能がありません。**
> サービスとして動かすと、接続できた人は誰でも **SYSTEM 権限の cmd.exe** を操作できます。
> 既定の待ち受けアドレスは `127.0.0.1`（自分の PC からのみ接続可）です。
> `--host 0.0.0.0` などで外部に公開するのは、信頼できるネットワーク内に限ってください。

---

## 1. 必要なもの

- Windows 10 / 11
- [Rust](https://www.rust-lang.org/ja/tools/install)（edition 2024 を使うため **Rust 1.85 以上**）
- 画面表示に使う xterm.js を CDN（cdn.jsdelivr.net）から読み込むため、ブラウザ側にインターネット接続が必要

Rust のバージョンは次のコマンドで確認できます。

```bash
rustc --version
```

## 2. ビルド

```bash
git clone https://github.com/CureSaba/system-shell.git
```

```bash
cd system-shell
```

```bash
cargo build --release
```

`target\release\system-shell.exe` ができます。
`index.html` は実行ファイルの中に埋め込まれるので、配布するのはこの exe 1つだけです。

## 3. まずはコンソールで動作確認

サービスとして登録する前に、普通のアプリとして起動して動くか確かめます。

```bash
cargo run --release -- --console
```

ブラウザで <http://127.0.0.1:8080> を開くと、ターミナルが表示されます。
止めるときはコンソールで `Ctrl+C` を押します。

### コマンドライン引数

| 引数 | 説明 | 既定値 |
| --- | --- | --- |
| `--console` | コンソールアプリとして起動（付けないとサービスとして起動しようとする） | なし |
| `--host <アドレス>` / `--host=<アドレス>` | 待ち受けるアドレス | `127.0.0.1` |
| `--port <番号>` / `-p <番号>` / `--port=<番号>` | 待ち受けるポート番号 | `8080` |

> `--console` を付けずに exe を直接ダブルクリックしても、サービスとして起動されていないため、すぐ終了します。

## 4. サービスとして登録（SYSTEM 権限・自動起動）

以下の手順は、**管理者として実行した PowerShell** で行います
（スタートメニューで PowerShell を右クリック →「管理者として実行」）。

> [!NOTE]
> PowerShell では `sc` が `Set-Content` の別名になっているため、必ず **`sc.exe`** と書いてください。

### 4-1. exe を決まった場所に置く

サービスは登録したパスの exe を直接起動するので、`target` フォルダではなく、
消したり作り直したりしない場所にコピーしておきます。

```powershell
New-Item -ItemType Directory -Force "C:\Program Files\system-shell"
```

```powershell
Copy-Item .\target\release\system-shell.exe "C:\Program Files\system-shell\"
```

### 4-2. サービスを作成する

```powershell
sc.exe create Katteguchi binPath= "\"C:\Program Files\system-shell\system-shell.exe\" --port 8080" start= auto obj= LocalSystem DisplayName= "Katteguchi Web Terminal"
```

| 指定 | 意味 |
| --- | --- |
| `Katteguchi` | サービス名（コード内の `SERVICE_NAME` と同じ名前にする） |
| `binPath=` | 起動する exe のパスと引数。パスに空白があるので `\"...\"` で囲む |
| `start= auto` | Windows の起動時に自動で開始する（スタートアップ） |
| `obj= LocalSystem` | **SYSTEM 権限**で実行する（省略しても LocalSystem になる） |
| `DisplayName=` | 「サービス」画面に表示される名前 |

> `sc.exe` の書式では、`binPath=` のように **`=` の直後に半角スペースが必須**です。

ポートやアドレスを変えたいときは、`binPath=` の引数部分（`--port 8080`）を書き換えます。
`--console` は付けないでください。

必要なら説明文も付けておきます。

```powershell
sc.exe description Katteguchi "Browser-based cmd.exe terminal (system-shell)"
```

### 4-3. サービスを開始する

```powershell
sc.exe start Katteguchi
```

`start= auto` にしているので、次回からは PC の起動時に自動で開始されます。

### 4-4. 動作確認

```powershell
sc.exe query Katteguchi
```

`STATE` が `4  RUNNING` になっていれば起動しています。
ブラウザで <http://127.0.0.1:8080> を開き、次のコマンドを打つと SYSTEM 権限で動いていることを確認できます。

```
whoami
```

`nt authority\system` と表示されれば成功です。

### 一時的に別の引数で起動する

`sc.exe start` に引数を付けると、その回だけ `binPath=` の引数の代わりにそちらが使われます。

```powershell
sc.exe start Katteguchi --port 9000
```

## 5. サービスの停止・削除

### 5-1. サービスを停止する

```powershell
sc.exe stop Katteguchi
```

### 5-2. サービスを削除する

```powershell
sc.exe delete Katteguchi
```

> 「サービス」画面（`services.msc`）を開いたままだと、削除が「削除の保留中」になることがあります。
> その場合は画面を閉じるか、PC を再起動すると削除されます。

### 5-3. exe を削除する

```powershell
Remove-Item -Recurse "C:\Program Files\system-shell"
```

## 6. 外部の PC から接続する場合

1. `binPath=` の引数を `--host 0.0.0.0 --port 8080` に変更する

   ```powershell
   sc.exe config Katteguchi binPath= "\"C:\Program Files\system-shell\system-shell.exe\" --host 0.0.0.0 --port 8080"
   ```

2. Windows ファイアウォールでポートを開ける

   ```powershell
   New-NetFirewallRule -DisplayName "Katteguchi 8080" -Direction Inbound -Protocol TCP -LocalPort 8080 -Action Allow
   ```

3. サービスを再起動する

   ```powershell
   sc.exe stop Katteguchi
   ```

   ```powershell
   sc.exe start Katteguchi
   ```

ファイアウォールのルールを削除するときは、次のコマンドを使います。

```powershell
Remove-NetFirewallRule -DisplayName "Katteguchi 8080"
```

> [!WARNING]
> 冒頭の警告のとおり、認証はありません。外部に公開すると、同じネットワークにいる人なら誰でも
> SYSTEM 権限でこの PC を操作できるようになります。

## 7. 困ったとき

| 症状 | 確認すること |
| --- | --- |
| `sc.exe create` が「アクセスが拒否されました」になる | PowerShell を管理者として実行しているか |
| `sc.exe start` で エラー 1053 になる | `binPath=` のパスが正しいか。exe がそのパスにあるか |
| サービスは RUNNING なのにブラウザで開けない | ポートがほかのアプリで使われていないか（`netstat -ano \| findstr :8080`） |
| 画面が真っ白で何も表示されない | ブラウザから cdn.jsdelivr.net に接続できるか |
| exe を更新したい | `sc.exe stop` → exe を上書きコピー → `sc.exe start` |
