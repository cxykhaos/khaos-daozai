//! 本机 Clash 订阅流量。只读 Clash Neo / Clash Verge 已经写好的配置，不访问订阅地址。
use std::{fs, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub name: String,
    pub upload: u64,
    pub download: u64,
    pub total: u64,
    pub expire: Option<i64>,
}

impl Usage {
    pub fn used(&self) -> u64 { self.upload.saturating_add(self.download).min(self.total) }
    pub fn used_percent(&self) -> u8 {
        if self.total == 0 { return 0; }
        ((self.used() as f64 / self.total as f64) * 100.0).round().clamp(0.0, 100.0) as u8
    }
    pub fn summary(&self) -> String {
        let mut text = format!("{} 已用 {} / {}", self.name, gib(self.used()), gib(self.total));
        if let Some(expire) = self.expire.and_then(|at| chrono::DateTime::from_timestamp(at, 0)) {
            text.push_str(&format!(" · {} 到期", expire.with_timezone(&chrono::Local).format("%Y-%m-%d")));
        }
        text
    }
}

pub fn load() -> Option<Usage> {
    candidates().into_iter().find_map(|path| fs::read_to_string(path).ok().and_then(|text| parse(&text)))
}

fn candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(roaming) = std::env::var_os("APPDATA") {
        let roaming = PathBuf::from(roaming);
        for name in ["io.github.clash-neo", "io.github.clash-verge-rev.clash-verge-rev", "clash-verge", "clash-verge-rev"] {
            paths.push(roaming.join(name).join("profiles.yaml"));
        }
    }
    paths
}

pub fn parse(text: &str) -> Option<Usage> {
    let current = text.lines().find_map(|line| line.trim().strip_prefix("current:").map(|value| value.trim().to_owned()))?;
    if current.is_empty() { return None; }
    let mut name = "Clash".to_owned();
    let mut upload = None;
    let mut download = None;
    let mut total = None;
    let mut expire = None;
    let mut in_item = false;
    let mut in_extra = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(uid) = trimmed.strip_prefix("- uid:") {
            in_item = uid.trim() == current;
            in_extra = false;
            continue;
        }
        if !in_item { continue; }
        if let Some(value) = trimmed.strip_prefix("name:") {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() && value != "null" { name = value.to_owned(); }
        }
        if trimmed == "extra:" { in_extra = true; continue; }
        if in_extra && !line.starts_with(' ') && !trimmed.is_empty() { in_extra = false; }
        if !in_extra { continue; }
        let Some((key, value)) = trimmed.split_once(':') else { continue };
        let value = value.trim();
        match key.trim() {
            "upload" => upload = value.parse().ok(),
            "download" => download = value.parse().ok(),
            "total" => total = value.parse().ok(),
            "expire" => expire = value.parse().ok(),
            _ => {}
        }
    }
    let total = total.filter(|total| *total > 0)?;
    Some(Usage { name, upload: upload.unwrap_or(0), download: download.unwrap_or(0), total, expire })
}

fn gib(bytes: u64) -> String {
    let value = bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    if value >= 10.0 { format!("{:.0} GB", value) } else { format!("{value:.1} GB") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_profile_reports_the_same_usage_as_clash() {
        let text = r#"
current: remote-1
items:
- uid: other
  type: remote
  name: old.yaml
  extra:
    upload: 1
    download: 1
    total: 10
    expire: 1
- uid: remote-1
  type: remote
  name: tntv2.net.yaml
  extra:
    upload: 1942755284
    download: 238660939234
    total: 326929219584
    expire: 1798646400
"#;
        let usage = parse(text).unwrap();
        assert_eq!(usage.name, "tntv2.net.yaml");
        assert_eq!(usage.used_percent(), 74);
        assert_eq!(gib(usage.used()), "224 GB");
        assert_eq!(gib(usage.total), "304 GB");
        assert_eq!(usage.expire, Some(1798646400));
        assert!(usage.summary().contains("到期"));
    }

    #[test]
    #[ignore]
    fn live_local_clash_profile_has_usage() {
        let usage = load().expect("本机没有 Clash 流量");
        assert!(usage.used_percent() > 0, "{}", usage.summary());
        assert!(usage.summary().contains("GB"));
    }
}
