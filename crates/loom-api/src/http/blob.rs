//! Bulk bytes for cells: `POST /v1/blob` stores a raw body as a kernel blob and answers its handle;
//! `GET /v1/blob/{hash}` returns the bytes. A cell reads a handle with `loom::kernel::get`, and returns a
//! large result by `loom::kernel::put`, so a mesh never travels as JSON or as DAG-CBOR numbers.
//!
//! Upload needs the define scope (it writes the store), download the read scope. The body is streamed to a
//! temporary file and hashed from there, so its size is bounded by `Runtime::MAX_BLOB_BYTES` (what a cell can
//! read back), not by memory or by the router's 16 MiB default. Only kernel blobs are readable here: a hash
//! of a definition or a component is "not found".
use super::upload::failure;
use super::*;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

pub(super) async fn upload(
    axum::Extension(tenant): axum::Extension<TenantService>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> HttpResponse {
    let service = &tenant.service;
    if !service.access.allows(Scope::Define) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or("").trim())
        != Some("application/octet-stream")
    {
        return failure(
            service,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            anyhow::anyhow!("a blob upload is application/octet-stream"),
        );
    }
    let limit = loom_rt::Runtime::MAX_BLOB_BYTES;
    if let Some(length) = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && length > limit
    {
        return failure(
            service,
            StatusCode::PAYLOAD_TOO_LARGE,
            anyhow::anyhow!("a blob is at most {limit} bytes"),
        );
    }
    let received = async {
        let temporary = tempfile::NamedTempFile::new().context("create blob temporary file")?;
        let mut file = tokio::fs::File::from_std(temporary.reopen()?);
        let mut stream = body.into_data_stream();
        let mut count = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("read blob body")?;
            count = count.checked_add(chunk.len()).context("upload size overflow")?;
            if count > limit {
                return Ok::<_, anyhow::Error>(None);
            }
            file.write_all(&chunk).await.context("write blob upload")?;
        }
        file.flush().await?;
        drop(file);
        let runtime = service.runtime.clone();
        let store = service.store.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let handle = runtime.put_blob_file(temporary.path())?;
            store.flush()?;
            Ok::<_, anyhow::Error>(handle)
        })
        .await??;
        Ok(Some((handle, count)))
    }
    .await;
    match received {
        Ok(Some((handle, len))) => Json(json!({
            "handle": handle.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
            "len": len,
        }))
        .into_response(),
        Ok(None) => failure(
            service,
            StatusCode::PAYLOAD_TOO_LARGE,
            anyhow::anyhow!("a blob is at most {limit} bytes"),
        ),
        Err(error) => failure(service, StatusCode::BAD_REQUEST, error),
    }
}

/// The bytes go out in chunks read from the mapping, so a large blob is never copied whole onto the heap.
const CHUNK: usize = 1 << 20;

pub(super) async fn download(
    axum::Extension(tenant): axum::Extension<TenantService>,
    Path(hash): Path<String>,
) -> HttpResponse {
    let service = &tenant.service;
    let handle: Option<[u8; 32]> = (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| {
            let mut out = [0u8; 32];
            for (index, pair) in hash.as_bytes().chunks(2).enumerate() {
                out[index] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
            }
            Some(out)
        })
        .flatten();
    let Some(handle) = handle else {
        return failure(
            service,
            StatusCode::BAD_REQUEST,
            anyhow::anyhow!("a blob handle is 64 hex characters"),
        );
    };
    let runtime = service.runtime.clone();
    // Mapping hashes the file once per process and clones it: blocking work, off the async workers.
    let mapped = match tokio::task::spawn_blocking(move || runtime.map_blob(&handle)).await {
        Ok(Ok(Some(mapped))) => std::sync::Arc::new(mapped),
        Ok(Ok(None)) => return StatusCode::NOT_FOUND.into_response(),
        Ok(Err(error)) => return failure(service, StatusCode::INTERNAL_SERVER_ERROR, error),
        Err(error) => return failure(service, StatusCode::INTERNAL_SERVER_ERROR, error.into()),
    };
    let length = mapped.len();
    let stream = futures_util::stream::unfold(0usize, move |at| {
        let mapped = mapped.clone();
        async move {
            (at < length).then(|| {
                let end = (at + CHUNK).min(length);
                let chunk = axum::body::Bytes::copy_from_slice(&mapped[at..end]);
                (Ok::<_, std::convert::Infallible>(chunk), end)
            })
        }
    });
    let mut response = axum::body::Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        axum::http::header::CONTENT_LENGTH,
        axum::http::HeaderValue::from(length),
    );
    response
}
