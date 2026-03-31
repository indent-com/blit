use clap::Parser;
use hmac::Hmac;
use pbkdf2::pbkdf2;
use sha2::Sha256;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const DEFAULT_SIGNAL_URL: &str = "wss://cloud.blit.sh";
const DEFAULT_URL_TEMPLATE: &str = "https://blit.sh/#{secret}";
const REPO_BASE: &str = "https://repo.blit.sh";
const PBKDF2_SALT: &[u8] = b"https://blit.sh";
const PBKDF2_ROUNDS: u32 = 100_000;

#[derive(Parser)]
#[command(name = "blitz", version, about = "Share a terminal session via WebRTC")]
struct Cli {
    /// Passphrase for the session (default: random UUID)
    #[arg(long)]
    passphrase: Option<String>,

    /// Signaling service URL
    #[arg(long, default_value = DEFAULT_SIGNAL_URL)]
    signal_url: String,

    /// URL template to display (use {secret} as placeholder)
    #[arg(long, default_value = DEFAULT_URL_TEMPLATE)]
    url: String,

    /// Connect to an existing blit-server socket instead of starting one
    #[arg(long)]
    socket: Option<String>,

    /// Don't print the sharing URL
    #[arg(long)]
    quiet: bool,
}

fn platform() -> (&'static str, &'static str) {
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        _ => {
            eprintln!("unsupported OS: {}", std::env::consts::OS);
            std::process::exit(1);
        }
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => {
            eprintln!("unsupported architecture: {}", std::env::consts::ARCH);
            std::process::exit(1);
        }
    };
    (os, arch)
}

fn bin_dir(revision: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".blit").join("bin").join(revision)
}

fn fetch_latest_revision() -> Result<String, Box<dyn std::error::Error>> {
    let url = format!("{REPO_BASE}/latest");
    let body = ureq::get(&url).call()?.body_mut().read_to_string()?;
    Ok(body.trim().to_owned())
}

fn download_binary(os: &str, arch: &str, name: &str, dest: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{REPO_BASE}/{os}/{arch}/{name}");
    eprintln!("downloading {name} from {url}");
    let mut reader = ureq::get(&url).call()?.into_body().into_reader();
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("tmp");
    let mut file = fs::File::create(&tmp)?;
    std::io::copy(&mut reader, &mut file)?;
    file.flush()?;
    drop(file);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn ensure_binary(os: &str, arch: &str, name: &str, dir: &PathBuf) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        if let Err(e) = download_binary(os, arch, name, &path) {
            eprintln!("failed to download {name}: {e}");
            std::process::exit(1);
        }
    }
    path
}

fn main() {
    let cli = Cli::parse();
    let (os, arch) = platform();

    let passphrase = cli
        .passphrase
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let revision = match fetch_latest_revision() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("failed to fetch latest revision: {e}");
            std::process::exit(1);
        }
    };
    let dir = bin_dir(&revision);

    let mut server_child: Option<Child> = None;
    let mut sock_to_clean: Option<String> = None;

    let sock_path = match cli.socket {
        Some(path) => path,
        None => {
            let server_bin = ensure_binary(os, arch, "blit-server", &dir);

            let mut hash = [0u8; 32];
            pbkdf2::<Hmac<Sha256>>(passphrase.as_bytes(), PBKDF2_SALT, PBKDF2_ROUNDS, &mut hash)
                .expect("HMAC can be initialized with any key length");
            let suffix: String = hash[..4].iter().map(|b| format!("{b:02x}")).collect();

            let sock = format!(
                "{}/blitz-{suffix}.sock",
                std::env::var("TMPDIR")
                    .or_else(|_| std::env::var("XDG_RUNTIME_DIR"))
                    .unwrap_or_else(|_| "/tmp".into()),
            );

            let child = Command::new(&server_bin)
                .arg("--socket")
                .arg(&sock)
                .stdin(Stdio::null())
                .spawn()
                .unwrap_or_else(|e| {
                    eprintln!("failed to start blit-server: {e}");
                    std::process::exit(1);
                });
            server_child = Some(child);

            for _ in 0..100 {
                if Path::new(&sock).exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if !Path::new(&sock).exists() {
                eprintln!("blit-server did not create socket in time");
                if let Some(mut c) = server_child.take() {
                    c.kill().ok();
                }
                std::process::exit(1);
            }

            eprintln!("blit-server listening on {sock}");
            sock_to_clean = Some(sock.clone());
            sock
        }
    };

    let forwarder_bin = ensure_binary(os, arch, "blit-webrtc-forwarder", &dir);

    let mut cmd = Command::new(&forwarder_bin);
    cmd.arg("--socket").arg(&sock_path);
    cmd.env("BLITZ_PASSPHRASE", &passphrase);
    cmd.arg("--signal-url").arg(&cli.signal_url);
    cmd.arg("--url").arg(&cli.url);
    if cli.quiet {
        cmd.arg("--quiet");
    }

    let status = cmd.status().unwrap_or_else(|e| {
        eprintln!("failed to start blit-webrtc-forwarder: {e}");
        std::process::exit(1);
    });

    if let Some(mut c) = server_child.take() {
        c.kill().ok();
        c.wait().ok();
    }
    if let Some(sock) = sock_to_clean {
        let _ = fs::remove_file(&sock);
    }

    std::process::exit(status.code().unwrap_or(1));
}
