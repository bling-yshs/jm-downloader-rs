use chrono::Utc;
use chrono_tz::Asia::Shanghai;
use rocket::{
    request::Request,
    response::{Responder, Result as RocketResult},
    serde::json::Json,
};
use rocket_okapi::{
    gen::OpenApiGenerator,
    okapi::openapi3::Responses,
    response::OpenApiResponderInner,
    util::add_schema_response,
};
use schemars::JsonSchema;
use serde::Serialize;
use thiserror::Error;

/// 统一响应结构：code / success / data / message / time
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(bound = "T: JsonSchema")]
#[serde(bound(serialize = "T: Serialize"))]
pub struct R<T> {
    pub code: String, // 成功："0"；失败：由 AppError::code() 决定
    pub success: bool,
    pub data: Option<T>,
    pub message: Option<String>,
    pub time: String, // 例如 "2025-09-28T14:50:12+08:00"
}

impl<T: Serialize> R<T> {
    /// 业务成功（HTTP 统一 200）
    pub fn success(data: T) -> Self {
        Self {
            code: "0".into(),
            success: true,
            data: Some(data),
            message: None,
            time: beijing_now(),
        }
    }

    /// 业务失败（HTTP 统一 200；通常由 `AppError` 使用）
    fn fail(code: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            success: false,
            data: None,
            message: Some(msg.into()),
            time: beijing_now(),
        }
    }
}

fn beijing_now() -> String {
    Utc::now()
        .with_timezone(&Shanghai)
        .format("%Y-%m-%dT%H:%M:%S%.3f%:z") // <-- 关键改动在这里
        .to_string()
}

/// 让 `R<T>` 可以直接作为 Responder，序列化为 JSON；状态码保持 200
impl<'r, T: Serialize> Responder<'r, 'static> for R<T> {
    fn respond_to(self, req: &'r Request<'_>) -> RocketResult<'static> {
        Json(self).respond_to(req) // Rocket 的 Json 默认 200 OK
    }
}

/// 你的应用错误类型：既支持业务错误，也可承载内部错误
#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),

    /// 未分类/内部错误
    #[error("{0}")]
    Internal(String),
}

impl AppError {
    /// 失败时写入 R.code（你可以根据需要改成你的业务码）
    pub fn code(&self) -> &'static str {
        match self {
            AppError::BadRequest(_) => "10001",
            AppError::Unauthorized(_) => "10002",
            AppError::Forbidden(_) => "10003",
            AppError::NotFound(_) => "10004",
            AppError::Internal(_) => "20000",
        }
    }

    pub fn message(&self) -> String {
        self.to_string()
    }
}

impl<'r> Responder<'r, 'static> for AppError {
    fn respond_to(self, req: &'r Request<'_>) -> RocketResult<'static> {
        let body: R<serde_json::Value> = R::fail(self.code(), self.message());
        Json(body).respond_to(req)
    }
}

impl<T> OpenApiResponderInner for R<T>
where
    T: Serialize + JsonSchema,
{
    fn responses(gen: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let mut responses = Responses::default();
        let schema = gen.json_schema::<R<T>>();
        add_schema_response(&mut responses, 200, "application/json", schema)?;
        Ok(responses)
    }
}

impl OpenApiResponderInner for AppError {
    fn responses(gen: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let mut responses = Responses::default();
        let schema = gen.json_schema::<R<serde_json::Value>>();
        add_schema_response(&mut responses, 200, "application/json", schema)?;
        Ok(responses)
    }
}

pub type ApiResult<T> = Result<T, AppError>;
