#!/bin/sh
# A walk through the API against a running server (see README, "動かす"). Run from the repository root:
#
#   sh examples/demo.sh
#
# Needs curl only. The Japanese request bodies are sent from files: typed on a command line they are mangled
# by some shells (Git Bash on Windows).
set -eu

BASE="${BASE:-http://127.0.0.1:8080}"
if [ -z "${API_TOKEN:-}" ] && [ -f .env ]; then
  API_TOKEN="$(sed -n 's/^API_TOKEN=//p' .env)"
fi
: "${API_TOKEN:?set API_TOKEN, or run from the repository root where .env is}"

call() { # METHOD PATH [curl arguments...]
  method="$1"; path="$2"; shift 2
  curl -sS -X "$method" "$BASE$path" \
    -H "Authorization: Bearer $API_TOKEN" -H 'Content-Type: application/json' "$@"
  echo
}
first_id() { grep -o '"id":"[^"]*"' | head -n 1 | cut -d'"' -f4; }
first_match_id() { grep -o '"matches":\[{"id":"[^"]*"' | cut -d'"' -f6; }
step() { printf '\n== %s\n' "$1"; }

step "1. 取引先を登録する（振込手数料を差し引いて支払う取引先）"
customer="$(call POST /v1/customers --data-binary @examples/customer.json)"
echo "$customer"
customer_id="$(echo "$customer" | first_id)"
if [ -z "$customer_id" ]; then
  echo "The customer was not registered. The demo needs an empty database:" >&2
  echo "run 'docker compose down -v' and start again (README)." >&2
  exit 1
fi

step "2. 請求を 4 件登録する"
while read -r number amount due; do
  call POST /v1/invoices \
    -d "{\"customer_id\":\"$customer_id\",\"invoice_number\":\"$number\",\"amount\":$amount,\"due_date\":\"$due\"}"
done <<EOF
INV-001 110000 2026-09-30
INV-002 55000 2026-10-31
INV-003 33000 2026-11-30
INV-004 220000 2026-11-30
EOF

step "3. 入金 4 件を取り込む（R2 で自動確定、R3 の候補、R4 の候補、未消込）"
key="demo-$(date +%Y%m%d%H%M%S)"
call POST /v1/payment-batches -H "Idempotency-Key: $key" --data-binary @examples/payments.json

step "4. 同じ Idempotency-Key で再送する（何も書き込まれず replayed: true）"
call POST /v1/payment-batches -H "Idempotency-Key: $key" --data-binary @examples/payments.json |
  grep -o '"replayed":[a-z]*,"summary":{[^}]*}'

step "5. 候補になっている入金"
suggested="$(call GET '/v1/payments?status=suggested')"
echo "$suggested"

step "6. 1 件目の候補（R3：手数料 440 円）を確定する"
payment_id="$(echo "$suggested" | first_id)"
match_id="$(call GET "/v1/payments/$payment_id" | first_match_id)"
call POST "/v1/matches/$match_id/confirm"

step "7. 2 件目の候補（R4：名義不明）を確定し、名義を別名として覚えさせる"
payment_id="$(call GET '/v1/payments?status=suggested' | first_id)"
match_id="$(call GET "/v1/payments/$payment_id" | first_match_id)"
call POST "/v1/matches/$match_id/confirm" -d '{"register_alias":true}'

step "8. 取引先の名義（learned が増えている）"
call GET "/v1/customers/$customer_id"

step "9. 未消込のまま残った入金と請求"
call GET '/v1/payments?status=unmatched'
call GET '/v1/invoices?status=open'
