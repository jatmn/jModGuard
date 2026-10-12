//! Carry browser save identity through the existing authenticated handlers.
//! This context grants no authority; each handler still captures its own scope.
use axum::{extract::Request, middleware::Next, response::Response};

#[derive(Clone)]
pub(super) struct SaveRequest {
    pub id: String,
    pub lookup: bool,
}

tokio::task_local! {
    static REQUEST: Option<SaveRequest>;
}

pub(super) fn current() -> Option<SaveRequest> {
    REQUEST.try_with(Clone::clone).ok().flatten()
}

pub(super) async fn middleware(request: Request, next: Next) -> Response {
    use axum::response::IntoResponse;
    let headers = request.headers();
    let save = if let Some(id) = headers.get("x-jmodguard-save") {
        let id = id.to_str().unwrap_or_default();
        let lookup = headers
            .get("x-jmodguard-save-lookup")
            .and_then(|v| v.to_str().ok());
        if id.len() != 64
            || !id.bytes().all(|v| v.is_ascii_hexdigit())
            || !matches!(lookup, Some("0" | "1"))
        {
            return super::ApiError(anyhow::anyhow!("invalid save request identity"))
                .into_response();
        }
        Some(SaveRequest {
            id: id.to_owned(),
            lookup: lookup == Some("1"),
        })
    } else {
        None
    };
    REQUEST.scope(save, next.run(request)).await
}

#[derive(Debug)]
pub(super) enum SaveError {
    Pending,
    Unknown,
    Expired,
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pending => {
                "Save accepted and still pending. Retry this save to check its result."
            }
            Self::Expired => {
                "Save expired before it was applied. Save again to make a new attempt."
            }
            Self::Unknown => {
                "Save outcome unavailable. Check the current configuration before saving again."
            }
        })
    }
}

impl std::error::Error for SaveError {}
