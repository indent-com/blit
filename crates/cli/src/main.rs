mod agent;
mod cli;
mod generate;
mod interactive;
mod transport;

use clap::Parser;
use cli::{Cli, Command, RecordTarget, RemoteCommand};

fn main() {
    // ProxyDaemon must run synchronously — blit_proxy::run() builds its own
    // tokio runtime, which panics if called from within an existing one.
    // Detect this subcommand before entering the async runtime. Use `any()`
    // rather than `nth(1)` so that global flags placed before the subcommand
    // (e.g. `blit --on foo proxy-daemon`) are handled correctly.
    if std::env::args().any(|a| a == "proxy-daemon") {
        blit_proxy::run(false);
        return;
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async_main());
}

async fn async_main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let cli = Cli::parse();

    match cli.command {
        Command::Server {
            socket,
            shell_flags,
            scrollback,
            #[cfg(unix)]
            fd_channel,
            verbose,
        } => {
            let ipc_path = socket
                .or_else(|| std::env::var("BLIT_SOCK").ok())
                .unwrap_or_else(blit_server::default_ipc_path);

            #[cfg(unix)]
            let shell_default = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            #[cfg(windows)]
            let shell_default = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());

            #[cfg(unix)]
            let flags_default = "li";
            #[cfg(windows)]
            let flags_default = "";

            let config = blit_server::Config {
                shell: shell_default,
                shell_flags: shell_flags
                    .or_else(|| std::env::var("BLIT_SHELL_FLAGS").ok())
                    .unwrap_or_else(|| flags_default.into()),
                scrollback: scrollback
                    .or_else(|| {
                        std::env::var("BLIT_SCROLLBACK")
                            .ok()
                            .and_then(|s| s.parse().ok())
                    })
                    .unwrap_or(10_000),
                ipc_path,
                surface_encoders: blit_server::SurfaceEncoderPreference::defaults(),
                surface_quality: std::env::var("BLIT_SURFACE_QUALITY")
                    .ok()
                    .and_then(|v| blit_server::SurfaceQuality::parse(&v))
                    .unwrap_or_default(),
                vaapi_device: std::env::var("BLIT_VAAPI_DEVICE")
                    .unwrap_or_else(|_| "/dev/dri/renderD128".into()),
                #[cfg(unix)]
                fd_channel: fd_channel.or_else(|| {
                    std::env::var("BLIT_FD_CHANNEL")
                        .ok()
                        .and_then(|s| s.parse().ok())
                }),
                verbose: verbose
                    || std::env::var("BLIT_VERBOSE")
                        .ok()
                        .map(|v| v == "1")
                        .unwrap_or(false),
                max_connections: 0,
                max_ptys: 0,
            };
            blit_server::run(config).await;
        }
        Command::Install { host } => match host {
            Some(host) => {
                if let Err(e) = cmd_install(&host).await {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            }
            None => {
                println!("# Linux / macOS");
                println!("curl -sf https://install.blit.sh | sh");
                println!();
                println!("# Windows (PowerShell)");
                println!("irm https://install.blit.sh/install.ps1 | iex");
            }
        },
        Command::Upgrade => {
            if let Err(e) = cmd_upgrade().await {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Share {
            passphrase,
            quiet,
            verbose,
        } => {
            let signal_url = blit_webrtc_forwarder::normalize_hub(&cli.connect.hub);
            let passphrase = passphrase.unwrap_or_else(|| {
                use rand::RngExt as _;
                const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
                let mut rng = rand::rng();
                let bytes: [u8; 26] = rng.random();
                bytes
                    .iter()
                    .map(|b| ALPHABET[(b & 0x1f) as usize] as char)
                    .collect()
            });

            let sock_path = transport::default_local_socket();
            if let Err(e) = transport::ensure_local_server(&sock_path).await {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }

            blit_webrtc_forwarder::run(blit_webrtc_forwarder::Config {
                sock_path,
                signal_url,
                passphrase,
                message_override: None,
                quiet,
                verbose,
            })
            .await;
        }
        Command::Remote(remote_cmd) => {
            cmd_remote(remote_cmd);
        }
        cmd @ (Command::List
        | Command::Start { .. }
        | Command::Show { .. }
        | Command::History { .. }
        | Command::Send { .. }
        | Command::Restart { .. }
        | Command::Kill { .. }
        | Command::Close { .. }
        | Command::Wait { .. }
        | Command::Surfaces
        | Command::Capture { .. }
        | Command::Record { .. }
        | Command::Click { .. }
        | Command::Key { .. }
        | Command::Type { .. }) => {
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            };
            let result = match cmd {
                Command::List => agent::cmd_list(transport).await,
                Command::Start {
                    command,
                    tag,
                    rows,
                    cols,
                    wait,
                    timeout,
                } => {
                    let start_result = agent::cmd_start(transport, tag, command, rows, cols).await;
                    if wait {
                        let pty_id = match start_result {
                            Ok(id) => id,
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(1);
                            }
                        };
                        let transport2 = match transport::connect(&conn.on, &conn.hub).await {
                            Ok(t) => t,
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(1);
                            }
                        };
                        match agent::cmd_wait(transport2, pty_id, timeout.unwrap(), None).await {
                            Ok(code) => std::process::exit(code),
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                    start_result.map(|_| ())
                }
                Command::Show {
                    id,
                    ansi,
                    rows,
                    cols,
                } => agent::cmd_show(transport, id, ansi, rows, cols).await,
                Command::History {
                    id,
                    from_start,
                    from_end,
                    limit,
                    ansi,
                    rows,
                    cols,
                } => {
                    let size = agent::capture_size(rows, cols);
                    agent::cmd_history(transport, id, from_start, from_end, limit, ansi, size).await
                }
                Command::Send { id, text } => {
                    let text = if text == "-" {
                        use std::io::Read;
                        let mut buf = String::new();
                        std::io::stdin().read_to_string(&mut buf).unwrap_or(0);
                        buf
                    } else {
                        text
                    };
                    agent::cmd_send(transport, id, text).await
                }
                Command::Restart { id } => agent::cmd_restart(transport, id).await,
                Command::Kill { id, signal } => agent::cmd_kill(transport, id, &signal).await,
                Command::Close { id } => agent::cmd_close(transport, id).await,
                Command::Wait {
                    id,
                    timeout,
                    pattern,
                } => match agent::cmd_wait(transport, id, timeout, pattern).await {
                    Ok(code) => std::process::exit(code),
                    Err(e) => {
                        eprintln!("blit: {e}");
                        std::process::exit(1);
                    }
                },
                Command::Surfaces => agent::cmd_surfaces(transport).await,
                Command::Capture {
                    id,
                    output,
                    format,
                    quality,
                    width,
                    height,
                } => {
                    agent::cmd_capture(transport, id, output, format, quality, width, height).await
                }
                Command::Click { id, x, y, button } => {
                    agent::cmd_click(transport, id, x, y, &button).await
                }
                Command::Key { id, key } => agent::cmd_key(transport, id, &key).await,
                Command::Type { id, text } => agent::cmd_type(transport, id, &text).await,
                Command::Record(target) => match target {
                    RecordTarget::Surface { id, output, frames } => {
                        agent::cmd_record(transport, id, output, frames, true).await
                    }
                    RecordTarget::Pty { id, output, frames } => {
                        agent::cmd_record(transport, id, output, frames, false).await
                    }
                },
                _ => unreachable!(),
            };
            if let Err(e) = result {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Open { port } => {
            let hub = blit_webrtc_forwarder::normalize_hub(&cli.connect.hub);
            interactive::run_browser(port, &hub).await;
        }
        Command::Gateway => {
            blit_gateway::run().await;
        }
        Command::Learn => {
            print!("{}", include_str!("learn.md"));
        }
        Command::Generate { output } => {
            generate::run(&output);
        }
        Command::ProxyDaemon => {
            // Normally handled synchronously in main() before the async runtime
            // starts. Reaching here means clap parsed the subcommand but the
            // early-exit in main() missed it (e.g. due to argument reordering).
            // Run the daemon now rather than panicking.
            blit_proxy::run(false);
        }
    }
}

/// Replace the passphrase in a `share:PASSPHRASE` URI with `****`.
/// URIs with an optional `?hub=...` query string are handled correctly.
/// Non-share URIs are returned unchanged.
fn mask_share_passphrase(uri: &str) -> String {
    let rest = match uri.strip_prefix("share:") {
        Some(r) => r,
        None => return uri.to_string(),
    };
    // Preserve any query string (e.g. ?hub=...)
    if let Some(q_pos) = rest.find('?') {
        format!("share:****{}", &rest[q_pos..])
    } else {
        "share:****".to_string()
    }
}

fn cmd_remote(cmd: RemoteCommand) {
    match cmd {
        RemoteCommand::List { reveal } => {
            let entries = blit_webserver::config::read_remotes();
            if entries.is_empty() {
                eprintln!("blit: no remotes configured (blit.remotes is empty or missing)");
            } else {
                for (name, uri) in &entries {
                    let display_uri = if !reveal {
                        mask_share_passphrase(uri)
                    } else {
                        uri.clone()
                    };
                    println!("{name}\t{display_uri}");
                }
            }
        }
        RemoteCommand::Add { name, uri } => {
            if name.is_empty() || name.contains('=') || name.contains('\n') {
                eprintln!("blit: invalid remote name '{name}'");
                std::process::exit(1);
            }
            let uri = match uri {
                Some(u) => u,
                None => {
                    eprint!("URI for '{name}' (ssh:host, tcp:h:p, socket:/path, local): ");
                    let mut input = String::new();
                    if std::io::stdin().read_line(&mut input).is_err() || input.trim().is_empty() {
                        eprintln!("\nblit: no URI provided");
                        std::process::exit(1);
                    }
                    input.trim().to_string()
                }
            };
            let mut entries = blit_webserver::config::read_remotes();
            // Update in place if name exists, preserving order; append if new.
            if let Some(pos) = entries.iter().position(|(n, _)| n == &name) {
                entries[pos].1 = uri.clone();
            } else {
                entries.push((name.clone(), uri.clone()));
            }
            blit_webserver::config::write_remotes(&entries);
            eprintln!("blit: remote '{name}' set to '{uri}'");
        }
        RemoteCommand::Remove { name } => {
            let mut entries = blit_webserver::config::read_remotes();
            let before = entries.len();
            entries.retain(|(n, _)| n != &name);
            if entries.len() == before {
                eprintln!("blit: no remote named '{name}'");
                std::process::exit(1);
            }
            blit_webserver::config::write_remotes(&entries);
            eprintln!("blit: remote '{name}' removed");
        }
        RemoteCommand::SetDefault { target } => {
            let mut config = blit_webserver::config::read_config();
            if target.is_empty() || target == "local" {
                config.remove("blit.target");
                eprintln!("blit: default target cleared (using local)");
            } else {
                config.insert("blit.target".into(), target.clone());
                eprintln!("blit: default target set to '{target}'");
            }
            blit_webserver::config::write_config(&config);
        }
    }
}

async fn cmd_install(host: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Reject hosts starting with '-' to prevent SSH option injection.
    let host_check = host.split('@').next_back().unwrap_or(host);
    if host_check.starts_with('-') {
        return Err(format!("invalid ssh host '{host}': must not start with '-'").into());
    }
    let ssh_base = |host: &str| {
        let mut cmd = std::process::Command::new("ssh");
        cmd.arg("-T")
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPath=/tmp/blit-ssh-%r@%h:%p")
            .arg("-o")
            .arg("ControlPersist=300")
            .arg(host);
        cmd
    };

    let detect = ssh_base(host)
        .arg("--")
        .arg("uname -s 2>/dev/null || echo WINDOWS")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()?;

    if !detect.status.success() {
        return Err("ssh failed to detect remote OS".into());
    }

    let os = String::from_utf8_lossy(&detect.stdout)
        .trim()
        .to_uppercase();

    let install_cmd = if os.contains("WINDOWS")
        || os.contains("MINGW")
        || os.contains("MSYS")
        || os.contains("CYGWIN")
    {
        r#"powershell -ExecutionPolicy Bypass -Command "irm https://install.blit.sh/install.ps1 | iex""#.to_string()
    } else {
        r#"sh -c 'if command -v curl >/dev/null 2>&1; then curl -sf https://install.blit.sh | sh; elif command -v wget >/dev/null 2>&1; then wget -qO- https://install.blit.sh | sh; else echo "error: neither curl nor wget found" >&2; exit 1; fi'"#.to_string()
    };

    eprintln!("installing blit on {host} ({os})...");

    let status = ssh_base(host)
        .arg("--")
        .arg(&install_cmd)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;

    if !status.success() {
        return Err(format!("remote install exited with {status}").into());
    }

    Ok(())
}

async fn cmd_upgrade() -> Result<(), Box<dyn std::error::Error>> {
    let exe_path = std::env::current_exe()?;
    let install_dir = exe_path
        .parent()
        .ok_or("cannot determine binary directory")?;

    let install_url = if cfg!(windows) {
        "https://install.blit.sh/install.ps1"
    } else {
        "https://install.blit.sh"
    };
    let script = reqwest::get(install_url)
        .await?
        .error_for_status()?
        .text()
        .await?;

    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let tmp = std::env::temp_dir().join(format!("blit-install-{}.{}", std::process::id(), ext));
    std::fs::write(&tmp, &script)?;

    #[cfg(unix)]
    {
        let status = std::process::Command::new("sh")
            .arg(&tmp)
            .env("BLIT_INSTALL_DIR", install_dir)
            .status()?;
        if status.success() {
            transport::stop_proxy().await;
        }
        std::process::exit(status.code().unwrap_or(1));
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-File"])
            .arg(&tmp)
            .env("BLIT_INSTALL_DIR", install_dir)
            .status()?;
        if status.success() {
            transport::stop_proxy().await;
        }
        std::process::exit(status.code().unwrap_or(1));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let status = std::process::Command::new("sh")
            .arg(&tmp)
            .env("BLIT_INSTALL_DIR", install_dir)
            .status()?;
        if status.success() {
            transport::stop_proxy().await;
        }
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::mask_share_passphrase;

    #[test]
    fn test_mask_share_passphrase() {
        assert_eq!(mask_share_passphrase("share:mysecret"), "share:****");
        assert_eq!(
            mask_share_passphrase("share:mysecret?hub=hub.blit.sh"),
            "share:****?hub=hub.blit.sh"
        );
        assert_eq!(
            mask_share_passphrase("ssh:alice@prod.co"),
            "ssh:alice@prod.co"
        );
        assert_eq!(mask_share_passphrase("local"), "local");
        assert_eq!(mask_share_passphrase("share:"), "share:****");
    }
}
