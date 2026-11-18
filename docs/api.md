# API リファレンス

基底 URL は `http://127.0.0.1:8080`（Compose の既定）。`/v1` 以下はすべて `Authorization: Bearer <API_TOKEN>` が必要です。

- 金額は円の整数（1〜999,999,999,999）。小数は受け付けません。
- 日付は `YYYY-MM-DD`、時刻は RFC 3339（UTC）。
- ID は UUID。
- リクエスト本文に未知のフィールドがあると 422 になります（綴りの誤りを黙って無視しないため）。
- すべての応答に `x-request-id` ヘッダーがつきます。リクエストで `x-request-id`（英数字とハイフン、64 文字まで）を送ると、その値を使います。

| メソッド | パス | 内容 | 成功 |
|---|---|---|---|
| GET | `/healthz` | プロセスが動いている | 200 |
| GET | `/readyz` | DB に届く | 200（届かなければ 503） |
| POST | `/v1/customers` | 取引先の登録 | 201 |
| GET | `/v1/customers/{id}` | 取引先と名義の参照 | 200 |
| POST | `/v1/customers/{id}/aliases` | 別名の登録 | 201 |
| DELETE | `/v1/customers/{id}/aliases/{alias_id}` | 別名の削除 | 204 |
| POST | `/v1/invoices` | 請求の登録 | 201 |
| GET | `/v1/invoices` | 請求の一覧 | 200 |
| GET | `/v1/invoices/{id}` | 請求と、それを消し込んだ消込 | 200 |
| POST | `/v1/invoices/{id}/void` | 請求の無効化 | 200 |
| POST | `/v1/payment-batches` | 入金の取り込みと自動突合 | 201（再送は 200） |
| GET | `/v1/payments` | 入金の一覧 | 200 |
| POST | `/v1/payments/rematch` | 未消込の入金の再判定 | 200 |
| GET | `/v1/payments/{id}` | 入金と、その消込の履歴 | 200 |
| POST | `/v1/payments/{id}/matches` | 手動消込 | 201 |
| POST | `/v1/matches/{id}/confirm` | 候補の確定 | 200 |
| POST | `/v1/matches/{id}/cancel` | 消込の取消、候補の却下 | 200 |

---

## エラー

RFC 9457 の problem details（`Content-Type: application/problem+json`）で返します。

```json
{
  "type": "about:blank",
  "title": "Unprocessable Entity",
  "status": 422,
  "code": "validation_failed",
  "detail": "The request content is not acceptable.",
  "errors": [{ "field": "payments[1].amount", "code": "out_of_range" }]
}
```

| 状態 | `code` | 意味 |
|---|---|---|
| 400 | `bad_request` | JSON として読めない。必須のヘッダーがない |
| 401 | `unauthorized` | トークンがない、または違う |
| 404 | `not_found` | 対象がない。パスの ID が UUID でない |
| 409 | `conflict` | 現在の状態と合わない（下の各エンドポイントを参照） |
| 413 | `payload_too_large` | 本文が 1 MiB を超える |
| 415 | `unsupported_media_type` | `Content-Type: application/json` がない |
| 422 | `invalid_request` | JSON やクエリの形が違う（型の違い、未知・不足のフィールド）。`detail` に箇所 |
| 422 | `validation_failed` | 値が受け付けられない。`errors` に `field` と `code` |
| 500 | `internal_error` | サーバー側の失敗。原因は返さず、`request_id` を返す |
| 503 | `unavailable` | `/readyz` で DB に届かない |
| 503 | `busy` | 混んでいて受け付けられなかった（消込のロックを 5 秒待っても取れない、DB の接続が空かない）。何も書き込まれていないので、同じ要求をそのまま再送できる。`Retry-After` つき |
| 503 | `timeout` | 処理が 30 秒を超えて打ち切られた。書き込みは取り消されるが、確定（コミット）の最中だった場合だけは反映されていることがあるので、書き込みを再送する前に状態を確かめる（取り込みは同じキーで再送すれば安全）。`Retry-After` つき |

`errors[].code` の値：`required`、`too_long`、`invalid_characters`（制御文字）、`out_of_range`、`unknown_value`、`not_normalizable`、`not_found`、`duplicate`、`different_customers`、`amount_mismatch`、`payer_name_not_normalizable`、`invalid`。

---

## 取引先

### `POST /v1/customers`

```json
{ "name": "株式会社山田商事", "name_kana": "カブシキガイシャ ヤマダショウジ", "fee_deducted": false }
```

| フィールド | 必須 | 内容 |
|---|---|---|
| `name` | ○ | 1〜200 文字 |
| `name_kana` | ○ | 振込名義として使われる読み。1〜200 文字。正規化して空になるもの（法人格だけ、など）は 422 `not_normalizable` |
| `fee_deducted` | | 振込手数料を差し引いて支払う取引先か。既定 `false`。`true` で規則 R3 の対象になる |

