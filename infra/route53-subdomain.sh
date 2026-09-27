#!/usr/bin/env bash
# 使い方: infra/route53-subdomain.sh <env>
# infra/env/<env>.env の TSUTE_APP_DOMAIN を Route 53 のホストゾーンとして用意し（なければ作成）、
#   - TSUTE_CERT_ARN の ACM DNS 検証用 CNAME
#   - deploy.sh 実行後なら CloudFront への ALIAS（A / AAAA）
# を UPSERT する。最後に、親ドメイン側の DNS に追加すべき NS レコードを表示する。
#
# 親ゾーンの DNS 事業者が「解決できない CNAME 値」を拒否する場合（ACM 検証用 CNAME は A を持たない）に、
# サブドメインだけ Route 53 に委任して回避するため。親ゾーンの操作はユーザーが手動で行う。
# ドメイン名は環境ファイル（gitignore 済み）からのみ読み、リポジトリに残さない。
# 何度実行しても同じ結果になる（UPSERT）ので、証明書の再発行やデプロイ後に再実行してよい。
set -euo pipefail
cd "$(dirname "$0")/.."

ENV_NAME="${1:?usage: infra/route53-subdomain.sh <env>}"
set -a; source "infra/env/${ENV_NAME}.env"; set +a
DOMAIN="${TSUTE_APP_DOMAIN:?TSUTE_APP_DOMAIN is required}"
# CloudFront の ALIAS 先ホストゾーン ID は AWS 全体で固定値
CLOUDFRONT_ZONE_ID=Z2FDTNDATAQYW2

ZONE_ID="$(aws route53 list-hosted-zones-by-name --dns-name "${DOMAIN}." --max-items 1 \
  --query "HostedZones[?Name=='${DOMAIN}.'].Id | [0]" --output text)"
if [[ -z "$ZONE_ID" || "$ZONE_ID" == "None" ]]; then
  echo "==> Creating hosted zone"
  ZONE_ID="$(aws route53 create-hosted-zone --name "$DOMAIN" \
    --caller-reference "tsute-${ENV_NAME}-$(date +%s)" \
    --hosted-zone-config Comment="tsute ${ENV_NAME}" \
    --query HostedZone.Id --output text)"
fi
ZONE_ID="${ZONE_ID#/hostedzone/}"

CHANGES=()
if [[ -n "${TSUTE_CERT_ARN:-}" ]]; then
  read -r VNAME VVALUE < <(aws acm describe-certificate --region us-east-1 --certificate-arn "$TSUTE_CERT_ARN" \
    --query "Certificate.DomainValidationOptions[0].ResourceRecord.[Name,Value]" --output text)
  CHANGES+=("{\"Action\":\"UPSERT\",\"ResourceRecordSet\":{\"Name\":\"${VNAME}\",\"Type\":\"CNAME\",\"TTL\":300,\"ResourceRecords\":[{\"Value\":\"${VVALUE}\"}]}}")
fi
BUILD_JSON="infra/.build/${ENV_NAME}.json"
if [[ -f "$BUILD_JSON" ]]; then
  DIST_DOMAIN="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["cloudfront_domain"])' "$BUILD_JSON")"
  for T in A AAAA; do
    CHANGES+=("{\"Action\":\"UPSERT\",\"ResourceRecordSet\":{\"Name\":\"${DOMAIN}.\",\"Type\":\"${T}\",\"AliasTarget\":{\"HostedZoneId\":\"${CLOUDFRONT_ZONE_ID}\",\"DNSName\":\"${DIST_DOMAIN}.\",\"EvaluateTargetHealth\":false}}}")
  done
fi
if (( ${#CHANGES[@]} > 0 )); then
  echo "==> Upserting ${#CHANGES[@]} record(s)"
  BATCH="{\"Changes\":[$(IFS=,; echo "${CHANGES[*]}")]}"
  aws route53 change-resource-record-sets --hosted-zone-id "$ZONE_ID" --change-batch "$BATCH" \
    --query ChangeInfo.Status --output text
fi

echo "==> 親ドメイン側の DNS に、次の NS レコードを追加してください（Name / Type / Value）:"
aws route53 get-hosted-zone --id "$ZONE_ID" --query "DelegationSet.NameServers" --output text \
  | tr '\t' '\n' | sed "s|^|${DOMAIN}.  NS  |; s|\$|.|"
