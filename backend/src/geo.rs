use crate::types::*;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json};
use memmap2::{Advice, MmapOptions};
use prost::bytes::Buf;
use prost::encoding::{DecodeContext, WireType, decode_key, decode_varint, skip_field};
use regex_lite::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};
use tokio::task;

#[derive(Serialize, Clone)]
pub struct GeoFilesResponse {
    site_files: Vec<String>,
    ip_files: Vec<String>,
}

#[derive(Serialize)]
struct GeoQueryResponse {
    categories: Vec<String>,
}

fn read_len_delim<'a>(buf: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = decode_varint(buf).ok()? as usize;
    if buf.remaining() < len {
        return None;
    }
    let slice = &buf[..len];
    buf.advance(len);
    Some(slice)
}

fn find_categories<F: FnMut(&[u8]) -> bool>(data: &[u8], mut matches: F) -> Vec<String> {
    let mut found = Vec::new();
    let mut buf = data;
    while buf.has_remaining() {
        let Ok((tag, wt)) = decode_key(&mut buf) else {
            break;
        };
        let Some(entry) = (tag == 1 && wt == WireType::LengthDelimited)
            .then(|| read_len_delim(&mut buf))
            .flatten()
        else {
            let _ = skip_field(wt, tag, &mut buf, DecodeContext::default());
            continue;
        };
        let mut buf2 = entry;
        let mut country_code = "";
        while buf2.has_remaining() {
            let Ok((tag, wt)) = decode_key(&mut buf2) else {
                break;
            };
            match (tag, wt) {
                (1, WireType::LengthDelimited) => {
                    let Some(s) = read_len_delim(&mut buf2) else {
                        break;
                    };
                    country_code = std::str::from_utf8(s).unwrap_or("");
                }
                (2, WireType::LengthDelimited) => {
                    let Some(payload) = read_len_delim(&mut buf2) else {
                        break;
                    };
                    if matches(payload) {
                        found.push(country_code.to_string());
                        break;
                    }
                }
                _ => {
                    let _ = skip_field(wt, tag, &mut buf2, DecodeContext::default());
                }
            }
        }
    }
    found
}

pub(crate) fn parse_cidr_and_match(mut buf: &[u8], target_v4: Option<u32>, target_v6: Option<u128>) -> bool {
    let (mut cidr_ip, mut prefix) = (&[][..], 0u32);
    while buf.has_remaining() {
        let Ok((t, wt)) = decode_key(&mut buf) else {
            break;
        };
        match (t, wt) {
            (1, WireType::LengthDelimited) => {
                let Some(s) = read_len_delim(&mut buf) else {
                    break;
                };
                cidr_ip = s;
            }
            (2, WireType::Varint) => {
                let Ok(v) = decode_varint(&mut buf) else {
                    break;
                };
                prefix = v as u32;
            }
            _ => {
                let _ = skip_field(wt, t, &mut buf, DecodeContext::default());
            }
        }
    }
    match (cidr_ip.len(), target_v4, target_v6) {
        (4, Some(t), _) => {
            prefix <= 32 && {
                let n = u32::from_be_bytes(cidr_ip.try_into().unwrap());
                prefix == 0 || (t >> (32 - prefix)) == (n >> (32 - prefix))
            }
        }
        (16, _, Some(t)) => {
            prefix <= 128 && {
                let n = u128::from_be_bytes(cidr_ip.try_into().unwrap());
                prefix == 0 || (t >> (128 - prefix)) == (n >> (128 - prefix))
            }
        }
        _ => false,
    }
}

static REGEX_CACHE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Option<Regex>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::with_capacity(64)));

fn match_cached_regex(pattern: &str, text: &str) -> bool {
    let mut cache = REGEX_CACHE.lock().unwrap();
    if let Some(opt_re) = cache.get(pattern) {
        return opt_re.as_ref().is_some_and(|re| re.is_match(text));
    }
    if cache.len() >= 128 {
        cache.clear();
    }
    let compiled = Regex::new(pattern).ok();
    let matches = compiled.as_ref().is_some_and(|re| re.is_match(text));
    cache.insert(pattern.to_string(), compiled);
    matches
}

