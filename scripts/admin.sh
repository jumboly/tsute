#!/usr/bin/env bash
# 使い方:
#   scripts/admin.sh <env> issue-key          # Enrollment Key を発行（10分有効・一回限り）
#   scripts/admin.sh <env> list               # Endpoint 一覧
#   scripts/admin.sh <env> revoke <endpoint>  # Endpoint を失効
# 管理経路は IAM 認証による Lambda 直接 Invoke のみ（アプリ独自の管理者パスワードは存在しない。ADR-0002）。
set -euo pipefail
cd "$(dirname "$0")/.."
ENV_NAME="${1:?env}"; OP="${2:?op}"
if [[ -f "infra/env/${ENV_NAME}.env" ]]; then set -a; source "infra/env/${ENV_NAME}.env"; set +a; fi
: "${AWS_REGION:?AWS_REGION is required}"
case "$OP" in
  issue-key) PAYLOAD='{"op":"issue_enrollment_key"}' ;;
  list) PAYLOAD='{"op":"list_endpoints"}' ;;
  revoke) PAYLOAD="{\"op\":\"revoke_endpoint\",\"endpoint_id\":\"${3:?endpoint_id}\"}" ;;
  *) echo "unknown op: $OP" >&2; exit 2 ;;
esac
OUT="$(mktemp)"; trap 'rm -f "$OUT"' EXIT
aws lambda invoke --region "$AWS_REGION" --function-name "tsute-${ENV_NAME}-admin" \
  --cli-binary-format raw-in-base64-out --payload "$PAYLOAD" "$OUT" >/dev/null
cat "$OUT"; echo
