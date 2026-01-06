use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes256;
use base64::engine::general_purpose;
use base64::Engine;
use jm_downloader_rs::AppError;
use reqwest::cookie::Jar;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware, Retryable, RetryableStrategy};
use serde_json::{json, Value};

use crate::models::{GetChapterRespData, GetComicRespData, JmResp};

const APP_TOKEN_SECRET: &str = "18comicAPP";
const APP_TOKEN_SECRET_2: &str = "18comicAPPContent";
const APP_DATA_SECRET: &str = "185Hcomic3PAPP7R";
const APP_VERSION: &str = "2.0.13";

type AppResult<T> = std::result::Result<T, AppError>;

struct JmRetryStrategy;

impl RetryableStrategy for JmRetryStrategy {
    fn handle(&self, res: &std::result::Result<reqwest::Response, reqwest_middleware::Error>) -> Option<Retryable> {
        match res {
            Err(reqwest_middleware::Error::Reqwest(_)) => Some(Retryable::Transient),
            Err(reqwest_middleware::Error::Middleware(_)) => Some(Retryable::Transient),
            Ok(success) => {
                let status = success.status();
                if status.is_server_error() || status.as_u16() == 429 {
                    Some(Retryable::Transient)
                } else {
                    None
                }
            }
        }
    }
}

pub struct JmClient {
    client: ClientWithMiddleware,
    #[allow(dead_code)]
    cookie_jar: Arc<Jar>,
    api_domain: String,
    #[allow(dead_code)]
    pub image_domain: String,
}

impl JmClient {
    pub fn new(api_domain: String, image_domain: String) -> Self {
        let cookie_jar = Arc::new(Jar::default());
        let reqwest_client = reqwest::Client::builder()
            .cookie_provider(cookie_jar.clone())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
        let client = ClientBuilder::new(reqwest_client)
            .with(RetryTransientMiddleware::new_with_policy_and_strategy(
                retry_policy,
                JmRetryStrategy,
            ))
            .build();

        Self {
            client,
            cookie_jar,
            api_domain,
            image_domain,
        }
    }

