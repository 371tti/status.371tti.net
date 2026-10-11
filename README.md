# wk-status-371tti-net

HTTP/HTTPS・Ping・TCPを監視する、Rust製のステータスサーバーです。監視先は `config.yaml` で設定し、履歴はCBORで保存します。

## 起動

```powershell
cargo run -p wk-status-371tti-net -- config.yaml
```

設定ファイルがない場合は、コメント付きの初期設定を生成します。初期状態では `http://127.0.0.1:3000` で開きます。設定変更後は再起動してください。

```powershell
cargo build --release -p wk-status-371tti-net
.\target\release\wk-status-371tti-net.exe config.yaml
```

テンプレート、CSS、フォントはバイナリに含まれます。

## 設定例

```yaml
server:
  name: wk-status-371tti-net
  bind: 127.0.0.1
  port: 3000

storage:
  path: data/records
  chunk_minutes: 5
  retention_days: 30

monitors:
  - id: production.web
    name: Web
    type: http
    url: https://example.com/health
    interval: 60
    timeout: 10
    slow_ms: 500

  - id: production.router
    name: Router
    type: ping
    host: 192.0.2.1
    interval: 30

  - id: production.cache
    name: Cache
    type: tcp
    host: cache.example
    port: 6379
    interval: 60
    timeout: 5
```

`interval` と `timeout` は秒、`slow_ms` はミリ秒です（既定値500）。HTTPの許可ステータスとヘッダーは `status_codes` と `headers`、TCPの送信内容は `send` で設定します。`response_codes` は `status.code` をサービス状態に対応付けます（既定: `0` 正常、`1` 応答低下、`2` 停止）。

監視詳細ページで監視先を隠す場合は `hide_target: true` を指定します。監視はそのまま継続します。

`id` の最後のドットより前がグループ名です。例: `production.web` と `production.cache` は `production` グループになります。ドットのないIDは `Other` に表示されます。`exclude_status: true` で全体・グループの状態集計から除外できます。

## メトリクス

`/ping` と `/healthz` は自己監視用のHTTP応答を返し、状態、アクセス数、稼働秒数を含みます。HTTP本文またはTCP応答の数値key-valueは履歴とグラフに記録されます。

```text
status.code 0
cpu.percent.usage 12.34
cpu.core.0.percent.usage 14.50
network.rx 1250000
network.tx 2340000
network.unit bytes/s
```

`status.code` は状態判定用の予約キーです。その他の数値キーはグラフに表示され、最後のドットより前の部分でグループ化されます。`network.unit bytes/s` のように `<group>.unit <unit_text>` を返すと、そのグラフの見出しとホバー値に単位を表示します。応答に `status.code` がない場合は、応答の成否と `slow_ms` から判定します。
グラフや応答時間の大きな値は `1.2k`、`3.4M` のように表示します。

## Linux agent

`agent/` はCPU、コア別使用率、I/O wait、メモリ、load average、ネットワークを返すLinux用TCPデーモンです。ネットワークは /proc/net/dev のカウンタ差分を前回要求からの経過秒で割り、インターフェース別に受信・送信bytes/s、packets/s、errors/s、drops/sを記録します。I/O waitはカーネルが公開する値のため目安として扱ってください。Linux向けReleaseアーカイブを展開してインストールします。

```bash
sudo bash ./install.sh install
sudo bash ./install.sh configure
sudo bash ./install.sh uninstall
sudo bash ./install.sh uninstall --purge
```

既定ポートは9791です。インストール後に表示されるtokenを使い、監視サーバーにTCP監視を設定します。
`configure` ではCPU（コア別・I/O waitを含む）、メモリ、load average、ネットワークの有効・無効を個別に選べます。既定ではすべて有効です。

```yaml
- id: production.host
  name: Production Host
  type: tcp
  host: 192.0.2.50
  port: 9791
  interval: 60
  slow_ms: 500
  send: "METRICS <token>\n"
```

tokenは暗号化されずに送信されるため、信頼できるネットワーク内で使い、ファイアウォールで接続元を制限してください。

## 保存

履歴は `storage.path/<monitor-id>/<年>/<日付>/<時刻>.cbor` に保存されます。`chunk_minutes` はファイルの時間枠、`retention_days` は保持日数です。
