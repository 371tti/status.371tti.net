# wk-status-371tti-net

HTTP/HTTPS・Ping・TCPを監視する、Rust製のステータスサーバーです。監視先は `config.yaml` で設定し、履歴はCBORで保存します。

## 起動

```powershell
cargo run -p wk-status-371tti-net -- config.yaml
```

設定ファイルがない場合は、コメント付きの初期設定を生成します。初期状態では `http://127.0.0.1:3000` で開きます。設定変更後は再起動してください。

ログは既定でINFOレベルです。起動、HTTPアクセス、監視結果、履歴の保存・整理エラーを出力します。`RUST_LOG=debug`（PowerShellでは `$env:RUST_LOG = "debug"`）で詳細ログに切り替えられます。

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
    order: 10
    type: http
    url: https://example.com/health
    interval: 60
    timeout: 10
    slow_ms: 500

  - id: production.router
    name: Router
    order: 20
    type: ping
    host: 192.0.2.1
    interval: 30

  - id: production.cache
    name: Cache
    order: 30
    type: tcp
    host: cache.example
    port: 6379
    interval: 60
    timeout: 5
```

`order` は監視一覧の表示順で、小さい値が先です。同じグループ内のサービスは個別の `order` で並び、グループ自体は所属サービスの最小 `order` で並びます。未指定のサービスは指定済みの後にconfig記述順で並び、未指定グループは指定済みグループの後に名前順で並びます。`Other` は常に最後です。

`interval` と `timeout` は秒、`slow_ms` はミリ秒です（既定値500）。HTTPの許可ステータスとヘッダーは `status_codes` と `headers`、TCPの送信内容は `send` で設定します。`response_codes` は `status.code` をサービス状態に対応付けます（既定: `0` 正常、`1` 応答低下、`2` 停止）。

監視詳細ページで監視先を隠す場合は `hide_target: true` を指定します。監視はそのまま継続します。

`id` の最後のドットより前がグループ名です。例: `production.web` と `production.cache` は `production` グループになります。ドットのないIDは `Other` に表示されます。`exclude_status: true` で全体・グループの状態集計から除外できます。

## メトリクス

`/ping` と `/healthz` は自己監視用のHTTP応答を返し、状態、アクセス数、稼働秒数を含みます。HTTP本文またはTCP応答の数値key-valueは履歴とグラフに記録されます。

```text
status.code 0
cpu.percent.all 12.34
cpu.percent.limit 100
cpu.percent.0 14.50
cpu.percent.1 10.25
cpu.iowait.all 2.10
cpu.iowait.limit 100
cpu.iowait.0 0.40
memory.bytes.total 17179869184
memory.percent.limit 100
network.bytes.rx.unit B/s
network.bytes.rx.eth0 1250000
network.bytes.rx.eth1 240000
network.error_drop.rx.percent.unit %
network.error_drop.rx.percent.eth0 0.10
network.error_drop.rx.percent.limit 100
network.rx 1250000
network.tx 2340000
network.unit bytes/s
```

`status.code` は状態判定用の予約キーです。その他の数値キーはグラフに表示され、最後のドットより前の部分でグループ化されます。`*.limit` はグラフに上限線を表示する系列です。`network.unit bytes/s` のように `<group>.unit <unit_text>` を返すと、そのグラフの見出しとホバー値に単位を表示します。応答に `status.code` がない場合は、応答の成否と `slow_ms` から判定します。
グラフや応答時間の大きな値は `1.2k`、`3.4M` のように表示します。

外部ソフトウェアからTCPで監視情報を返す場合の接続手順、応答形式、予約キー、制限は[TCP Key-Value Monitoring Protocol](docs/tcp-monitor-protocol.md)を参照してください。

## Linux agent

`agent/` はLinux用TCPデーモンです。CPU系列は `cpu.percent.all` と `cpu.percent.<コア番号>`、I/O waitは `cpu.iowait.*` にまとめて表示します。CPU使用率・I/O waitを有効にすると上限線 `*.limit 100` を出します。メモリ上限は取得できる場合 `memory.bytes.total`、割合の上限は `memory.percent.limit 100` で示します。ネットワークは `network.bytes.rx.<interface>` などを出し、`network.error_drop.rx.percent.<interface>` と `network.error_drop.tx.percent.<interface>` はNICカウンターのdrop/error比率を示します。これはNICで観測した値で、経路全体の実パケットロス測定ではありません。VMではゲストOSの `/proc` を使います。LXCなどのコンテナでは環境を検出し、CPU・メモリはcgroup v1/v2の値を優先します。cgroup v1が `cpuacct.usage_percpu` を公開する場合はコア別CPUも取得します。cgroup v2はCPU合計値のみのため、コア別値にはLXCFSの `/proc/stat` が必要です。コンテナ内のload averageやhost-wideなuptimeなど、範囲を特定できない値は出しません。I/O waitはカーネルが公開する値のため目安として扱ってください。

agentはCPU・メモリ・I/O wait・NIC drop/error比率を設定閾値と比較し、閾値以上または計測エラーを `status.code 1`（劣化）として返します。警告がなく計測に問題がなければ `0` です。既定閾値はCPU 90%、メモリ90%、I/O wait20%、NIC drop/error比率1%です。NIC判定は各方向・interfaceごとに、成功packet・error・dropの合計が100件以上のサンプルで行います。比率は `(errors + drops) / (successful_packets + errors + drops)` です。`sudo bash ./install.sh configure` で0%超〜100%（小数可）の閾値を設定でき、`off` で個別に無効化できます。agentは `status.code 2` を返しません。TCP応答がない場合は監視サーバー側が停止として扱います。ネットワーク速度とdrop/error比率は前回値との比較ができる2回目以降に出ます。ReleaseのLinuxアーカイブにはバイナリとinstallerが含まれます。単体の `.sh` はGitHub Releaseから最新バイナリを取得します。

```bash
curl -fL https://github.com/371tti/status.371tti.net/releases/latest/download/wk-status-371tti-net-agent.sh -o install.sh
sudo bash ./install.sh install
sudo bash ./install.sh configure
sudo bash ./install.sh uninstall
sudo bash ./install.sh uninstall --purge
```

既定ポートは9791です。初回の `install` と以降の `configure` はMonitor ID、表示名、監視サーバーから到達できるagentのIPアドレスを尋ねます。既定ルートのIPを先頭に、検出したネットワーク候補を表示し、手動入力も選べます。NATや別ネットワーク越しの場合は、監視サーバーから実際に到達できるアドレスを選んでください。

設定後はtokenや状態コードの対応を含む完全なTCP monitor entryを表示します。監視サーバーの `config.yaml` の `monitors:` 配下に追加してください。IDは既定で `agent.<hostname>` となり、ドット区切りの先頭部分がグループ名になります。ID、表示名、選択したhostは `configure` の次回実行時にも既定値として保持します。

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
