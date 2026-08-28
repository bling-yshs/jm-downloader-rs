use crate::config::Config;
use crate::global_client::GlobalJmClient;
use crate::image_processor::{
    compress_pdf_with_gs, create_download_dir, download_image, merge_images_to_pdf,
    process_and_save_image,
};
use crate::jm_client::calculate_block_num;
use crate::models::{
    ChapterDownloadData, ComicDownloadData, DownloadChapterRequest, DownloadComicRequest,
    DownloadTaskError, DownloadTaskInfo, DownloadTaskProgress, DownloadTaskResult,
    DownloadTaskStatus, DownloadTaskType, SingleChapterData,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use chrono_tz::Asia::Shanghai;
use jm_downloader_rs::AppError;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{
    policies::ExponentialBackoff, RetryTransientMiddleware, Retryable, RetryableStrategy,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, RwLock, Semaphore};
use tokio::task::JoinSet;

const TERMINAL_TASK_RETENTION_SECONDS: i64 = 600;

/// 对网络错误、服务端错误和限流响应执行重试的策略。
struct CustomRetryStrategy;

impl RetryableStrategy for CustomRetryStrategy {
    /// 判断图片请求结果是否需要重试。
    ///
    /// # 参数
    /// - `res`: 图片请求结果。
    ///
    /// # 返回
    /// 需要重试时返回瞬时错误标记，否则返回空。
    fn handle(
        &self,
        res: &Result<reqwest::Response, reqwest_middleware::Error>,
    ) -> Option<Retryable> {
        match res {
            Err(reqwest_middleware::Error::Reqwest(error)) => {
                warn!("检测到图片网络错误，将重试: {}", error);
                Some(Retryable::Transient)
            }
            Err(reqwest_middleware::Error::Middleware(error)) => {
                warn!("检测到图片中间件错误，将重试: {}", error);
                Some(Retryable::Transient)
            }
            Ok(response)
                if response.status().is_server_error() || response.status().as_u16() == 429 =>
            {
                warn!("检测到图片服务错误 {}，将重试", response.status());
                Some(Retryable::Transient)
            }
            Ok(_) => None,
        }
    }
}

/// 可用于任务判重的规范化下载参数。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum DownloadTaskKey {
    Comic {
        comic_id: i64,
        merge: bool,
        password: Option<String>,
    },
    Chapter {
        comic_id: i64,
        chapter_ids: Vec<i64>,
    },
}

/// 后台任务实际执行所需的请求载荷。
#[derive(Clone, Debug)]
enum DownloadTaskPayload {
    Comic(DownloadComicRequest),
    Chapter(DownloadChapterRequest),
}

/// 共享章节图片缓存的资源键。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ResourceKey {
    comic_id: i64,
    chapter_id: i64,
}

impl ResourceKey {
    /// 返回共享章节图片目录。
    ///
    /// # 返回
    /// 当前资源对应的章节目录。
    fn path(&self) -> PathBuf {
        PathBuf::from("download")
            .join(self.comic_id.to_string())
            .join(self.chapter_id.to_string())
    }
}

/// 任务执行过程中共享的取消状态。
#[derive(Clone)]
struct TaskCancellation {
    requested: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl TaskCancellation {
    /// 创建未请求取消的状态。
    ///
    /// # 返回
    /// 新的取消状态。
    fn new() -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// 返回任务是否已经请求取消。
    ///
    /// # 返回
    /// 已请求取消时返回 `true`。
    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// 请求取消任务并唤醒等待者。
    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.notify.notify_one();
    }
}

/// 任务注册表中保存的内部任务数据。
struct DownloadTaskRecord {
    info: DownloadTaskInfo,
    key: DownloadTaskKey,
    payload: DownloadTaskPayload,
    retention_seconds: i64,
    expires_at: Option<DateTime<Utc>>,
    cleanup_generation: u64,
    cancellation: TaskCancellation,
    resources: Vec<ResourceKey>,
}

/// 任务注册表、判重索引和共享缓存引用。
#[derive(Default)]
struct TaskState {
    tasks: HashMap<String, DownloadTaskRecord>,
    task_keys: HashMap<DownloadTaskKey, String>,
    resource_holders: HashMap<ResourceKey, HashSet<String>>,
}

/// 任务管理器的共享内部状态。
struct TaskManagerInner {
    global_client: GlobalJmClient,
    http_client: ClientWithMiddleware,
    task_semaphore: Arc<Semaphore>,
    image_semaphore: Arc<Semaphore>,
    queue_capacity: usize,
    sequence: AtomicU64,
    state: RwLock<TaskState>,
}

