use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes256;
use base64::{engine::general_purpose::STANDARD, Engine};
use jm_downloader_rs::AppError;
use reqwest::Client;
use serde_json::Value;
use tokio::task::JoinSet;

use crate::jm_client::{APP_DATA_SECRET, APP_TOKEN_SECRET, APP_VERSION};

type Result<T> = std::result::Result<T, AppError>;

const HOST_CONFIG_URLS: [&str; 2] = [
    "https://rup4a04-c02.tos-cn-hongkong.bytepluses.com/newsvr-2025.txt",
    "https://rup4a04-c01.tos-ap-southeast-1.bytepluses.com/newsvr-2025.txt",
];
const HOST_CONFIG_SECRET: &str = "diosfjckwpqpdfjkvnqQjsik";
const FALLBACK_BASE: &str = "https://www.cdnhjk.net";
const PROBE_CONCURRENCY: usize = 6;

/// 使用指定域名，或获取远程候选列表并按业务响应耗时排序。
///
/// # 参数
/// - `configured`: 用户通过环境变量指定的域名；为空时自动选线。
///
/// # 返回
/// 返回非空的主机名列表；全部候选和备用线路不可用时返回错误。
pub async fn select_api_domains(configured: Option<&str>) -> Result<Vec<String>> {
    if let Some(domain) = configured
        .map(str::trim)
        .filter(|domain| !domain.is_empty())
    {
        info!("使用指定的 JM API 域名: {}", domain);
        return Ok(vec![domain.to_owned()]);
    }

    info!("开始获取并探测 JM API 候选域名");
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AppError::Internal(format!("创建选线客户端失败: {}", e)))?;
    let mut sources = JoinSet::new();
    for url in HOST_CONFIG_URLS {
        let client = client.clone();
        sources.spawn(async move {
            let result: Result<Vec<String>> = async {
                let response = client
                    .get(url)
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status)
                    .map_err(|e| AppError::Internal(format!("获取域名列表失败: {}", e)))?;
                let body = response
                    .text()
                    .await
                    .map_err(|e| AppError::Internal(format!("读取域名列表失败: {}", e)))?;
                decode_api_domains(&body)
            }
            .await;
            (url, result)
        });
    }

    let mut domains = Vec::new();
    while let Some(result) = sources.join_next().await {
        match result {
            Ok((_, Ok(candidates))) => {
                domains = candidates;
                break;
            }
            Ok((url, Err(error))) => warn!("JM 域名配置源 {} 不可用: {}", url, error),
            Err(error) => warn!("JM 域名配置获取任务失败: {}", error),
        }
    }
    sources.abort_all();

    if !domains.is_empty() {
        info!("获取到 {} 个 JM API 候选域名，开始探测", domains.len());
        let ranked = rank_domains(&client, domains).await;
        if !ranked.is_empty() {
            return Ok(ranked);
        }
    }

    warn!("JM 自动选线未找到可用线路，探测备用域名 {}", FALLBACK_BASE);
    probe_domain(&client, FALLBACK_BASE)
        .await
        .map_err(|error| {
            AppError::Internal(format!(
                "JM API 自动选线失败，备用域名也不可用: {}；可设置 JM_API_DOMAIN 指定域名",
                error
            ))
        })?;
    info!("选中 JM API 备用域名: {}", FALLBACK_BASE);
    Ok(vec![FALLBACK_BASE
        .trim_start_matches("https://")
        .to_owned()])
}

/// 解密远程域名配置，筛选 HTTPS 主机地址并去重。
///
/// # 参数
/// - `body`: 远程配置源返回的 Base64 密文。
///
/// # 返回
/// 返回非空的候选地址列表，配置损坏或缺少有效主机时返回错误。
fn decode_api_domains(body: &str) -> Result<Vec<String>> {
    let key = format!("{:x}", md5::compute(HOST_CONFIG_SECRET));
    let payload = decrypt_json(body, key.as_bytes())?;
    let servers = payload
        .get("Server")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Internal("域名配置缺少 Server 数组".to_string()))?;
    let mut domains = Vec::new();
    for server in servers {
        let Some(host) = server.as_str() else {
            continue;
        };
        let host = host.trim();
        let base = if host.starts_with("https://") {
            host.to_owned()
        } else {
            format!("https://{}", host)
        };
        let Ok(url) = reqwest::Url::parse(&base) else {
            continue;
        };
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            continue;
        }
        let base = url.as_str().trim_end_matches('/').to_owned();
        if !domains.contains(&base) {
            domains.push(base);
        }
    }
    if domains.is_empty() {
        return Err(AppError::Internal(
            "域名配置没有可用的 HTTPS 主机".to_string(),
        ));
    }
    Ok(domains)
}

