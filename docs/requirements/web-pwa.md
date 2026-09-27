# Web / PWA Client 追加要件（原文）

ユーザーから 2026-09-27 に与えられた追加要件の原文。要約と完了条件は `docs/REQUIREMENTS.md`、
技術方針は `docs/adr/0015-web-pwa-client.md` を参照。

---

「つて」はmacOS / WindowsのNative Clientに加え、Web BrowserおよびPWAからも利用できるようにする。

Web版の目的は、Native Clientを完全に代替することではない。

主なユースケースは、

- スマートフォンからPCへTextを送る
- スマートフォンのOS標準音声入力で入力したTextをPCへ送る
- BrowserでコピーしたText / Imageを別Endpointへ送る
- Native Applicationをインストールしていない環境から一時的に「つて」を利用する
- PWAとしてホーム画面等から素早く起動する

ことである。

Web / PWA Clientも、Native Clientと同じEndpoint / Authentication / Backend / Message Modelを可能な限り共有する。

ただしBrowserやPWAの制約を無理に回避しようとせず、Native ClientとWeb ClientのCapability差を明確にすること。

## Web Endpoint

Web ClientもEndpointとして扱う。

例:

- MacBook / native
- Windows / native
- iPhone / PWA
- Android / Chrome
- Mac / Safari

物理端末ではなく、そのBrowser / PWA InstanceをEndpointとして扱う。

Web EndpointもNative Endpointと同様に、明示的なEnrollmentを経て登録する。

## Web Endpoint Enrollment

ユーザーが所定のWeb Applicationページを開き、Enrollment Keyを入力することで、そのBrowser / PWAを信頼済みEndpointとして登録できるようにする。

想定フロー:

1. Web Applicationを開く
2. Enrollment Keyを入力
3. Browser内でEndpoint用Credential / Keyを生成
4. Public側の情報をBackendへ登録
5. 以降はそのBrowserを同一Endpointとして認識する

Web Crypto API等を利用したBrowser内鍵生成を有力候補とする。

Private Key等のCredentialは、可能であればextract不可のCryptoKey等として安全に保持する。

IndexedDB等へのCredential保存について、現行Browserの対応状況と制約を調査すること。

Native ClientのKeychain / Credential Storeと同等の永続性を前提にしない。

Browser Dataが削除されCredentialを失った場合は、そのWeb Endpointを再Enrollmentする設計でよい。

Web Clientへ共通秘密鍵や固定Credentialを埋め込まないこと。

## Web Application URL

Web Applicationは、このサービスで使用する単一FQDN配下に配置する。

論理的には例えば以下を想定する。

```
/
    Technical Blog

/app/
    つて Web / PWA

/api/
    HTTP API

/ws
    WebSocket
```

実際のFQDNは固定しない。

APP_BASE_URL等の環境依存設定から導出する。

Technical BlogとWeb Applicationは別Repositoryとして管理してよい。

CloudFront等を使用する場合、Path Routingによってそれぞれ独立してDeploy可能な構成にする。

BlogだけのDeployでWeb ApplicationやBackend全体を再Deployしないこと。

## Web / PWA MVP

Web / PWA版のMVPでは、以下を主対象とする。

- Endpoint Enrollment
- Endpoint一覧取得
- 送信先Endpoint選択
- Text入力
- Text Clipboard
- Image Clipboard
- Text受信
- Image受信
- Clipboardへの明示的Copy
- PWA install
- Foreground時のWebSocket
- Background時のWeb Push
- Android等、対応環境でのWeb Share Target

以下はWeb版MVPの必須要件とはしない。

- Video Clipboard
- Large File Transfer
- Native Clientと同等のBackground常駐
- 常時WebSocket接続
- iOSでのWeb Share Target
- 独自音声認識Engine

将来必要になれば既存のTransfer Engineへ接続してFile Transfer等を追加可能なArchitectureにしておく。

## Text Input

Web版ではText送信を主要機能とする。

UI上にText入力領域を用意し、

- Keyboard入力
- Paste
- Smartphone OSの音声入力

のいずれでも同じText Payloadを作れるようにする。

特にスマートフォンでは、

```
OS Keyboardの音声入力
    ->
Text Area
    ->
送信先Endpoint選択
    ->
内容確認
    ->
Send
```

というユースケースを重要視する。

