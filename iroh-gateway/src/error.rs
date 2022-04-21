use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub struct GatewayError {
    pub status_code: StatusCode,
    pub message: String,
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let body = axum::Json(json!({
            "code": self.status_code.as_u16(),
            "success": false,
            "message": self.message,
            "trace_id": "some_trace_id",
        }));

        (self.status_code, body).into_response()
    }
}
