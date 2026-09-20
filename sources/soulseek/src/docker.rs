//! Running slskd in Docker: detection, the `slskd.yml` medley reads its API key from, and the container itself.

use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

pub const CONTAINER: &str = "slskd";
pub const HTTP_PORT: u16 = 5030;
const PORTS: [u16; 3] = [HTTP_PORT, 5031, 50300];
const DOWNLOADS_MOUNT: &str = "/downloads";

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    NotInstalled,
    Unavailable(String),
    Ready,
}

pub fn status() -> Status {
    match Command::new("docker").arg("info").output() {
        Err(e) if e.kind() == ErrorKind::NotFound => Status::NotInstalled,
        Err(e) => Status::Unavailable(e.to_string()),
        Ok(out) if out.status.success() => Status::Ready,
        Ok(out) => Status::Unavailable(first_line(&out.stderr)),
    }
}

/// The `slskd` container's state (`running`, `exited`, ...), `None` when there is none.
pub fn container_state() -> Option<String> {
    let out = Command::new("docker").args(["inspect", "--format", "{{.State.Status}}", CONTAINER]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn remove() -> Result<(), String> {
    run(&["rm", "-f", CONTAINER])
}

/// Creates the container: `folder` is slskd's `/app`, `downloads` its downloads directory.
pub fn create(folder: &Path, downloads: &Path) -> Result<(), String> {
    if let Some(port) = PORTS.into_iter().find(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_err()) {
        return Err(format!("port {port} is already in use"));
    }
    std::fs::create_dir_all(downloads.join("medley")).map_err(|e| format!("creating shared folder: {e}"))?;
    let owner = std::fs::metadata(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    // Rootless podman maps container root to the invoking user already; --user would map to a subuid.
    // Asked of the engine, not `--version`: a `docker` symlink to podman doesn't say "podman" there.
    let rootless_podman = Command::new("docker")
        .args(["info", "--format", "{{.Host.Security.Rootless}}"])
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true");
    let user = if rootless_podman { Vec::new() } else { vec!["--user".to_string(), format!("{}:{}", owner.uid(), owner.gid())] };
    let mut args = vec![
        "run".to_string(),
        "-d".into(),
        "--name".into(),
        CONTAINER.into(),
        "--restart".into(),
        "unless-stopped".into(),
        "-p".into(),
        format!("{HTTP_PORT}:5030"),
        "-p".into(),
        "5031:5031".into(),
        "-p".into(),
        "50300:50300".into(),
        "-v".into(),
        format!("{}:/app", folder.display()),
        "-v".into(),
        format!("{}:{DOWNLOADS_MOUNT}", downloads.display()),
        "-e".into(),
        format!("SLSKD_DOWNLOADS_DIR={DOWNLOADS_MOUNT}"),
        "slskd/slskd".into(),
    ];
    args.splice(6..6, user);
    log::info!("soulseek: docker {}", args.join(" "));
    run(&args)
}

/// Writes `<folder>/slskd.yml` with a fresh API key (which `yaml_config` reads back) and the Soulseek login.
pub fn write_config(folder: &Path, soulseek_user: &str, soulseek_pass: &str) -> Result<(), String> {
    let key = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let yaml = format!(
        "web:\n  authentication:\n    api_keys:\n      medley:\n        key: {key}\n        role: readwrite\n        \
         cidr: 0.0.0.0/0,::/0\nsoulseek:\n  username: {}\n  password: {}\n\
         shares:\n  directories:\n    - {DOWNLOADS_MOUNT}/\n  cache:\n    retention: 60\n",
        quote(soulseek_user),
        quote(soulseek_pass)
    );
    std::fs::write(folder.join("slskd.yml"), yaml).map_err(|e| format!("writing slskd.yml: {e}"))
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn run<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Result<(), String> {
    let out = Command::new("docker").args(args).output().map_err(|e| format!("docker: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
}

fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).lines().next().unwrap_or_default().trim().to_string()
}
