#!/usr/bin/env bash
# 使い方:
#   scripts/admin.sh <env> issue-key <namespace>  # Enrollment Key を発行（10分有効・一回限り）。登録した Endpoint は <namespace> に属する
#   scripts/admin.sh <env> list [namespace]       # Endpoint 一覧（所属 Namespace 付き。指定するとその Namespace だけ）
#   scripts/admin.sh <env> revoke <endpoint>      # Endpoint を失効
# 管理経路は IAM 認証による Lambda 直接 Invoke のみ（アプリ独自の管理者パスワードは存在しない。ADR-0002）。
# Namespace 導入前に登録した Endpoint は `default` に属する（ADR-0017）。
set -euo pipefail
cd "$(dirname "$0")/.."
ENV_NAME="${1:?env}"; OP="${2:?op}"
if [[ -f "infra/env/${ENV_NAME}.env" ]]; then set -a; source "infra/env/${ENV_NAME}.env"; set +a; fi
: "${AWS_REGION:?AWS_REGION is required}"
# JSON を文字列連結で組むため、Lambda 側と同じ規則で先に弾く（引用符等で Payload を壊させない）
check_ns() {
  [[ "$1" =~ ^[a-z0-9][a-z0-9_-]{0,63}$ ]] || { echo "invalid namespace: use 1..64 chars of [a-z0-9_-], starting with [a-z0-9]" >&2; exit 2; }
}
case "$OP" in
  issue-key)
    NS="${3:?namespace is required (e.g. default)}"; check_ns "$NS"
    PAYLOAD="{\"op\":\"issue_enrollment_key\",\"namespace\":\"${NS}\"}" ;;
  list)
    if [[ -n "${3:-}" ]]; then check_ns "$3"; PAYLOAD="{\"op\":\"list_endpoints\",\"namespace\":\"$3\"}"
    else PAYLOAD='{"op":"list_endpoints"}'; fi ;;
  revoke) PAYLOAD="{\"op\":\"revoke_endpoint\",\"endpoint_id\":\"${3:?endpoint_id}\"}" ;;
  *) echo "unknown op: $OP" >&2; exit 2 ;;
esac
OUT="$(mktemp)"; trap 'rm -f "$OUT"' EXIT
aws lambda invoke --region "$AWS_REGION" --function-name "tsute-${ENV_NAME}-admin" \
  --cli-binary-format raw-in-base64-out --payload "$PAYLOAD" "$OUT" >/dev/null
cat "$OUT"; echo