201 で取引先を返します。`aliases` の先頭は `name_kana` を正規化したもの（`source: "kana"`）です。

```json
{
  "id": "019a…", "name": "株式会社山田商事", "name_kana": "カブシキガイシャ ヤマダショウジ",
  "fee_deducted": false, "created_at": "2026-11-18T03:00:00.123456Z",
  "aliases": [
    { "id": "019a…", "alias": "カブシキガイシャ ヤマダショウジ", "normalized": "ヤマダシヨウジ",
      "source": "kana", "created_at": "2026-11-18T03:00:00.123456Z" }
  ]
}
```

- 409：正規化した名義が、既にいずれかの取引先を指している。

### `GET /v1/customers/{id}`
上と同じ形。`aliases` は登録順。

### `POST /v1/customers/{id}/aliases`

```json
{ "alias": "ﾔﾏﾀﾞｼﾖｳｼﾞ ｵｵｻｶｼﾃﾝ" }
```

201 で登録した別名（`source: "manual"`）を返します。409：その名義は既に登録済み（同じ取引先でも別の取引先でも）。

### `DELETE /v1/customers/{id}/aliases/{alias_id}`

間違って登録した（または確定時に覚えさせた）別名を削除します。本文は不要で、204 を返します。`alias_id` は `GET /v1/customers/{id}` の `aliases[].id` です。

- 削除した後に取り込む入金と、再判定する入金は、この名義ではその取引先と見なされません。名義は他の取引先に登録できるようになります。
- **この別名を通して既にできた消込や候補は変わりません。** 間違いなら、`POST /v1/matches/{id}/cancel` で取り消してください。
- 409：取引先自身のカナ名（`source: "kana"`）は削除できません。
- 404：その取引先にその別名はない。

---

## 請求

### `POST /v1/invoices`

```json
{ "customer_id": "019a…", "invoice_number": "INV-2026-001", "amount": 110000, "due_date": "2026-11-30" }
```

201 で請求を返します（`status` は `open`）。

- 422 `customer_id: not_found`：取引先がない。
- 409：`invoice_number` が登録済み。無効（`void`）にした請求の番号は、もう一度登録できます。

### `GET /v1/invoices`

| クエリ | 内容 |
|---|---|
| `status` | `open`／`cleared`／`void` |
| `customer_id` | 取引先で絞る |
| `limit` | 1〜200、既定 50 |
| `cursor` | 前のページの `next_cursor` |

```json
{ "items": [ { "id": "019a…", "customer_id": "019a…", "invoice_number": "INV-2026-001",
               "amount": 110000, "due_date": "2026-11-30", "status": "open",
               "created_at": "2026-11-18T03:00:00.123456Z" } ],
  "next_cursor": null }
```

登録順に並びます。`next_cursor` が `null` なら最後のページです。

### `GET /v1/invoices/{id}`

請求に、それを消し込んだ確定済みの消込を加えたものです。「この請求はどの入金で消えたか」を請求の側から辿れます。

```json
{ "id": "019a…", "customer_id": "019a…", "invoice_number": "INV-2026-001",
  "amount": 110000, "due_date": "2026-11-30", "status": "cleared",
  "created_at": "2026-11-18T03:00:00.123456Z",
  "cleared_by": { "match_id": "019a…", "payment_id": "019a…", "rule": "R3", "fee_amount": 440,
                  "amount": 109560, "confirmed_at": "2026-11-18T03:05:00.123456Z" } }
```

`cleared_by` は消込済み（`cleared`）でなければ `null`。`amount` はこの請求に充てた入金額、`fee_amount` は消込全体の手数料です。根拠と履歴は `GET /v1/payments/{payment_id}` で見ます。

### `POST /v1/invoices/{id}/void`

間違って登録した請求を無効にします。本文は不要で、200 で請求（`status: "void"`）を返します。行は消さず、突合の対象から外します。

- 無効にできるのは未消込（`open`）の請求だけです。409：消込済み（先に `POST /v1/matches/{id}/cancel` で消込を取り消す）、または既に無効。
- この請求を含む候補は取消になり（`cancel_reason: "invoice_voided"`）、その入金は `unmatched` に戻ります。`POST /v1/payments/rematch` で判定し直せます。
- 無効にした請求は元に戻せません。同じ `invoice_number` で正しい内容を登録し直してください。
- 無効の請求は自動突合にも手動消込にも使えません（手動消込は 409）。

---

## 入金

### `POST /v1/payment-batches`

