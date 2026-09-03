//! `recipedia` — builder-side CLI: login, publish, status.
//!
//! publish scans the given paths for `*.pkg.tar.zst`, skips files the server
//! already has (ETag == local sha256), and streams the rest to
//! POST /api/repo/publish/{filename}.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use sha2::Digest;

fn client_config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    base.join("recipedia").join("config.toml")
}

#[derive(Clone)]
struct ClientConfig {
    url: String,
    token: String,
}

impl ClientConfig {
    fn load() -> Result<Self> {
        let path = client_config_path();
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "no client config — run `recipedia login` first ({})",
                path.display()
            )
        })?;
        let tbl: toml::Table = toml::from_str(&text)?;
        Ok(Self {
            url: tbl
                .get("url")
                .and_then(|v| v.as_str())
                .context("client config missing url")?
                .trim_end_matches('/')
                .to_string(),
            token: tbl
                .get("token")
                .and_then(|v| v.as_str())
                .context("client config missing token")?
                .to_string(),
        })
    }

    fn save(self) -> Result<()> {
        let path = client_config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = format!(
            "# recipedia client credentials\nurl = \"{}\"\ntoken = \"{}\"\n",
            self.url.trim_end_matches('/'),
            self.token,
        );
        std::fs::write(&path, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }
        println!("saved {}", path.display());
        Ok(())
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = (!args.is_empty()).then(|| args.remove(0)) else {
        print_usage();
        return Ok(());
    };
    match cmd.as_str() {
        "login" => login(args),
        "publish" => publish(args),
        "unpublish" => unpublish(args),
        "status" => status(args),
        "--help" | "-h" | "help" => {
            print_usage();
            Ok(())
        }
        other => bail!("unknown command '{other}'"),
    }
}

fn print_usage() {
    println!(
        "recipedia — sclinux package publisher\n\n\
         USAGE:\n  \
         recipedia login <server-url> --token <TOKEN>\n  \
         recipedia publish <dir-or-file>...\n  \
         recipedia unpublish <filename>\n  \
         recipedia status [--state missing|outdated|built|ahead]\n\n\
         A build log is attached automatically when a sibling\n\
         `<archive>.log` file exists next to a published archive."
    );
}

/// Split args into (positional values, flag values by name), so
/// `publish a --state b` gives positionals [a] and flags {--state: b}.
fn parse_args(args: &[String]) -> (Vec<String>, std::collections::BTreeMap<String, String>) {
    let mut positional = Vec::new();
    let mut flags = std::collections::BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            if let Some(value) = args.get(i + 1) {
                flags.insert(args[i].clone(), value.clone());
            }
            i += 2;
        } else {
            positional.push(args[i].clone());
            i += 1;
        }
    }
    (positional, flags)
}

fn login(args: Vec<String>) -> Result<()> {
    let (_, flags) = parse_args(&args);
    let token = flags
        .get("--token")
        .cloned()
        .context("usage: recipedia login <server-url> --token <TOKEN>")?;
    let url = args
        .iter()
        .find(|a| !a.starts_with("--") && a.as_str() != token)
        .cloned()
        .context("usage: recipedia login <server-url> --token <TOKEN>")?;
    ClientConfig { url, token }.save()
}

