//! Consistent JSON error responses for the control-plane API.
//!
//! Every failure is rendered as the SRD §12 envelope:
//! `{"error": {"code", "message", "request_id"}}`. Internal details are logged
//! server-side and never leaked to clients.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use uuid::Uuid;

/// An error carrying a stable machine-readable code and an HTTP status.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub request_id: Uuid,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    request_id: Uuid,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            request_id: Uuid::new_v4(),
        }
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn unauthorized(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code, message)
    }

    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message)
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn gone(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::GONE, code, message)
    }

    pub fn too_many_requests(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, code, message)
    }

    /// A JSON body that failed to parse.
    pub fn invalid_json() -> Self {
        Self::bad_request("INVALID_JSON", "The request body is not valid JSON.")
    }

    /// Logs the internal cause and returns an opaque 500.
    pub fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "internal server error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "Internal server error.",
        )
    }

    /// A database failure: logged, reported ambiguously.
    pub fn database(error: impl std::fmt::Display) -> Self {
        Self::internal(format!("database error: {error}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorEnvelope {
            error: ErrorBody {
                code: self.code,
                message: self.message,
                request_id: self.request_id,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn error_response_has_the_documented_envelope() {
        let response = ApiError::forbidden("DEVICE_NOT_AUTHORIZED", "nope").into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["error"]["code"], "DEVICE_NOT_AUTHORIZED");
        assert_eq!(value["error"]["message"], "nope");
        assert!(value["error"]["request_id"].is_string());
    }

    #[tokio::test]
    async fn internal_errors_do_not_leak_details() {
        let response = ApiError::internal("secret connection string").into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8(bytes.to_vec()).expect("utf-8");
        assert!(!text.contains("secret"), "got: {text}");
        assert!(text.contains("INTERNAL_ERROR"), "got: {text}");
    }
}
