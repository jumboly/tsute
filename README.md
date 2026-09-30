# つて (tsute)

自分の端末どうしで、Clipboard の内容やファイルを **明示的な操作で** 受け渡す個人用アプリです。
Mac・Windows・iPhone などの間で、場所やネットワークに関係なく使えます。

- **送ると決めたものだけを送る**: Clipboard を監視せず、自動同期もしません。送信するのは「Clipboard を送る…」
  を押して内容を確認し、「送信」を押したときだけです。
- **受け取っただけでは上書きしない**: 受信した内容は、自分で「Clipboard にコピー」または「保存」を押したときに反映されます。
- **自分の AWS に置く**: Backend は利用者自身の AWS アカウントにデプロイします。一般向けのサインアップはありません。
  端末は、管理者が発行する一回限りの Enrollment Key で登録します。

## 1. できること / しないこと

| できること | しないこと |
|---|---|
| Text・画像・動画の Clipboard を送る（受信側でプレビューを確認してから反映） | Clipboard の常時監視・自動同期 |
| Finder / Explorer からドロップしたファイルを送る（複数可。1 回の転送で最大 1000 個・合計 50GiB） | フォルダのドロップ（未対応） |
| 大きいファイルの分割転送。途中で止まっても再開できる。受信時にチェックサムで検証する | 端末間の直接通信（必ず自分の AWS を経由する） |
| iPhone / Android のブラウザ（PWA）から Text・画像を送受信する | PWA での動画・ファイルの受信 |
| Namespace で端末のグループを分ける（別の Namespace の端末は見えず、送信もできない） | 一般ユーザーの登録・ログイン |

## 2. 構成

```
 Mac / Windows アプリ（Tauri）  ─┐
 iPhone / Android（PWA, /app/） ─┼─ HTTPS / WebSocket ─ CloudFront ─ API Gateway ─ Lambda ─ DynamoDB
                                 └─ 転送データ ────────────────────────────────── S3（presigned URL）
 管理者 ── AWS IAM で認証 ── 管理用 Lambda を直接呼び出す（Key の発行・一覧・失効）
```

- **Endpoint**: アプリの「プロファイル」1 つ、または PWA 1 つが 1 つの Endpoint です（物理的な端末と 1 対 1 ではありません）。
  同じ Mac で `--profile` を変えて、複数の Endpoint を同時に動かせます。
- **Namespace**: Endpoint のグループです。Enrollment Key を発行するときに決まり、登録後は変わりません。
  送信先一覧に出るのも、送受信できるのも、同じ Namespace の Endpoint だけです（サーバー側で強制します）。
  Namespace 導入前に登録した Endpoint は `default` に属します。
- **認証**: Endpoint ごとに Ed25519 の鍵を作ります。秘密鍵は OS の資格情報ストアに置きます
  （macOS: Keychain、Windows: 資格情報マネージャー、ブラウザ: 取り出せない CryptoKey）。アプリに共通の秘密は埋め込みません。
- 転送データは受信が完了した時点で S3 から削除します。受け取られなかった転送は 7 日で期限切れになります
  （S3 に残ったデータも、ライフサイクル設定により 8 日で消えます）。

設計の判断は [docs/adr/](docs/adr/README.md) にあります。

## 3. デプロイ

### 前提

- AWS アカウントと、管理者権限のある AWS CLI v2 のログイン（`aws login` など）
- Rust stable、`cargo-lambda`（`uv tool install cargo-lambda`。zig 同梱）
- デスクトップアプリをビルドする場合: Tauri CLI（`cargo install tauri-cli --version "=2.12.0" --locked`）

### 環境ファイル

環境（例: `test`、`production`）ごとに設定ファイルを作ります。このファイルは `.gitignore` 済みです。
ドメインなどの環境固有の値はここにだけ書き、リポジトリには書きません。

```sh
cp infra/env/example.env infra/env/<env>.env
# AWS_REGION などを編集。独自ドメインを使わないなら TSUTE_APP_DOMAIN / TSUTE_CERT_ARN は空のまま
```

### 初回