pub(crate) fn parse_domain_and_match(mut buf: &[u8], dom_low: &str) -> bool {
    let (mut domain_type, mut value) = (0i32, "");
    while buf.has_remaining() {
        let Ok((t, wt)) = decode_key(&mut buf) else {
            break;
        };
        match (t, wt) {
            (1, WireType::Varint) => {
                let Ok(v) = decode_varint(&mut buf) else {
                    break;
                };
                domain_type = v as i32;
            }
            (2, WireType::LengthDelimited) => {
                let Some(s) = read_len_delim(&mut buf) else {
                    break;
                };
                value = std::str::from_utf8(s).unwrap_or("");
            }
            _ => {
                let _ = skip_field(wt, t, &mut buf, DecodeContext::default());
            }
        }
    }
    match domain_type {
        0 => dom_low.contains(value),
        1 => match_cached_regex(value, dom_low),
        2 => {
            dom_low == value
                || (dom_low.len() > value.len()
                    && dom_low.ends_with(value)
                    && dom_low.as_bytes()[dom_low.len() - value.len() - 1] == b'.')
        }
        3 => dom_low == value,
        _ => false,
    }
}

pub(crate) fn detect_geo_file_type(data: &[u8]) -> (bool, bool) {
    let mut buf = data;
    let Ok((1, WireType::LengthDelimited)) = decode_key(&mut buf) else {
        return (false, false);
    };
    let Some(mut entry) = read_len_delim(&mut buf) else {
        return (false, false);
    };
    while entry.has_remaining() {
        let Ok((tag, wt)) = decode_key(&mut entry) else {
            break;
        };
        if tag == 2 && wt == WireType::LengthDelimited {
            let Some(mut sub) = read_len_delim(&mut entry) else {
                break;
            };
            while sub.has_remaining() {
                let Ok((sub_tag, sub_wt)) = decode_key(&mut sub) else {
                    break;
                };
                match (sub_tag, sub_wt) {
                    (1, WireType::Varint) => return (true, false),
                    (1, WireType::LengthDelimited) => return (false, true),
                    (2, WireType::Varint) => return (false, true),
                    (2, WireType::LengthDelimited) => return (true, false),
                    _ => {
                        let _ = skip_field(sub_wt, sub_tag, &mut sub, DecodeContext::default());
                    }
                }
            }
            break;
        }
        let _ = skip_field(wt, tag, &mut entry, DecodeContext::default());
    }
    (false, false)
}

pub fn list_geo_files(cache_arc: Arc<RwLock<crate::types::GeoCache>>) -> Result<GeoFilesResponse, String> {
    list_geo_files_in(Path::new(XRAY_ASSET_DIR), cache_arc)
}

fn list_geo_files_in(base: &Path, cache_arc: Arc<RwLock<crate::types::GeoCache>>) -> Result<GeoFilesResponse, String> {
    let mut site_files = Vec::new();
    let mut ip_files = Vec::new();
    let mut new_cache_entries = Vec::new();

    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GeoFilesResponse { site_files, ip_files });
        }
        Err(error) => return Err(error.to_string()),
    };
    let canonical_base = std::fs::canonicalize(base).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        // XKeen uses aliases such as zkeenip.dat -> geoip_zkeenip.dat.
        // Only regular files and symlinks resolving inside the asset directory qualify.
        let Ok(resolved) = std::fs::canonicalize(&path) else { continue };
        if !resolved.starts_with(&canonical_base) || !resolved.is_file() {
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "dat") {
            continue;
        }

        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let mtime = std::fs::metadata(&resolved)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let cached = cache_arc.read().unwrap().get(&name).cloned();
        let (is_site, is_ip) = match cached {
            Some((cached_time, s, i)) if cached_time == mtime => (s, i),
            _ => {
                let result = File::open(&path)
                    .ok()
                    .and_then(|f| unsafe { MmapOptions::new().map(&f) }.ok())
                    .map(|mmap| detect_geo_file_type(&mmap))
                    .unwrap_or((false, false));
                new_cache_entries.push((name.clone(), mtime, result.0, result.1));
                result
            }
        };
        if is_site {
            site_files.push(name.clone());
        }
        if is_ip {
            ip_files.push(name);
        }
    }

    if !new_cache_entries.is_empty() {
        let mut cache = cache_arc.write().unwrap();
        for (name, mtime, s, i) in new_cache_entries {
            cache.insert(name, (mtime, s, i));
        }
    }

    site_files.sort();
    ip_files.sort();
    Ok(GeoFilesResponse { site_files, ip_files })
}

