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
    #[serde(default = "default_task_concurrency")]
    pub task_concurrency: usize,
    #[serde(default = "default_task_queue_capacity")]
    pub task_queue_capacity: usize,
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

/// 返回默认的任务执行并发数。
///
/// # 返回
/// 默认任务执行并发数。
fn default_task_concurrency() -> usize {
    1
}

/// 返回默认的任务排队容量。
///
/// # 返回
/// 默认任务排队容量。
fn default_task_queue_capacity() -> usize {
    100
}

/// 从环境变量加载应用配置。
///
/// # 返回
/// 加载成功时返回配置，配置缺失或格式错误时返回应用错误。
pub fn load_config() -> Result<Config> {
    let jm_username = read_required_env("JM_USERNAME")?;
    let jm_password = read_required_env("JM_PASSWORD")?;
    let api_domain = read_optional_env("JM_API_DOMAIN").unwrap_or_else(default_api_domain);
    let image_domain = read_optional_env("JM_IMAGE_DOMAIN").unwrap_or_else(default_image_domain);
    let img_concurrency = read_optional_env("JM_IMG_CONCURRENCY")
        .map(|value| parse_positive_usize("JM_IMG_CONCURRENCY", &value))
        .transpose()?
        .unwrap_or_else(default_img_concurrency);
    let task_concurrency = read_optional_env("JM_TASK_CONCURRENCY")
        .map(|value| parse_positive_usize("JM_TASK_CONCURRENCY", &value))
        .transpose()?
        .unwrap_or_else(default_task_concurrency);
    let task_queue_capacity = read_optional_env("JM_TASK_QUEUE_CAPACITY")
        .map(|value| parse_positive_usize("JM_TASK_QUEUE_CAPACITY", &value))
        .transpose()?
        .unwrap_or_else(default_task_queue_capacity);

    Ok(Config {
        jm_username,
        jm_password,
        api_domain,
        image_domain,
        img_concurrency,
        task_concurrency,
        task_queue_capacity,
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

/// 解析必须大于零的无符号整数配置。
///
/// # 参数
/// - `key`: 环境变量名称。
/// - `value`: 待解析的环境变量值。
///
/// # 返回
/// 解析成功时返回正整数，格式错误或数值为零时返回应用错误。
fn parse_positive_usize(key: &str, value: &str) -> Result<usize> {
    let parsed = value
        .parse::<usize>()
        .map_err(|e| AppError::Internal(format!("环境变量 {} 解析失败: {}: {}", key, value, e)))?;
    if parsed == 0 {
        return Err(AppError::Internal(format!("环境变量 {} 必须大于 0", key)));
    }
    Ok(parsed)
}
