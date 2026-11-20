//! 本机 Cursor 额度。登录保存在 Cursor 自己的状态库里，刷新时解密后只用来查询两档剩余占比。
use super::{Account, Provider, Snapshot, Window};
use base64::Engine;
use serde_json::Value;
use std::{fs, io::Read, path::PathBuf};

const PREFIX: &str = "cursor-safe-storage:v1:";

pub fn db_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Cursor").join("User").join("globalStorage").join("state.vscdb"))
}

/// 配置里还没有 Cursor 时，接上本机状态库。已经接过就保持用户原来的开关。
pub fn ensure(config: &mut super::Config) -> bool {
    if config.accounts.iter().any(|account| account.provider == Provider::Cursor) { return false; }
    let Some(path) = db_path().filter(|path| path.is_file()) else { return false; };
    let id = "Cursor-local".to_owned();
    if config.selected.is_empty() { config.selected = id.clone(); }
    config.accounts.push(Account { id, provider: Provider::Cursor, alias: "Cursor".into(), source: path, slot: None, enabled: true, pinned: true });
    true
}

pub fn fetch(agent: &ureq::Agent, _account: &Account) -> Result<Snapshot, (String, u64)> {
    let tokens = session_tokens().map_err(|message| (message, 0))?;
    let mut rejected = None;
    for token in tokens.into_iter().take(4) {
        match request_usage(agent, &token) {
            Ok(snapshot) => return Ok(snapshot),
            Err((message, wait)) if wait == 0 && message.contains("重新登录") => rejected = Some((message, wait)),
            Err(error) => return Err(error),
        }
    }
    Err(rejected.unwrap_or_else(|| ("请在 Cursor 中重新登录后再刷新额度".into(), 0)))
}

fn request_usage(agent: &ureq::Agent, token: &str) -> Result<Snapshot, (String, u64)> {
    let response = agent.post("https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage")
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_bytes(b"{}")
        .map_err(|error| match error {
            ureq::Error::Status(401 | 403, _) => ("请在 Cursor 中重新登录后再刷新额度".into(), 0),
            ureq::Error::Status(429, response) => ("请求过于频繁，稍后自动重试".into(), response.header("Retry-After").and_then(|value| value.parse().ok()).unwrap_or(180).clamp(30, 3600)),
            ureq::Error::Status(code, _) => (format!("Cursor 额度服务暂不可用（{code}）"), 60),
            _ => ("无法连接 Cursor 额度服务，请检查网络或代理".into(), 30),
        })?;
    let mut body = String::new();
    response.into_reader().take(1024 * 1024).read_to_string(&mut body).map_err(|_| ("读取 Cursor 额度失败".into(), 30))?;
    let value = serde_json::from_str(&body).map_err(|_| ("Cursor 没有返回有效的额度数据".into(), 60))?;
    parse(&value).map_err(|message| (message, 60))
}

pub fn parse(value: &Value) -> Result<Snapshot, String> {
    let mut auto = None;
    let mut api = None;
    find_ratios(value, &mut auto, &mut api);
    let (Some(auto), Some(api)) = (auto, api) else { return Err("额度里没有两档占比".into()); };
    let window = |label: &str, used: f64| Window { label: label.into(), seconds: None, remaining: (100.0 - used).clamp(0.0, 100.0) as f32, reset: None, amount: None };
    Ok(Snapshot { plan: "Cursor".into(), windows: vec![window("Cursor 模型", auto), window("套餐模型", api)], at: chrono::Utc::now().timestamp(), ..Snapshot::default() })
}

fn find_ratios(value: &Value, auto: &mut Option<f64>, api: &mut Option<f64>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match key.as_str() {
                    "autoPercentUsed" | "composerPercentUsed" | "cursorPercentUsed" if auto.is_none() => *auto = child.as_f64(),
                    "apiPercentUsed" | "namedPercentUsed" | "premiumPercentUsed" if api.is_none() => *api = child.as_f64(),
                    _ => find_ratios(child, auto, api),
                }
            }
        }
        Value::Array(items) => { for item in items { find_ratios(item, auto, api); } }
        _ => {}
    }
}

fn session_tokens() -> Result<Vec<String>, String> {
    let path = db_path().ok_or("没有找到本机 Cursor 登录")?;
    let mut tokens = Vec::new();
    let mut error = "请在 Cursor 中重新登录后再刷新额度".to_owned();
    for candidate in [path.clone(), path.with_extension("vscdb-wal")] {
        for stored in stored_tokens(&candidate) {
            match reveal(&stored) {
                Ok(token) if !tokens.contains(&token) => tokens.push(token),
                Ok(_) => {}
                Err(message) => error = message,
            }
        }
    }
    tokens.sort_by_key(|token| std::cmp::Reverse(jwt_exp(token)));
    if tokens.is_empty() { Err(error) } else { Ok(tokens) }
}

fn stored_tokens(path: &std::path::Path) -> Vec<String> {
    let Ok(bytes) = fs::read(path) else { return Vec::new(); };
    let key = b"cursorAuth/accessToken";
    let mut from = 0;
    let mut found = Vec::new();
    while let Some(pos) = bytes[from..].windows(key.len()).position(|window| window == key) {
        let start = from + pos;
        let end = (start + 1400).min(bytes.len());
        if let Some(token) = extract(&bytes[start..end]) { found.push(token); }
        from = start + key.len();
        if from >= bytes.len() { break; }
    }
    found
}

