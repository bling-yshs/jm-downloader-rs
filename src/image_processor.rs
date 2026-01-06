use bytes::Bytes;
use image::{ImageFormat, RgbImage};
use jm_downloader_rs::AppError;
use printpdf::{Image as PdfImage, ImageTransform, Mm, PdfDocument};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use reqwest_middleware::ClientWithMiddleware;

type Result<T> = std::result::Result<T, AppError>;

const IMG_BODY_READ_MAX_RETRIES: usize = 3;
const IMG_BODY_READ_BACKOFF_MS: u64 = 200;
const IMG_BODY_READ_MAX_BACKOFF_MS: u64 = 2_000;
const PDF_DPI: f32 = 300.0;

/// 从URL下载图片
pub async fn download_image(client: &ClientWithMiddleware, url: &str) -> Result<Bytes> {
    let mut retries = 0;
    let mut backoff = Duration::from_millis(IMG_BODY_READ_BACKOFF_MS);

    loop {
        let response = client
            .get(url)
            .header("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
            .header("referer", "https://www.jmcomic.me/")
            .send()
            .await
            .map_err(|e| AppError::Internal(format!(
                "发送请求到 {} 失败: {} (is_timeout: {}, is_connect: {}, is_body: {}, is_decode: {})",
                url,
                e,
                e.is_timeout(),
                e.is_connect(),
                e.is_body(),
                e.is_decode()
            )))?;

        // 检查HTTP状态码
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Internal(format!(
                "从 {} 下载图片失败: HTTP状态码 {} ({})",
                url,
                status.as_u16(),
                status.canonical_reason().unwrap_or("未知错误")
            )));
        }

        match response.bytes().await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                let err_msg = format!(
                    "从 {} 读取响应字节失败: {} (is_timeout: {}, is_connect: {}, is_body: {}, is_decode: {})",
                    url,
                    e,
                    e.is_timeout(),
                    e.is_connect(),
                    e.is_body(),
                    e.is_decode()
                );

                if retries >= IMG_BODY_READ_MAX_RETRIES {
                    return Err(AppError::Internal(format!(
                        "{} (已重试{}次)",
                        err_msg, retries
                    )));
                }

                retries += 1;
                warn!(
                    "{}，将在 {}ms 后重试 ({}/{})",
                    err_msg,
                    backoff.as_millis(),
                    retries,
                    IMG_BODY_READ_MAX_RETRIES
                );
                tokio::time::sleep(backoff).await;
                let next_backoff = backoff.checked_mul(2).unwrap_or(backoff);
                backoff = std::cmp::min(next_backoff, Duration::from_millis(IMG_BODY_READ_MAX_BACKOFF_MS));
            }
        }
    }
}

/// 将图片块拼接回原图
/// 这会还原JMComic应用的打乱效果
fn stitch_img(src_img: &mut RgbImage, block_num: u32) -> RgbImage {
    let (width, height) = src_img.dimensions();
    let mut stitched_img = image::ImageBuffer::new(width, height);
    let remainder_height = height % block_num;

    for i in 0..block_num {
        let mut block_height = height / block_num;
        let src_img_y_start = height - (block_height * (i + 1)) - remainder_height;
        let mut dst_img_y_start = block_height * i;

        if i == 0 {
            block_height += remainder_height;
        } else {
            dst_img_y_start += remainder_height;
        }

        for y in 0..block_height {
            let src_y = src_img_y_start + y;
            let dst_y = dst_img_y_start + y;
            for x in 0..width {
                stitched_img.put_pixel(x, dst_y, *src_img.get_pixel(x, src_y));
            }
        }
    }

    stitched_img
}

/// 处理并保存图片（可选拼接）
pub async fn process_and_save_image(
    img_data: Bytes,
    block_num: u32,
    save_path: &Path,
) -> Result<()> {
    // 检测图片格式
    let format = image::guess_format(&img_data)
        .map_err(|e| AppError::Internal(format!("检测图片格式失败: {}", e)))?;

    // GIF图片不需要拼接，直接保存
    if format == ImageFormat::Gif {
        std::fs::write(save_path, img_data)
            .map_err(|e| AppError::Internal(format!(
                "保存GIF图片到 {} 失败: {}",
                save_path.display(),
                e
            )))?;
        return Ok(());
    }

    // 在阻塞任务中处理图片（CPU密集型）
    let save_path = save_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut src_img = image::load_from_memory(&img_data)
            .map_err(|e| AppError::Internal(format!("解码图片失败: {}", e)))?
            .to_rgb8();

        // 如果 block_num > 0 则拼接图片
        let dst_img = if block_num == 0 {
            src_img
        } else {
            stitch_img(&mut src_img, block_num)
        };

        // 保存为PNG格式
        dst_img
            .save_with_format(&save_path, ImageFormat::Png)
            .map_err(|e| AppError::Internal(format!(
                "保存图片到 {} 失败: {}",
                save_path.display(),
                e
            )))?;

        Ok(())
    })
    .await
    .map_err(|e| AppError::Internal(format!("图片处理任务崩溃: {}", e)))??;

    Ok(())
}