MVPではBrowser独自の音声認識Engineを必須にしない。

Web Speech API / SpeechRecognition等を利用する場合は、現行BrowserのCompatibility、Privacy、Server-side Recognitionの可能性等を調査し、Progressive Enhancementとして採用する。

対応していないBrowserでもOS Keyboard音声入力によって主要ユースケースを実現できるようにする。

## Web Clipboard

Web Clipboard APIを利用できる場合は利用する。

ただしBrowserごとのPermission、User Activation、Paste Confirmation等の違いを前提とする。

Native Clientのように自由なClipboardアクセスを前提にしない。

最低限、以下の2経路を検討する。

1. 「Clipboardから読み込む」ボタン
2. Text Area / Input AreaへユーザーがPaste

Clipboard Access APIが利用できない環境でも、Paste操作によって主要機能を利用できるようにする。

## Web Clipboard形式

Web / PWA MVPでサポートするClipboard Typeは、

- Text
- Image

とする。

Native Clientで対応するVideo ClipboardはWeb版MVPでは対象外とする。

Browser上のClipboardが複数形式を持つ場合は、Web Clientの内部Payloadへ適切に正規化する。

Text:
- text/plainを基本とする。

Image:
- Browserが安定して取得可能な形式を調査する。
- PNG等を有力候補とする。
- Previewを表示する。
- サイズ、解像度等、取得可能な情報を表示する。

Web Clipboardの形式対応については、実装時点のChrome / Safari / Firefox等の現行仕様を確認する。

## 明示的Send

Native Clientと同様に、自動同期はしない。

Textを入力しただけ、Clipboardを読み込んだだけでは送信しない。

必ず、

1. 内容取得
2. Preview / 内容確認
3. 送信先Endpoint確認
4. Send

というユーザーの明示操作を経る。

「つて」全体の、

「ユーザーが明示的に送ると決めたものだけを送る」

という原則をWeb版でも維持する。

## 受信

Web Clientで受信したText / Imageも、受信しただけでOS Clipboardへ自動反映しない。

ユーザーに内容を表示し、

- ClipboardへCopy
- 必要に応じてImageを保存
- Dismiss

等の明示操作を提供する。

ClipboardへのWriteにもBrowserのUser Activation等が必要になる可能性があるため、現行仕様を確認する。

## PWA

Web ApplicationはPWAとして利用可能にする。

最低限、

- Web App Manifest
- Application Name
- Icons
- Standalone display
- Service Worker
- Installability

を適切に構成する。

iOS / iPadOS / Android等でHome Screenから起動できることを確認する。

各PlatformでInstall UIや挙動が異なることを許容し、無理に完全統一しない。

## Foreground / Background Communication

Native Clientと異なり、PWA / BrowserがBackgroundまたはClosedの状態でWebSocket接続を維持できることを前提にしない。

Foreground:

- WebSocketを使用してリアルタイム通知を受ける。

Background / Closed:

- Web Pushを利用できる環境ではWeb Pushを利用する。

Service WorkerのLifecycleを正しく理解し、常駐Processのような設計にはしない。

Push通知を受けた場合は、ユーザーが通知を開いてWeb Applicationを起動し、その後PayloadをBackendから取得する方式を基本とする。

Push PayloadへClipboard本文や機密情報を不用意に直接含めない。

## iOS / iPadOS Web Push

Home ScreenへInstallされたPWAでWeb Pushを利用できることを前提候補とする。

ただし実装開始時に、最新のWebKit / Safari仕様を改めて確認すること。

Permission取得はユーザーの明示操作に伴って行う。

Web Pushが利用できない環境では、次回Web Applicationを開いた際に未読Messageを取得できるFallbackを用意する。

## Share Target

対応Browser / OSではWeb Share Target APIをProgressive Enhancementとして使用してよい。

ユースケース:

```
他Application
    ->
OS Share
    ->
つて
    ->
送信先Endpoint選択
    ->
Preview
    ->
Send
```

Text、URL、Image等を受け取れる構成を検討する。

ただしWeb Share TargetはCross-browserで一様に利用できる機能ではない。

Android / Chromium等、対応環境でのみ有効化する。

未対応環境では通常のCopy / Paste / Text入力で利用できるようにする。

## iOS Share Target