/// 进程内漫画下载任务管理器。
#[derive(Clone)]
pub struct TaskManager {
    inner: Arc<TaskManagerInner>,
}

/// 已解析并可直接下载的章节元数据。
struct PreparedChapter {
    chapter_id: i64,
    chapter_title: String,
    images: Vec<String>,
    scramble_id: i64,
}

impl TaskManager {
    /// 创建任务管理器以及进程级图片 HTTP 客户端。
    ///
    /// # 参数
    /// - `config`: 应用配置。
    /// - `global_client`: 已登录的 JMComic 客户端。
    ///
    /// # 返回
    /// 创建成功时返回任务管理器，HTTP 客户端初始化失败时返回应用错误。
    pub fn new(config: &Config, global_client: GlobalJmClient) -> Result<Self, AppError> {
        let reqwest_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|error| AppError::Internal(format!("创建图片 HTTP 客户端失败: {}", error)))?;
        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
        let http_client = ClientBuilder::new(reqwest_client)
            .with(RetryTransientMiddleware::new_with_policy_and_strategy(
                retry_policy,
                CustomRetryStrategy,
            ))
            .build();

        Ok(Self {
            inner: Arc::new(TaskManagerInner {
                global_client,
                http_client,
                task_semaphore: Arc::new(Semaphore::new(config.task_concurrency)),
                image_semaphore: Arc::new(Semaphore::new(config.img_concurrency)),
                queue_capacity: config.task_queue_capacity,
                sequence: AtomicU64::new(0),
                state: RwLock::new(TaskState::default()),
            }),
        })
    }

    /// 提交普通漫画下载任务，或复用内容一致的现有任务。
    ///
    /// # 参数
    /// - `request`: 普通漫画下载请求。
    ///
    /// # 返回
    /// 返回新建或复用的任务 ID。
    pub async fn submit_comic(
        &self,
        mut request: DownloadComicRequest,
    ) -> Result<String, AppError> {
        request.encrypt = if request.merge {
            normalize_password(request.encrypt.as_deref())
        } else {
            None
        };
        let key = DownloadTaskKey::Comic {
            comic_id: request.comic_id,
            merge: request.merge,
            password: request.encrypt.clone(),
        };
        let resources = vec![ResourceKey {
            comic_id: request.comic_id,
            chapter_id: request.comic_id,
        }];
        self.submit(
            key,
            DownloadTaskPayload::Comic(request.clone()),
            DownloadTaskType::DownloadComic,
            request.expire_seconds,
            resources,
        )
        .await
    }

    /// 提交章节漫画下载任务，或复用内容一致的现有任务。
    ///
    /// # 参数
    /// - `request`: 章节漫画下载请求。
    ///
    /// # 返回
    /// 返回新建或复用的任务 ID。
    pub async fn submit_chapter(
        &self,
        request: DownloadChapterRequest,
    ) -> Result<String, AppError> {
        let key = DownloadTaskKey::Chapter {
            comic_id: request.comic_id,
            chapter_ids: request.chapter_ids.clone(),
        };
        let mut seen = HashSet::new();
        let resources = request
            .chapter_ids
            .iter()
            .copied()
            .filter(|chapter_id| seen.insert(*chapter_id))
            .map(|chapter_id| ResourceKey {
                comic_id: request.comic_id,
                chapter_id,
            })
            .collect();
        self.submit(
            key,
            DownloadTaskPayload::Chapter(request.clone()),
            DownloadTaskType::DownloadChapter,
            request.expire_seconds,
            resources,
        )
        .await
    }

    /// 查询指定下载任务的当前信息。
    ///
    /// # 参数
    /// - `task_id`: 任务 ID。
    ///
    /// # 返回
    /// 任务存在时返回任务信息，否则返回 NotFound。
    pub async fn find_task(&self, task_id: &str) -> Result<DownloadTaskInfo, AppError> {
        let state = self.inner.state.read().await;
        state
            .tasks
            .get(task_id)
            .map(|record| record.info.clone())
            .ok_or_else(|| AppError::NotFound(format!("任务 {} 不存在或已过期", task_id)))
    }

    /// 请求取消指定下载任务。
    ///
    /// # 参数
    /// - `task_id`: 任务 ID。
    ///
    /// # 返回
    /// 任务存在时返回任务 ID，否则返回 NotFound。
    pub async fn cancel_task(&self, task_id: &str) -> Result<String, AppError> {
        let mut cleanup = None;
        {
            let mut state = self.inner.state.write().await;
            let (key, status, cancellation) = state
                .tasks
                .get(task_id)
                .map(|record| {
                    (
                        record.key.clone(),
                        record.info.status.clone(),
                        record.cancellation.clone(),
                    )
                })
                .ok_or_else(|| AppError::NotFound(format!("任务 {} 不存在或已过期", task_id)))?;

            match status {
                DownloadTaskStatus::Queued => {
                    let now = Utc::now();
                    let record = state
                        .tasks
                        .get_mut(task_id)
                        .expect("任务已在写锁内确认存在");
                    record.cancellation.request();
                    record.info.status = DownloadTaskStatus::Cancelled;
                    record.info.finished_at = Some(format_timestamp(now));
                    record.expires_at = Some(expiry_after(now, TERMINAL_TASK_RETENTION_SECONDS));
                    record.info.expires_at = record.expires_at.map(format_timestamp);
                    record.cleanup_generation += 1;
                    cleanup = Some(record.cleanup_generation);
                    state.task_keys.remove(&key);
                }
                DownloadTaskStatus::Running => {
                    cancellation.request();
                    let record = state
                        .tasks
                        .get_mut(task_id)
                        .expect("任务已在写锁内确认存在");
                    record.info.status = DownloadTaskStatus::Cancelling;
                    state.task_keys.remove(&key);
                }
                DownloadTaskStatus::Cancelling
                | DownloadTaskStatus::Succeeded
                | DownloadTaskStatus::Failed
                | DownloadTaskStatus::Cancelled => {}
            }
        }

        if let Some(generation) = cleanup {
            self.schedule_cleanup(
                task_id.to_string(),
                generation,
                Duration::from_secs(TERMINAL_TASK_RETENTION_SECONDS as u64),
            );
        }
        Ok(task_id.to_string())
    }

    /// 原子执行任务复用、容量检查和新任务注册。
    ///
    /// # 参数
    /// - `key`: 规范化后的任务键。
    /// - `payload`: 后台执行载荷。
    /// - `task_type`: 对外公开的任务类型。
    /// - `retention_seconds`: 成功后的保留秒数。
    /// - `resources`: 任务引用的共享章节资源。
    ///
    /// # 返回
    /// 返回新建或复用的任务 ID。
    async fn submit(
        &self,
        key: DownloadTaskKey,
        payload: DownloadTaskPayload,
        task_type: DownloadTaskType,
        retention_seconds: i64,
        resources: Vec<ResourceKey>,
    ) -> Result<String, AppError> {
        let now = Utc::now();
        let mut rescheduled_cleanup = None;
        let task_id;
        {
            let mut state = self.inner.state.write().await;
            if let Some(existing_id) = state.task_keys.get(&key).cloned() {
                if let Some(record) = state.tasks.get_mut(&existing_id) {
                    if task_is_reusable(record, now) {
                        record.retention_seconds =
                            merge_retention(record.retention_seconds, retention_seconds);
                        if record.info.status == DownloadTaskStatus::Succeeded {
                            let new_expiry = if record.retention_seconds < 0 {
                                None
                            } else {
                                Some(expiry_after(now, retention_seconds))
                            };
                            if (record.retention_seconds < 0 && record.expires_at.is_some())
                                || new_expiry > record.expires_at
                            {
                                record.expires_at = new_expiry;
                                record.info.expires_at = new_expiry.map(format_timestamp);
                                record.cleanup_generation += 1;
                                if let Some(expiry) = new_expiry {
                                    rescheduled_cleanup = Some((
                                        existing_id.clone(),
                                        record.cleanup_generation,
                                        duration_until(expiry),
                                    ));
                                }
                            }
                        }
                        task_id = existing_id;
                        drop(state);
                        if let Some((id, generation, delay)) = rescheduled_cleanup {
                            self.schedule_cleanup(id, generation, delay);
                        }
                        return Ok(task_id);
                    }
                }
                state.task_keys.remove(&key);
            }

            let queued_tasks = state
                .tasks
                .values()
                .filter(|record| record.info.status == DownloadTaskStatus::Queued)
                .count();
            if queued_tasks >= self.inner.queue_capacity {
                return Err(AppError::TooManyTasks(format!(
                    "下载任务队列已满，最大排队数量为 {}",
                    self.inner.queue_capacity
                )));
            }

            task_id = self.generate_task_id();
            let cancellation = TaskCancellation::new();
            let info = DownloadTaskInfo {
                task_id: task_id.clone(),
                task_type,
                status: DownloadTaskStatus::Queued,
                progress: DownloadTaskProgress {
                    total_images: None,
                    succeeded_images: 0,
                    failed_images: 0,
                    current_chapter_id: None,
                },
                created_at: format_timestamp(now),
                started_at: None,
                finished_at: None,
                expires_at: None,
                result: None,
                error: None,
            };
            for resource in &resources {
                state
                    .resource_holders
                    .entry(resource.clone())
                    .or_default()
                    .insert(task_id.clone());
            }
            state.task_keys.insert(key.clone(), task_id.clone());
            state.tasks.insert(
                task_id.clone(),
                DownloadTaskRecord {
                    info,
                    key,
                    payload,
                    retention_seconds,
                    expires_at: None,
                    cleanup_generation: 0,
                    cancellation,
                    resources,
                },
            );
        }

        let manager = self.clone();
        let spawned_task_id = task_id.clone();
        tokio::spawn(async move {
            manager.run_task(spawned_task_id).await;
        });
        Ok(task_id)
    }

    /// 等待执行许可并运行一个已注册任务。
    ///
    /// # 参数
    /// - `task_id`: 待执行任务 ID。
    async fn run_task(&self, task_id: String) {
        let cancellation = {
            let state = self.inner.state.read().await;
            match state.tasks.get(&task_id) {
                Some(record) => record.cancellation.clone(),
                None => return,
            }
        };
        let permit = tokio::select! {
            permit = self.inner.task_semaphore.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return,
            },
            _ = cancellation.notify.notified() => return,
        };

        let payload = {
            let mut state = self.inner.state.write().await;
            let Some(record) = state.tasks.get_mut(&task_id) else {
                return;
            };
            if record.info.status != DownloadTaskStatus::Queued {
                return;
            }
            record.info.status = DownloadTaskStatus::Running;
            record.info.started_at = Some(format_timestamp(Utc::now()));
            record.payload.clone()
        };

        let outcome = self
            .execute_task(&task_id, payload, cancellation.clone())
            .await;
        drop(permit);
        self.finish_task(&task_id, outcome, cancellation.is_requested())
            .await;
    }

    /// 执行下载任务载荷。
    ///
    /// # 参数
    /// - `task_id`: 当前任务 ID。
    /// - `payload`: 下载任务载荷。
    /// - `cancellation`: 取消状态。
    ///
    /// # 返回
    /// 成功时返回下载结果，取消时返回空，失败时返回应用错误。
    async fn execute_task(
        &self,
        task_id: &str,
        payload: DownloadTaskPayload,
        cancellation: TaskCancellation,
    ) -> Result<Option<DownloadTaskResult>, AppError> {
        match payload {
            DownloadTaskPayload::Comic(request) => {
                self.execute_comic(task_id, request, cancellation).await
            }
            DownloadTaskPayload::Chapter(request) => {
                self.execute_chapters(task_id, request, cancellation).await
            }
        }
    }

    /// 执行普通漫画下载任务。
    ///
    /// # 参数
    /// - `task_id`: 当前任务 ID。
    /// - `request`: 普通漫画下载请求。
    /// - `cancellation`: 取消状态。
    ///
    /// # 返回
    /// 成功时返回普通漫画下载结果，取消时返回空。
    async fn execute_comic(
        &self,
        task_id: &str,
        request: DownloadComicRequest,
        cancellation: TaskCancellation,
    ) -> Result<Option<DownloadTaskResult>, AppError> {
        info!("任务 {} 开始解析普通漫画 {}", task_id, request.comic_id);
        let comic = self.inner.global_client.get_comic(request.comic_id).await?;
        if cancellation.is_requested() {
            return Ok(None);
        }
        if !comic.series.is_empty() {
            return Err(AppError::BadRequest(
                "该漫画为章节漫画，请使用章节漫画下载接口".to_string(),
            ));
        }
        let chapter = self
            .inner
            .global_client
            .get_chapter(request.comic_id)
            .await?;
        let scramble_id = self
            .inner
            .global_client
            .get_scramble_id(request.comic_id)
            .await?;
        if cancellation.is_requested() {
            return Ok(None);
        }
        self.set_total_images(task_id, chapter.images.len()).await;
        let prepared = PreparedChapter {
            chapter_id: request.comic_id,
            chapter_title: "第1话".to_string(),
            images: chapter.images,
            scramble_id,
        };
        let Some((images, image_files)) = self
            .download_chapter_images(task_id, request.comic_id, &prepared, cancellation.clone())
            .await?
        else {
            return Ok(None);
        };
        if image_files.is_empty() {
            return Err(AppError::Internal("没有成功下载的图片".to_string()));
        }

        let pdf_path = if request.merge {
            info!("任务 {} 开始生成 PDF", task_id);
            let task_dir = PathBuf::from("download").join("tasks").join(task_id);
            tokio::fs::create_dir_all(&task_dir)
                .await
                .map_err(|error| AppError::Internal(format!("创建任务 PDF 目录失败: {}", error)))?;
            let temporary_pdf = task_dir.join("merged.working.pdf");
            let final_pdf = task_dir.join("merged.pdf");
            merge_images_to_pdf(&image_files, &temporary_pdf, cancellation.requested.clone())
                .await?;
            if cancellation.is_requested() {
                return Ok(None);
            }
            compress_pdf_with_gs(
                &temporary_pdf,
                request.encrypt.as_deref(),
                cancellation.requested.clone(),
            )
            .await?;
            if cancellation.is_requested() {
                return Ok(None);
            }
            tokio::fs::rename(&temporary_pdf, &final_pdf)
                .await
                .map_err(|error| AppError::Internal(format!("发布任务 PDF 失败: {}", error)))?;
            Some(format!("download/tasks/{}/merged.pdf", task_id))
        } else {
            None
        };

        Ok(Some(DownloadTaskResult::DownloadComic(ComicDownloadData {
            comic_id: request.comic_id,
            comic_title: comic.name,
            images: if request.merge { None } else { Some(images) },
            pdf_path,
        })))
    }

    /// 执行章节漫画下载任务。
    ///
    /// # 参数
    /// - `task_id`: 当前任务 ID。
    /// - `request`: 章节漫画下载请求。
    /// - `cancellation`: 取消状态。
    ///
    /// # 返回
    /// 成功时返回章节下载结果，取消时返回空。
    async fn execute_chapters(
        &self,
        task_id: &str,
        request: DownloadChapterRequest,
        cancellation: TaskCancellation,
    ) -> Result<Option<DownloadTaskResult>, AppError> {
        info!("任务 {} 开始解析漫画 {} 的章节", task_id, request.comic_id);
        let comic = self.inner.global_client.get_comic(request.comic_id).await?;
        let mut prepared_chapters = Vec::with_capacity(request.chapter_ids.len());
        for chapter_id in &request.chapter_ids {
            if cancellation.is_requested() {
                return Ok(None);
            }
            let chapter_title = if comic.series.is_empty() {
                if *chapter_id != request.comic_id {
                    return Err(AppError::NotFound(format!(
                        "章节 {} 不存在，该漫画的章节 ID 应为 {}",
                        chapter_id, request.comic_id
                    )));
                }
                "第1话".to_string()
            } else {
                comic
                    .series
                    .iter()
                    .find(|series| series.id.parse::<i64>().ok() == Some(*chapter_id))
                    .map(|series| series.name.clone())
                    .ok_or_else(|| AppError::NotFound(format!("章节 {} 不存在", chapter_id)))?
            };
            let chapter = self.inner.global_client.get_chapter(*chapter_id).await?;
            let scramble_id = self
                .inner
                .global_client
                .get_scramble_id(*chapter_id)
                .await?;
            prepared_chapters.push(PreparedChapter {
                chapter_id: *chapter_id,
                chapter_title,
                images: chapter.images,
                scramble_id,
            });
        }
        let total_images = prepared_chapters
            .iter()
            .map(|chapter| chapter.images.len())
            .sum();
        self.set_total_images(task_id, total_images).await;

        let mut downloaded_chapters = Vec::with_capacity(prepared_chapters.len());
        for chapter in prepared_chapters {
            let Some((images, _)) = self
                .download_chapter_images(task_id, request.comic_id, &chapter, cancellation.clone())
                .await?
            else {
                return Ok(None);
            };
            downloaded_chapters.push(SingleChapterData {
                chapter_id: chapter.chapter_id,
                chapter_title: chapter.chapter_title,
                images,
            });
        }
        let succeeded_images = self.succeeded_images(task_id).await;
        if succeeded_images == 0 {
            return Err(AppError::Internal("没有成功下载的图片".to_string()));
        }

        Ok(Some(DownloadTaskResult::DownloadChapter(
            ChapterDownloadData {
                comic_id: request.comic_id,
                comic_title: comic.name,
                chapters: downloaded_chapters,
            },
        )))
    }

    /// 下载并处理一个章节的图片，同时复用完整的本地图片。
    ///
    /// # 参数
    /// - `task_id`: 当前任务 ID。
    /// - `comic_id`: 漫画 ID。
    /// - `chapter`: 已解析章节。
    /// - `cancellation`: 取消状态。
    ///
    /// # 返回
    /// 返回公开图片路径和本地图片路径；任务取消时返回空。
    async fn download_chapter_images(
        &self,
        task_id: &str,
        comic_id: i64,
        chapter: &PreparedChapter,
        cancellation: TaskCancellation,
    ) -> Result<Option<(Vec<String>, Vec<PathBuf>)>, AppError> {
        self.set_current_chapter(task_id, chapter.chapter_id).await;
        let chapter_dir = create_download_dir(comic_id, chapter.chapter_id)?;
        let mut join_set = JoinSet::new();
        for (index, filename) in chapter.images.iter().enumerate() {
            let url = format!(
                "https://{}/media/photos/{}/{}",
                self.inner.global_client.image_domain(),
                chapter.chapter_id,
                filename
            );
            let save_filename = format!("{:04}.png", index + 1);
            let save_path = chapter_dir.join(&save_filename);
            let relative_path = format!(
                "download/{}/{}/{}",
                comic_id, chapter.chapter_id, save_filename
            );
            let block_num = calculate_block_num(chapter.scramble_id, chapter.chapter_id, filename);
            let http_client = self.inner.http_client.clone();
            let image_semaphore = self.inner.image_semaphore.clone();
            let cancellation_for_image = cancellation.clone();
            let task_id_for_image = task_id.to_string();
            join_set.spawn(async move {
                let _permit = image_semaphore
                    .acquire_owned()
                    .await
                    .map_err(|_| AppError::Internal("图片并发控制器已关闭".to_string()))?;
                if cancellation_for_image.is_requested() {
                    return Err(AppError::Internal("任务已取消".to_string()));
                }
                if tokio::fs::metadata(&save_path).await.is_ok() {
                    return Ok((index, relative_path, save_path));
                }
                let image_data = download_image(&http_client, &url).await?;
                if cancellation_for_image.is_requested() {
                    return Err(AppError::Internal("任务已取消".to_string()));
                }
                process_and_save_image(
                    image_data,
                    block_num,
                    &save_path,
                    &task_id_for_image,
                    cancellation_for_image.requested.clone(),
                )
                .await?;
                Ok((index, relative_path, save_path))
            });
        }

        let mut images = Vec::new();
        let mut image_files = Vec::new();
        while !join_set.is_empty() {
            if cancellation.is_requested() {
                join_set.abort_all();
                while join_set.join_next().await.is_some() {}
                return Ok(None);
            }
            tokio::select! {
                _ = cancellation.notify.notified() => {
                    join_set.abort_all();
                    while join_set.join_next().await.is_some() {}
                    return Ok(None);
                }
                result = join_set.join_next() => {
                    match result {
                        Some(Ok(Ok((index, relative_path, save_path)))) => {
                            self.increment_succeeded_images(task_id).await;
                            images.push((index, relative_path));
                            image_files.push((index, save_path));
                        }
                        Some(Ok(Err(error))) if !cancellation.is_requested() => {
                            warn!("任务 {} 跳过下载失败的图片: {}", task_id, error);
                            self.increment_failed_images(task_id).await;
                        }
                        Some(Err(error)) if !error.is_cancelled() => {
                            warn!("任务 {} 的图片子任务异常: {}", task_id, error);
                            self.increment_failed_images(task_id).await;
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
            }
        }
        images.sort_by_key(|(index, _)| *index);
        image_files.sort_by_key(|(index, _)| *index);
        Ok(Some((
            images.into_iter().map(|(_, path)| path).collect(),
            image_files.into_iter().map(|(_, path)| path).collect(),
        )))
    }

    /// 使用任务执行结果完成状态迁移并安排清理。
    ///
    /// # 参数
    /// - `task_id`: 当前任务 ID。
    /// - `outcome`: 任务执行结果。
    /// - `cancelled`: 是否已经请求取消。
    async fn finish_task(
        &self,
        task_id: &str,
        outcome: Result<Option<DownloadTaskResult>, AppError>,
        cancelled: bool,
    ) {
        let now = Utc::now();
        let cleanup = {
            let mut state = self.inner.state.write().await;
            let (key, remove_key, cleanup) = {
                let Some(record) = state.tasks.get_mut(task_id) else {
                    return;
                };
                if matches!(
                    record.info.status,
                    DownloadTaskStatus::Succeeded
                        | DownloadTaskStatus::Failed
                        | DownloadTaskStatus::Cancelled
                ) {
                    return;
                }
                record.info.progress.current_chapter_id = None;
                record.info.finished_at = Some(format_timestamp(now));
                let (delay, remove_key) = if cancelled
                    || outcome.as_ref().is_ok_and(|result| result.is_none())
                {
                    record.info.status = DownloadTaskStatus::Cancelled;
                    record.expires_at = Some(expiry_after(now, TERMINAL_TASK_RETENTION_SECONDS));
                    (
                        Some(Duration::from_secs(TERMINAL_TASK_RETENTION_SECONDS as u64)),
                        true,
                    )
                } else {
                    match outcome {
                        Ok(Some(result)) => {
                            record.info.status = DownloadTaskStatus::Succeeded;
                            record.info.result = Some(result);
                            let delay = if record.retention_seconds < 0 {
                                record.expires_at = None;
                                None
                            } else {
                                record.expires_at =
                                    Some(expiry_after(now, record.retention_seconds));
                                Some(Duration::from_secs(record.retention_seconds as u64))
                            };
                            (delay, record.info.progress.failed_images > 0)
                        }
                        Ok(None) => unreachable!("取消结果已在前置分支处理"),
                        Err(error) => {
                            record.info.status = DownloadTaskStatus::Failed;
                            record.info.error = Some(DownloadTaskError {
                                code: error.code().to_string(),
                                message: error.message(),
                            });
                            record.expires_at =
                                Some(expiry_after(now, TERMINAL_TASK_RETENTION_SECONDS));
                            (
                                Some(Duration::from_secs(TERMINAL_TASK_RETENTION_SECONDS as u64)),
                                true,
                            )
                        }
                    }
                };
                record.info.expires_at = record.expires_at.map(format_timestamp);
                let cleanup = delay.map(|delay| {
                    record.cleanup_generation += 1;
                    (record.cleanup_generation, delay)
                });
                (record.key.clone(), remove_key, cleanup)
            };
            if remove_key {
                state.task_keys.remove(&key);
            }
            cleanup
        };
        if let Some((generation, delay)) = cleanup {
            self.schedule_cleanup(task_id.to_string(), generation, delay);
        }
    }

    /// 更新任务的图片总数。
    ///
    /// # 参数
    /// - `task_id`: 任务 ID。
    /// - `total_images`: 图片总数。
    async fn set_total_images(&self, task_id: &str, total_images: usize) {
        let mut state = self.inner.state.write().await;
        if let Some(record) = state.tasks.get_mut(task_id) {
            record.info.progress.total_images = Some(total_images);
        }
    }

    /// 更新任务当前处理的章节 ID。
    ///
    /// # 参数
    /// - `task_id`: 任务 ID。
    /// - `chapter_id`: 当前章节 ID。
    async fn set_current_chapter(&self, task_id: &str, chapter_id: i64) {
        let mut state = self.inner.state.write().await;
        if let Some(record) = state.tasks.get_mut(task_id) {
            record.info.progress.current_chapter_id = Some(chapter_id);
        }
    }

    /// 增加任务成功图片数量。
    ///
    /// # 参数
    /// - `task_id`: 任务 ID。
    async fn increment_succeeded_images(&self, task_id: &str) {
        let mut state = self.inner.state.write().await;
        if let Some(record) = state.tasks.get_mut(task_id) {
            record.info.progress.succeeded_images += 1;
        }
    }

    /// 增加任务失败图片数量。
    ///
    /// # 参数
    /// - `task_id`: 任务 ID。
    async fn increment_failed_images(&self, task_id: &str) {
        let mut state = self.inner.state.write().await;
        if let Some(record) = state.tasks.get_mut(task_id) {
            record.info.progress.failed_images += 1;
        }
    }

    /// 返回任务当前成功图片数量。
    ///
    /// # 参数
    /// - `task_id`: 任务 ID。
    ///
    /// # 返回
    /// 当前成功图片数量；任务不存在时返回零。
    async fn succeeded_images(&self, task_id: &str) -> usize {
        let state = self.inner.state.read().await;
        state
            .tasks
            .get(task_id)
            .map(|record| record.info.progress.succeeded_images)
            .unwrap_or(0)
    }

    /// 安排终态任务和关联文件的延迟清理。
    ///
    /// # 参数
    /// - `task_id`: 任务 ID。
    /// - `generation`: 清理代次，用于使旧计时器失效。
    /// - `delay`: 清理前等待时长。
    fn schedule_cleanup(&self, task_id: String, generation: u64, delay: Duration) {
        let manager = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            manager.cleanup_task(&task_id, generation).await;
        });
    }

    /// 清理指定代次的终态任务及不再使用的缓存目录。
    ///
    /// # 参数
    /// - `task_id`: 任务 ID。
    /// - `generation`: 清理代次。
    async fn cleanup_task(&self, task_id: &str, generation: u64) {
        let (resource_paths, task_path) = {
            let mut state = self.inner.state.write().await;
            let Some(record) = state.tasks.get(task_id) else {
                return;
            };
            if record.cleanup_generation != generation {
                return;
            }
            if let Some(expires_at) = record.expires_at {
                if expires_at > Utc::now() {
                    return;
                }
            } else {
                return;
            }
            let key = record.key.clone();
            let resources = record.resources.clone();
            state.tasks.remove(task_id);
            if state.task_keys.get(&key).is_some_and(|id| id == task_id) {
                state.task_keys.remove(&key);
            }
            let mut resource_paths = Vec::new();
            for resource in resources {
                if let Some(holders) = state.resource_holders.get_mut(&resource) {
                    holders.remove(task_id);
                    if holders.is_empty() {
                        resource_paths.push(resource.path());
                        state.resource_holders.remove(&resource);
                    }
                }
            }
            (
                resource_paths,
                PathBuf::from("download").join("tasks").join(task_id),
            )
        };

        for path in resource_paths {
            if let Err(error) = tokio::fs::remove_dir_all(&path).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    warn!("删除共享图片目录 {} 失败: {}", path.display(), error);
                }
            }
        }
        if let Err(error) = tokio::fs::remove_dir_all(&task_path).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!("删除任务目录 {} 失败: {}", task_path.display(), error);
            }
        }
    }

    /// 生成进程内唯一的任务 ID。
    ///
    /// # 返回
    /// 32 位十六进制任务 ID。
    fn generate_task_id(&self) -> String {
        let sequence = self.inner.sequence.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{:x}", md5::compute(format!("{}:{}", nanos, sequence)))
    }
}