    pub async fn login(&self, username: &str, password: &str) -> AppResult<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| AppError::Internal(format!("系统时间异常: {}", e)))?
            .as_secs();
        let token = generate_token(ts, APP_TOKEN_SECRET);
        let tokenparam = format!("{},{}", ts, APP_VERSION);

        let form = json!({
            "username": username,
            "password": password,
        });

        let url = format!("https://{}/login", self.api_domain);
        let http_resp = self
            .client
            .post(&url)
            .header("token", token)
            .header("tokenparam", tokenparam)
            .header("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
            .form(&form)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("登录请求失败: {}", e)))?;

        let status = http_resp.status();
        let body = http_resp
            .text()
            .await
            .map_err(|e| AppError::Internal(format!("读取登录响应失败: {}", e)))?;

        if status != reqwest::StatusCode::OK {
            return Err(AppError::Internal(format!(
                "Login failed with status {}: {}",
                status, body
            )));
        }

        let jm_resp: JmResp = serde_json::from_str(&body).map_err(|e| {
            AppError::Internal(format!("Failed to parse login response: {}: {}", body, e))
        })?;

        if jm_resp.code != 200 {
            return Err(AppError::Internal(format!(
                "Login failed with code {}: {}",
                jm_resp.code, jm_resp.error_msg
            )));
        }

        Ok(())
    }

    pub async fn get_comic(&self, aid: i64) -> AppResult<GetComicRespData> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| AppError::Internal(format!("系统时间异常: {}", e)))?
            .as_secs();
        let token = generate_token(ts, APP_TOKEN_SECRET);
        let tokenparam = format!("{},{}", ts, APP_VERSION);

        let url = format!("https://{}/album?id={}", self.api_domain, aid);
        let http_resp = self
            .client
            .get(&url)
            .header("token", token)
            .header("tokenparam", tokenparam)
            .header("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("获取漫画请求失败: {}", e)))?;

        let status = http_resp.status();
        let body = http_resp
            .text()
            .await
            .map_err(|e| AppError::Internal(format!("读取漫画响应失败: {}", e)))?;

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(AppError::NotFound(format!("漫画 {} 未找到", aid)));
        }
        if status != reqwest::StatusCode::OK {
            return Err(AppError::Internal(format!(
                "Get comic failed with status {}: {}",
                status, body
            )));
        }

        let jm_resp: JmResp = serde_json::from_str(&body).map_err(|e| {
            AppError::Internal(format!("Failed to parse comic response: {}: {}", body, e))
        })?;

        if jm_resp.code != 200 {
            let error_msg_lower = jm_resp.error_msg.to_lowercase();
            if jm_resp.code == 404 || error_msg_lower.contains("not found") {
                return Err(AppError::NotFound(format!("漫画 {} 未找到", aid)));
            }
            return Err(AppError::Internal(format!(
                "Get comic failed with code {}: {}",
                jm_resp.code, jm_resp.error_msg
            )));
        }

        let data = jm_resp
            .data
            .as_str()
            .ok_or_else(|| AppError::Internal("Comic data is not a string".to_string()))?;

        let decrypted_data = decrypt_data(ts, data)?;
        if raw_missing_comic(&decrypted_data) {
            return Err(AppError::NotFound(format!("漫画 {} 未找到", aid)));
        }
        let parse_context = format!("Failed to parse decrypted comic data: {}", decrypted_data);
        let comic_value: Value = match serde_json::from_str(&decrypted_data) {
            Ok(value) => value,
            Err(e) => {
                if raw_missing_comic(&decrypted_data) {
                    return Err(AppError::NotFound(format!("漫画 {} 未找到", aid)));
                }
                return Err(AppError::Internal(format!("{}: {}", parse_context, e)));
            }
        };

        if is_missing_comic(&comic_value) {
            return Err(AppError::NotFound(format!("漫画 {} 未找到", aid)));
        }

        let comic: GetComicRespData = serde_json::from_value(comic_value)
            .map_err(|e| AppError::Internal(format!("{}: {}", parse_context, e)))?;

        Ok(comic)
    }

    pub async fn get_chapter(&self, id: i64) -> AppResult<GetChapterRespData> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| AppError::Internal(format!("系统时间异常: {}", e)))?
            .as_secs();
        let token = generate_token(ts, APP_TOKEN_SECRET);
        let tokenparam = format!("{},{}", ts, APP_VERSION);

        let url = format!("https://{}/chapter?id={}", self.api_domain, id);
        let http_resp = self
            .client
            .get(&url)
            .header("token", token)
            .header("tokenparam", tokenparam)
            .header("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("获取章节请求失败: {}", e)))?;

        let status = http_resp.status();
        let body = http_resp
            .text()
            .await
            .map_err(|e| AppError::Internal(format!("读取章节响应失败: {}", e)))?;

        if status != reqwest::StatusCode::OK {
            return Err(AppError::Internal(format!(
                "Get chapter failed with status {}: {}",
                status, body
            )));
        }

        let jm_resp: JmResp = serde_json::from_str(&body).map_err(|e| {
            AppError::Internal(format!("Failed to parse chapter response: {}: {}", body, e))
        })?;

        if jm_resp.code != 200 {
            return Err(AppError::Internal(format!(
                "Get chapter failed with code {}: {}",
                jm_resp.code, jm_resp.error_msg
            )));
        }

        let data = jm_resp
            .data
            .as_str()
            .ok_or_else(|| AppError::Internal("Chapter data is not a string".to_string()))?;

        let decrypted_data = decrypt_data(ts, data)?;
        let chapter: GetChapterRespData = serde_json::from_str(&decrypted_data)
            .map_err(|e| {
                AppError::Internal(format!(
                    "Failed to parse decrypted chapter data: {}: {}",
                    decrypted_data, e
                ))
            })?;

        Ok(chapter)
    }

    pub async fn get_scramble_id(&self, id: i64) -> AppResult<i64> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| AppError::Internal(format!("系统时间异常: {}", e)))?
            .as_secs();
        let token = generate_token(ts, APP_TOKEN_SECRET_2);
        let tokenparam = format!("{},{}", ts, APP_VERSION);

        let url = format!(
            "https://{}/chapter_view_template?id={}&v={}&mode=vertical&page=0&app_img_shunt=1&express=off",
            self.api_domain, id, ts
        );
        let http_resp = self
            .client
            .get(&url)
            .header("token", token)
            .header("tokenparam", tokenparam)
            .header("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("获取 scramble_id 请求失败: {}", e)))?;

        let status = http_resp.status();
        let body = http_resp
            .text()
            .await
            .map_err(|e| AppError::Internal(format!("读取 scramble_id 响应失败: {}", e)))?;

        if status != reqwest::StatusCode::OK {
            return Err(AppError::Internal(format!(
                "Get scramble_id failed with status {}: {}",
                status, body
            )));
        }

        // 从 HTML 响应中提取 scramble_id
        let scramble_id = body
            .split("var scramble_id = ")
            .nth(1)
            .and_then(|s| s.split(';').next())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(220_980);

        Ok(scramble_id)
    }
}

