use rocket::serde::json::Json;
use rocket::State;
use rocket_okapi::openapi;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::sleep;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff, Retryable, RetryableStrategy};

use crate::config::Config;
use crate::global_client::GlobalJmClient;
use crate::image_processor::{compress_pdf_with_gs, create_download_dir, download_image, merge_images_to_pdf, process_and_save_image};
use crate::jm_client::calculate_block_num;
use crate::models::{GetComicInfoRequest, ComicInfo, DownloadChapterRequest, DownloadComicRequest, ChapterDownloadData, SingleChapterData, ComicDownloadData};
use jm_downloader_rs::{ApiResult, AppError, R};

/// 自定义重试策略：对网络错误和5xx错误都进行重试
struct CustomRetryStrategy;

impl RetryableStrategy for CustomRetryStrategy {
    fn handle(&self, res: &Result<reqwest::Response, reqwest_middleware::Error>) -> Option<Retryable> {
        match res {
            // 网络错误：重试
            Err(reqwest_middleware::Error::Reqwest(e)) => {
                warn!("检测到网络错误，将重试: {} (is_timeout: {}, is_connect: {}, is_body: {})",
                    e, e.is_timeout(), e.is_connect(), e.is_body());
                Some(Retryable::Transient)
            }
            // 中间件错误：重试
            Err(reqwest_middleware::Error::Middleware(_)) => {
                warn!("检测到中间件错误，将重试");
                Some(Retryable::Transient)
            }
            // HTTP 响应成功
            Ok(success) => {
                let status = success.status();
                // 5xx 服务器错误：重试
                if status.is_server_error() {
                    warn!("检测到服务器错误 {}，将重试", status);
                    Some(Retryable::Transient)
                }
                // 429 请求过多：重试
                else if status.as_u16() == 429 {
                    warn!("检测到请求限流 429，将重试");
                    Some(Retryable::Transient)
                }
                // 其他成功或客户端错误：不重试
                else {
                    None
                }
            }
        }
    }
}

/// # 获取漫画信息
/// 根据漫画 ID 获取标题、类型、作者、简介等信息。
#[openapi]
#[post("/api/comic/getInfo", data = "<request>")]
pub async fn get_comic_info(
    global_client: &State<GlobalJmClient>,
    request: Json<GetComicInfoRequest>,
) -> ApiResult<R<ComicInfo>> {
    // 使用全局客户端获取漫画信息（带自动重试）
    let comic = match global_client.get_comic(request.id).await {
        Ok(comic) => comic,
        Err(e) => {
            error!("获取漫画 {} 失败: {}", request.id, e);
            return Err(e);
        }
    };

    // 判断漫画类型：如果有章节则为章节漫画，否则为普通漫画
    let comic_type = if comic.series.is_empty() {
        "普通漫画".to_string()
    } else {
        "章节漫画".to_string()
    };

    // 计算总页数（仅普通漫画返回，避免章节漫画因请求过多被风控）
    let total_pages = if comic.series.is_empty() {
        // 普通漫画：获取漫画本身的图片数量
        let chapter = match global_client.get_chapter(request.id).await {
            Ok(chapter) => chapter,
            Err(e) => {
                error!("获取章节 {} 失败: {}", request.id, e);
                return Err(e);
            }
        };
        Some(chapter.images.len())
    } else {
        // 章节漫画：不返回页数，避免遍历所有章节导致请求过多被风控
        None
    };

    // 构建响应数据
    let comic_info = ComicInfo {
        comic_id: request.id,
        title: comic.name,
        comic_type,
        total_views: if comic.total_views.is_empty() {
            None
        } else {
            Some(comic.total_views)
        },
        likes: if comic.likes.is_empty() {
            None
        } else {
            Some(comic.likes)
        },
        authors: comic.author,
        description: comic.description,
        total_pages,
    };

    info!("获取漫画 {} 信息成功", request.id);

    Ok(R::success(comic_info))
}

