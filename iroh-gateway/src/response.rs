use std::{collections::HashMap, str::FromStr};

use axum::{
    body::BoxBody,
    http::{header::HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

#[derive(Debug, Clone)]
pub enum ResponseFormat {
    HTML,
    Raw,
    Car,
    FS,
}

pub struct GatewayResponse {
    pub status_code: StatusCode,
    pub body: BoxBody,
    pub headers: HashMap<String, String>,
}

impl GatewayResponse {
    pub fn new(status_code: StatusCode, body: BoxBody) -> Self {
        Self {
            status_code,
            body,
            headers: HashMap::new(),
        }
    }

    pub fn new_with_headers(
        status_code: StatusCode,
        body: BoxBody,
        headers: HashMap<String, String>,
    ) -> Self {
        Self {
            status_code,
            body,
            headers,
        }
    }

    pub fn add_header(&mut self, name: &str, value: &str) {
        self.headers.insert(name.to_string(), value.to_string());
    }
}

impl IntoResponse for GatewayResponse {
    fn into_response(self) -> Response {
        let mut rb = Response::builder().status(self.status_code);
        let headers = rb.headers_mut().unwrap();
        for (key, value) in &self.headers {
            let header_name = HeaderName::from_str(&key).unwrap();
            headers.insert(header_name, HeaderValue::from_str(&value).unwrap());
        }
        rb.body(self.body).unwrap()
    }
}
