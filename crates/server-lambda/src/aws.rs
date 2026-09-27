//! DynamoDB / S3 / API Gateway Management API による trait 実装（データモデルは ADR-0006）。

use std::collections::HashMap;
use std::time::Duration;

use aws_sdk_dynamodb::types::{AttributeValue, ReturnValue};
use aws_sdk_s3::presigning::PresigningConfig;
use tsute_proto::{ChunkInfo, Platform, ServerEvent, Transfer, TransferState};
use tsute_server_core::traits::*;

type Item = HashMap<String, AttributeValue>;

fn s(v: impl Into<String>) -> AttributeValue {
    AttributeValue::S(v.into())
}
fn n(v: i64) -> AttributeValue {
    AttributeValue::N(v.to_string())
}
fn get_s(i: &Item, k: &str) -> Result<String> {
    i.get(k)
        .and_then(|v| v.as_s().ok())
        .cloned()
        .ok_or_else(|| format!("missing attr {k}").into())
}
fn get_n(i: &Item, k: &str) -> Result<i64> {
    i.get(k)
        .and_then(|v| v.as_n().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("missing attr {k}").into())
}
fn state_str(st: TransferState) -> String {
    serde_json::to_value(st)
        .expect("state")
        .as_str()
        .expect("str")
        .to_string()
}

pub struct DynamoStore {
    pub client: aws_sdk_dynamodb::Client,
    pub table: String,
}

impl DynamoStore {
    fn key(pk: &str, sk: &str) -> Item {
        HashMap::from([("pk".into(), s(pk)), ("sk".into(), s(sk))])
    }

