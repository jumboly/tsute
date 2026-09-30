#!/usr/bin/env bash
# 使い方: infra/request-cert.sh <fqdn> <env>
# CloudFront 用の ACM 証明書を us-east-1 に DNS 検証で要求し、外部 DNS に追加すべき検証用 CNAME を表示する。
# DNS 事業者は問わない（レコード追加はユーザーが手動で行う）。
set -euo pipefail
FQDN="${1:?fqdn}"; ENV_NAME="${2:?env (コスト配分タグ env に使う)}"
# CloudFormation の外で作るので、スタックと同じコスト配分タグをここで付ける（ADR-0009）
ARN="$(aws acm request-certificate --region us-east-1 --domain-name "$FQDN" --validation-method DNS \
  --tags Key=app,Value=tsute "Key=env,Value=${ENV_NAME}" --query CertificateArn --output text)"
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
