//! Upload bodies are bounded while streaming; guest-visible references contain
//! content identities, never the temporary host path used during admission.
use super::*;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

const MAX_RAW_BYTES: usize = 512 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;

pub(super) async fn upload(
    axum::Extension(tenant): axum::Extension<TenantService>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> HttpResponse {
    let service = &tenant.service;
    if !service.access.allows(Scope::Define) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    let limit = match content_type {
        Some("application/octet-stream") => MAX_RAW_BYTES,
        Some("application/json") => MAX_JSON_BYTES,
        _ => {
            return failure(
                service,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                anyhow::anyhow!("CAS upload requires application/octet-stream or application/json"),
            );
        }
    };
    if let Some(length) = headers.get(axum::http::header::CONTENT_LENGTH) {
        match length
            .to_str()
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
        {
            Some(size) if size <= limit => {}
            Some(_) => {
                return failure(
                    service,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    anyhow::anyhow!("CAS upload exceeds {limit} bytes"),
                );
            }
            None => {
                return failure(
                    service,
                    StatusCode::BAD_REQUEST,
                    anyhow::anyhow!("invalid Content-Length"),
                );
            }
        }
    }
    match receive(
        &service.store,
        body,
        limit,
        content_type == Some("application/json"),
    )
    .await
    {
        Ok(reference) => Json(json!({"$ref":reference})).into_response(),
        Err(error) => {
            let status = if error.downcast_ref::<TooLarge>().is_some() {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            failure(service, status, error)
        }
    }
}

#[derive(Debug)]
struct TooLarge;
impl std::fmt::Display for TooLarge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CAS upload exceeds its byte limit")
    }
}
impl std::error::Error for TooLarge {}

async fn receive(
    store: &Store,
    body: axum::body::Body,
    limit: usize,
    json_body: bool,
) -> Result<String> {
    let mut stream = body.into_data_stream();
    let mut count = 0usize;
    if json_body {
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("read CAS JSON body")?;
            count = count
                .checked_add(chunk.len())
                .context("upload size overflow")?;
            if count > limit {
                return Err(TooLarge.into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).context("invalid CAS JSON")?;
        let store = store.clone();
        let hash = tokio::task::spawn_blocking(move || {
            let hash = store.put_value("uploaded_json", &value)?;
            store.flush()?;
            Ok::<_, anyhow::Error>(hash)
        })
        .await??;
        loom_proto::cid_for_hash(&hash, loom_proto::DAG_CBOR_CODEC).map_err(anyhow::Error::msg)
    } else {
        let temporary =
            tempfile::NamedTempFile::new().context("create CAS upload temporary file")?;
        let mut file = tokio::fs::File::from_std(temporary.reopen()?);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("read CAS file body")?;
            count = count
                .checked_add(chunk.len())
                .context("upload size overflow")?;
            if count > limit {
                return Err(TooLarge.into());
            }
            file.write_all(&chunk).await.context("write CAS upload")?;
        }
        file.flush().await?;
        drop(file);
        let store = store.clone();
        let hash = tokio::task::spawn_blocking(move || {
            let hash = store.put_file("uploaded_file", temporary.path())?;
            store.flush()?;
            Ok::<_, anyhow::Error>(hash)
        })
        .await??;
        loom_proto::cid_for_hash(&hash, loom_proto::RAW_CODEC).map_err(anyhow::Error::msg)
    }
}

fn failure(service: &Service, status: StatusCode, error: anyhow::Error) -> HttpResponse {
    let mut response = Json(service.response(Err(error))).into_response();
    *response.status_mut() = status;
    response
}
