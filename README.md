# payment-matching-api

[![CI](https://github.com/TakahiroNago/payment-matching-api/actions/workflows/ci.yml/badge.svg)](https://github.com/TakahiroNago/payment-matching-api/actions/workflows/ci.yml)
![license: MIT](https://img.shields.io/badge/license-MIT-blue)

請求（売掛金）と銀行入金の明細を突き合わせる「入金消込」の API サーバーです。Rust（axum + sqlx）と PostgreSQL で書いています。

- 振込名義のゆれ（半角カナ、`ｶ)` などの法人格の略語）を正規化して取引先を特定します。
- 1 回の入金で複数の請求を支払う合算入金、振込手数料を差し引いた入金を判定します。
- 説明が 1 通りに決まるときだけ自動で消し込み、それ以外は候補を出して人が確定します。
- 同じ明細を 2 回取り込んでも二重に計上しません。
- どの規則で、どの請求に、なぜ消し込んだかを入金ごとに記録します。

> **個人開発・学習目的のリポジトリです。** 題材は一般的な経理業務で、特定の製品の再現ではありません。データはすべて架空で、実在の取引データは使っていません。画面はなく、本番運用を想定した認証・権限管理も持ちません（[限界](#限界)）。

*English: an accounts-receivable reconciliation API in Rust (axum, sqlx, PostgreSQL). It normalizes Japanese payer names from bank statements, matches payments to open invoices with four deterministic rules (exact, combined, fee-deducted, amount-only), confirms a match automatically only when it is the single explanation, and keeps imports idempotent. A personal learning project with fictional data. Details: [docs/requirements.md](docs/requirements.md), [docs/api.md](docs/api.md) (both in Japanese).*

## 突合の規則

上から順に評価し、最初に成立したものを採用します。

| 規則 | 条件 | 説明が 1 通り | 複数 |
|---|---|---|---|
| R1 単一一致 | 名義から取引先を特定でき、同額の未消込請求がある | 自動確定 | 最も古い請求を候補に |
| R2 合算一致 | 取引先を特定でき、未消込請求 2 件以上の合計が入金額に一致する | 自動確定 | 古い請求を優先した組み合わせを候補に |
| R3 手数料差引 | 手数料を差し引いて支払う取引先で、R1・R2 が「入金額 + 手数料」で成立する | 候補 | 候補 |
| R4 金額のみ | 取引先を特定できず、同額の未消込請求がある | 候補 | 候補 |

名義の照合は完全一致だけで、あいまい一致はしません。間違った請求を消し込むほうが、人の確認待ちになるより害が大きいためです。表記のゆれは別名の登録で吸収し、候補の確定時に名義を覚えさせることもできます。

## 動かす

Docker があれば動きます。

```sh
cp .env.example .env            # API_TOKEN に 16 文字以上の値を入れる（openssl rand -hex 24 など）
docker compose build
docker compose up -d db
docker compose run --rm api migrate     # スキーマを作る。サーバーは自動では適用しない
docker compose up -d api
curl http://127.0.0.1:8080/readyz       # {"status":"ok"}
```

一連の流れは `examples/demo.sh` で試せます。curl だけを使います。空のデータベースが前提なので、やり直すときは `docker compose down -v` で消してから上の手順を繰り返してください。

```sh
sh examples/demo.sh
```

取引先 1 社と請求 4 件（110,000／55,000／33,000／220,000 円）を登録し、入金 4 件を取り込みます。

| 入金 | 振込名義 | 判定 |
|---|---|---|
| 165,000 円 | `ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ` | **R2 で自動確定**：110,000 + 55,000 の組み合わせだけが一致 |
| 219,560 円 | `ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ` | **R3 の候補**：220,000 円の請求から手数料 440 円を引いた額 |
| 33,000 円 | `ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ` | **R4 の候補**：名義は未登録だが、同額の請求がある |
| 5,000 円 | `ﾌﾒｲ ｼﾖｳｶｲ` | **未消込**：名義も金額も一致しない |

続けて、同じ `Idempotency-Key` での再送（何も書き込まれない）、候補 2 件の確定、名義 `ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ` の別名登録までを実行します。

手で叩く場合の例です。日本語を含む本文は、ファイルから送ってください（Windows の Git Bash では、コマンドラインに書いた日本語が化けます）。

```sh
TOKEN=...   # .env の API_TOKEN

curl -s -X POST http://127.0.0.1:8080/v1/customers \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  --data-binary @examples/customer.json

curl -s -X POST http://127.0.0.1:8080/v1/payment-batches \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: statement-2026-11-10' \
  --data-binary @examples/payments.json
```

エンドポイントの一覧と入出力は [docs/api.md](docs/api.md) にあります。

## 設計判断

**自動確定するのは、説明が 1 通りのときだけ。** 同額の請求が 2 件ある、組み合わせが 2 通りある、といった場合は候補にとどめます。手数料の差引（R3）と名義不明（R4）は、1 通りでも人が確定します。判定は DB に触れない純粋関数（[`src/domain/matching.rs`](src/domain/matching.rs)）で、「自動確定は説明が 1 通りのときだけ」を総当たりとの比較でプロパティテストしています。

**金額は円の整数。** `Yen(i64)` の newtype で持ち、浮動小数点を使いません。ID もテーブルごとの型（`InvoiceId`、`PaymentId` など）にして、取り違えをコンパイル時に防ぎます。

**不変条件は、守れる場所でいちばん低い層に置く。**

| 不変条件 | 守る場所 |
|---|---|
| 1 件の入金に有効な消込は 1 つまで | 部分一意インデックス |
| 1 件の請求は 1 回しか消し込めない | `UPDATE … WHERE status = 'open'` の更新行数を検査。足りなければ 409 |
| 内訳の合計 = 入金額、内訳 + 手数料 = 請求額の合計 | 確定のトランザクション内で検証 |
| 1 つの名義は 1 つの取引先だけを指す | 取引先自身のカナ名も別名と同じテーブルに入れ、1 つの一意制約で守る |
| 候補が指す請求は未消込のものだけ | 請求を消し込む処理が、同じ請求を含む他の入金の候補を取消にする |

**消込の状態を変える処理は直列に実行する。** 取り込み・確定・手動消込・取消は、トランザクションの先頭で同じ advisory lock を取ります。取り込みは未消込の請求を読んで判定し、確定は請求を消し込むので、並行させるには行ロックの取得順をすべての経路で揃える必要があります。ロックを 1 つにして、その検討自体をなくしました。処理量が増えたら取引先単位に分けられます。ロックを持つ時間を短くするため、取り込みは入金の挿入・照合先の読み込み・判定結果の保存をバッチ全体で 1 文ずつにまとめ、行ごとに発行するのは消込が生じた行の 1〜2 文だけにしています（1,000 行で約 1 秒。手元の Docker 上の PostgreSQL での計測）。ロックを待つのは 5 秒までで、超えたら何も書き込まずに 503（`busy`）を返します。待っている間は接続プールの接続を 1 本使うので、上限がないと、書き込みの列が長くなったときに参照や `/readyz` の分まで使い切ってしまうためです。

**取り込みは冪等で、全部成功するか何も残らないか。** `Idempotency-Key` とリクエスト内容のハッシュを保存します。同じキー・同じ内容の再送は何も書き込まず、同じキー・違う内容は 409 です。ハッシュは解析した内容を書き出し直したものから取るので、空白やキーの順序の違いは同じリクエストとして扱います。途中で失敗するとキーごとロールバックされ、同じキーで再送できます。別のキーでも、内容が取り込み済みのバッチと同じなら 409 です（`allow_duplicate: true` で取り込めます）。リクエストごとにキーを作るクライアントが同じ明細を 2 回送ると、2 回目の入金は翌月の同額の請求に自動確定されてしまうので、キーだけには頼りません。

**SQL はコンパイル時に検査する。** sqlx の `query!` 系マクロで、列名・型・NULL 可否をスキーマと照合します。CI では、コードのクエリがすべて `.sqlx` にあること（オフライン）と、`.sqlx` がマイグレーションと一致すること（`sqlx prepare --check`）の両方を確認します。

**エラーは RFC 9457。** 入力の誤りは `errors: [{ field, code }]` で場所と理由を返します。500 では原因を返さず `request_id` だけを返し、原因はログに残します。ログには本文・振込名義・金額・トークンを出しません。

要件と、最初の案から変えた点とその理由は [docs/requirements.md](docs/requirements.md) にあります。

## 構成

```
src/
  domain/    型（Yen、各 ID）、名義の正規化、突合の規則。I/O なし
  repo/      SQL（sqlx のマクロでコンパイル時に検査）
  service/   取り込み・確定・手動消込・取消。1 関数 = 1 トランザクション
  http/      ルーティング、認証、入力検証、エラー変換
  config.rs  環境変数の読み込みと検証
migrations/  スキーマ
tests/api.rs 結合テスト（ルーター経由、実 PostgreSQL）
```

## テスト

```sh
docker compose up -d db
cargo test
```

- **単体・プロパティテスト（34 件）**：名義の正規化（事例 39 件と冪等性）、突合の各規則、判定結果の金額が必ず合うこと、請求の並び順に依存しないこと、設定の検証、ハンドラーのパニックと時間切れの応答。
- **結合テスト（38 件）**：`#[sqlx::test]` がテストごとに専用のデータベースを作ります。各エンドポイントの正常系・異常系に加え、同じ請求への同時確定で片方だけが成功すること、同じキーの同時取り込みでバッチが 1 つしかできないこと、取り込みの途中で失敗すると何も残らないこと、同じ内容を別のキーで送ると拒否されること、ロックを待ちきれない要求が 503 になること、請求が消し込まれると他の入金の候補が取消になること、取消の理由が残ること、未消込の入金を判定し直せることを確認しています。
- **整合性テスト（結合テストのうち 3 件）**：取り込み・請求登録・確定・取消・手動消込・再判定・別名登録をシードから決めた順に 250 回ずつ実行し、1 回ごとに入金・消込・請求の間の規則（I1〜I3、I5、入金の `status` と消込の `state` の対応など 7 つ）を SQL で検査します。規則の検査自体は、データをわざと壊して検出できることを別のテストで確かめています。

## 開発

Rust 1.94 以降と Docker が必要です。

```sh
docker compose up -d db
cargo install sqlx-cli --no-default-features --features postgres,rustls
sqlx migrate run                         # DATABASE_URL は .env から読まれる
cargo run                                # 環境変数は .env と同じものを設定しておく
```

SQL を変えたら `.sqlx` を作り直してコミットします。

```sh
cargo sqlx prepare -- --all-targets
```

CI（GitHub Actions）は `cargo fmt --check`、`cargo clippy -D warnings`、`cargo sqlx prepare --check`、`cargo test`、`cargo deny check`、Docker イメージのビルドと起動確認を実行します。

| 環境変数 | 既定 | 内容 |
|---|---|---|
| `DATABASE_URL` | （必須） | PostgreSQL の接続先 |
| `API_TOKEN` | （必須） | ベアラートークン。16 文字以上 |
| `BIND_ADDR` | `127.0.0.1:8080` | 待ち受けるアドレス |
| `FEE_AMOUNTS` | `110,220,…,880` | R3 が試す振込手数料（円、カンマ区切り） |
| `DB_MAX_CONNECTIONS` | `10` | 接続プールの上限 |
| `RUST_LOG` | `info` | ログの水準 |

## 限界

- 名義の正規化は完全ではありません。法人格の略語は表にあるものだけを扱い、支店名や担当者名のついた名義は別名の登録が必要です。
- 分割入金（1 件の請求を複数回で支払う）、過入金、外貨には対応していません。
- 取り込みは JSON だけです。全銀フォーマットや CSV は読みません。
- 候補は請求を押さえません。同じ請求が複数の入金の候補になることがあり、先に確定したほうが勝ちます。残った候補は取消になり、その入金は `unmatched` に戻ります。
- 取り込みの重複は、バッチ全体の内容が同じときだけ検出します。行の一部が重なる明細は検出できません（明細側の通番のような、行を特定する値が入力にないため）。
- 別名や請求を後から登録しても、取り込み済みの入金は自動では判定し直しません。`POST /v1/payments/rematch` を呼ぶと、`unmatched` の入金を判定し直します（`suggested` の入金は対象外で、人が確定か却下をします）。
- 認証は固定のトークン 1 つで、利用者・権限・「誰が確定したか」の記録はありません。
- 消込の状態を変える処理は直列に実行するので、書き込みの処理量には上限があります。

拡張する場合の方針は [docs/requirements.md §9](docs/requirements.md) に書いています。

## ライセンス

[MIT](LICENSE)
