use axum::{
    body::{self, Body, BoxBody},
    error_handling::HandleErrorLayer,
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    BoxError, Router,
};
use serde::Deserialize;
use std::{borrow::Cow, collections::HashMap, time::Duration};
use tower::ServiceBuilder;

use crate::client::Client;
use crate::config::Config;
use crate::error::GatewayError;
use crate::response::{GatewayResponse, ResponseFormat};

pub struct Handler {
    pub config: Config,
    client: Client,
}

#[derive(Debug, Deserialize)]
pub struct GetParams {
    format: Option<String>,
    filename: Option<String>,
    download: Option<String>,
}

impl Handler {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    async fn get_ipfs(
        Extension(config): Extension<Config>,
        Extension(client): Extension<Client>,
        Path(params): Path<HashMap<String, String>>,
        Query(query_params): Query<GetParams>,
    ) -> Result<GatewayResponse, GatewayError> {
        let cid = params.get("cid").unwrap();
        let cpath = "".to_string();
        let cpath = params.get("cpath").unwrap_or(&cpath);
        let full_content_path = format!("/ipfs/{}{}", cid, cpath);
        println!("cid: {}", cid);
        println!("cpath: {}", cpath);
        println!("fullpath: {}", full_content_path);

        let format = query_params.format.unwrap_or("".to_string());
        let format = Handler::response_format(&format);
        let format = match format {
            Ok(f) => f,
            Err(error) => {
                let msg = format!("{}", error);
                return Handler::error(StatusCode::BAD_REQUEST, &msg);
            }
        };
        let query_file_name = query_params.filename.unwrap_or("".to_string());
        let download = query_params.download.unwrap_or("".to_string()) == "true";

        let mut headers = Handler::format_headers(&format);
        headers.insert("X-Ipfs-Path".to_string(), full_content_path.clone());
        let mut headers = Handler::add_user_headers(&headers, config.headers.clone());

        match format {
            ResponseFormat::Raw => {
                let body = client
                    .get_file(format!("{}", full_content_path).as_str())
                    .await;
                let body = match body {
                    Ok(b) => b,
                    Err(e) => {
                        let msg = format!("{}", e);
                        return Handler::error(StatusCode::INTERNAL_SERVER_ERROR, &msg);
                    }
                };

                headers = Handler::set_content_disposition_headers(
                    &headers,
                    format!("{}.bin", cid).as_str(),
                    "attachment",
                );
                Handler::response(StatusCode::OK, body::boxed(Body::from(body)), headers)
            }
            ResponseFormat::Car => {
                let body = client
                    .get_file(format!("{}", full_content_path).as_str())
                    .await;
                let body = match body {
                    Ok(b) => b,
                    Err(e) => {
                        let msg = format!("{}", e);
                        return Handler::error(StatusCode::INTERNAL_SERVER_ERROR, &msg);
                    }
                };
                headers = Handler::set_content_disposition_headers(
                    &headers,
                    format!("{}.car", cid).as_str(),
                    "attachment",
                );
                Handler::response(StatusCode::OK, body::boxed(Body::from(body)), headers)
            }
            ResponseFormat::HTML => {
                let body = format!("<p>{}</p>", cid);
                Handler::response(StatusCode::OK, body::boxed(Body::from(body)), headers)
            }
            ResponseFormat::FS => {
                let body = client
                    .get_file(format!("{}", full_content_path).as_str())
                    .await;
                let body = match body {
                    Ok(b) => b,
                    Err(e) => {
                        let msg = format!("{}", e);
                        return Handler::error(StatusCode::INTERNAL_SERVER_ERROR, &msg);
                    }
                };
                let (name, headers) = Handler::add_content_disposition_headers(
                    &headers,
                    &query_file_name,
                    &cpath,
                    download,
                );
                let headers = Handler::add_content_type_headers(&headers, &name);
                Handler::response(StatusCode::OK, body::boxed(Body::from(body)), headers)
            }
        }
    }

    fn response_format(format: &str) -> Result<ResponseFormat, String> {
        match format {
            "raw" => Ok(ResponseFormat::Raw),
            "car" => Ok(ResponseFormat::Car),
            "html" => Ok(ResponseFormat::HTML),
            "" => Ok(ResponseFormat::FS),
            _ => Err("format not supported".to_string()),
        }
    }

