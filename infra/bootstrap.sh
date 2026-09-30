#!/usr/bin/env bash
# 使い方: infra/bootstrap.sh <region> <github-owner/app-repo> [create-oidc-provider=true|false]
# アカウント+リージョンにつき一度、管理者が実行する（CI 用の OIDC ロール・成果物バケット・CFN 実行ロール）。
# 環境をまたいで共有するので、コスト配分タグの env は shared にする（ADR-0009）。
set -euo pipefail
cd "$(dirname "$0")/.."
REGION="${1:?region}"; REPO="${2:?owner/repo}"; CREATE_OIDC="${3:-true}"
aws cloudformation deploy --region "$REGION" --stack-name tsute-bootstrap \
  --template-file infra/cloudformation/bootstrap.yaml \
  --capabilities CAPABILITY_NAMED_IAM --no-fail-on-empty-changeset \
  --tags app=tsute env=shared \
  --parameter-overrides GitHubAppRepo="$REPO" CreateOidcProvider="$CREATE_OIDC"
aws cloudformation describe-stacks --region "$REGION" --stack-name tsute-bootstrap --query "Stacks[0].Outputs" --output table
