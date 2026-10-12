# TCP Key-Value Monitoring Protocol

この文書は `wk-status-371tti-net` server 0.1.x が受け付けるTCP監視の要求・応答仕様です。現在のプロトコルにはバージョン交渉用のフィールドはありません。

## 接続手順

監視間隔ごとに監視サーバーが新しいTCP接続を開きます。設定に `send` がある場合、そのUTF-8文字列をバイト列としてそのまま送信します。改行などは自動追加しません。コマンドや認証方法は監視対象ソフトウェア側で定義します。

要求送信後、監視サーバーは次のいずれかまで応答を読みます。

- `response_end` が設定されている場合、そのバイト列が最初に現れた位置まで読みます。終端文字列も応答データに含まれます。
- `response_end` がない場合、監視対象が接続または送信側を閉じてEOFになるまで読みます。
- `max_bytes` に達した場合、読み取りを終了します。`response_end` がなければ応答は切り詰め扱いで記録されます。`response_end` が設定されているのに上限までに届かなければ監視失敗です。
- 設定された `timeout` を超えた場合、監視失敗です。DNS解決、TCP接続、要求送信、応答受信の全体に適用されます。

複数行の応答で `response_end: "\n"` を使うと、最初の行だけで読み取りが終わります。複数行では末尾の空行（LFなら `\n\n`、CRLFなら `\r\n\r\n`）を終端にするか、応答完了後に監視対象が接続または送信側を閉じてください。

`send` を省略するとTCPポートへの接続確認だけを行い、応答データは取得しません。

## 応答形式

応答はUTF-8テキストで、1行に1組のkey-valueを記述します。LFとCRLFを受け付けます。空行と、`#` で始まるコメント行は無視されます。区切りには空白または `=` を使えます。

```text
status.code 0
cpu.percent.unit %
cpu.percent.all 12.5
cpu.percent.0 15.0
memory.bytes.unit B
memory.bytes.used 1073741824
```

`=` 形式も使えます。

```text
status.code=0
cpu.percent.unit=%
cpu.percent.all=12.5
```

キーは大文字・小文字を区別します。キーの各セグメントにはASCII英字、数字、`_`、`-` を使えます。`.` は空でないセグメントの区切りです。キー長は128バイト以下です。

数値メトリクスの値は有限の数値で指定します。小数と指数表記を受け付けます。文字列、`NaN`、無限大はグラフ値として無視されます。1応答あたりの上限は数値メトリクス512種類、単位グループ512種類です。有効な数値キーまたは単位キーが重複した場合は最後に解析できた値が使われ、無効な値は無視されます。送信側は各キーを1回だけ出力してください。

## 予約キー

### `status.code`

`status.code` はサービス状態を表す符号付き整数です。各監視対象の `response_codes` で状態に対応付けます。既定の対応は次のとおりです。

| Code | 既定の状態 |
| ---: | --- |
| `0` | `operational` |
| `1` | `degraded` |
| `2` | `down` |

設定例:

```yaml
response_codes:
  0: operational
  1: degraded
  2: down
```

コード値は変更できます。応答には整数を返し、その値を `response_codes` に登録してください。未登録または整数でない値は `down` として扱います。`status.code` がない場合、チェック成功時の状態は監視サーバーの実測応答時間と全 `response.<name>` 値を `slow_ms` と比較して決めます。いずれかが閾値を超えれば `degraded`、それ以外は `operational` です。`status.code` が `operational` に対応付けられている場合も、閾値超過で `degraded` になります。明示的な `degraded` または `down` は対応付けどおりの状態です。接続失敗、タイムアウト、応答なしは返されたコードにかかわらず `down` です。`status.code` が複数ある場合は最後の値を使います。送信側は1つだけ出力してください。

`slow_ms` の既定値は500ミリ秒です。`null` にすると実測応答時間と `response.<name>` の閾値判定を無効にできます。

サービスが `down` を返した場合も、応答中の数値メトリクスは監視記録に保存されます。

### Response Times の予約キー

Response Times の実測値は監視サーバーが計測します。応答で `response.ms` が返されても、実測値の上書きには使いません。

