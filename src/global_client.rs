// 全局 JmClient 管理模块
// 提供线程安全的客户端访问和自动会话管理

use jm_downloader_rs::AppError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::domain_selector::select_api_domains;
use crate::jm_client::JmClient;
use crate::models::{GetChapterRespData, GetComicRespData};

type Result<T> = std::result::Result<T, AppError>;

/// 全局 JmClient 管理器，提供线程安全的客户端访问和自动会话管理
#[derive(Clone)]
pub struct GlobalJmClient {
    /// 内部客户端实例，使用 RwLock 保证并发安全
    client: Arc<RwLock<JmClient>>,
    /// 认证凭据 - 用户名
    username: String,
    /// 认证凭据 - 密码
    password: String,
    /// 图片域名
    pub image_domain: String,
    /// 会话状态标记（用于优化：避免频繁检查）
    session_valid: Arc<RwLock<bool>>,
}

impl GlobalJmClient {
    /// 按配置指定或自动选择 API 域名，创建全局客户端并立即登录。
    ///
    /// # 参数
    /// - config: 应用配置
    ///
    /// # 返回
    /// - Ok(GlobalJmClient): 成功创建并登录的客户端
    /// - Err: 创建或登录失败
    pub async fn new(config: &Config) -> Result<Self> {
        let api_domain = select_api_domains(config.api_domain.as_deref())
            .await?
            .remove(0);
        info!("选中 JM API 线路: {}", api_domain);
        let client = JmClient::new(api_domain, config.image_domain.clone());

        // 立即执行登录
        client
            .login(&config.jm_username, &config.jm_password)
            .await?;

        info!("全局 JmClient 初始化成功，已完成登录");

        Ok(Self {
            client: Arc::new(RwLock::new(client)),
            username: config.jm_username.clone(),
            password: config.jm_password.clone(),
            image_domain: config.image_domain.clone(),
            session_valid: Arc::new(RwLock::new(true)),
        })
    }

    /// 获取客户端的只读引用（用于读操作）
    ///
    /// 在执行操作前会自动检查会话有效性，如果会话失效会自动重新登录
    async fn get_client(&self) -> Result<tokio::sync::RwLockReadGuard<'_, JmClient>> {
        // 先检查会话是否有效
        self.ensure_session_valid().await?;

