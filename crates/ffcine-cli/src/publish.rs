//! `ffcine publish --immich ALBUM FILE...`: uploads review renders to an Immich album (created if
//! missing) so they can be watched on a phone. Talks to the Immich API through `curl`.
//!
//! Server and key come from `$XDG_CONFIG_HOME/ffcine/config.json` (or `--config`):
//! `{"immich": {"url": "https://photos.example", "api_key_file": "/path/to/key"}}`, or the
//! `FFCINE_IMMICH_URL` / `FFCINE_IMMICH_API_KEY_FILE` environment variables. The key is read from
//! its file and handed to curl on stdin, never on the command line.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

use anyhow::{Context as _, bail};
use serde_json::{Value, json};

pub struct Immich {
    url: String,
    key: String,
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
        .map(|d| d.join("ffcine/config.json"))
}

impl Immich {
    pub fn from_config(config: Option<&Path>) -> anyhow::Result<Self> {
        let path = config.map(Path::to_owned).or_else(config_path);
        let cfg: Value = match &path {
            Some(p) if p.exists() => serde_json::from_str(
                &std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
            )
            .with_context(|| format!("parsing {}", p.display()))?,
            _ => Value::Null,
        };
        let get = |env: &str, key: &str| {
            std::env::var(env).ok().or_else(|| {
                cfg.pointer(&format!("/immich/{key}"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
        };
        let where_ = || {
            path.as_ref()
                .map_or_else(|| "config".to_owned(), |p| p.display().to_string())
        };
        let url = get("FFCINE_IMMICH_URL", "url")
            .with_context(|| format!("no immich.url in {} or FFCINE_IMMICH_URL", where_()))?;
        let key_file = get("FFCINE_IMMICH_API_KEY_FILE", "api_key_file").with_context(|| {
            format!(
                "no immich.api_key_file in {} or FFCINE_IMMICH_API_KEY_FILE",
                where_()
            )
        })?;
        let key = std::fs::read_to_string(&key_file)
            .with_context(|| format!("reading API key {key_file}"))?
            .trim()
            .to_owned();
        Ok(Self {
            url: url.trim_end_matches('/').to_owned(),
            key,
        })
    }

    fn curl(&self, method: &str, path: &str, extra: &[String]) -> anyhow::Result<Value> {
        let mut child = Command::new("curl")
            .args([
                "-sS",
                "--fail-with-body",
                "-H",
                "@-",
                "-H",
                "Accept: application/json",
            ])
            .args(["-X", method])
            .args(extra)
            .arg(format!("{}/api{path}", self.url))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running curl")?;
        if let Some(mut stdin) = child.stdin.take() {
            writeln!(stdin, "x-api-key: {}", self.key)?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "immich {method} {path}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
    }

    fn json_body(v: &Value) -> Vec<String> {
        vec![
            "-H".into(),
            "Content-Type: application/json".into(),
            "--data".into(),
            v.to_string(),
        ]
    }

    /// The album's id, creating it if needed.
    fn album(&self, name: &str) -> anyhow::Result<String> {
        let albums = self.curl("GET", "/albums", &[])?;
        let found = albums.as_array().into_iter().flatten().find_map(|a| {
            (a["albumName"] == name)
                .then(|| a["id"].as_str().map(str::to_owned))
                .flatten()
        });
        if let Some(id) = found {
            return Ok(id);
        }
        let a = self.curl(
            "POST",
            "/albums",
            &Self::json_body(&json!({"albumName": name})),
        )?;
        a["id"]
            .as_str()
            .map(str::to_owned)
            .context("album create returned no id")
    }

    /// Uploads `files` into album `name`; returns the asset ids.
    pub fn publish(&self, name: &str, files: &[PathBuf]) -> anyhow::Result<Vec<String>> {
        let album = self.album(name)?;
        let mut ids = Vec::new();
        for f in files {
            let meta = std::fs::metadata(f).with_context(|| format!("{}", f.display()))?;
            let mtime = meta
                .modified()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let stamp = rfc3339(mtime);
            let file_name = f.file_name().unwrap_or_default().to_string_lossy();
            let form = |k: &str, v: &str| ["-F".to_owned(), format!("{k}={v}")];
            let mut extra = Vec::new();
            extra.extend(form("assetData", &format!("@{}", f.display())));
            extra.extend(form(
                "deviceAssetId",
                &format!("ffcine-{file_name}-{}-{mtime}", meta.len()),
            ));
            extra.extend(form("deviceId", "ffcine"));
            extra.extend(form("fileCreatedAt", &stamp));
            extra.extend(form("fileModifiedAt", &stamp));
            let r = self.curl("POST", "/assets", &extra)?;
            let id = r["id"].as_str().context("upload returned no id")?;
            eprintln!(
                "{}: {} ({id})",
                f.display(),
                r["status"].as_str().unwrap_or("uploaded")
            );
            ids.push(id.to_owned());
        }
        self.curl(
            "PUT",
            &format!("/albums/{album}/assets"),
            &Self::json_body(&json!({ "ids": ids })),
        )?;
        Ok(ids)
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds (civil-from-days, Howard Hinnant).
fn rfc3339(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_793_534_400), "2026-11-01T12:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }
}