fn extract(window: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(window);
    if let Some(at) = text.find(PREFIX) {
        if at <= 800 {
            let rest = &text[at + PREFIX.len()..];
            let end = rest.find(|c: char| !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' | '=')).unwrap_or(rest.len());
            if end >= 40 { return Some(format!("{PREFIX}{}", &rest[..end])); }
        }
    }
    if let Some(at) = text.find("eyJ") {
        if at <= 800 {
            let rest = &text[at..];
            let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))).unwrap_or(rest.len());
            let jwt = &rest[..end];
            if jwt.split('.').count() == 3 && jwt.len() > 40 { return Some(jwt.to_owned()); }
        }
    }
    None
}

fn reveal(stored: &str) -> Result<String, String> {
    if let Some(body) = stored.strip_prefix(PREFIX) {
        let blob = base64::engine::general_purpose::STANDARD.decode(body).map_err(|_| "Cursor 登录格式无法识别".to_owned())?;
        decrypt_storage(&blob)
    } else if stored.starts_with("eyJ") && stored.split('.').count() == 3 {
        Ok(stored.to_owned())
    } else {
        Err("Cursor 登录格式无法识别".into())
    }
}

fn jwt_exp(token: &str) -> i64 {
    let Some(payload) = token.split('.').nth(1) else { return 0; };
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload)).unwrap_or_default();
    serde_json::from_slice::<Value>(&decoded).ok().and_then(|value| value.get("exp").and_then(Value::as_i64)).unwrap_or(0)
}

#[cfg(not(windows))]
fn decrypt_storage(_blob: &[u8]) -> Result<String, String> { Err("请在 Windows 上打开 Cursor 并登录后再刷新额度".into()) }

#[cfg(windows)]
fn decrypt_storage(blob: &[u8]) -> Result<String, String> {
    if blob.len() < 31 || &blob[..3] != b"v10" { return Err("Cursor 登录格式无法识别".into()); }
    let key = os_crypt_key()?;
    let unbound = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key).map_err(|_| "无法解密 Cursor 登录")?;
    let key = ring::aead::LessSafeKey::new(unbound);
    let nonce = ring::aead::Nonce::try_assume_unique_for_key(&blob[3..15]).map_err(|_| "无法解密 Cursor 登录")?;
    let mut sealed = blob[15..].to_vec();
    let plain = key.open_in_place(nonce, ring::aead::Aad::empty(), &mut sealed).map_err(|_| "无法解密 Cursor 登录")?;
    String::from_utf8(plain.to_vec()).map_err(|_| "无法解密 Cursor 登录".into())
}

#[cfg(windows)]
fn os_crypt_key() -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let path = std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Cursor").join("Local State")).ok_or("没有找到本机 Cursor 登录")?;
    let raw = fs::read(&path).map_err(|_| "没有找到本机 Cursor 登录")?;
    let value: Value = serde_json::from_slice(&raw).map_err(|_| "没有找到本机 Cursor 登录")?;
    let encoded = value.pointer("/os_crypt/encrypted_key").and_then(Value::as_str).ok_or("没有找到本机 Cursor 登录")?;
    let mut encrypted = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_| "没有找到本机 Cursor 登录")?;
    if encrypted.len() <= 5 || &encrypted[..5] != b"DPAPI" { return Err("没有找到本机 Cursor 登录".into()); }
    let encrypted = &mut encrypted[5..];
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: encrypted.len() as u32, pbData: encrypted.as_mut_ptr() };
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        CryptUnprotectData(&input, None, None, None, None, 0x1, &mut output).map_err(|_| "无法解密 Cursor 登录")?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        if !output.pbData.is_null() { let _ = LocalFree(HLOCAL(output.pbData.cast())); }
        if bytes.len() != 32 { return Err("无法解密 Cursor 登录".into()); }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cursor_usage_splits_two_remaining_ratios() {
        let snapshot = parse(&json!({"planUsage":{"autoPercentUsed":21.2,"apiPercentUsed":80.3,"totalPercentUsed":25.0}})).unwrap();
        assert_eq!(snapshot.windows[0].label, "Cursor 模型");
        assert_eq!(snapshot.windows[1].label, "套餐模型");
        assert!((snapshot.windows[0].remaining - 78.8).abs() < 0.05);
        assert!((snapshot.windows[1].remaining - 19.7).abs() < 0.05);
    }

    #[test]
    fn nested_aliases_still_split_the_two_pools() {
        let snapshot = parse(&json!({"billing":{"composerPercentUsed":10.0,"premiumPercentUsed":40.0}})).unwrap();
        assert!((snapshot.windows[0].remaining - 90.0).abs() < 0.01);
        assert!((snapshot.windows[1].remaining - 60.0).abs() < 0.01);
    }

    #[test]
    fn missing_ratios_are_reported() {
        assert_eq!(parse(&json!({"planUsage":{"totalPercentUsed":1.0}})).unwrap_err(), "额度里没有两档占比");
    }

    #[test]
    #[ignore = "读取本机 Cursor 登录并查询两档额度"]
    fn local_cursor_reports_two_ratios() {
        let path = db_path().expect("本机没有 Cursor 状态库");
        let account = Account { id: "Cursor-local".into(), provider: Provider::Cursor, alias: "Cursor".into(), source: path, slot: None, enabled: true, pinned: true };
        let snapshot = fetch(&super::super::http_agent(), &account).unwrap_or_else(|(message, _)| panic!("{message}"));
        assert_eq!(snapshot.windows.len(), 2);
        assert!(snapshot.windows.iter().all(|window| (0.0..=100.0).contains(&window.remaining)));
    }
}
