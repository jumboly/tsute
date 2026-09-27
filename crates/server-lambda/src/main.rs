//! Lambda エントリポイント。
//!
//! 1 つのバイナリを 2 つの関数としてデプロイする:
//! - `TSUTE_ROLE=api`   : API Gateway HTTP API (payload v2) と WebSocket API のイベント
//! - `TSUTE_ROLE=admin` : IAM 認証の直接 Invoke 専用（Enrollment Key 発行など）。API Gateway には接続しない。
//!
//! 役割を環境変数で固定し、api 関数が管理イベントを受け付けることはない（権限境界を IAM で明確にするため）。

mod aws;

use std::collections::HashMap;
use std::sync::OnceLock;

use base64::Engine;
use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};
use tsute_server_core::{Config, Core, Request};

type AwsCore = Core<aws::DynamoStore, aws::S3Blob, aws::ApiGwNotifier>;
static CORE: OnceLock<AwsCore> = OnceLock::new();

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("env {k} is required"))
}

async fn init() -> AwsCore {
    let conf = aws_config::load_from_env().await;
    let ws_endpoint = env("WS_MANAGEMENT_ENDPOINT");
    let mgmt_conf = aws_sdk_apigatewaymanagement::config::Builder::from(&conf)
        .endpoint_url(ws_endpoint)
        .build();
    Core::new(
        aws::DynamoStore {
            client: aws_sdk_dynamodb::Client::new(&conf),
            table: env("TABLE_NAME"),
        },
        aws::S3Blob {
            client: aws_sdk_s3::Client::new(&conf),
            bucket: env("BUCKET_NAME"),
        },
        aws::ApiGwNotifier {
            client: aws_sdk_apigatewaymanagement::Client::from_conf(mgmt_conf),
        },
        Config::default(),
    )
}

fn lower_headers(v: &Value) -> HashMap<String, String> {
    v.get("headers")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.to_ascii_lowercase(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

async fn handle_api(core: &AwsCore, ev: Value) -> Result<Value, Error> {
    let rc = &ev["requestContext"];
    if let Some(conn) = rc.get("connectionId").and_then(Value::as_str) {
        // WebSocket
        let conn = conn.to_string();
        return Ok(match rc["eventType"].as_str().unwrap_or("") {
            "CONNECT" => match core.ws_connect(&conn, &lower_headers(&ev)).await {
                Ok(_) => json!({"statusCode": 200}),
                Err(e) => json!({"statusCode": e.status}),
            },
            "DISCONNECT" => {
                core.ws_disconnect(&conn).await.map_err(|e| e.to_string())?;
                json!({"statusCode": 200})
            }
            _ => {
                let body = ev["body"].as_str().unwrap_or("");
                if let Some(reply) = core.ws_message(&conn, body).await {
                    use tsute_server_core::traits::Notifier;
                    let _ = core.notifier.send(&conn, &reply).await;
                }
                json!({"statusCode": 200})
            }
        });
    }
    // HTTP API payload v2
    let method = rc["http"]["method"].as_str().unwrap_or("GET").to_string();
    let path = ev["rawPath"].as_str().unwrap_or("/").to_string();
    let body = match (ev["body"].as_str(), ev["isBase64Encoded"].as_bool()) {
        (Some(b), Some(true)) => base64::engine::general_purpose::STANDARD.decode(b).unwrap_or_default(),
        (Some(b), _) => b.as_bytes().to_vec(),
        (None, _) => vec![],
    };
    let r = core
        .handle_http(Request {
            method,
            path,
            headers: lower_headers(&ev),
            body,
        })
        .await;
    Ok(json!({
        "statusCode": r.status,
        "headers": {"content-type": "application/json", "cache-control": "no-store"},
        "body": String::from_utf8_lossy(&r.body),
        "isBase64Encoded": false,
    }))
}

async fn handle_admin(core: &AwsCore, ev: Value) -> Result<Value, Error> {
    match ev["op"].as_str().unwrap_or("") {
        "issue_enrollment_key" => {
            let (key, exp) = core.issue_enrollment_key().await.map_err(|e| e.to_string())?;
            Ok(json!({"enrollment_key": key, "expires_at": exp}))
        }
        "list_endpoints" => Ok(json!({"endpoints": core.list_endpoints_admin().await.map_err(|e| e.to_string())?})),
        "revoke_endpoint" => {
            let id = ev["endpoint_id"].as_str().ok_or("endpoint_id required")?;
            core.revoke_endpoint(id).await.map_err(|e| e.to_string())?;
            Ok(json!({"ok": true}))
        }
        op => Err(format!("unknown op {op:?}").into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .without_time()
        .with_current_span(false)
        .init();
    let core = init().await;
    let _ = CORE.set(core);
    let role = env("TSUTE_ROLE");
    lambda_runtime::run(service_fn(move |e: LambdaEvent<Value>| {
        let role = role.clone();
        async move {
            let core = CORE.get().expect("core");
            // リクエスト本文やヘッダ（トークン・Clipboard 内容）はログに出さない
            match role.as_str() {
                "admin" => handle_admin(core, e.payload).await,
                _ => handle_api(core, e.payload).await,
            }
        }
    }))
    .await
}
