use actix_web::{
    App, HttpRequest, HttpResponse, HttpServer, Responder, Result, get,
    http::header::{ContentRange, ContentRangeSpec, RANGE},
    web::{self, resource},
};
use base64::{Engine, engine};
use mime_guess::{from_path, mime};
use std::process::Command;
use std::{
    fs,
    io::{Cursor, Read, Seek},
    path::Path,
};
const BASE64_ENGINE: engine::GeneralPurpose = engine::general_purpose::STANDARD;

#[get("/")]
async fn hello() -> impl Responder {
    HttpResponse::Ok().body("Hello world!")
}

#[get("/ip")]
async fn ip(request: HttpRequest) -> impl Responder {
    HttpResponse::Ok().body(format!("Find you at {:?}", request.peer_addr()))
}

async fn manual_hello() -> impl Responder {
    HttpResponse::Ok().body("Hey there!")
}

async fn file_handler(req: HttpRequest, path: web::Path<String>) -> Result<HttpResponse> {
    println!("file {:?}", path);
    let path_str = path.into_inner();
    let root_dir = "."; // 以当前目录为根目录
    let full_path = Path::new(root_dir).join(&path_str);

    // 检查路径是否存在
    if !full_path.exists() {
        return Ok(HttpResponse::NotFound().body("File or directory not found"));
    }

    // 如果是文件，返回文件内容
    if full_path.is_file() {
        handle_file(&req, &full_path).await
    } else if full_path.is_dir() {
        handle_directory(&req, &full_path, &path_str).await
    } else {
        // 其他情况（如特殊文件）
        Ok(HttpResponse::InternalServerError().body("Unknown file type"))
    }
}

async fn handle_file(req: &HttpRequest, full_path: &Path) -> Result<HttpResponse> {
    println!("file {:?}", full_path);
    let mime = from_path(full_path).first_or_octet_stream();

    // 检查是否有 Range 请求头
    if let Some(range_header) = req.headers().get(RANGE) {
        if let Ok(range) = range_header.to_str() {
            if let Some((start, end)) = parse_range(range, full_path.metadata()?.len()) {
                let mut file = fs::File::open(full_path)?;
                file.seek(std::io::SeekFrom::Start(start)).unwrap();
                let mut content = vec![0; (end - start + 1) as _];
                file.read_exact(&mut content)?;

                let content_range = ContentRange(ContentRangeSpec::Bytes {
                    range: Some((start, end)),
                    instance_length: Some(full_path.metadata()?.len()),
                });

                let response = HttpResponse::PartialContent()
                    .content_type(mime.to_string())
                    .insert_header(content_range)
                    .body(content);

                return Ok(response);
            }
        }
    }

    // 如果没有 Range 请求，返回整个文件
    let mut file = fs::File::open(full_path)?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)?;

    Ok(HttpResponse::Ok()
        .content_type(mime.to_string())
        .body(content))
}

