#!/usr/bin/env bash
# 使い方: infra/tag-resources.sh <env> [--check]
# CloudFormation の外で作ったリソース（ACM 証明書・Route 53 ホストゾーン・VAPID 鍵の SSM パラメータ）に
# コスト配分タグ app=tsute / env=<env> を付ける（ADR-0009）。スタック内のリソースは deploy.sh がスタックのタグで付ける。
# 作成用のスクリプトは作成時に付けるので、これはタグ導入前に作ったものへの一度きりの付け直し用。何度実行してもよい。
# --check は付けずに、タグの付いたリソースの一覧だけを表示する（リソース名の ARN を表示する。ドメイン名は出さない）。
set -euo pipefail
cd "$(dirname "$0")/.."

ENV_NAME="${1:?usage: infra/tag-resources.sh <env> [--check]}"
MODE="${2:-}"
if [[ -f "infra/env/${ENV_NAME}.env" ]]; then
  set -a; source "infra/env/${ENV_NAME}.env"; set +a
fi
: "${AWS_REGION:?AWS_REGION is required}"
TAGS=(Key=app,Value=tsute "Key=env,Value=${ENV_NAME}")

if [[ "$MODE" != "--check" ]]; then
  if [[ -n "${TSUTE_CERT_ARN:-}" ]]; then
    aws acm add-tags-to-certificate --region us-east-1 --certificate-arn "$TSUTE_CERT_ARN" --tags "${TAGS[@]}"
    echo "tagged: ACM certificate"
  fi
  if [[ -n "${TSUTE_APP_DOMAIN:-}" ]]; then
    ZONE_ID="$(aws route53 list-hosted-zones-by-name --dns-name "${TSUTE_APP_DOMAIN}." --max-items 1 \
      --query "HostedZones[?Name=='${TSUTE_APP_DOMAIN}.'].Id | [0]" --output text)"
    # 委任せず親ゾーンに直接 CNAME を置く構成では、このアカウントにホストゾーンは無い
    if [[ -n "$ZONE_ID" && "$ZONE_ID" != "None" ]]; then
      aws route53 change-tags-for-resource --resource-type hostedzone --resource-id "${ZONE_ID#/hostedzone/}" \
        --add-tags "${TAGS[@]}"
      echo "tagged: Route 53 hosted zone"
    fi
  fi
  PARAM="/tsute/${ENV_NAME}/vapid-private-key"
  if aws ssm get-parameter --region "$AWS_REGION" --name "$PARAM" >/dev/null 2>&1; then
    aws ssm add-tags-to-resource --region "$AWS_REGION" --resource-type Parameter --resource-id "$PARAM" \
      --tags "${TAGS[@]}"
    echo "tagged: SSM parameter ${PARAM}"
  fi
fi

# Route 53 はグローバルなサービスで、Resource Groups Tagging API では us-east-1 から見える
echo "==> Resources tagged app=tsute, env=${ENV_NAME}"
for R in $(printf '%s\n' "$AWS_REGION" us-east-1 | sort -u); do
  aws resourcegroupstaggingapi get-resources --region "$R" \
    --tag-filters Key=app,Values=tsute "Key=env,Values=${ENV_NAME}" \
    --query "ResourceTagMappingList[].ResourceARN" --output text | tr '\t' '\n' | sed '/^$/d'
done