    async fn put(&self, mut item: Item, pk: &str, sk: &str) -> Result<()> {
        item.insert("pk".into(), s(pk));
        item.insert("sk".into(), s(sk));
        self.client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item))
            .send()
            .await?;
        Ok(())
    }

    async fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>> {
        let r = self
            .client
            .get_item()
            .table_name(&self.table)
            .set_key(Some(Self::key(pk, sk)))
            .consistent_read(true)
            .send()
            .await?;
        Ok(r.item)
    }

    async fn query(&self, pk: &str, sk_prefix: Option<&str>) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        let mut start: Option<Item> = None;
        loop {
            let mut q = self
                .client
                .query()
                .table_name(&self.table)
                .consistent_read(true)
                .expression_attribute_values(":pk", s(pk))
                .set_exclusive_start_key(start.take());
            q = match sk_prefix {
                Some(p) => q
                    .key_condition_expression("pk = :pk AND begins_with(sk, :p)")
                    .expression_attribute_values(":p", s(p)),
                None => q.key_condition_expression("pk = :pk"),
            };
            let r = q.send().await?;
            out.extend(r.items.unwrap_or_default());
            match r.last_evaluated_key {
                Some(k) => start = Some(k),
                None => return Ok(out),
            }
        }
    }

    /// 未失効なら削除して削除前の item を返す（一回限りの消費）
    async fn consume(&self, pk: &str, now: i64) -> Result<Option<Item>> {
        let r = self
            .client
            .delete_item()
            .table_name(&self.table)
            .set_key(Some(Self::key(pk, "-")))
            .condition_expression("attribute_exists(pk) AND #t > :now")
            .expression_attribute_names("#t", "ttl")
            .expression_attribute_values(":now", n(now))
            .return_values(ReturnValue::AllOld)
            .send()
            .await;
        match r {
            Ok(o) => Ok(o.attributes),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn transfer_from(item: &Item) -> Result<Transfer> {
        let mut t: Transfer = serde_json::from_str(&get_s(item, "json")?)?;
        // state は条件付き更新の対象なので JSON ではなく独立属性を正とする
        t.state = serde_json::from_value(serde_json::Value::String(get_s(item, "state")?))?;
        Ok(t)
    }
}

impl Store for DynamoStore {
    async fn put_enrollment_key(&self, h: &str, exp: i64) -> Result<()> {
        self.put(HashMap::from([("ttl".into(), n(exp))]), &format!("EKEY#{h}"), "-")
            .await
    }
    async fn consume_enrollment_key(&self, h: &str, now: i64) -> Result<bool> {
        Ok(self.consume(&format!("EKEY#{h}"), now).await?.is_some())
    }
    async fn put_endpoint(&self, ep: &EndpointRecord) -> Result<()> {
        let platform = serde_json::to_value(ep.platform)?
            .as_str()
            .unwrap_or("other")
            .to_string();
        self.put(
            HashMap::from([
                ("endpoint_id".into(), s(&ep.endpoint_id)),
                ("name".into(), s(&ep.name)),
                ("platform".into(), s(platform)),
                ("public_key".into(), s(&ep.public_key)),
                ("created_at".into(), n(ep.created_at)),
            ]),
            "ENDPOINTS",
            &format!("EP#{}", ep.endpoint_id),
        )
        .await
    }
    async fn get_endpoint(&self, id: &str) -> Result<Option<EndpointRecord>> {
        self.get("ENDPOINTS", &format!("EP#{id}"))
            .await?
            .map(|i| endpoint_from(&i))
            .transpose()
    }
    async fn list_endpoints(&self) -> Result<Vec<EndpointRecord>> {
        self.query("ENDPOINTS", Some("EP#"))
            .await?
            .iter()
            .map(endpoint_from)
            .collect()
    }
    async fn delete_endpoint(&self, id: &str) -> Result<()> {
        self.client
            .delete_item()
            .table_name(&self.table)
            .set_key(Some(Self::key("ENDPOINTS", &format!("EP#{id}"))))
            .send()
            .await?;
        Ok(())
    }
    async fn put_challenge(&self, nonce: &str, ep: &str, exp: i64) -> Result<()> {
        self.put(
            HashMap::from([("endpoint_id".into(), s(ep)), ("ttl".into(), n(exp))]),
            &format!("CHAL#{nonce}"),
            "-",
        )
        .await
    }
    async fn consume_challenge(&self, nonce: &str, ep: &str, now: i64) -> Result<bool> {
        // endpoint_id も条件に含め、他 Endpoint 宛の nonce を消費できないようにする
        let r = self
            .client
            .delete_item()
            .table_name(&self.table)
            .set_key(Some(Self::key(&format!("CHAL#{nonce}"), "-")))
            .condition_expression("attribute_exists(pk) AND #t > :now AND endpoint_id = :ep")
            .expression_attribute_names("#t", "ttl")
            .expression_attribute_values(":now", n(now))
            .expression_attribute_values(":ep", s(ep))
            .send()
            .await;
        match r {
            Ok(_) => Ok(true),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }
    async fn put_token(&self, h: &str, ep: &str, exp: i64) -> Result<()> {
        self.put(
            HashMap::from([("endpoint_id".into(), s(ep)), ("ttl".into(), n(exp))]),
            &format!("TOKEN#{h}"),
            "-",
        )
        .await
    }
    async fn get_token(&self, h: &str, now: i64) -> Result<Option<String>> {
        match self.get(&format!("TOKEN#{h}"), "-").await? {
            Some(i) if get_n(&i, "ttl")? > now => Ok(Some(get_s(&i, "endpoint_id")?)),
            _ => Ok(None),
        }
    }
    async fn delete_tokens_of(&self, _ep: &str) -> Result<()> {
        // トークンは endpoint_id で索引していない。authenticate() が毎回 Endpoint の存在を確認するため、
        // Endpoint 削除の時点で実質無効になり、残った item は TTL(1時間) で消える。
        Ok(())
    }
    async fn put_connection(&self, c: &ConnectionRecord) -> Result<()> {
        self.put(
            HashMap::from([
                ("endpoint_id".into(), s(&c.endpoint_id)),
                ("connected_at".into(), n(c.connected_at)),
                ("ttl".into(), n(c.expires_at)),
            ]),
            "CONNS",
            &format!("C#{}", c.connection_id),
        )
        .await
    }
    async fn delete_connection(&self, id: &str) -> Result<Option<String>> {
        let r = self
            .client
            .delete_item()
            .table_name(&self.table)
            .set_key(Some(Self::key("CONNS", &format!("C#{id}"))))
            .return_values(ReturnValue::AllOld)
            .send()
            .await?;
        Ok(r.attributes.and_then(|a| get_s(&a, "endpoint_id").ok()))
    }
    async fn list_connections(&self, now: i64) -> Result<Vec<ConnectionRecord>> {
        let mut out = Vec::new();
        for i in self.query("CONNS", Some("C#")).await? {
            let exp = get_n(&i, "ttl")?;
            if exp <= now {
                continue;
            }
            out.push(ConnectionRecord {
                connection_id: get_s(&i, "sk")?.trim_start_matches("C#").to_string(),
                endpoint_id: get_s(&i, "endpoint_id")?,
                connected_at: get_n(&i, "connected_at").unwrap_or(0),
                expires_at: exp,
            });
        }
        Ok(out)
    }
    async fn put_transfer(&self, t: &Transfer) -> Result<()> {
        self.put(
            HashMap::from([
                ("json".into(), s(serde_json::to_string(t)?)),
                ("state".into(), s(state_str(t.state))),
                ("ttl".into(), n(t.expires_at)),
            ]),
            "TRANSFERS",
            &format!("T#{}", t.transfer_id),
        )
        .await
    }
    async fn get_transfer(&self, id: &str) -> Result<Option<Transfer>> {
        let Some(item) = self.get("TRANSFERS", &format!("T#{id}")).await? else {
            return Ok(None);
        };
        let mut t = Self::transfer_from(&item)?;
        for f in self.query(&format!("XFER#{id}"), Some("F#")).await? {
            let idx: usize = get_s(&f, "sk")?.trim_start_matches("F#").parse()?;
            if let Some(file) = t.files.get_mut(idx) {
                file.sha256 = Some(get_s(&f, "sha256")?);
            }
        }
        Ok(Some(t))
    }
    async fn list_transfers(&self, now: i64) -> Result<Vec<Transfer>> {
        let mut out = Vec::new();
        for i in self.query("TRANSFERS", Some("T#")).await? {
            if get_n(&i, "ttl")? <= now {
                continue;
            }
            let t = Self::transfer_from(&i)?;
            // finalize 済み sha は一覧では不要（詳細取得時に付与）
            out.push(t);
        }
        Ok(out)
    }
    async fn transition(&self, id: &str, from: &[TransferState], to: TransferState) -> Result<bool> {
        let mut req = self
            .client
            .update_item()
            .table_name(&self.table)
            .set_key(Some(Self::key("TRANSFERS", &format!("T#{id}"))))
            .update_expression("SET #s = :to")
            .expression_attribute_names("#s", "state")
            .expression_attribute_values(":to", s(state_str(to)));
        let mut conds = Vec::new();
        for (i, f) in from.iter().enumerate() {
            conds.push(format!(":f{i}"));
            req = req.expression_attribute_values(format!(":f{i}"), s(state_str(*f)));
        }
        req = req.condition_expression(format!("attribute_exists(pk) AND #s IN ({})", conds.join(",")));
        match req.send().await {
            Ok(_) => Ok(true),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }
    async fn put_file_sha(&self, id: &str, file: u32, sha: &str, exp: i64) -> Result<()> {
        self.put(
            HashMap::from([("sha256".into(), s(sha)), ("ttl".into(), n(exp))]),
            &format!("XFER#{id}"),
            &format!("F#{file}"),
        )
        .await
    }
    async fn put_chunk(&self, id: &str, c: &ChunkInfo, exp: i64) -> Result<()> {
        self.put(
            HashMap::from([
                ("file".into(), n(c.file as i64)),
                ("index".into(), n(c.index as i64)),
                ("size".into(), n(c.size as i64)),
                ("sha256".into(), s(&c.sha256)),
                ("ttl".into(), n(exp)),
            ]),
            &format!("XFER#{id}"),
            &format!("C#{:05}#{:07}", c.file, c.index),
        )
        .await
    }
    async fn list_chunks(&self, id: &str) -> Result<Vec<ChunkInfo>> {
        self.query(&format!("XFER#{id}"), Some("C#"))
            .await?
            .iter()
            .map(|i| {
                Ok(ChunkInfo {
                    file: get_n(i, "file")? as u32,
                    index: get_n(i, "index")? as u32,
                    size: get_n(i, "size")? as u64,
                    sha256: get_s(i, "sha256")?,
                })
            })
            .collect()
    }
}

fn endpoint_from(i: &Item) -> Result<EndpointRecord> {
    Ok(EndpointRecord {
        endpoint_id: get_s(i, "endpoint_id")?,
        name: get_s(i, "name")?,
        platform: serde_json::from_value::<Platform>(serde_json::Value::String(get_s(i, "platform")?))
            .unwrap_or(Platform::Other),
        public_key: get_s(i, "public_key")?,
        created_at: get_n(i, "created_at")?,
    })
}

pub struct S3Blob {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
}

impl BlobStore for S3Blob {
    async fn presign_put(&self, key: &str, size: u64, sha: &str, ttl: u64) -> Result<PresignedPut> {
        let req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_length(size as i64)
            .checksum_sha256(sha)
            .presigned(PresigningConfig::expires_in(Duration::from_secs(ttl))?)
            .await?;
        // 署名に含まれたヘッダはクライアントがそのまま送る必要がある（host は HTTP クライアントが付与）
        let headers = req
            .headers()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("host") && !k.eq_ignore_ascii_case("content-length"))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Ok(PresignedPut {
            url: req.uri().to_string(),
            headers,
        })
    }
    async fn presign_get(&self, key: &str, ttl: u64) -> Result<String> {
        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(PresigningConfig::expires_in(Duration::from_secs(ttl))?)
            .await?;
        Ok(req.uri().to_string())
    }
    async fn head(&self, key: &str) -> Result<Option<(u64, Option<String>)>> {
        let r = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await;
        match r {
            Ok(o) => Ok(Some((o.content_length.unwrap_or(0) as u64, o.checksum_sha256))),
            Err(e) if e.as_service_error().is_some_and(|se| se.is_not_found()) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    async fn delete_prefix(&self, prefix: &str) -> Result<()> {
        let mut token = None;
        loop {
            let r = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .set_continuation_token(token.take())
                .send()
                .await?;
            let ids: Vec<_> = r
                .contents()
                .iter()
                .filter_map(|o| o.key())
                .filter_map(|k| aws_sdk_s3::types::ObjectIdentifier::builder().key(k).build().ok())
                .collect();
            if !ids.is_empty() {
                self.client
                    .delete_objects()
                    .bucket(&self.bucket)
                    .delete(
                        aws_sdk_s3::types::Delete::builder()
                            .set_objects(Some(ids))
                            .quiet(true)
                            .build()?,
                    )
                    .send()
                    .await?;
            }
            match r.next_continuation_token {
                Some(t) => token = Some(t),
                None => return Ok(()),
            }
        }
    }
}

pub struct ApiGwNotifier {
    pub client: aws_sdk_apigatewaymanagement::Client,
}

impl Notifier for ApiGwNotifier {
    async fn send(&self, id: &str, ev: &ServerEvent) -> Result<bool> {
        let data = serde_json::to_vec(ev)?;
        match self
            .client
            .post_to_connection()
            .connection_id(id)
            .data(aws_smithy_types::Blob::new(data))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if e.as_service_error().is_some_and(|se| se.is_gone_exception()) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}