iOS / iPadOSのPWAがOS Share SheetのShare Targetとして利用できることをMVP要件にしない。

実装開始時点のWebKit対応状況を確認するが、未対応であれば無理にWorkaroundを作らない。

iOSでは、

```
Copy
    ->
つてPWA
    ->
Paste / Clipboard
    ->
Send
```

を基本フローとしてよい。

将来どうしてもShare Sheet連携が必要になった場合は、

- Native Share Extension
- Shortcuts等との連携

を別要件として検討する。

## Web Push Backend

Web Pushを利用する場合、既存BackendへPush Subscription管理を追加する。

Endpointごとに、

- WebSocket Connection
- Web Push Subscription

を別Capabilityとして管理できるようにする。

例:

```
Endpoint
    capabilities:
        native_clipboard
        websocket
        web_push
        web_share_target
        ...
```

Client種類によってCapabilityが異なることを自然に表現できるModelを検討する。

Native / Web / PWAの違いを大量のif文で埋め込むのではなく、Capabilityによる拡張性を検討する。

## Web Security

Web版追加によってAttack Surfaceが広がるため、以下を特に確認する。

- XSS
- CSRF
- CSP
- Secure Cookie
- Web Crypto Key handling
- IndexedDB credential storage
- Push subscription ownership
- Enrollment Token replay
- Endpoint impersonation
- Message ownership / authorization
- Cross-origin access
- Presigned URL exposure
- Clipboard data logging

Text Clipboardはユーザーが意図せず秘密情報を含む可能性が高い。

Clipboard本文やImage内容をApplication Log / Access Log / Analytics等へ不用意に記録しない。

## Offline / Reconnect

Web EndpointがOfflineでも、Message送信側が正常にTransferを作成できるようにする。

Receiverが次回、

- PWAを開く
- Web Pushを受信する
- WebSocketへ再接続する

等した際に未受信Messageを取得できること。

リアルタイム通知そのものをMessage Deliveryの唯一のSource of Truthにしない。

WebSocket / PushはNotificationであり、Transfer / Message StateはBackendに永続化する。

## Capability Model

Native ClientとWeb ClientではCapabilityが異なる。

例えば:

macOS Native:
- text clipboard
- image clipboard
- video clipboard
- file transfer
- large file transfer
- resident process
- websocket

Windows Native:
- text clipboard
- image clipboard
- video clipboard
- file transfer
- large file transfer
- resident process
- websocket

Web/PWA:
- text
- text clipboard
- image clipboard
- foreground websocket
- web push
- optional share target

Endpoint一覧UI等では、送信しようとしているPayloadを受信側Endpointが扱えるか判定できることを検討する。

Capability negotiationの詳細はエージェントが現在のArchitectureと照らして合理的に決定し、ADRに残す。

## Phase Planning

Web / PWA追加によって、現在のPhase 1 macOS Nativeの完了条件を不必要に広げないこと。

まず既存計画どおりmacOS Native ClientのPhase 1を完成させ、実際のE2E確認が可能な状態にすることを優先する。

Web / PWAは独立したPhaseとして扱えるArchitectureにしておく。

macOS Phase 1が完了した後、

- Windows Native
- Web / PWA

のどちらを次に実装するかは、その時点の状況とユーザー方針を確認して決める。

Web対応を理由にmacOS Phase 1が際限なく長期化しないよう注意する。

## Web / PWA Definition of Done

Web / PWA Phaseを実施する場合、最低限以下を実機確認する。

- BrowserからEndpoint Enrollmentできる
- 再起動 / Browser再Open後も同じEndpointとして利用できる
- Smartphone PWAとして起動できる
- TextをPC Endpointへ送れる
- OS Keyboard音声入力したTextをPC Endpointへ送れる
- Textを受信できる
- Clipboardへ明示的Copyできる
- 対応BrowserでImage Clipboardを送受信できる
- Foreground時にWebSocket通知を受けられる
- PWAを閉じた状態で、対応PlatformではWeb Push通知を受けられる
- Offline後に未受信Messageを回収できる
- Android等の対応環境ではShare Targetを実機確認する
- 未対応Browserでも基本のText入力 / Paste / Sendが機能する

単にBrowser上で動いたというだけでは完了としない。

特にスマートフォン実機で、

「音声入力 -> Send -> PCで受信」

という主要ユースケースを確認すること。
