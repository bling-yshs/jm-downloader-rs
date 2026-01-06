use jm_downloader_rs::AppError;
use serde::Deserialize;
use std::env;

type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub jm_username: String,
    pub jm_password: String,
    #[serde(default = "default_api_domain")]
    pub api_domain: String,
    #[serde(default = "default_image_domain")]
    pub image_domain: String,
    #[serde(default = "default_img_concurrency")]
    pub img_concurrency: usize,
}

fn default_api_domain() -> String {
    "www.cdnhth.cc".to_string()
}

fn default_image_domain() -> String {
    "cdn-msp2.jmapiproxy2.cc".to_string()
}

fn default_img_concurrency() -> usize {
    32
}

pub fn load_config() -> Result<Config> {
    let jm_username = read_required_env("JM_USERNAME")?;
    let jm_password = read_required_env("JM_PASSWORD")?;
    let api_domain = read_optional_env("JM_API_DOMAIN").unwrap_or_else(default_api_domain);
    let image_domain = read_optional_env("JM_IMAGE_DOMAIN").unwrap_or_else(default_image_domain);
    let img_concurrency = read_optional_env("JM_IMG_CONCURRENCY")
        .map(|value| parse_img_concurrency(&value))
        .transpose()?
        .unwrap_or_else(default_img_concurrency);

    Ok(Config {
        jm_username,
        jm_password,
        api_domain,
        image_domain,
        img_concurrency,
    })
}

fn read_required_env(key: &str) -> Result<String> {
    let value = env::var(key)
        .map_err(|e| AppError::Internal(format!("读取环境变量 {} 失败或未设置: {}", key, e)))?;
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(AppError::Internal(format!("环境变量 {} 不能为空", key)));
    }
    Ok(value)
}

fn read_optional_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_img_concurrency(value: &str) -> Result<usize> {
    let parsed = value
        .parse::<usize>()
        .map_err(|e| AppError::Internal(format!("环境变量 JM_IMG_CONCURRENCY 解析失败: {}: {}", value, e)))?;
    if parsed == 0 {
        return Err(AppError::Internal(
            "环境变量 JM_IMG_CONCURRENCY 必须大于 0".to_string(),
        ));
    }
    Ok(parsed)
}