/// 有限并发探测候选域名，按有效业务响应耗时升序排列线路。
///
/// # 参数
/// - `client`: 具有请求超时的 HTTP 客户端。
/// - `domains`: 待探测的候选基础地址。
///
/// # 返回
/// 返回按耗时排序的可用主机名列表，全部失败时返回空列表。
async fn rank_domains(client: &Client, domains: Vec<String>) -> Vec<String> {
    let mut pending = domains.into_iter();
    let mut probes = JoinSet::new();
    let mut available = Vec::new();
    loop {
        while probes.len() < PROBE_CONCURRENCY {
            let Some(domain) = pending.next() else {
                break;
            };
            let client = client.clone();
            probes.spawn(async move {
                let result = probe_domain(&client, &domain).await;
                (domain, result)
            });
        }
        let Some(result) = probes.join_next().await else {
            break;
        };
        match result {
            Ok((domain, Ok(elapsed))) => {
                info!(
                    "JM API 线路 {} 可用，耗时 {} ms",
                    domain,
                    elapsed.as_millis()
                );
                available.push((domain, elapsed));
            }
            Ok((domain, Err(error))) => warn!("JM API 线路 {} 探测失败: {}", domain, error),
            Err(error) => warn!("JM API 线路探测任务失败: {}", error),
        }
    }
    available.sort_by_key(|(_, elapsed)| *elapsed);
    available
        .into_iter()
        .map(|(domain, _)| domain.trim_start_matches("https://").to_owned())
        .collect()
}

/// 使用现有 API 签名探测 setting 接口，校验状态、业务码和数据内容。
///
/// # 参数
/// - `client`: 具有请求超时的 HTTP 客户端。
/// - `base`: 候选域名的完整基础地址。
///
/// # 返回
/// 返回有效业务响应的完整请求耗时；状态异常或响应无效时返回错误。
async fn probe_domain(client: &Client, base: &str) -> Result<Duration> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AppError::Internal(format!("系统时间异常: {}", e)))?
        .as_secs();
    let token = format!("{:x}", md5::compute(format!("{}{}", ts, APP_TOKEN_SECRET)));
    let started = Instant::now();
    let response = client
        .get(format!("{}/setting?app_img_shunt=1&t={}", base, ts))
        .header("token", token)
        .header("tokenparam", format!("{},{}", ts, APP_VERSION))
        .header("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("setting 请求失败: {}", e)))?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(AppError::Internal(format!(
            "setting HTTP 状态: {}",
            response.status()
        )));
    }
    let body = response
        .text()
        .await
        .map_err(|e| AppError::Internal(format!("读取 setting 响应失败: {}", e)))?;
    let payload: Value = serde_json::from_str(body.trim_start_matches('\u{feff}'))
        .map_err(|e| AppError::Internal(format!("setting 响应格式无效: {}", e)))?;
    if payload.get("code").and_then(Value::as_u64) != Some(200) {
        return Err(AppError::Internal("setting 业务状态异常".to_string()));
    }
    let data = match payload.get("data") {
        Some(Value::String(data)) => {
            let key = format!("{:x}", md5::compute(format!("{}{}", ts, APP_DATA_SECRET)));
            decrypt_json(data, key.as_bytes())?
        }
        Some(data) => data.clone(),
        None => Value::Null,
    };
    if !data.is_object() {
        return Err(AppError::Internal("setting 数据格式无效".to_string()));
    }
    Ok(started.elapsed())
}

/// 解密采用 AES-256-ECB 和 PKCS#7 填充的 Base64 JSON 数据。
///
/// # 参数
/// - `encoded`: Base64 编码的密文。
/// - `key`: 32 字节的 AES 密钥。
///
/// # 返回
/// 返回解密后的 JSON，密文、填充或 JSON 无效时返回错误。
fn decrypt_json(encoded: &str, key: &[u8]) -> Result<Value> {
    let normalized: String = encoded
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '\u{feff}')
        .collect();
    let mut bytes = STANDARD
        .decode(normalized)
        .map_err(|e| AppError::Internal(format!("Base64 配置或响应无效: {}", e)))?;
    if bytes.is_empty() || bytes.len() % 16 != 0 {
        return Err(AppError::Internal("AES 密文长度无效".to_string()));
    }
    let cipher = Aes256::new_from_slice(key)
        .map_err(|e| AppError::Internal(format!("AES 密钥无效: {}", e)))?;
    for block in bytes.chunks_exact_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    let padding = *bytes.last().unwrap() as usize;
    if padding == 0
        || padding > 16
        || !bytes[bytes.len() - padding..]
            .iter()
            .all(|byte| *byte as usize == padding)
    {
        return Err(AppError::Internal("AES 填充无效".to_string()));
    }
    bytes.truncate(bytes.len() - padding);
    serde_json::from_slice(&bytes)
        .map_err(|e| AppError::Internal(format!("解密后的 JSON 无效: {}", e)))
}
