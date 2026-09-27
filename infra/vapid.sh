#!/usr/bin/env bash
# 使い方: infra/vapid.sh <env> [--rotate]
# Web Push 用の VAPID 秘密鍵を生成し、SSM Parameter Store（SecureString）/tsute/<env>/vapid-private-key に置く。
# 鍵は端末・リポジトリに残さない（一時ファイルは所有者のみ読める権限で作り、登録後すぐ消す）。
# --rotate で作り直すと、既存の Browser の Push 購読はすべて無効になる（各 Browser で通知を有効にし直す）。
set -euo pipefail
cd "$(dirname "$0")/.."

ENV_NAME="${1:?usage: infra/vapid.sh <env> [--rotate]}"
ROTATE="${2:-}"
if [[ -f "infra/env/${ENV_NAME}.env" ]]; then
  set -a; source "infra/env/${ENV_NAME}.env"; set +a
fi
: "${AWS_REGION:?AWS_REGION is required}"
PARAM="/tsute/${ENV_NAME}/vapid-private-key"

if aws ssm get-parameter --region "$AWS_REGION" --name "$PARAM" >/dev/null 2>&1 && [[ "$ROTATE" != "--rotate" ]]; then
  echo "既に存在します: ${PARAM}（作り直す場合は --rotate）"
  exit 0
fi

cargo build -q -p tsute-webpush --bin tsute-vapid-keygen
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
target/debug/tsute-vapid-keygen "$TMP/key"
# file:// で渡し、鍵がプロセス一覧（コマンドライン引数）に現れないようにする
aws ssm put-parameter --region "$AWS_REGION" --name "$PARAM" --type SecureString --overwrite \
  --value "file://$TMP/key" >/dev/null
echo "作成しました: ${PARAM}"
echo "API 関数は起動時に読み込むため、反映には infra/deploy.sh ${ENV_NAME}（または Lambda のコールドスタート）が必要です。"
