use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_expire_seconds() -> i64 {
    600
}

// 获取漫画信息请求
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetComicInfoRequest {
    pub id: i64,
}

// 获取漫画信息响应
#[derive(Debug, Serialize, JsonSchema)]
pub struct ComicInfo {
    pub comic_id: i64,
    pub title: String,
    pub comic_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_views: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likes: Option<String>,
    pub authors: Vec<String>,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_pages: Option<usize>,
}

// 下载章节漫画请求
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DownloadChapterRequest {
    pub comic_id: i64,
    pub chapter_ids: Vec<i64>,
    /// 下载完成后多少秒自动删除目录，默认600秒，-1为不过期
    #[serde(default = "default_expire_seconds")]
    pub expire_seconds: i64,
}

// 下载普通漫画请求
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DownloadComicRequest {
    pub comic_id: i64,
    /// 是否合并为PDF，默认false
    #[serde(default)]
    pub merge: bool,
    /// 合并PDF密码，传入则启用加密
    #[serde(default)]
    pub encrypt: Option<String>,
    /// 下载完成后多少秒自动删除目录，默认600秒，-1为不过期
    #[serde(default = "default_expire_seconds")]
    pub expire_seconds: i64,
}

// 单个章节下载数据
#[derive(Debug, Serialize, JsonSchema)]
pub struct SingleChapterData {
    pub chapter_id: i64,
    pub chapter_title: String,
    pub images: Vec<String>,
}

// 下载章节漫画响应数据
#[derive(Debug, Serialize, JsonSchema)]
pub struct ChapterDownloadData {
    pub comic_id: i64,
    pub comic_title: String,
    pub chapters: Vec<SingleChapterData>,
}

// 下载普通漫画响应数据
#[derive(Debug, Serialize, JsonSchema)]
pub struct ComicDownloadData {
    pub comic_id: i64,
    pub comic_title: String,
    /// 图片路径列表（merge为false时返回）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    /// 合并PDF文件路径（仅在merge为true时返回）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_path: Option<String>,
}

// 内部 API 响应模型（来自 JMComic API）
#[derive(Debug, Deserialize)]
pub struct JmResp {
    pub code: i64,
    pub data: serde_json::Value,
    #[serde(default)]
    pub error_msg: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesRespData {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct GetComicRespData {
    pub name: String,
    pub series: Vec<SeriesRespData>,
    #[serde(rename = "total_views")]
    pub total_views: String,
    pub likes: String,
    pub author: Vec<String>,
    pub description: String,
}

#[derive(Debug, Deserialize)]
pub struct GetChapterRespData {
    pub images: Vec<String>,
}