```sh
# 1. アカウント・リージョンにつき 1 回。成果物バケット、CloudFormation 実行ロール、GitHub Actions 用の OIDC ロールを作る
#    <owner/repo> は GitHub の OIDC subject の形式に合わせる（immutable subject が有効なら owner@ownerId/repo@repoId）
#    確認方法: gh api repos/<owner>/<repo>/actions/oidc/customization/sub の sub_claim_prefix から "repo:" を除いた値
infra/bootstrap.sh <region> <owner/repo>

# 2. Backend（ap-northeast-1 など）と Edge（CloudFront, us-east-1）をデプロイする
#    初回はアプリの URL がまだ決まっていないため、S3 CORS と Web Push の設定が空になる。もう一度実行すると設定される
infra/deploy.sh <env>
infra/deploy.sh <env>

# 3. 任意: Web Push（PWA を閉じている間の通知）を使う場合、VAPID 鍵を作る（SSM に保存され、端末には残らない）
infra/vapid.sh <env>

# 4. Web / PWA クライアントを配信する
infra/deploy-web.sh <env>
```

`infra/deploy.sh` が最後に表示する `APP_BASE_URL`（例: `https://xxxx.cloudfront.net`）がアプリの接続先です。
値は `infra/.build/<env>.json` にも保存されます。

### 独自ドメイン（任意）

ドメインが無くても、CloudFront が割り当てる `*.cloudfront.net` の URL ですべての機能が動きます。
独自ドメインを使う場合の手順です。

```sh
infra/request-cert.sh <fqdn> <env>    # us-east-1 に ACM 証明書を要求し、DNS 検証用の CNAME を表示する
# infra/env/<env>.env に TSUTE_APP_DOMAIN と TSUTE_CERT_ARN を設定する
# DNS に検証用の CNAME を追加する（親ゾーンの DNS 事業者が受け付けない場合は、次のスクリプトでサブドメインを Route 53 に委任する）
infra/route53-subdomain.sh <env>      # 任意: Route 53 に委任し、親ゾーンに追加すべき NS レコードを表示する
infra/deploy.sh <env>                 # 証明書が ISSUED になってから実行する
infra/route53-subdomain.sh <env>      # 委任している場合: CloudFront への ALIAS を更新する
# 委任していない場合は、DNS に <fqdn> → CloudFront のドメインの CNAME を追加する
```

接続先の URL を変えると、Web / PWA は別のオリジンになるため、登録し直しが必要です。
デスクトップアプリは登録時の URL を使い続けます。

### コストの集計

すべてのリソースに、コスト配分タグ `app=tsute` と `env=<env>` を付けます（bootstrap のリソースは `env=shared`）。
CloudFormation のスタックは `deploy.sh` / `bootstrap.sh` が、スタックの外で作るもの（証明書、ホストゾーン、VAPID 鍵）は
それぞれの作成スクリプトがタグを付けます。

1. 請求コンソールの「コスト配分タグ」で、ユーザー定義タグの `app` と `env` を有効にします（初回だけ。タグが付いてから
   候補に現れ、反映まで最大 24 時間かかります）。
2. Cost Explorer で `app` タグ（または `env` タグ）でグループ化すると、アプリ・環境ごとの費用が見られます。

`infra/tag-resources.sh <env> --check` で、タグの付いたリソースを一覧できます。

### 更新

- Backend: `infra/deploy.sh <env>`。main への push で GitHub Actions（`deploy-backend.yml`）からも実行されます。
  CI で使う場合は、GitHub Environment `<env>` に設定値を登録します。`AWS_REGION` は Variables に、
  `AWS_DEPLOY_ROLE_ARN`（bootstrap の出力 `AppDeployRoleArn`）・`TSUTE_APP_DOMAIN`・`TSUTE_CERT_ARN`・`TSUTE_GITHUB_BLOG_REPO`
  は Secrets に置きます。値は `infra/env/<env>.env` とそろえてください（ずれていると、CI のデプロイで CloudFront の設定が書き換わります）。
- Web クライアント: `infra/deploy-web.sh <env>`（Backend には触れません）
- デプロイ済みの版は `<APP_BASE_URL>/api/health` の `commit`（Backend）と、Web の設定画面の「バージョン」で確かめられます。