ヘッダー `Idempotency-Key`（必須。空白を含まない ASCII、100 文字まで）。銀行明細 1 回分のように、取り込みの単位ごとに決めた値を使います。

```json
{ "payments": [
    { "paid_on": "2026-11-10", "amount": 110000, "payer_name": "ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ" },
    { "paid_on": "2026-11-10", "amount": 88000,  "payer_name": "ﾌﾒｲ ｼﾖｳｶｲ" }
] }
```

- `payments` は 1〜1,000 行。`payer_name` は 200 文字まで。空文字も可（金額だけで判定されます）。
- `allow_duplicate`（省略可、既定 `false`）：取り込み済みのバッチと同じ内容を、別のキーで取り込むことを認めます。
- 行は上から順に判定され、前の行が消し込んだ請求は後の行の対象になりません。

201（新規）または 200（同じキー・同じ内容の再送。`replayed: true`）：

```json
{
  "batch_id": "019a…",
  "replayed": false,
  "summary": { "total": 2, "matched": 1, "suggested": 1, "unmatched": 0 },
  "payments": [
    { "id": "019a…", "batch_id": "019a…", "line_no": 1, "paid_on": "2026-11-10", "amount": 110000,
      "payer_name": "ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ", "payer_name_normalized": "ヤマダシヨウジ", "status": "matched",
      "decision": { "outcome": "auto_confirmed", "rule": "R1", "invoice_ids": ["019a…"], "fee_amount": 0 },
      "decided_at": "2026-11-18T03:00:00.123456Z", "created_at": "2026-11-18T03:00:00.123456Z" }
  ]
}
```

再送のときの `payments` は、そのバッチの現在の状態です（取り込み後に確定・取消していれば反映されています）。

- 400：`Idempotency-Key` がない。
- 409：同じキーが、別の内容で使われている。
- 409：別のキーで、同じ内容（同じ行が同じ順）が取り込み済み。`detail` にそのバッチの ID。同じ明細を 2 回送ったときの二重計上を防ぐためで、本当に同じ内容をもう一度取り込むなら `allow_duplicate: true` をつけます。このとき、キーは消費されません。
- 取り込みが途中で失敗した場合は何も書き込まれず、同じキーで再送できます。

#### `decision`

| `outcome` | 追加のフィールド | 入金の `status` |
|---|---|---|
| `auto_confirmed` | `rule`（`R1`／`R2`）、`invoice_ids`、`fee_amount` | `matched` |
| `suggested` | `rule`（`R1`〜`R4`）、`invoice_ids`、`fee_amount`、`reason` | `suggested` |
| `unmatched` | `reason`（`payer_unknown`／`no_candidate`）、`subset_search_skipped` | `unmatched` |

`suggested` の `reason.kind`：

| `kind` | 意味 |
|---|---|
| `same_amount_invoices` | R1：同額の未消込請求が複数ある（`count`）。最も古いものを提示 |
| `several_combinations` | R2・R3：説明できる組み合わせが複数ある。古い請求を優先したものを提示 |
| `fee_deducted` | R3：振込手数料の分だけ少ない |
| `payer_unknown` | R4：名義から取引先を特定できず、金額だけが一致。`candidate_invoice_ids` に最大 5 件 |

`subset_search_skipped` が `true` のときは、取引先の未消込請求が 20 件を超えていて、組み合わせの探索を行っていません。

`decision` は自動判定の記録で、その後の確定・取消では変わりません。変わるのは再判定（`POST /v1/payments/rematch`）のときだけで、`decided_at` がその判定の時刻です（取り込み時の判定のままなら `created_at` と同じ）。現在の状態は `status` と `matches` で見ます。

同じバッチの後の行が請求を消し込んだために、前の行の候補が取消になることがあります。その行は `decision.outcome` が `suggested` のまま、`status` は `unmatched` になります。

### `POST /v1/payments/rematch`

`unmatched` の入金を、いま登録されている別名と未消込の請求に対して判定し直します。別名を登録した後、請求を後から登録した後、候補が取消になった後に使います。規則は取り込みと同じで、説明が 1 通りなら自動確定、複数なら候補になります。本文は不要です。

| クエリ | 内容 |
|---|---|
| `limit` | 1 回に見る入金の数。1〜200、既定 50 |
| `cursor` | 前回の `next_cursor`。続きの入金を見る |

```json
{
  "summary": { "total": 3, "matched": 1, "suggested": 1, "unmatched": 1 },
  "items": [ { "id": "019a…", "status": "matched", "decision": { "outcome": "auto_confirmed", "rule": "R1", "…": "…" },
               "decided_at": "2026-11-18T04:00:00.123456Z", "…": "…" } ],
  "next_cursor": null
}
```