`response.<name> <milliseconds>`（`response.ms` 以外）は、Response Times グラフに重ねるミリ秒の系列です。`<name>` はグラフの凡例とホバー表示に使われ、ドットを含む名前もそのまま表示されます。これらの系列は通常のメトリクスグラフには表示されません。チェックが成功していて `status.code` が `down` を示さない場合、各系列のいずれかが `slow_ms` を超えると、実測応答時間が閾値を超えた場合と同様にサービスを `degraded` と判定します。チェック失敗と明示的な `down` は優先されます。

```text
response.application 19.2
response.database 12.8
```

`response.*` の値は有限の0以上の数値で指定してください。これらは応答時間と同じミリ秒軸に表示されます。`slow_ms: null` の場合、実測値と `response.*` 系列の閾値判定を無効にします。

### `<group>.unit`

末尾が `.unit` のキーは単位ラベルとして扱い、数値グラフにはしません。値は制御文字を含まない64バイト以下のテキストです。たとえば `cpu.percent.unit %` は `cpu.percent` グループの単位を `%` にします。単位を付ける場合は、数値キーと同じグループ名を使ってください。

ドットを含まない数値キーは `Metrics` グループにまとめられます。このグループに単位を付ける場合は `Metrics.unit <unit>` を返してください。

## メトリクスのグループ化

ドットを含む数値キーは、最後の `.` より前をグラフグループ、最後のセグメントを系列名として扱います。同じグループの系列は、同じ縦軸を使うグラフに重ねて表示されます。

たとえば `cpu.percent.all` と `cpu.percent.0` は `cpu.percent` グループ、`network.bytes.rx.eth0` と `network.bytes.rx.eth1` は `network.bytes.rx` グループです。

```text
cpu.percent.unit %
cpu.percent.all 12.5
cpu.percent.0 15.0
cpu.percent.1 10.0
network.bytes.rx.unit B/s
network.bytes.rx.eth0 1250000
network.bytes.rx.eth1 240000
```

アプリケーション独自の数値は送信された値のまま記録・表示されます。サーバー側でレート計算や単位変換は行いません。
1監視対象あたり、詳細画面のグラフには最大128種類のメトリクスキーが表示されます。グラフ化するキーは必要なものに絞ってください。

## 監視設定例

次の例は要求を送り、応答末尾の空行まで受信します。

```yaml
- id: production.worker
  name: Production Worker
  type: tcp
  host: 192.0.2.20
  port: 9791
  interval: 60
  timeout: 5
  slow_ms: 500
  send: "METRICS\n"
  response_end: "\n\n"
  max_bytes: 16384
  response_codes:
    0: operational
    1: degraded
    2: down
```

| 設定 | 既定値・範囲 |
| --- | --- |
| `host` | 必須。ホスト名またはIPアドレス |
| `port` | 必須。1–65535 |
| `interval` | 秒。既定60、1–86400 |
| `timeout` | 秒。既定10、1–300 |
| `slow_ms` | ミリ秒。既定500、1–300000、または `null` |
| `send` | 省略可。要求データは65536バイト以下 |
| `response_end` | 省略可。`send` が必要。空でない終端文字列を指定し、長さは `max_bytes` 以下 |
| `max_bytes` | 応答の読取上限。既定16384、1–65536 |
| `response_codes` | 省略時は `0: operational`、`1: degraded`、`2: down` |

TCP応答時間にはDNS解決と接続が含まれます。`send` を設定した場合は、要求送信と応答の読み取りも含みます。`send` を省略した接続確認では、TCP接続確立までを測ります。

## セキュリティと保存

TCP監視はTLS暗号化や要求認証を追加しません。`send` の内容は通常のTCPで送信されます。信頼できるネットワークや保護されたトンネルを使い、ファイアウォールで接続元を制限してください。

受信した応答本文はチェック履歴に保存され、監視詳細ページにも表示されます。パスワード、トークンなどの秘密情報を応答に含めないでください。`max_bytes` は1回の監視で保存する応答本文の上限です。