/// # 下载章节漫画
/// 批量下载指定章节，返回每章图片路径列表，支持过期自动清理。
#[openapi]
#[post("/api/comic/downloadChapter", data = "<request>")]
pub async fn download_chapter(
    config: &State<Config>,
    global_client: &State<GlobalJmClient>,
    request: Json<DownloadChapterRequest>,
) -> ApiResult<R<ChapterDownloadData>> {
    let comic_id = request.comic_id;
    let chapter_ids = &request.chapter_ids;
    let expire_seconds = request.expire_seconds;

    // 验证章节ID列表不为空
    if chapter_ids.is_empty() {
        return Err(AppError::BadRequest("章节ID列表不能为空".to_string()));
    }
    if expire_seconds < -1 {
        return Err(AppError::BadRequest("过期时间必须为-1或非负数".to_string()));
    }

    info!("开始下载章节漫画: comic_id={}, chapter_ids={:?}", comic_id, chapter_ids);

    // 使用全局客户端获取漫画信息（带自动重试）
    let comic = match global_client.get_comic(comic_id).await {
        Ok(comic) => comic,
        Err(e) => {
            error!("获取漫画 {} 失败: {}", comic_id, e);
            return Err(e);
        }
    };

    // 创建用于下载图片的HTTP客户端，带重试机制
    let reqwest_client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return Err(AppError::Internal(format!("创建HTTP客户端失败: {}", e)));
        }
    };

    // 配置指数退避重试策略：最多重试3次
    let retry_policy = ExponentialBackoff::builder()
        .build_with_max_retries(3);

    let http_client = ClientBuilder::new(reqwest_client)
        .with(RetryTransientMiddleware::new_with_policy_and_strategy(
            retry_policy,
            CustomRetryStrategy,
        ))
        .build();

    info!("已配置图片下载重试策略：最多重试3次，使用指数退避");

    let img_concurrency = config.img_concurrency;
    let image_domain = global_client.image_domain().to_string();

    // 创建信号量控制并发数
    let semaphore = Arc::new(Semaphore::new(img_concurrency));

    // 存储所有章节的下载结果
    let mut all_chapters_data = Vec::new();

    // 遍历每个章节ID进行下载
    for &chapter_id in chapter_ids {
        info!("处理章节: {}", chapter_id);

        // 查找指定的章节
        let chapter_name = if comic.series.is_empty() {
            // 普通漫画没有章节列表，检查 chapter_id 是否等于 comic_id
            if chapter_id != comic_id {
                return Err(AppError::NotFound(format!(
                    "章节 {} 不存在，该漫画为普通漫画，章节ID应等于漫画ID {}",
                    chapter_id, comic_id
                )));
            }
            "第1话".to_string()
        } else {
            // 章节漫画，查找章节名称
            comic
                .series
                .iter()
                .find(|s| s.id.parse::<i64>().ok() == Some(chapter_id))
                .map(|s| s.name.clone())
                .ok_or_else(|| {
                    AppError::NotFound(format!("章节 {} 不存在", chapter_id))
                })?
        };

        // 使用全局客户端获取章节详情和 scramble ID
        let chapter = match global_client.get_chapter(chapter_id).await {
            Ok(chapter) => chapter,
            Err(e) => {
                error!("获取章节 {} 失败: {}", chapter_id, e);
                return Err(e);
            }
        };

        let scramble_id = match global_client.get_scramble_id(chapter_id).await {
            Ok(scramble_id) => scramble_id,
            Err(e) => {
                error!("获取 scramble_id 失败: {}", e);
                return Err(e);
            }
        };

        // 创建下载目录
        let chapter_dir = match create_download_dir(comic_id, chapter_id) {
            Ok(chapter_dir) => chapter_dir,
            Err(e) => {
                error!("创建下载目录失败: {}", e);
                return Err(e);
            }
        };

        info!("开始并发下载章节 {} 的 {} 张图片，并发数 {}",
            chapter_id, chapter.images.len(), img_concurrency);

        // 创建 JoinSet 用于并发下载
        let mut join_set = JoinSet::new();

        let total_images = chapter.images.len();

        for (index, filename) in chapter.images.iter().enumerate() {
            let url = format!(
                "https://{}/media/photos/{}/{}",
                image_domain, chapter_id, filename
            );
            let block_num = calculate_block_num(scramble_id, chapter_id, filename);
            let save_filename = format!("{:04}.png", index + 1);
            let save_path = chapter_dir.join(&save_filename);
            let relative_path = format!("download/{}/{}/{}", comic_id, chapter_id, save_filename);

            // 克隆用于异步任务
            let http_client = http_client.clone();
            let filename = filename.clone();
            let semaphore = semaphore.clone();

            // 启动并发下载任务
            join_set.spawn(async move {
                // 获取信号量许可
                let _permit = semaphore.acquire().await.unwrap();

                if tokio::fs::metadata(&save_path).await.is_ok() {
                    info!("图片已存在，跳过下载: {}", save_path.display());
                    return Ok::<(usize, String), AppError>((index, relative_path));
                }

                info!("下载图片 {}/{}: {}", index + 1, total_images, url);

                // 下载图片
                let img_data = download_image(&http_client, &url).await?;

                // 处理并保存图片
                info!("处理图片: {} (block_num: {})", filename, block_num);
                process_and_save_image(img_data, block_num, &save_path).await?;

                // 返回图片路径
                Ok::<(usize, String), AppError>((index, relative_path))
            });
        }

        // 等待所有下载完成并收集结果
        let mut images = Vec::new();
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok((index, file_path))) => {
                    images.push((index, file_path));
                }
                Ok(Err(e)) => {
                    error!("下载图片失败: {}", e);
                    return Err(e);
                }
                Err(e) => {
                    error!("任务崩溃: {}", e);
                    return Err(AppError::Internal(format!("任务崩溃: {}", e)));
                }
            }
        }

        // 按索引排序以保持顺序
        images.sort_by_key(|(index, _)| *index);
        let images: Vec<String> = images.into_iter().map(|(_, path)| path).collect();

        info!("完成下载章节 {} 的 {} 张图片", chapter_id, images.len());

        // 添加到结果列表
        all_chapters_data.push(SingleChapterData {
            chapter_id,
            chapter_title: chapter_name,
            images,
        });

        schedule_delete_dir(chapter_dir, expire_seconds);
    }

    let response_data = ChapterDownloadData {
        comic_id,
        comic_title: comic.name,
        chapters: all_chapters_data,
    };

    Ok(R::success(response_data))
}

