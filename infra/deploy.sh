#!/usr/bin/env bash
# 使い方: infra/deploy.sh <env>
# backend スタック（AWS_REGION）と edge スタック（us-east-1）をデプロイし、APP_BASE_URL を表示する。
# 事前に infra/bootstrap.sh を一度実行しておくこと。
set -euo pipefail
cd "$(dirname "$0")/.."

ENV_NAME="${1:?usage: infra/deploy.sh <env>}"
if [[ -f "infra/env/${ENV_NAME}.env" ]]; then
  set -a; source "infra/env/${ENV_NAME}.env"; set +a
fi
: "${AWS_REGION:?AWS_REGION is required}"
EDGE_REGION=us-east-1
ACCOUNT_ID="$(aws sts get-caller-identity --query Account --output text)"
ARTIFACT_BUCKET="tsute-artifacts-${ACCOUNT_ID}-${AWS_REGION}"
CFN_ROLE_ARN="arn:aws:iam::${ACCOUNT_ID}:role/tsute-cfn-exec"
OUT_DIR="infra/.build"
mkdir -p "$OUT_DIR"

echo "==> Building Lambda (arm64)"
if ! command -v zig >/dev/null; then
  # uv でインストールした cargo-lambda 同梱の zig を使う
  ZIG_DIR="$(dirname "$(find "$HOME/.local/share/uv/tools/cargo-lambda" -name zig -type f 2>/dev/null | head -1)")"
  export PATH="$ZIG_DIR:$PATH"
fi
# ホスト向け release ビルド（デスクトップ）と proc-macro 成果物が衝突しないよう target を分ける
export CARGO_TARGET_DIR=target/lambda-build
cargo lambda build --release --arm64 -p tsute-server-lambda --output-format zip
ZIP=target/lambda-build/lambda/tsute-lambda/bootstrap.zip
# 内容ハッシュをキーにして、コード変更時だけ Lambda が更新されるようにする
KEY="lambda/tsute-$(shasum -a 256 "$ZIP" | cut -c1-16).zip"
aws s3 cp --region "$AWS_REGION" --only-show-errors "$ZIP" "s3://${ARTIFACT_BUCKET}/${KEY}"

echo "==> Deploying backend stack (tsute-${ENV_NAME}-backend, ${AWS_REGION})"
aws cloudformation deploy --region "$AWS_REGION" \
  --stack-name "tsute-${ENV_NAME}-backend" \
  --template-file infra/cloudformation/backend.yaml \
  --role-arn "$CFN_ROLE_ARN" \
  --capabilities CAPABILITY_IAM \
  --no-fail-on-empty-changeset \
  --parameter-overrides EnvName="$ENV_NAME" ArtifactBucket="$ARTIFACT_BUCKET" LambdaS3Key="$KEY"

out() { aws cloudformation describe-stacks --region "$1" --stack-name "$2" --query "Stacks[0].Outputs[?OutputKey=='$3'].OutputValue" --output text; }
HTTP_DOMAIN="$(out "$AWS_REGION" "tsute-${ENV_NAME}-backend" HttpApiDomain)"
WS_DOMAIN="$(out "$AWS_REGION" "tsute-${ENV_NAME}-backend" WsApiDomain)"

echo "==> Deploying edge stack (tsute-${ENV_NAME}-edge, ${EDGE_REGION})"
aws cloudformation deploy --region "$EDGE_REGION" \
  --stack-name "tsute-${ENV_NAME}-edge" \
  --template-file infra/cloudformation/edge.yaml \
  --role-arn "$CFN_ROLE_ARN" \
  --capabilities CAPABILITY_NAMED_IAM \
  --no-fail-on-empty-changeset \
  --parameter-overrides EnvName="$ENV_NAME" HttpApiDomain="$HTTP_DOMAIN" WsApiDomain="$WS_DOMAIN" \
    AppDomainName="${TSUTE_APP_DOMAIN:-}" CertificateArn="${TSUTE_CERT_ARN:-}" GitHubBlogRepo="${TSUTE_GITHUB_BLOG_REPO:-}"

APP_BASE_URL="$(out "$EDGE_REGION" "tsute-${ENV_NAME}-edge" AppBaseUrl)"
DIST_DOMAIN="$(out "$EDGE_REGION" "tsute-${ENV_NAME}-edge" DistributionDomainName)"
cat > "$OUT_DIR/${ENV_NAME}.json" <<JSON
{
  "env": "${ENV_NAME}",
  "app_base_url": "${APP_BASE_URL}",
  "cloudfront_domain": "${DIST_DOMAIN}",
  "distribution_id": "$(out "$EDGE_REGION" "tsute-${ENV_NAME}-edge" DistributionId)",
  "blog_bucket": "$(out "$EDGE_REGION" "tsute-${ENV_NAME}-edge" BlogBucketName)",
  "admin_function": "$(out "$AWS_REGION" "tsute-${ENV_NAME}-backend" AdminFunctionName)",
  "region": "${AWS_REGION}"
}
JSON
echo "==> Done"
echo "APP_BASE_URL=${APP_BASE_URL}"
if [[ -n "${TSUTE_APP_DOMAIN:-}" ]]; then
  echo "外部 DNS に次の CNAME が必要です: ${TSUTE_APP_DOMAIN}  CNAME  ${DIST_DOMAIN}"
fi