fn generate_token(ts: u64, secret: &str) -> String {
    let data = format!("{}{}", ts, secret);
    format!("{:x}", md5::compute(data))
}

fn is_missing_comic(value: &Value) -> bool {
    match value.get("name") {
        None | Some(Value::Null) => true,
        Some(Value::String(name)) => name.trim().is_empty(),
        _ => false,
    }
}

fn raw_missing_comic(data: &str) -> bool {
    data.contains("\"name\":null")
        || data.contains("\"name\": null")
        || data.contains("\"name\":\"\"")
        || data.contains("\"name\": \"\"")
}

fn decrypt_data(ts: u64, data: &str) -> AppResult<String> {
    // Base64解码加密数据
    let aes256_ecb_encrypted_data = general_purpose::STANDARD
        .decode(data)
        .map_err(|e| AppError::Internal(format!("Base64解码失败: {}", e)))?;

    // 使用MD5生成密钥
    let key_str = format!("{}{}", ts, APP_DATA_SECRET);
    let key = format!("{:x}", md5::compute(&key_str));

    // 使用AES-256-ECB解密
    let cipher = Aes256::new(GenericArray::from_slice(key.as_bytes()));
    let decrypted_data_with_padding: Vec<u8> = aes256_ecb_encrypted_data
        .chunks(16)
        .map(GenericArray::clone_from_slice)
        .flat_map(|mut block| {
            cipher.decrypt_block(&mut block);
            block.to_vec()
        })
        .collect();

    // 移除PKCS#7填充
    let padding_length = decrypted_data_with_padding.last().copied().unwrap() as usize;
    let decrypted_data_without_padding =
        decrypted_data_with_padding[..decrypted_data_with_padding.len() - padding_length].to_vec();

    // 转换为UTF-8字符串
    let decrypted_data = String::from_utf8(decrypted_data_without_padding)
        .map_err(|e| AppError::Internal(format!("解密数据转UTF-8失败: {}", e)))?;
    Ok(decrypted_data)
}

pub fn calculate_block_num(scramble_id: i64, chapter_id: i64, filename: &str) -> u32 {
    if chapter_id < scramble_id {
        0
    } else if chapter_id < 268_850 {
        10
    } else {
        let x = if chapter_id < 421_926 { 10 } else { 8 };

        // 从文件名中移除文件扩展名
        let filename_without_ext = filename
            .rsplit_once('.')
            .map(|(name, _)| name)
            .unwrap_or(filename);

        let s = format!("{}{}", chapter_id, filename_without_ext);
        let md5_hash = format!("{:x}", md5::compute(&s));

        let mut block_num = md5_hash.chars().last().unwrap() as u32;
        block_num %= x;
        block_num = block_num * 2 + 2;
        block_num
    }
}