fn publish(args: Vec<String>) -> Result<()> {
    let cfg = ClientConfig::load()?;
    let (roots, _) = parse_args(&args);
    if roots.is_empty() {
        bail!("publish needs at least one directory or file");
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for root in &roots {
        collect_archives(Path::new(root), &mut files)?;
    }
    files.sort();

    let mut uploaded = 0usize;
    let mut uptodate = 0usize;
    for file in &files {
        let filename = file
            .file_name()
            .and_then(|n| n.to_str())
            .context("bad filename")?;
        let local_sha = sha256_file(file)?;
        // Server ETag is the stored sha256; matching means it already has this exact file.
        let head = ureq::head(format!("{}/api/repo/publish/{filename}", cfg.url).as_str())
            .set("Authorization", &format!("Bearer {}", cfg.token))
            .call();
        if let Ok(resp) = head {
            let want = format!("\"{local_sha}\"");
            if resp.header("etag").map(str::trim) == Some(want.as_str()) {
                println!("= {filename} (already published)");
                uptodate += 1;
                continue;
            }
        }

        let size = std::fs::metadata(file)?.len();
        let resp = ureq::post(format!("{}/api/repo/publish/{filename}", cfg.url).as_str())
            .set("Authorization", &format!("Bearer {}", cfg.token))
            .set("x-sha256", &local_sha)
            .send(std::fs::File::open(file)?)
            .map_err(|e| anyhow::anyhow!("upload failed for {filename}: {e}"))?;
        let receipt: serde_json::Value = serde_json::from_reader(resp.into_reader())?;
        let ver_rel = format!(
            "{}-{}",
            receipt["version"].as_str().unwrap_or("?"),
            receipt["release"].as_str().unwrap_or("?")
        );
        println!(
            "+ {} · {} {ver_rel} · {:.1} MiB · state {}",
            filename,
            receipt["name"].as_str().unwrap_or("?"),
            size as f64 / 1048576.0,
            receipt["state"].as_str().unwrap_or("?"),
        );
        attach_build_log(&cfg, file, filename);
        uploaded += 1;
    }
    println!(
        "{uploaded} uploaded, {uptodate} up-to-date, {} total",
        files.len()
    );
    Ok(())
}

/// Upload the sibling `<archive>.log` when present — sage build output lands
/// next to the archive, and the hub shows it on the package page. Best-effort:
/// a failed log upload never fails the publish that already succeeded.
fn attach_build_log(cfg: &ClientConfig, archive: &Path, filename: &str) {
    let log_path = PathBuf::from(format!("{}.log", archive.display()));
    let Ok(content) = std::fs::read(&log_path) else {
        return;
    };
    if content.len() > 1024 * 1024 {
        println!(
            "  · build log skipped ({} exceeds 1 MiB)",
            log_path.display()
        );
        return;
    }
    match ureq::post(format!("{}/api/repo/publish/{filename}/log", cfg.url).as_str())
        .set("Authorization", &format!("Bearer {}", cfg.token))
        .set("Content-Type", "text/plain; charset=utf-8")
        .send(&content[..])
    {
        Ok(_) => println!("  · build log attached ({} bytes)", content.len()),
        Err(e) => println!("  · build log upload failed: {e}"),
    }
}

fn unpublish(args: Vec<String>) -> Result<()> {
    let cfg = ClientConfig::load()?;
    let (positional, _) = parse_args(&args);
    let Some(filename) = positional.first() else {
        bail!("usage: recipedia unpublish <filename>");
    };
    match ureq::delete(format!("{}/api/repo/publish/{filename}", cfg.url).as_str())
        .set("Authorization", &format!("Bearer {}", cfg.token))
        .call()
    {
        Ok(_) => println!("- {filename} withdrawn"),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            bail!("server refused ({code}): {body}");
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn collect_archives(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if root.is_file() {
        out.push(root.to_path_buf());
        return Ok(());
    }
    for entry in std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_archives(&path, out)?;
        } else if path.to_string_lossy().ends_with(".pkg.tar.zst") {
            out.push(path);
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn status(args: Vec<String>) -> Result<()> {
    let cfg = ClientConfig::load()?;
    let (_, flags) = parse_args(&args);
    let want = flags.get("--state").map(String::as_str);
    let resp = ureq::get(format!("{}/api/status", cfg.url).as_str()).call()?;
    let entries: serde_json::Value = serde_json::from_reader(resp.into_reader())?;

    let list = entries.as_array().context("unexpected /api/status shape")?;
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for e in list {
        let st = e["state"].as_str().unwrap_or("?");
        *counts.entry(st).or_default() += 1;
    }
    println!(
        "total {} · {}",
        list.len(),
        counts
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if let Some(want) = want {
        for e in list {
            if e["state"].as_str() == Some(want) {
                println!(
                    "  {} {}-{}",
                    e["name"].as_str().unwrap_or("?"),
                    e["recipe_version"].as_str().unwrap_or("?"),
                    e["recipe_release"].as_str().unwrap_or("?")
                );
            }
        }
    }
    Ok(())
}
