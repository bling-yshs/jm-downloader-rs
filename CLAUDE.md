# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

- 永远使用中文编写注释

## 项目概述

这是一个基于 Rust 和 Rocket 框架的 JMComic 漫画下载器后端服务。它提供 RESTful API 来获取漫画信息、下载章节图片，并自动处理 JMComic 的图片打乱算法。

服务器默认运行在 `http://0.0.0.0:8000`

## 配置

通过环境变量提供账号密码与可选配置：

```bash
JM_USERNAME=your_username
JM_PASSWORD=your_password
# 可选配置
# JM_API_DOMAIN=www.cdnhth.cc
# JM_IMAGE_DOMAIN=cdn-msp2.jmapiproxy2.cc
# JM_IMG_CONCURRENCY=32
```

## 核心架构

### 模块结构

- **main.rs**: Rocket 应用入口，配置 CORS、路由和全局状态
- **jm_client.rs**: JMComic API 客户端，处理登录、获取漫画/章节信息、token 生成和数据解密
- **global_client.rs**: 全局客户端管理器，提供线程安全的客户端访问和自动会话管理（会话失效时自动重新登录）
- **handlers.rs**: API 路由处理器，实现漫画图片下载和类型查询接口
- **image_processor.rs**: 图片处理模块，负责下载、拼接打乱的图片块、格式转换
- **models.rs**: 数据模型定义（请求/响应结构）
- **config.rs**: 环境变量加载
- **lib.rs**: 统一响应结构和错误处理

### 关键设计模式

1. **全局客户端管理**: `GlobalJmClient` 使用 `Arc<RwLock<JmClient>>` 实现线程安全的客户端共享，自动处理会话失效和重新登录

2. **并发下载**: 使用 `tokio::sync::Semaphore` 控制图片并发下载数量，使用 `JoinSet` 管理并发任务

3. **图片打乱还原**: JMComic 对图片进行了分块打乱，`calculate_block_num()` 计算打乱块数，`stitch_img()` 还原原图

4. **加密通信**:
   - Token 生成: `MD5(timestamp + secret)`
   - 数据解密: AES-256-ECB，密钥为 `MD5(timestamp + APP_DATA_SECRET)`

5. **统一响应格式**: 所有 API 返回 `R<T>` 结构，包含 code/success/data/message/time 字段

### API 端点

- `POST /api/comic/images`: 获取漫画图片并下载（支持按章节过滤）
- `POST /api/comic/getType`: 获取漫画类型（章节漫画或普通漫画）
- `GET /api/hi`: 测试端点
- `GET /api/delay/<secs>`: 延迟测试端点

## 重要实现细节

### JMComic API 认证

- 使用 cookie-based 会话认证
- 每个请求需要 `token` 和 `tokenparam` 头
- `token` = MD5(timestamp + secret)
- `tokenparam` = "timestamp,version"

### 图片处理流程

1. 获取漫画信息和章节列表
2. 并行获取每个章节的详细信息和 `scramble_id`
3. 创建下载目录 `./download/{comic_id}/{chapter_id}/`
4. 并发下载所有图片（受 `img_concurrency` 限制）
5. 根据 `block_num` 还原打乱的图片
6. 保存为 PNG 格式（GIF 除外）

### 错误处理

- 使用 `anyhow::Error` 处理内部错误
- 使用 `AppError` 枚举定义业务错误类型
- 所有 API 响应统一返回 HTTP 200，通过 `code` 字段区分成功/失败

## 日志配置

日志配置在 `log4rs.yaml`，支持：
- 控制台输出（带颜色）
- 滚动文件日志（`logs/app.log`，10MB 轮转，保留 5 个文件）
- 可按模块调整日志级别