    fn format_headers(format: &ResponseFormat) -> HashMap<String, String> {
        match format {
            ResponseFormat::Raw => {
                let mut headers = HashMap::new();
                headers.insert(
                    "Content-Type".to_string(),
                    "application/vnd.ipld.raw".to_string(),
                );
                headers.insert("X-Content-Type-Options".to_string(), "nosniff".to_string());
                headers
            }
            ResponseFormat::Car => {
                let mut headers = HashMap::new();
                headers.insert(
                    "Content-Type".to_string(),
                    "application/vnd.ipld.car; version=1".to_string(),
                );
                headers.insert("X-Content-Type-Options".to_string(), "nosniff".to_string());
                headers
            }
            ResponseFormat::HTML => {
                let mut headers = HashMap::new();
                headers.insert("Content-Type".to_string(), "text/html".to_string());
                headers
            }
            ResponseFormat::FS => {
                let mut headers = HashMap::new();
                headers.insert(
                    "Content-Type".to_string(),
                    "application/vnd.ipld.raw".to_string(),
                );
                headers
            }
        }
    }

    fn add_user_headers(
        headers: &HashMap<String, String>,
        user_headers: HashMap<String, String>,
    ) -> HashMap<String, String> {
        let mut headers = headers.clone();
        for (key, value) in user_headers {
            headers.insert(key, value);
        }
        headers
    }

    fn add_content_type_headers(
        headers: &HashMap<String, String>,
        name: &str,
    ) -> HashMap<String, String> {
        let mut headers = headers.clone();
        let guess = mime_guess::from_path(name);
        let content_type = guess.first_or_octet_stream().to_string();
        headers.insert("Content-Type".to_string(), content_type);
        headers
    }

    fn add_content_disposition_headers(
        headers: &HashMap<String, String>,
        filename: &str,
        content_path: &str,
        download: bool,
    ) -> (String, HashMap<String, String>) {
        let headers = headers.clone();
        let mut name = Handler::get_filename(content_path);
        if filename != "" {
            let mut disposition = "inline";
            if download {
                disposition = "attachment";
            }
            name = filename.to_string();
            return (
                name,
                Handler::set_content_disposition_headers(&headers, filename, disposition),
            );
        }
        (name, headers)
    }

    fn set_content_disposition_headers(
        headers: &HashMap<String, String>,
        filename: &str,
        disposition: &str,
    ) -> HashMap<String, String> {
        let mut headers = headers.clone();
        headers.insert(
            "Content-Disposition".to_string(),
            format!("{}; filename={}", disposition, filename),
        );
        headers
    }

    fn get_filename(content_path: &str) -> String {
        let mut name = "".to_string();
        let mut parts = content_path.split("/");
        while let Some(part) = parts.next() {
            if part.len() > 0 {
                name = part.to_string();
            }
        }
        name
    }

    fn response(
        status_code: StatusCode,
        body: BoxBody,
        headers: HashMap<String, String>,
    ) -> Result<GatewayResponse, GatewayError> {
        Ok(GatewayResponse {
            status_code,
            body: body,
            headers,
        })
    }

    fn error(status_code: StatusCode, message: &str) -> Result<GatewayResponse, GatewayError> {
        Err(GatewayError {
            status_code,
            message: message.to_string(),
        })
    }

    pub async fn serve(&self) {
        let app = Router::new()
            .route("/ipfs/:cid", get(Handler::get_ipfs))
            .route("/ipfs/:cid/*cpath", get(Handler::get_ipfs))
            .layer(Extension(self.config.clone()))
            .layer(Extension(self.client.clone()))
            .layer(
                ServiceBuilder::new()
                    // Handle errors from middleware
                    .layer(HandleErrorLayer::new(Handler::middleware_error_handler))
                    .load_shed()
                    .concurrency_limit(1024)
                    .timeout(Duration::from_secs(10))
                    .into_inner(),
            );
        let addr = format!("0.0.0.0:{}", self.config.port);
        axum::Server::bind(&addr.parse().unwrap())
            .http1_preserve_header_case(true)
            .http1_title_case_headers(true)
            .serve(app.into_make_service())
            .await
            .unwrap();
    }

    async fn middleware_error_handler(error: BoxError) -> impl IntoResponse {
        if error.is::<tower::timeout::error::Elapsed>() {
            return (StatusCode::REQUEST_TIMEOUT, Cow::from("request timed out"));
        }

        if error.is::<tower::load_shed::error::Overloaded>() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Cow::from("service is overloaded, try again later"),
            );
        }

        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Cow::from(format!("unhandled internal error: {}", error)),
        )
    }
}