- `summary.total` は見た入金の数、残りはその結果です。`items` は `unmatched` でなくなった入金（古い順）。
- 入金は古い順に判定し、前の入金が消し込んだ請求は後の入金の対象になりません。見た入金は、結果が変わらなくても `decision` と `decided_at` が更新されます。
- `next_cursor` が `null` でなければ、まだ見ていない `unmatched` の入金があります。
- `suggested` の入金は対象外です。却下（取消）すれば `unmatched` になり、次の再判定で見られます。
- 1 回の呼び出しは 1 トランザクションで、失敗したら何も変わりません。

### `GET /v1/payments`

クエリ：`status`（`unmatched`／`suggested`／`matched`）、`limit`、`cursor`。応答の形は請求の一覧と同じで、`items` は上の入金です。

### `GET /v1/payments/{id}`

入金に、これまでの消込すべて（取消済みを含む。古い順）を加えたものです。確定・手動消込・取消も、この形を返します。

```json
{
  "id": "019a…", "status": "matched", "amount": 109560, "payer_name": "ﾔﾏﾀﾞｼﾖｳｼﾞ", "…": "…",
  "decision": { "outcome": "suggested", "rule": "R3", "invoice_ids": ["019a…"], "fee_amount": 440,
                "reason": { "kind": "fee_deducted" } },
  "decided_at": "2026-11-18T03:00:00.123456Z",
  "matches": [
    { "id": "019a…", "rule": "R3", "state": "confirmed", "fee_amount": 440,
      "created_at": "2026-11-18T03:00:00.123456Z", "confirmed_at": "2026-11-18T03:05:00.123456Z",
      "cancelled_at": null, "cancel_reason": null,
      "allocations": [
        { "invoice_id": "019a…", "invoice_number": "INV-2026-001", "invoice_amount": 110000, "amount": 109560 }
      ] }
  ]
}
```

`matches[].cancel_reason` は取消になった消込の理由です。

| `cancel_reason` | 意味 |
|---|---|
| `dismissed` | 人が候補を却下した |
| `reversed` | 人が確定済みの消込を取り消した |
| `replaced` | 人が候補を確定せず、手動消込をした |
| `invoice_cleared` | 候補の請求が、他の入金で消し込まれた（確定できなくなったので自動で取消） |
| `invoice_voided` | 候補の請求が無効にされた（同上） |
| `null` | 取消されていない。または、理由を記録する前に取消された |

`allocations[].amount` はその請求に充てた入金額で、合計が入金額になります。手数料は支払期日が最も古い請求から引かれます（`invoice_amount - amount` が手数料）。

---

## 消込

### `POST /v1/matches/{id}/confirm`

候補（`state: "suggested"`）を確定します。本文は省略できます。

```json
{ "register_alias": true }
```

`register_alias` を `true` にすると、入金の名義をその取引先の別名（`source: "learned"`）として登録し、次回から名義で特定できるようにします。

請求が消し込まれると（確定・手動消込・自動確定のどれでも）、同じ請求を含む他の入金の候補は取消になり（`cancel_reason: "invoice_cleared"`）、その入金は `unmatched` に戻ります。確定できなくなった候補を残さないためです。戻った入金は `POST /v1/payments/rematch` で判定し直せます。

- 409：消込が `suggested` でない。請求が既に消込済み。`register_alias` の名義が別の取引先のもの。
- 422 `register_alias: payer_name_not_normalizable`：入金に、別名にできる名義がない。

失敗したときは何も変わりません。

### `POST /v1/payments/{id}/matches`

人が請求を選んで消し込みます。入金に候補があれば、その候補は取消になります。

```json
{ "invoice_ids": ["019a…", "019a…"], "fee_amount": 660, "register_alias": false }
```

| フィールド | 必須 | 内容 |
|---|---|---|
| `invoice_ids` | ○ | 1〜50 件、重複なし。すべて同じ取引先の未消込請求 |
| `fee_amount` | | 取引先が差し引いた振込手数料。既定 0 |
| `register_alias` | | 確定と同じ |

「請求額の合計 − `fee_amount` = 入金額」でなければ 422 `invoice_ids: amount_mismatch`。

- 409：入金が既に消込済み。請求が既に消込済み。`register_alias` の名義が別の取引先のもの。
- 422：`invoice_ids` が `out_of_range`／`duplicate`／`not_found`／`different_customers`／`amount_mismatch`、`fee_amount` が `out_of_range`（負、または最も古い請求の額以上）。

### `POST /v1/matches/{id}/cancel`

- 確定済みの消込：請求を `open` に、入金を `unmatched` に戻します。
- 候補：却下します。入金は `unmatched` になります。

消込の行は `state: "cancelled"` で残ります。409：既に取消済み。
