# ADR-0011: Credential Storage

- 状態: Accepted（2026-09-27）

## 決定

- 既定: macOS Keychain（generic password, service=`dev.tsute.desktop`,
  account=`<profile>|<base_url>|<endpoint_id>`）。profile と接続先で分け、複数 profile・複数環境で衝突しない。
- `--insecure-file-credentials`（開発・自動テスト専用）: profile 配下の 0600 ファイル。
  理由: ad-hoc 署名のアプリは再ビルドのたびにコード署名が変わり、既存 Keychain 項目へのアクセスで
  許可ダイアログが出て無人テストが止まるため。UI に「INSECURE」と表示する。
- Data Protection Keychain（iOS 型）は entitlement と provisioning profile が必要で、Developer ID 署名がない現状では使えない。
  配布用に Developer ID 署名する段階で再検討する。
- Windows: 資格情報マネージャー（DPAPI, `CRED_PERSIST_LOCAL_MACHINE`）。詳細は ADR-0016。
