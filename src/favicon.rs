//! 去网站扒 favicon，统一压成 64×64 的 PNG

use image::ImageFormat;
use std::io::Cursor;
use std::time::Duration;

/// 下下来转 PNG 搞不定就 None，调用方自己兜底
pub fn download_site_icon_png(web_url: &str) -> Option<Vec<u8>> {
    let parsed = url::Url::parse(web_url.trim()).ok()?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = parsed.host_str()?;

    let agent = ureq::Agent::new();

    let fav_url = parsed.join("/favicon.ico").ok()?;
    let raw =
        try_fetch_bytes(&agent, fav_url.as_str()).or_else(|| try_google_fallback(&agent, host))?;

    normalize_image_bytes_to_png(&raw)
}

fn try_fetch_bytes(agent: &ureq::Agent, url: &str) -> Option<Vec<u8>> {
    let resp = agent.get(url).timeout(Duration::from_secs(8)).call().ok()?;
    let status = resp.status();
    if !(200..400).contains(&status) {
        return None;
    }
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf).ok()?;
    if buf.len() < 8 {
        return None;
    }
    Some(buf)
}

fn try_google_fallback(agent: &ureq::Agent, domain: &str) -> Option<Vec<u8>> {
    let u = format!(
        "https://www.google.com/s2/favicons?sz=64&domain={}",
        domain
    );
    try_fetch_bytes(agent, &u)
}

fn normalize_image_bytes_to_png(raw: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(raw).ok()?;
    let resized = img.resize_exact(64, 64, image::imageops::FilterType::Triangle);
    let mut cursor = Cursor::new(Vec::new());
    resized.write_to(&mut cursor, ImageFormat::Png).ok()?;
    Some(cursor.into_inner())
}