pub async fn get_geo(State(state): State<AppState>) -> impl IntoResponse {
    let cache = state.geo_cache.clone();
    match task::spawn_blocking(move || list_geo_files(cache)).await {
        Ok(Ok(data)) => Json(ApiResponse {
            success: true,
            error: None,
            data: Some(data),
        }),
        Ok(Err(e)) => Json(ApiResponse::<GeoFilesResponse> {
            success: false,
            error: Some(e),
            data: None,
        }),
        Err(e) => Json(ApiResponse::<GeoFilesResponse> {
            success: false,
            error: Some(format!("Task panic: {}", e)),
            data: None,
        }),
    }
}

pub async fn get_geoip(
    State(state): State<AppState>, Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    geo_query_handler(params, true, Some(state.http_client)).await
}

pub async fn get_geosite(
    State(_): State<AppState>, Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    geo_query_handler(params, false, None).await
}

pub(crate) async fn resolve_domain(client: &reqwest::Client, domain: &str) -> Result<IpAddr, String> {
    #[derive(serde::Deserialize)]
    struct DohResponse {
        #[serde(rename = "Answer")]
        answer: Option<Vec<DohAnswer>>,
    }
    #[derive(serde::Deserialize)]
    struct DohAnswer {
        data: String,
    }

    async fn fetch_doh(client: &reqwest::Client, provider: &str, qtype: &str, domain: &str) -> Result<IpAddr, ()> {
        let resp = client
            .get(provider)
            .header("accept", "application/dns-json")
            .query(&[("name", domain), ("type", qtype)])
            .timeout(Duration::from_millis(2500))
            .send()
            .await
            .map_err(|_| ())?;
        let parsed: DohResponse = resp.json().await.map_err(|_| ())?;
        if let Some(answers) = parsed.answer {
            for answer in answers {
                if let Ok(ip) = answer.data.parse::<IpAddr>() {
                    return Ok(ip);
                }
            }
        }
        Err(())
    }

    // Query both providers concurrently for IPv4
    let (cf_a, gg_a) = tokio::join!(
        fetch_doh(client, "https://1.1.1.1/dns-query", "A", domain),
        fetch_doh(client, "https://8.8.8.8/dns-query", "A", domain),
    );
    if let Ok(ip) = cf_a.or(gg_a) {
        return Ok(ip);
    }

    // Fallback: Query both providers concurrently for IPv6
    let (cf_aaaa, gg_aaaa) = tokio::join!(
        fetch_doh(client, "https://1.1.1.1/dns-query", "AAAA", domain),
        fetch_doh(client, "https://8.8.8.8/dns-query", "AAAA", domain),
    );
    if let Ok(ip) = cf_aaaa.or(gg_aaaa) {
        return Ok(ip);
    }

    Err(format!("Не удалось разрешить домен: {}", domain))
}