### アプリのリリース

1. `Cargo.toml` の `[workspace.package]` の `version` を上げ、main にマージします。
2. `git tag v<version> && git push origin v<version>` を実行します。
3. GitHub Actions（`release.yml`）が Mac（Apple Silicon）と Windows のアプリをビルドし、`SHA256SUMS.txt` と一緒に
   リリースの **下書き** に添付します。タグと `version` が一致しないときは失敗します。
4. 下書きの内容（添付ファイル・リリースノート）を確かめ、GitHub 上で公開します。

## 4. 管理

管理操作はすべて、AWS IAM で認証して管理用 Lambda を直接呼び出します。アプリ独自の管理者パスワードや管理画面はありません。

```sh
scripts/admin.sh <env> issue-key <namespace>   # Enrollment Key を発行（10 分有効・一回限り）
scripts/admin.sh <env> list [namespace]        # Endpoint の一覧（Namespace 付き。指定するとその Namespace だけ）
scripts/admin.sh <env> revoke <endpoint_id>    # Endpoint を失効（トークン・未完了の転送・Push 購読も消える）
```

- Namespace 名は `[a-z0-9_-]` の 1〜64 文字で、先頭は英数字です。既存の端末と同じグループに加えるなら、その Namespace
  （導入前から使っているなら `default`）で Key を発行します。
- Endpoint を別の Namespace に移す機能はありません。移したいときは `revoke` し、新しい Namespace の Key で登録し直します。
- 端末を手放すときや、アプリの設定で登録情報を削除したときは `revoke` しておきます（アプリ側で削除しても、サーバーの Endpoint は残るため）。

## 5. クライアントの導入と登録

どのクライアントも、登録画面で **接続先 URL**、**Enrollment Key**、**Endpoint 名** を入力します。
Key は管理者が `issue-key` で発行し、10 分以内に使います。1 つの Key で登録できるのは 1 回だけです。