/// 创建下载目录结构
pub fn create_download_dir(comic_id: i64, chapter_id: i64) -> Result<PathBuf> {
    let base_dir = PathBuf::from("./download");
    let comic_dir = base_dir.join(comic_id.to_string());
    let chapter_dir = comic_dir.join(chapter_id.to_string());

    std::fs::create_dir_all(&chapter_dir).map_err(|e| AppError::Internal(format!(
        "创建目录 {} 失败: {}",
        chapter_dir.display(),
        e
    )))?;

    Ok(chapter_dir)
}

/// 合并图片为PDF
pub async fn merge_images_to_pdf(image_paths: &[PathBuf], output_path: &Path) -> Result<()> {
    let image_paths = image_paths.to_vec();
    let output_path = output_path.to_path_buf();

    tokio::task::spawn_blocking(move || -> Result<()> {
        if image_paths.is_empty() {
            return Err(AppError::Internal("没有可合并的图片".to_string()));
        }

        let first_image = printpdf::image_crate::open(&image_paths[0])
            .map_err(|e| AppError::Internal(format!(
                "读取图片失败: {}: {}",
                image_paths[0].display(),
                e
            )))?;
        let (width, height) = (first_image.width(), first_image.height());

        let (doc, page1, layer1) = PdfDocument::new(
            "jm-downloader-rs",
            px_to_mm(width),
            px_to_mm(height),
            "Layer 1",
        );
        let current_layer = doc.get_page(page1).get_layer(layer1);
        PdfImage::from_dynamic_image(&first_image).add_to_layer(
            current_layer,
            ImageTransform {
                translate_x: Some(Mm(0.0)),
                translate_y: Some(Mm(0.0)),
                rotate: None,
                scale_x: Some(1.0),
                scale_y: Some(1.0),
                dpi: Some(PDF_DPI),
            },
        );

        for path in image_paths.iter().skip(1) {
            let image = printpdf::image_crate::open(path)
                .map_err(|e| AppError::Internal(format!(
                    "读取图片失败: {}: {}",
                    path.display(),
                    e
                )))?;
            let (width, height) = (image.width(), image.height());
            let (page, layer) = doc.add_page(px_to_mm(width), px_to_mm(height), "Layer 1");
            let layer_ref = doc.get_page(page).get_layer(layer);
            PdfImage::from_dynamic_image(&image).add_to_layer(
                layer_ref,
                ImageTransform {
                    translate_x: Some(Mm(0.0)),
                    translate_y: Some(Mm(0.0)),
                    rotate: None,
                    scale_x: Some(1.0),
                    scale_y: Some(1.0),
                    dpi: Some(PDF_DPI),
                },
            );
        }

        let mut writer = BufWriter::new(File::create(&output_path).map_err(|e| {
            AppError::Internal(format!("创建PDF文件失败: {}: {}", output_path.display(), e))
        })?);
        doc.save(&mut writer)
            .map_err(|e| AppError::Internal(format!("写入PDF失败: {}", e)))?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::Internal(format!("合并PDF任务崩溃: {}", e)))??;

    Ok(())
}

fn px_to_mm(px: u32) -> Mm {
    Mm(px as f32 * (25.4 / PDF_DPI))
}

/// 使用GhostScript压缩PDF并可选加密
pub async fn compress_pdf_with_gs(pdf_path: &Path, password: Option<&str>) -> Result<()> {
    let pdf_path = pdf_path.to_path_buf();
    let password = password.map(|value| value.to_string());

    tokio::task::spawn_blocking(move || -> Result<()> {
        let file_name = pdf_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("merged.pdf");
        let tmp_path = pdf_path.with_file_name(format!("{}.tmp", file_name));

        info!("开始压缩PDF: {}", pdf_path.display());
        let mut cmd = Command::new("gs");
        cmd.arg("-q")
            .arg("-dNOPAUSE")
            .arg("-dBATCH")
            .arg("-sDEVICE=pdfwrite");
        if let Some(pwd) = password.as_deref() {
            cmd.arg(format!("-sUserPassword={}", pwd))
                .arg(format!("-sOwnerPassword={}", pwd));
        }
        cmd.arg("-dPDFSETTINGS=/printer")
            .arg("-dSAFER")
            .arg("-o")
            .arg(&tmp_path)
            .arg(&pdf_path);

        let output = cmd
            .output()
            .map_err(|e| AppError::Internal(format!("执行GhostScript失败: {}", e)))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppError::Internal(format!(
                "GhostScript处理失败: {}",
                stderr.trim()
            )));
        }

        std::fs::rename(&tmp_path, &pdf_path).map_err(|e| {
            AppError::Internal(format!("替换PDF文件失败: {}: {}", pdf_path.display(), e))
        })?;
        info!("PDF压缩完成: {}", pdf_path.display());
        Ok(())
    })
    .await
    .map_err(|e| AppError::Internal(format!("PDF压缩任务崩溃: {}", e)))??;

    Ok(())
}