        // 返回只读锁
        Ok(self.client.read().await)
    }

    /// 确保会话有效，如果无效则重新登录
    async fn ensure_session_valid(&self) -> Result<()> {
        // 快速路径：如果标记为有效，直接返回
        {
            let valid = self.session_valid.read().await;
            if *valid {
                return Ok(());
            }
        }

        // 会话可能失效，需要重新登录
        self.relogin().await
    }

    /// 在检测到会话失效时重新登录。
    ///
    /// # 返回
    /// 登录成功时返回空，登录失败时返回应用错误。
    async fn relogin(&self) -> Result<()> {
        // 获取写锁以执行重新登录
        let mut session_valid = self.session_valid.write().await;

        // 双重检查：可能其他线程已经完成了重新登录
        if *session_valid {
            return Ok(());
        }

        warn!("检测到会话失效，正在重新登录...");

        // 获取客户端读锁
        let client = self.client.read().await;

        // 执行登录
        client.login(&self.username, &self.password).await?;

        // 标记会话为有效
        *session_valid = true;

        info!("重新登录成功");
        Ok(())
    }

    /// 标记会话为失效（当 API 调用返回认证错误时调用）
    async fn mark_session_invalid(&self) {
        let mut valid = self.session_valid.write().await;
        *valid = false;
        warn!("会话已标记为失效");
    }

    /// 获取漫画信息，失败时最多尝试两条低延迟备选线路。
    ///
    /// # 参数
    /// - `aid`: 漫画 ID。
    ///
    /// # 返回
    /// 返回漫画数据，全部尝试失败时返回错误。
    pub async fn get_comic(&self, aid: i64) -> Result<GetComicRespData> {
        self.request_with_failover(|client| Box::pin(client.get_comic(aid)))
            .await
    }

    /// 获取章节信息，失败时最多尝试两条低延迟备选线路。
    ///
    /// # 参数
    /// - `id`: 章节 ID。
    ///
    /// # 返回
    /// 返回章节数据，全部尝试失败时返回错误。
    pub async fn get_chapter(&self, id: i64) -> Result<GetChapterRespData> {
        self.request_with_failover(|client| Box::pin(client.get_chapter(id)))
            .await
    }

    /// 获取图片重排 ID，失败时最多尝试两条低延迟备选线路。
    ///
    /// # 参数
    /// - `id`: 章节 ID。
    ///
    /// # 返回
    /// 返回图片重排 ID，全部尝试失败时返回错误。
    pub async fn get_scramble_id(&self, id: i64) -> Result<i64> {
        self.request_with_failover(|client| Box::pin(client.get_scramble_id(id)))
            .await
    }

    /// 执行读取请求，保留认证重试，并在故障时按实时探测耗时尝试两条备选。
    ///
    /// # 参数
    /// - `request`: 可在不同客户端上重复执行的异步读取操作。
    ///
    /// # 返回
    /// 返回请求结果并将成功备选设为当前线路；全部失败时返回最后一次错误。
    async fn request_with_failover<T, F>(&self, request: F) -> Result<T>
    where
        T: Send,
        F: for<'a> Fn(&'a JmClient) -> Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>
            + Send
            + Sync,
    {
        let mut failed_domain = self.client.read().await.api_domain.clone();
        let initial = async {
            let client = self.get_client().await?;
            failed_domain = client.api_domain.clone();
            let result = request(&client).await;
            drop(client);
            match result {
                Err(error) if is_auth_error(&error) => {
                    warn!("检测到认证错误，尝试重新登录: {}", error);
                    self.mark_session_invalid().await;
                    self.relogin().await?;
                    let client = self.get_client().await?;
                    failed_domain = client.api_domain.clone();
                    request(&client).await
                }
                result => result,
            }
        }
        .await;
        let mut last_error = match initial {
            Ok(result) => return Ok(result),
            Err(error @ (AppError::NotFound(_) | AppError::BadRequest(_))) => return Err(error),
            Err(error) => error,
        };

        warn!(
            "JM API 线路 {} 请求失败，开始探测备选: {}",
            failed_domain, last_error
        );
        let domains = match select_api_domains(None).await {
            Ok(domains) => domains,
            Err(error) => {
                warn!("获取备选线路失败: {}", error);
                return Err(last_error);
            }
        };
        for domain in domains
            .iter()
            .filter(|domain| *domain != &failed_domain)
            .take(2)
        {
            info!("尝试 JM API 备选线路: {}", domain);
            let candidate = JmClient::new(domain.clone(), self.image_domain.clone());
            let result = async {
                candidate.login(&self.username, &self.password).await?;
                request(&candidate).await
            }
            .await;
            match result {
                Ok(result) => {
                    // 与重新登录保持相同的锁顺序，完整替换客户端及其登录会话。
                    let mut session_valid = self.session_valid.write().await;
                    let mut client = self.client.write().await;
                    *client = candidate;
                    *session_valid = true;
                    info!("JM API 请求成功，当前线路已切换为: {}", domain);
                    return Ok(result);
                }
                Err(error) => {
                    warn!("JM API 备选线路 {} 失败: {}", domain, error);
                    last_error = error;
                }
            }
        }
        Err(last_error)
    }

    /// 获取图片域名（用于构建图片 URL）
    pub fn image_domain(&self) -> &str {
        &self.image_domain
    }
}

/// 判断错误是否为认证错误
fn is_auth_error(error: &AppError) -> bool {
    let error_msg = error.to_string().to_lowercase();

    // 常见的认证失败标识
    error_msg.contains("unauthorized")
        || error_msg.contains("401")
        || error_msg.contains("登录")
        || error_msg.contains("认证")
        || error_msg.contains("session")
        || error_msg.contains("cookie")
        // JMComic API 特定的错误码
        || error_msg.contains("code 401")
        || error_msg.contains("code 403")
}