async fn geo_query_handler(
    params: HashMap<String, String>, is_ip: bool, client: Option<reqwest::Client>,
) -> impl IntoResponse {
    let key = if is_ip { "ip" } else { "domain" };
    let (Some(filename), Some(target)) = (params.get("file"), params.get(key)) else {
        return Json(ApiResponse::<GeoQueryResponse> {
            success: false,
            error: Some(format!("Отсутствует file или {key}")),
            data: None,
        });
    };
    let base = Path::new(XRAY_ASSET_DIR);
    let Some(path) = Path::new(filename)
        .file_name()
        .map(|n| base.join(n))
        .filter(|p| p.starts_with(base))
    else {
        return Json(ApiResponse::<GeoQueryResponse> {
            success: false,
            error: Some("Некорректное имя файла".into()),
            data: None,
        });
    };
    let target = target.clone();

    let resolved_ip: Option<IpAddr> = if is_ip {
        match target.parse::<IpAddr>() {
            Ok(ip) => Some(ip),
            Err(_) => match client.as_ref() {
                Some(c) => match resolve_domain(c, &target).await {
                    Ok(ip) => Some(ip),
                    Err(e) => {
                        return Json(ApiResponse::<GeoQueryResponse> {
                            success: false,
                            error: Some(e),
                            data: None,
                        });
                    }
                },
                None => {
                    return Json(ApiResponse::<GeoQueryResponse> {
                        success: false,
                        error: Some(format!("Некорректный IP: {}", target)),
                        data: None,
                    });
                }
            },
        }
    } else {
        None
    };

    let result = task::spawn_blocking(move || -> Result<Vec<String>, String> {
        let file = File::open(&path).map_err(|e| format!("Ошибка открытия: {}", e))?;
        let mmap = unsafe {
            MmapOptions::new()
                .map(&file)
                .map_err(|e| format!("Ошибка mmap: {}", e))?
        };
        mmap.advise(Advice::Sequential).ok();
        if is_ip {
            let (v4, v6) = match resolved_ip.unwrap() {
                IpAddr::V4(v4) => (Some(u32::from(v4)), None),
                IpAddr::V6(v6) => (None, Some(u128::from(v6))),
            };
            Ok(find_categories(&mmap, |buf| parse_cidr_and_match(buf, v4, v6)))
        } else {
            let dom_low = target.to_lowercase();
            Ok(find_categories(&mmap, |buf| parse_domain_and_match(buf, &dom_low)))
        }
    })
    .await;

    match result {
        Ok(Ok(categories)) => Json(ApiResponse {
            success: true,
            error: None,
            data: Some(GeoQueryResponse { categories }),
        }),
        Ok(Err(e)) => Json(ApiResponse::<GeoQueryResponse> {
            success: false,
            error: Some(e),
            data: None,
        }),
        Err(e) => Json(ApiResponse::<GeoQueryResponse> {
            success: false,
            error: Some(format!("Ошибка: {}", e)),
            data: None,
        }),
    }
}

#[cfg(test)]
mod file_listing_tests {
    use super::*;
    #[test]
    fn lists_internal_geodata_aliases_but_not_external_or_broken_links() {
        let base = std::env::temp_dir().join(format!("xkeen-geo-list-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&base).unwrap();
        let cache = Arc::new(RwLock::new(GeoCache::new()));
        assert!(list_geo_files_in(&base.join("missing"), cache.clone()).unwrap().site_files.is_empty());
        let ip = [10, 14, 10, 2, b'R', b'U', 18, 8, 10, 4, 1, 2, 3, 0, 16, 24];
        std::fs::write(base.join("geoip_zkeenip.dat"), ip).unwrap();
        std::os::unix::fs::symlink("geoip_zkeenip.dat", base.join("zkeenip.dat")).unwrap();
        std::os::unix::fs::symlink("missing.dat", base.join("broken.dat")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", base.join("outside.dat")).unwrap();
        let result = list_geo_files_in(&base, cache).unwrap();
        assert_eq!(result.ip_files, vec!["geoip_zkeenip.dat", "zkeenip.dat"]);
        assert!(result.site_files.is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn cached_regex_matching_works_and_reuses_cache() {
        assert!(match_cached_regex(r"^google\..*", "google.com"));
        assert!(match_cached_regex(r"^google\..*", "google.org"));
        assert!(!match_cached_regex(r"^google\..*", "yahoo.com"));

        // Invalid regex should safely return false without panicking
        assert!(!match_cached_regex(r"[unclosed-regex", "test"));

        // Test protobuf encoding with domain_type = 1 (regex)
        // Tag 1 (varint, type 1): 0x08, 0x01
        // Tag 2 (len-delimited, ".*\\.org"): 0x12, 0x06, '.', '*', '\\', '.', 'o', 'r', 'g'
        let mut proto = vec![0x08, 0x01, 0x12, 0x07];
        proto.extend_from_slice(b".*\\.org");
        assert!(parse_domain_and_match(&proto, "wikipedia.org"));
        assert!(!parse_domain_and_match(&proto, "wikipedia.com"));
    }
}