/// # 下载普通漫画
/// 仅支持无章节漫画，merge为true时会合并为PDF，encrypt传入则启用加密，支持过期自动清理。
#[openapi]
#[post("/api/comic/downloadComic", data = "<request>")]
pub async fn download_comic(
    config: &State<Config>,
    global_client: &State<GlobalJmClient>,
    request: Json<DownloadComicRequest>,
) -> ApiResult<R<ComicDownloadData>> {
    let comic_id = request.comic_id;
    let merge = request.merge;
    let pdf_password = request
        .encrypt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let expire_seconds = request.expire_seconds;
    let total_start = Instant::now();

    info!("开始下载普通漫画: comic_id={}", comic_id);
    if expire_seconds < -1 {
        return Err(AppError::BadRequest("过期时间必须为-1或非负数".to_string()));
    }

    // 使用全局客户端获取漫画信息（带自动重试）
    let comic = match global_client.get_comic(comic_id).await {
        Ok(comic) => comic,
        Err(e) => {
            error!("获取漫画 {} 失败: {}", comic_id, e);
            return Err(e);
        }
    };

    // 检查是否为普通漫画
    if !comic.series.is_empty() {
        return Err(AppError::BadRequest(
            "该漫画为章节漫画，请使用 /api/comic/downloadChapter 接口并指定章节ID".to_string()
        ));
    }

    // 普通漫画使用漫画ID作为章节ID
    let chapter_id = comic_id;

    // 使用全局客户端获取章节详情和 scramble ID
    let chapter = match global_client.get_chapter(chapter_id).await {
        Ok(chapter) => chapter,
        Err(e) => {
            error!("获取章节 {} 失败: {}", chapter_id, e);
            return Err(e);
        }
    };

    let scramble_id = match global_client.get_scramble_id(chapter_id).await {
        Ok(scramble_id) => scramble_id,
        Err(e) => {
            error!("获取 scramble_id 失败: {}", e);
            return Err(e);
        }
    };

    // 创建下载目录
    let chapter_dir = match create_download_dir(comic_id, chapter_id) {
        Ok(chapter_dir) => chapter_dir,
        Err(e) => {
            error!("创建下载目录失败: {}", e);
            return Err(e);
        }
    };

    if merge {
        let pdf_filename = "merged.pdf";
        let pdf_full_path = chapter_dir.join(pdf_filename);
        if tokio::fs::metadata(&pdf_full_path).await.is_ok() {
            info!("PDF已存在，跳过下载与合并: {}", pdf_full_path.display());
            schedule_delete_dir(chapter_dir, expire_seconds);
            let response_data = ComicDownloadData {
                comic_id,
                comic_title: comic.name,
                images: None,
                pdf_path: Some(format!("download/{}/{}/{}", comic_id, chapter_id, pdf_filename)),
            };
            info!("downloadComic完成，总耗时: {}ms", total_start.elapsed().as_millis());
            return Ok(R::success(response_data));
        }
    }

    // 创建用于下载图片的HTTP客户端，带重试机制
    let reqwest_client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return Err(AppError::Internal(format!("创建HTTP客户端失败: {}", e)));
        }
    };

    // 配置指数退避重试策略：最多重试3次
    let retry_policy = ExponentialBackoff::builder()
        .build_with_max_retries(3);

    let http_client = ClientBuilder::new(reqwest_client)
        .with(RetryTransientMiddleware::new_with_policy_and_strategy(
            retry_policy,
            CustomRetryStrategy,
        ))
        .build();

    info!("已配置图片下载重试策略：最多重试3次，使用指数退避");

    let img_concurrency = config.img_concurrency;
    let image_domain = global_client.image_domain().to_string();

    info!("开始并发下载 {} 张图片，并发数 {}",
        chapter.images.len(), img_concurrency);

    // 创建信号量控制并发数
    let semaphore = Arc::new(Semaphore::new(img_concurrency));

    // 创建 JoinSet 用于并发下载
    let mut join_set = JoinSet::new();

    let total_images = chapter.images.len();

    for (index, filename) in chapter.images.iter().enumerate() {
        let url = format!(
            "https://{}/media/photos/{}/{}",
            image_domain, chapter_id, filename
        );
        let block_num = calculate_block_num(scramble_id, chapter_id, filename);
        let save_filename = format!("{:04}.png", index + 1);
        let save_path = chapter_dir.join(&save_filename);
        let relative_path = format!("download/{}/{}/{}", comic_id, chapter_id, save_filename);

        // 克隆用于异步任务
        let http_client = http_client.clone();
        let filename = filename.clone();
        let semaphore = semaphore.clone();

        // 启动并发下载任务
        join_set.spawn(async move {
            // 获取信号量许可
            let _permit = semaphore.acquire().await.unwrap();

            if tokio::fs::metadata(&save_path).await.is_ok() {
                info!("图片已存在，跳过下载: {}", save_path.display());
                return Ok::<(usize, String, std::path::PathBuf), AppError>((index, relative_path, save_path));
            }

            info!("下载图片 {}/{}: {}", index + 1, total_images, url);

            // 下载图片
            let img_data = download_image(&http_client, &url).await?;

            // 处理并保存图片
            info!("处理图片: {} (block_num: {})", filename, block_num);
            process_and_save_image(img_data, block_num, &save_path).await?;

            // 返回图片路径和保存路径
            Ok::<(usize, String, std::path::PathBuf), AppError>((index, relative_path, save_path))
        });
    }

    // 等待所有下载完成并收集结果
    let download_start = Instant::now();
    let mut images = Vec::new();
    let mut image_files = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok((index, file_path, save_path))) => {
                images.push((index, file_path));
                image_files.push((index, save_path));
            }
            Ok(Err(e)) => {
                error!("下载图片失败: {}", e);
                return Err(e);
            }
            Err(e) => {
                error!("任务崩溃: {}", e);
                return Err(AppError::Internal(format!("任务崩溃: {}", e)));
            }
        }
    }

    // 按索引排序以保持顺序
    images.sort_by_key(|(index, _)| *index);
    let images: Vec<String> = images.into_iter().map(|(_, path)| path).collect();

    image_files.sort_by_key(|(index, _)| *index);
    let image_files: Vec<std::path::PathBuf> = image_files.into_iter().map(|(_, path)| path).collect();

    info!("完成下载普通漫画 {} 的 {} 张图片", comic_id, images.len());
    info!("downloadComic图片下载耗时: {}ms", download_start.elapsed().as_millis());

    let pdf_path = if merge {
        let pdf_filename = "merged.pdf";
        let pdf_full_path = chapter_dir.join(pdf_filename);
        let merge_start = Instant::now();
        merge_images_to_pdf(&image_files, &pdf_full_path).await?;
        info!("downloadComic合并PDF耗时: {}ms", merge_start.elapsed().as_millis());
        let compress_start = Instant::now();
        compress_pdf_with_gs(&pdf_full_path, pdf_password).await?;
        info!("downloadComic压缩PDF耗时: {}ms", compress_start.elapsed().as_millis());
        Some(format!("download/{}/{}/{}", comic_id, chapter_id, pdf_filename))
    } else {
        None
    };

    schedule_delete_dir(chapter_dir, expire_seconds);

    let response_data = ComicDownloadData {
        comic_id,
        comic_title: comic.name,
        images: if merge { None } else { Some(images) },
        pdf_path,
    };

    info!("downloadComic完成，总耗时: {}ms", total_start.elapsed().as_millis());
    Ok(R::success(response_data))
}

fn schedule_delete_dir(path: PathBuf, expire_seconds: i64) {
    if expire_seconds < 0 {
        return;
    }

    tokio::spawn(async move {
        if expire_seconds > 0 {
            sleep(Duration::from_secs(expire_seconds as u64)).await;
        }
        let path_for_delete = path.clone();
        let result = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&path_for_delete)).await;
        match result {
            Ok(Ok(())) => info!("已删除目录: {}", path.display()),
            Ok(Err(e)) => warn!("删除目录 {} 失败: {}", path.display(), e),
            Err(e) => warn!("删除目录 {} 失败: {}", path.display(), e),
        }
    });
}
