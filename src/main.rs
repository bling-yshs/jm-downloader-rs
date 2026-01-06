#[macro_use]
extern crate rocket;

mod config;
mod models;
mod jm_client;
mod handlers;
mod image_processor;
mod global_client;

use rocket::http::Method;
use rocket::fs::FileServer;
use rocket_cors::{AllowedHeaders, AllowedOrigins, CorsOptions};
use rocket_okapi::{openapi, openapi_get_routes};
use rocket_okapi::swagger_ui::{make_swagger_ui, SwaggerUIConfig};
use jm_downloader_rs::{ApiResult, R};
use global_client::GlobalJmClient;

/// # 健康检查
/// 返回服务运行状态。
#[openapi]
#[get("/api/health")]
async fn health() -> ApiResult<R<String>> {
    Ok(R::success("ok".to_string()))
}

#[launch]
async fn rocket() -> _ {
    log4rs::init_file("log4rs.yaml", Default::default()).expect("init log4rs");

    // 加载配置
    let config = config::load_config().expect("Failed to load config");

    // 创建全局 JmClient 实例并登录
    let global_client = GlobalJmClient::new(&config)
        .await
        .expect("Failed to initialize global JmClient");

    info!("全局 JmClient 已创建并完成初始登录");
    std::fs::create_dir_all("download").expect("创建下载目录失败");

    let cors = CorsOptions::default()
        .allowed_origins(AllowedOrigins::all())
        .allowed_headers(AllowedHeaders::all())
        .allowed_methods(
            vec![Method::Get, Method::Post, Method::Options]
                .into_iter()
                .map(From::from)
                .collect(),
        )
        .allow_credentials(true);
    info!("健康检查地址 http://127.0.0.1:8000/api/health");
    info!("在线调试 http://127.0.0.1:8000/docs");
    rocket::build()
        .attach(cors.to_cors().unwrap())
        .manage(config)
        .manage(global_client)
        .mount(
            "/",
            openapi_get_routes![
                health,
                handlers::download_chapter,
                handlers::download_comic,
                handlers::get_comic_info
            ],
        )
        .mount("/download", FileServer::from("download"))
        .mount(
            "/docs",
            make_swagger_ui(&SwaggerUIConfig {
                url: "/openapi.json".to_string(),
                ..Default::default()
            }),
        )
}
