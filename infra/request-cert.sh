#!/usr/bin/env bash
# 使い方: infra/request-cert.sh <fqdn>
# CloudFront 用の ACM 証明書を us-east-1 に DNS 検証で要求し、外部 DNS に追加すべき検証用 CNAME を表示する。
# DNS 事業者は問わない（レコード追加はユーザーが手動で行う）。
set -euo pipefail
FQDN="${1:?fqdn}"
ARN="$(aws acm request-certificate --region us-east-1 --domain-name "$FQDN" --validation-method DNS --query CertificateArn --output text)"
echo "CertificateArn: $ARN"
for _ in $(seq 1 10); do
  REC="$(aws acm describe-certificate --region us-east-1 --certificate-arn "$ARN" \
    --query "Certificate.DomainValidationOptions[0].ResourceRecord" --output text 2>/dev/null || true)"
  if [[ -n "$REC" && "$REC" != "None" ]]; then break; fi
  sleep 3
done
echo "外部 DNS に次の検証用レコードを追加してください（Name / Type / Value）:"
echo "$REC"
echo "発行状態の確認: aws acm describe-certificate --region us-east-1 --certificate-arn $ARN --query Certificate.Status"
