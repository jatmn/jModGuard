//! Independent conductor entry point. Operational APIs are not enabled until their
//! authentication, control store and recovery contracts are implemented.
pub mod config;

use axum::{
    Json, Router,
    http::{StatusCode, header},
    response::Response,
    routing::get,
};
use serde::Serialize;

#[derive(Serialize)]
struct Version {
    service: &'static str,
    version: &'static str,
    operations_available: bool,
}
#[derive(Serialize)]
struct NotReady {
    code: &'static str,
}

pub fn router() -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/health/", get(|| async { "ok" }))
        .route(
            "/ready",
            get(|| async {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(NotReady {
                        code: "control_not_configured",
                    }),
                )
            }),
        )
        .route(
            "/version",
            get(|| async {
                Json(Version {
                    service: "conductor",
                    version: env!("CARGO_PKG_VERSION"),
                    operations_available: false,
                })
            }),
        )
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    "no-store".parse().expect("static header"),
                );
                response
            },
        ))
}