/// 规范化 PDF 密码。
///
/// # 参数
/// - `password`: 原始可选密码。
///
/// # 返回
/// 去除首尾空白后的非空密码。
fn normalize_password(password: Option<&str>) -> Option<String> {
    password
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 合并两个任务保留时长。
///
/// # 参数
/// - `current`: 当前保留秒数。
/// - `requested`: 新请求的保留秒数。
///
/// # 返回
/// 永久保留优先，否则返回较大的秒数。
fn merge_retention(current: i64, requested: i64) -> i64 {
    if current < 0 || requested < 0 {
        -1
    } else {
        current.max(requested)
    }
}

/// 判断任务是否允许通过任务键复用。
///
/// # 参数
/// - `record`: 任务记录。
/// - `now`: 当前 UTC 时间。
///
/// # 返回
/// 任务仍可复用时返回 `true`。
fn task_is_reusable(record: &DownloadTaskRecord, now: DateTime<Utc>) -> bool {
    match record.info.status {
        DownloadTaskStatus::Queued | DownloadTaskStatus::Running => true,
        DownloadTaskStatus::Succeeded => {
            record.info.progress.failed_images == 0
                && record.info.result.is_some()
                && record.expires_at.is_none_or(|expires_at| expires_at > now)
        }
        DownloadTaskStatus::Cancelling
        | DownloadTaskStatus::Failed
        | DownloadTaskStatus::Cancelled => false,
    }
}

/// 将 UTC 时间格式化为北京时间接口字符串。
///
/// # 参数
/// - `time`: UTC 时间。
///
/// # 返回
/// 带毫秒和时区偏移的北京时间字符串。
fn format_timestamp(time: DateTime<Utc>) -> String {
    time.with_timezone(&Shanghai)
        .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
        .to_string()
}

/// 计算给定秒数后的 UTC 过期时间。
///
/// # 参数
/// - `base`: 起始 UTC 时间。
/// - `seconds`: 正数秒数。
///
/// # 返回
/// 可表示范围内的目标时间，发生上溢时返回 UTC 最大时间。
fn expiry_after(base: DateTime<Utc>, seconds: i64) -> DateTime<Utc> {
    base.checked_add_signed(ChronoDuration::seconds(seconds))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// 计算从当前时间到目标时间的非负等待时长。
///
/// # 参数
/// - `target`: 目标 UTC 时间。
///
/// # 返回
/// 非负标准库时长。
fn duration_until(target: DateTime<Utc>) -> Duration {
    (target - Utc::now())
        .to_std()
        .unwrap_or_else(|_| Duration::from_secs(0))
}
