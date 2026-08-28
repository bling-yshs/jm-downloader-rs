use crate::global_client::GlobalJmClient;
use crate::models::{
    ComicInfo, DownloadChapterRequest, DownloadComicRequest, DownloadTaskInfo, GetComicInfoRequest,
    TaskIdRequest,
};
use crate::task_manager::TaskManager;
use jm_downloader_rs::{ApiResult, AppError, R};
use rocket::serde::json::Json;
use rocket::State;
use rocket_okapi::openapi;

/// 根据漫画 ID 获取标题、类型、作者、简介和普通漫画页数。
///
/// # 参数
/// - `global_client`: 全局 JMComic 客户端。
/// - `request`: 漫画信息请求。
///
/// # 返回
/// 查询成功时返回漫画信息，查询失败时返回应用错误。
#[openapi]
#[post("/api/comic/getInfo", data = "<request>")]
pub async fn get_comic_info(
    global_client: &State<GlobalJmClient>,
    request: Json<GetComicInfoRequest>,
) -> ApiResult<R<ComicInfo>> {
    let comic = global_client.get_comic(request.id).await?;
    let comic_type = if comic.series.is_empty() {
        "普通漫画".to_string()
    } else {
        "章节漫画".to_string()
    };
    let total_pages = if comic.series.is_empty() {
        Some(global_client.get_chapter(request.id).await?.images.len())
    } else {
        None
    };
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

/// 提交章节漫画后台下载任务。
///
/// # 参数
/// - `task_manager`: 下载任务管理器。
/// - `request`: 章节漫画下载请求。
///
/// # 返回
/// 提交成功时返回任务 ID，参数错误或队列已满时返回应用错误。
#[openapi]
#[post("/api/comic/downloadChapter", data = "<request>")]
pub async fn download_chapter(
    task_manager: &State<TaskManager>,
    request: Json<DownloadChapterRequest>,
) -> ApiResult<R<String>> {
    if request.chapter_ids.is_empty() {
        return Err(AppError::BadRequest("章节 ID 列表不能为空".to_string()));
    }
    validate_expire_seconds(request.expire_seconds)?;
    let task_id = task_manager.submit_chapter(request.into_inner()).await?;
    Ok(R::success(task_id))
}

/// 提交普通漫画后台下载任务。
///
/// # 参数
/// - `task_manager`: 下载任务管理器。
/// - `request`: 普通漫画下载请求。
///
/// # 返回
/// 提交成功时返回任务 ID，参数错误或队列已满时返回应用错误。
#[openapi]
#[post("/api/comic/downloadComic", data = "<request>")]
pub async fn download_comic(
    task_manager: &State<TaskManager>,
    request: Json<DownloadComicRequest>,
) -> ApiResult<R<String>> {
    validate_expire_seconds(request.expire_seconds)?;
    let task_id = task_manager.submit_comic(request.into_inner()).await?;
    Ok(R::success(task_id))
}

/// 按任务 ID 查询下载任务信息。
///
/// # 参数
/// - `task_manager`: 下载任务管理器。
/// - `request`: 任务 ID 请求。
///
/// # 返回
/// 任务存在时返回任务信息，否则返回 NotFound。
#[openapi]
#[post("/api/task/findTaskInfoById", data = "<request>")]
pub async fn find_task_info_by_id(
    task_manager: &State<TaskManager>,
    request: Json<TaskIdRequest>,
) -> ApiResult<R<DownloadTaskInfo>> {
    let task_id = request.task_id.trim();
    if task_id.is_empty() {
        return Err(AppError::BadRequest("任务 ID 不能为空".to_string()));
    }
    Ok(R::success(task_manager.find_task(task_id).await?))
}

/// 按任务 ID 取消下载任务。
///
/// # 参数
/// - `task_manager`: 下载任务管理器。
/// - `request`: 任务 ID 请求。
///
/// # 返回
/// 取消请求受理后返回任务 ID，否则返回应用错误。
#[openapi]
#[post("/api/task/cancelTaskById", data = "<request>")]
pub async fn cancel_task_by_id(
    task_manager: &State<TaskManager>,
    request: Json<TaskIdRequest>,
) -> ApiResult<R<String>> {
    let task_id = request.task_id.trim();
    if task_id.is_empty() {
        return Err(AppError::BadRequest("任务 ID 不能为空".to_string()));
    }
    Ok(R::success(task_manager.cancel_task(task_id).await?))
}

/// 校验异步任务的结果保留秒数。
///
/// # 参数
/// - `expire_seconds`: 成功后保留秒数。
///
/// # 返回
/// 参数为正数或 `-1` 时返回空，否则返回 BadRequest。
fn validate_expire_seconds(expire_seconds: i64) -> Result<(), AppError> {
    if expire_seconds == -1 || expire_seconds > 0 {
        return Ok(());
    }
    Err(AppError::BadRequest("过期时间必须为 -1 或正数".to_string()))
}
