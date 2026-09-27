#!/usr/bin/env bash
# 使い方: infra/deploy-web.sh <env>
# Web / PWA Client（web/）を App バケットの app/ 配下へ同期し、/app/* だけを invalidate する。
# Backend や Blog は再デプロイしない（ADR-0015 §1 の独立デプロイ）。
# 成果物は環境非依存（URL は location.origin から導出する）なので、同じ web/ をどの環境にも配れる。
set -euo pipefail
cd "$(dirname "$0")/.."

ENV_NAME="${1:?usage: infra/deploy-web.sh <env>}"
if [[ -f "infra/env/${ENV_NAME}.env" ]]; then
  set -a; source "infra/env/${ENV_NAME}.env"; set +a
fi
EDGE_REGION=us-east-1
out() { aws cloudformation describe-stacks --region "$EDGE_REGION" --stack-name "tsute-${ENV_NAME}-edge" --query "Stacks[0].Outputs[?OutputKey=='$1'].OutputValue" --output text; }
BUCKET="${TSUTE_APP_BUCKET:-$(out AppBucketName)}"
DIST="${TSUTE_DISTRIBUTION_ID:-$(out DistributionId)}"

# HTML・Service Worker・manifest は毎回取り直させ（新しい版を確実に配る）、その他は短めにキャッシュする。
# ファイル名にハッシュを付けないビルドなしの構成なので、長期キャッシュ（immutable）は使わない
aws s3 sync web/ "s3://${BUCKET}/app/" --delete --only-show-errors \
  --exclude "*" --include "*.html" --include "sw.js" --include "*.webmanifest" \
  --cache-control "no-cache"
aws s3 sync web/ "s3://${BUCKET}/app/" --delete --only-show-errors \
  --exclude "*.html" --exclude "sw.js" --exclude "*.webmanifest" --exclude ".*" \
  --cache-control "public, max-age=300"
# .webmanifest は拡張子から MIME が推定されないことがあるため明示する
aws s3 cp web/manifest.webmanifest "s3://${BUCKET}/app/manifest.webmanifest" --only-show-errors \
  --content-type "application/manifest+json" --cache-control "no-cache"
aws cloudfront create-invalidation --distribution-id "$DIST" --paths "/app" "/app/*" --query Invalidation.Id --output text
echo "==> Deployed web client to /app/"
