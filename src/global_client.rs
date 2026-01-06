// 全局 JmClient 管理模块
// 提供线程安全的客户端访问和自动会话管理

use std::sync::Arc;
use tokio::sync::RwLock;
use jm_downloader_rs::AppError;

use crate::jm_client::JmClient;
use crate::config::Config;
use crate::models::{GetComicRespData, GetChapterRespData};

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
    /// 创建新的全局客户端实例并立即登录
    ///
    /// # 参数
    /// - config: 应用配置
    ///
    /// # 返回
    /// - Ok(GlobalJmClient): 成功创建并登录的客户端
    /// - Err: 创建或登录失败
    pub async fn new(config: &Config) -> Result<Self> {
        let client = JmClient::new(
            config.api_domain.clone(),
            config.image_domain.clone(),
        );

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

    /// 重新登录（当检测到会话失效时调用）
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
        client
            .login(&self.username, &self.password)
            .await?;

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

    /// 执行带自动重试的 API 调用 - 获取漫画信息
    ///
    /// 如果第一次调用因认证失败，会自动重新登录并重试一次
    pub async fn get_comic(&self, aid: i64) -> Result<GetComicRespData> {
        // 第一次尝试
        let client = self.get_client().await?;
        match client.get_comic(aid).await {
            Ok(result) => Ok(result),
            Err(e) => {
                // 检查是否是认证错误
                if is_auth_error(&e) {
                    warn!("检测到认证错误，尝试重新登录: {}", e);
                    drop(client); // 释放读锁

                    // 标记会话失效
                    self.mark_session_invalid().await;

                    // 重新登录
                    self.relogin().await?;

                    // 重试一次
                    let client = self.get_client().await?;
                    client.get_comic(aid).await
                } else {
                    // 非认证错误，直接返回
                    Err(e)
                }
            }
        }
    }

    /// 执行带自动重试的 API 调用 - 获取章节信息
    pub async fn get_chapter(&self, id: i64) -> Result<GetChapterRespData> {
        // 第一次尝试
        let client = self.get_client().await?;
        match client.get_chapter(id).await {
            Ok(result) => Ok(result),
            Err(e) => {
                // 检查是否是认证错误
                if is_auth_error(&e) {
                    warn!("检测到认证错误，尝试重新登录: {}", e);
                    drop(client); // 释放读锁

                    // 标记会话失效
                    self.mark_session_invalid().await;

                    // 重新登录
                    self.relogin().await?;

                    // 重试一次
                    let client = self.get_client().await?;
                    client.get_chapter(id).await
                } else {
                    // 非认证错误，直接返回
                    Err(e)
                }
            }
        }
    }

    /// 执行带自动重试的 API 调用 - 获取 scramble ID
    pub async fn get_scramble_id(&self, id: i64) -> Result<i64> {
        // 第一次尝试
        let client = self.get_client().await?;
        match client.get_scramble_id(id).await {
            Ok(result) => Ok(result),
            Err(e) => {
                // 检查是否是认证错误
                if is_auth_error(&e) {
                    warn!("检测到认证错误，尝试重新登录: {}", e);
                    drop(client); // 释放读锁

                    // 标记会话失效
                    self.mark_session_invalid().await;

                    // 重新登录
                    self.relogin().await?;

                    // 重试一次
                    let client = self.get_client().await?;
                    client.get_scramble_id(id).await
                } else {
                    // 非认证错误，直接返回
                    Err(e)
                }
            }
        }
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