Mac / Windows のアプリは [Releases](https://github.com/jumboly/tsute/releases) からダウンロードできます
（インストール手順と OS の警告への対処はリリースノートにあります）。リリースのアプリには接続先 URL が入っていないので、
登録画面で自分の環境の `APP_BASE_URL` を入力します。設定画面の「バージョン」に、版番号とコミット（例: `0.1.0 (9c11edf)`）が表示されます。

### Mac

リリースの `Tsute-<版>-macos-arm64.zip`（Apple Silicon 用）を展開して使います。自分でビルドする場合:

```sh
cd apps/desktop && TSUTE_DEFAULT_BASE_URL=<APP_BASE_URL> cargo tauri build --bundles app
open target/release/bundle/macos/Tsute.app
```

- `TSUTE_DEFAULT_BASE_URL` を指定してビルドすると、登録画面の URL 欄にその値が最初から入ります。指定しなければ手で入力します。
- メニューバーに常駐します。ウィンドウを閉じても受信は続きます。終了はメニューの「つて を終了」です。
- 現在は ad-hoc 署名です（配布用の署名と公証は未対応）。ビルドし直すと、初回起動時に Keychain へのアクセス許可ダイアログが出ます。
- OS の通知は ad-hoc 署名では出ないため、受信するとメニューバーのアイコンに印が付きます。
- 設定画面で、受信フォルダ（既定は `~/Downloads/Tsute`。`default` 以外のプロファイルでは `Tsute-<プロファイル名>`）と
  「ログイン時に起動」を変更できます。

### Windows

- リリースの `Tsute-<版>-windows-x64.zip` を展開し、フォルダを `%LOCALAPPDATA%\Programs\tsute\` などに置いて
  `tsute.exe` を実行します。インストーラーはありません。
  手元でビルドする場合は `cd apps/desktop && cargo tauri build --no-bundle` です（Visual Studio Build Tools が必要）。
- 前提は WebView2 ランタイムです（Windows 11 と更新済みの Windows 10 には標準で入っています）。
- コード署名がないため、初回は SmartScreen の警告が出ます。
- 通知領域に常駐します。受信フォルダの既定は `%USERPROFILE%\Downloads\Tsute` です。
- アンインストールはフォルダの削除です。先に設定の「この PC の登録情報を削除」を実行すると、資格情報も消えます。

### iPhone / Android（Web / PWA）

1. ブラウザで `<APP_BASE_URL>/app/` を開きます。
2. iPhone は Safari の共有メニューから「ホーム画面に追加」を選び、ホーム画面のアイコンから開きます。
   Android は Chrome の「インストール」を使います（Android の実機での確認はまだです。[#14](https://github.com/jumboly/tsute/issues/14)）。
3. 登録画面で Endpoint 名と Enrollment Key を入力します（接続先は開いた URL で決まります）。
4. 任意: 設定の「通知を有効にする」を押すと、アプリを閉じている間に届いたものを通知で知らせます（通知に内容は表示されません）。

iOS では、ホーム画面の Web アプリと Safari で保存領域が分かれています。Safari で登録済みでも、ホーム画面のアプリでは登録し直しになります。
PWA で受け取れるのは Text と画像だけです。PWA 宛てには動画やファイルを送れません（送信前に理由が表示されます）。

### 複数のプロファイル（デスクトップ）

同じ PC に別の Endpoint を作るときは、プロファイル名を指定して起動します。プロファイルごとに登録・履歴・受信フォルダが分かれます。

```sh
open -n target/release/bundle/macos/Tsute.app --args --profile work   # Mac
tsute.exe --profile work                                              # Windows
```

## 6. 使い方

### Clipboard を送る

1. 他のアプリで Text・画像・動画をコピーします。
2. つてのウィンドウを開き、**送信先** を選びます。Mac ではメニューバーの「つて を開く」から開けます。
3. **「Clipboard を送る…」** を押します。Clipboard を読むのはこの時点です。ウィンドウを開いただけでは読みません。
4. 確認画面で内容（Text の本文とサイズ、画像の形式・解像度、動画の長さなど）を確かめ、**「送信」** を押します。

PWA では、テキスト欄に入力する（貼り付け・音声入力も可）か「Clipboard から読み込む」を使い、
「内容を確認して送る」→「Send」で送ります。入力しただけでは送りません。

### ファイルを送る

1. Finder / Explorer からファイルをウィンドウにドロップします（複数可）。
2. 確認画面でファイル名・パス・サイズ・個数・合計・送信先を確かめ、**「送信」** を押します。

大きいファイルは分割して転送します。途中でアプリを終了しても、次に起動したときに続きから再開します。

### 受け取る

受信した内容は履歴に表示されます。この時点では、OS の Clipboard は変わりません。

- **「Clipboard にコピー」**: 受信した内容を Clipboard に反映します。動画・ファイルは、ファイルとしてコピーされます。
- **「保存…」**: 画像や動画をファイルとして保存します。
- **「Finder で表示」/「Explorer で表示」**: 受信フォルダに保存されたファイルを表示します。

受信側が起動していなかった場合、送信した内容はサーバーで保持されます（7 日間）。
受信側を次に起動したときに受け取られます。

## 7. 既知の制約と詳しいドキュメント

- Mac の配布用の署名・公証と、Windows のコード署名は未対応です（[#8](https://github.com/jumboly/tsute/issues/8)）。
- フォルダのドロップには未対応です（[#7](https://github.com/jumboly/tsute/issues/7)）。
- Windows 版の未実装・未確認の項目は [#9](https://github.com/jumboly/tsute/issues/9) と [#10](https://github.com/jumboly/tsute/issues/10) にあります。
- 課題の一覧は [GitHub issues](https://github.com/jumboly/tsute/issues) にあります。

| ドキュメント | 内容 |
|---|---|
| [docs/TESTING.md](docs/TESTING.md) | 開発者向け: ビルド、テスト、E2E、ローカル開発サーバー |
| [docs/adr/](docs/adr/README.md) | 設計判断（認証、転送、データモデル、Namespace など） |
| [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) | 要件と完了条件 |
| [docs/PROGRESS.md](docs/PROGRESS.md) | 現在の進捗と既知の問題 |