async fn handle_directory(
    req: &HttpRequest,
    full_path: &Path,
    path_str: &str,
) -> Result<HttpResponse> {
    println!(
        "{:?} asking for dir {} ({})",
        req.peer_addr(),
        full_path.display(),
        path_str
    );
    let mut entries = fs::read_dir(full_path)?
        .filter_map(|entry| entry.ok())
        .collect::<Vec<_>>();

    // 按名称排序
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    let parent_path = if path_str != "" {
        Path::new(path_str)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
    } else {
        None
    };

    // 生成 HTML 响应
    let html = {
        let mut html = String::new();
        html.push_str("<!DOCTYPE html><html><head><title>Directory Listing</title></head><body>");
        html.push_str(&format!("<h1>Directory: {}</h1>", path_str));
        html.push_str("<ul>");

        if let Some(parent) = parent_path {
            html.push_str(&format!(
                "<li><a href=\"/file/{}\">..</a></li><li></li>",
                parent
            ));
        }

        for entry in &entries {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = format!(
                "/file{}{}{}{}",
                if path_str.starts_with('/') { "" } else { "/" },
                path_str,
                if path_str.ends_with('/') || path_str.is_empty() {
                    ""
                } else {
                    "/"
                },
                name
            );
            let full_file_path = full_path.join(&name);
            let mime = from_path(&full_file_path).first_or_octet_stream();

            // 检查是否是图片或视频
            if let Some(thumbnail_data) = generate_thumbnail(&full_file_path, &mime) {
                html.push_str(&format!(
                        "<li><a href=\"{}\">{}<img src=\"data:image/png;base64,{}\" height=\"200\" /></a></li>",
                        path, name,thumbnail_data,
                    ));
            } else {
                html.push_str(&format!("<li><a href=\"{}\">{}</a></li>", path, name));
            }
        }

        html.push_str("</ul></body></html>");
        html
    };

    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

fn parse_range(range_str: &str, file_size: u64) -> Option<(u64, u64)> {
    if !range_str.starts_with("bytes=") {
        return None;
    }

    let range_part = &range_str[6..];
    let parts: Vec<&str> = range_part.split('-').collect();

    if parts.len() != 2 {
        return None;
    }

    let start = parts[0].parse::<u64>().ok()?;
    let end = if parts[1].is_empty() {
        file_size - 1
    } else {
        parts[1].parse::<u64>().ok()?
    };

    if start > end || end >= file_size {
        return None;
    }

    Some((start, end))
}

fn generate_thumbnail(path: &Path, mime: &mime_guess::Mime) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let cache = Path::new("./.cache/").join(path);
    if cache.exists() {
        return Some(fs::read_to_string(cache).unwrap());
    } else {
        let res = match mime.type_() {
            mime::IMAGE => generate_image_thumbnail(path)
                .map(|x| BASE64_ENGINE.encode(x))
                .ok(),
            mime::VIDEO => generate_video_thumbnail(path)
                .map(|x| BASE64_ENGINE.encode(x))
                .ok(),
            _ => None,
        };
        if let Some(ref res) = res {
            fs::create_dir_all(cache.parent().unwrap()).unwrap();
            fs::write(cache, res).unwrap();
        }
        res
    }
}

fn generate_image_thumbnail(path: &Path) -> Result<Vec<u8>, String> {
    println!("generate_image_thumbnail for {:?}", path);
    let img = image::open(path).map_err(|e| e.to_string())?;
    let thumbnail = img.thumbnail(200, 1080);
    let mut buffer = Cursor::new(Vec::new());
    thumbnail
        .write_to(&mut buffer, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(buffer.into_inner())
}

fn generate_video_thumbnail(path: &Path) -> Result<Vec<u8>, String> {
    println!("generate_video_thumbnail for {:?}", path);
    let duration = get_video_duration(path)?;

    // 计算中间时间点
    let middle_time = format!("{}s", (duration / 2.0).min(15.0));

    let output = Command::new("ffmpeg")
        .args(&[
            "-i",
            path.to_str().ok_or("Invalid path")?,
            "-ss",
            &middle_time,
            "-vframes",
            "1",
            "-f",
            "image2",
            "-c:v",
            "png",
            "-",
        ])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        let img = image::load_from_memory(&output.stdout).unwrap();
        let thumbnail = img.thumbnail(200, 1080);
        let mut buffer = Cursor::new(Vec::new());
        thumbnail
            .write_to(&mut buffer, image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok(buffer.into_inner())
    } else {
        Err("Failed to generate video thumbnail".to_string())
    }
}

fn get_video_duration(path: &Path) -> Result<f64, String> {
    let output = Command::new("ffprobe")
        .args(&[
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            path.to_str().ok_or("Invalid path")?,
        ])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        let duration_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let duration = duration_str
            .parse::<f64>()
            .map_err(|_| "Failed to parse video duration".to_string())?;
        Ok(duration)
    } else {
        Err("Failed to get video duration".to_string())
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(|| {
        App::new()
            .service(hello)
            .service(ip)
            .service(resource("/file/{path:.*}").to(file_handler))
            .route("/hey", web::get().to(manual_hello))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
