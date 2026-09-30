//! Bulk bytes for cells: `POST /v1/blob` stores a raw body as a kernel blob and answers its handle;
//! `GET /v1/blob/{hash}` returns the bytes. A cell reads a handle with `loom::kernel::get`, and returns a
//! large result by `loom::kernel::put`, so a mesh never travels as JSON or as DAG-CBOR numbers.
//!
//! Upload needs the define scope (it writes the store), download the read scope. Only objects stored as
//! kernel blobs are readable here: a hash of a definition or a component is "not found".
use super::upload::failure;
use super::*;

const MAX_BLOB_BYTES: usize = 512 * 1024 * 1024;

pub(super) async fn upload(
    axum::Extension(tenant): axum::Extension<TenantService>,
    headers: HeaderMap,
    body: axum::body::Bytes,
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
    if body.len() > MAX_BLOB_BYTES {
        return failure(
            service,
            StatusCode::PAYLOAD_TOO_LARGE,
            anyhow::anyhow!("a blob is at most {MAX_BLOB_BYTES} bytes"),
        );
    }
    match service.runtime.call_kernel("loom.put", &[&body]) {
        Ok(handle) => Json(json!({
            "handle": handle.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
            "len": body.len(),
        }))
        .into_response(),
        Err(error) => failure(service, StatusCode::BAD_REQUEST, anyhow::anyhow!(error)),
    }
}

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
    let mapped = match service.runtime.map_blob(&handle) {
        Ok(Some(mapped)) => mapped,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return failure(service, StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let mut response = axum::body::Bytes::copy_from_slice(&mapped).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/octet-stream"),
    );
    response
}
