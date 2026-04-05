#[cfg(unix)]
use std::os::unix::io::RawFd;

fn usage() -> &'static str {
    "usage: blit-server [--socket PATH] [--fd-channel FD] [--shell-flags FLAGS] [PATH]"
}

#[cfg(unix)]
fn parse_fd_value(s: &str, label: &str) -> RawFd {
    s.parse::<RawFd>().unwrap_or_else(|_| {
        eprintln!("invalid fd number for {label}: {s}");
        std::process::exit(2);
    })
}

fn parse_surface_encoders(value: &str) -> Vec<blit_server::SurfaceEncoderPreference> {
    blit_server::SurfaceEncoderPreference::parse_list(value).unwrap_or_else(|err| {
        eprintln!("invalid BLIT_SURFACE_ENCODERS value: {err}");
        eprintln!("expected comma-separated list of: nvenc-av1, nvenc-h265, nvenc-h264, h265-vaapi, h264-vaapi, av1, h264-software");
        std::process::exit(2);
    })
}

fn parse_config() -> blit_server::Config {
    #[cfg(unix)]
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    #[cfg(windows)]
    let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());

    let mut shell_flags = std::env::var("BLIT_SHELL_FLAGS").unwrap_or_else(|_| {
        if cfg!(unix) {
            "li".into()
        } else {
            String::new()
        }
    });
    let scrollback = std::env::var("BLIT_SCROLLBACK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10_000);
    let mut ipc_path = std::env::var("BLIT_SOCK").ok();
    #[cfg(unix)]
    let mut fd_channel: Option<RawFd> = std::env::var("BLIT_FD_CHANNEL")
        .ok()
        .map(|s| parse_fd_value(&s, "BLIT_FD_CHANNEL"));
    let mut verbose = std::env::var("BLIT_VERBOSE")
        .ok()
        .map(|v| v == "1")
        .unwrap_or(false);
    let surface_encoders = std::env::var("BLIT_SURFACE_ENCODERS")
        .or_else(|_| std::env::var("BLIT_SURFACE_ENCODER"))
        .ok()
        .map(|value| parse_surface_encoders(&value))
        .unwrap_or_else(blit_server::SurfaceEncoderPreference::defaults);
    let surface_quality = std::env::var("BLIT_SURFACE_QUALITY")
        .ok()
        .map(|v| {
            blit_server::SurfaceQuality::parse(&v).unwrap_or_else(|| {
                eprintln!("invalid BLIT_SURFACE_QUALITY value: {v}");
                eprintln!("expected one of: low, medium, high, lossless");
                std::process::exit(2);
            })
        })
        .unwrap_or_default();
    let vaapi_device =
        std::env::var("BLIT_VAAPI_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".into());

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("{}", usage());
            println!("  --socket PATH            IPC socket/pipe path (or set BLIT_SOCK)");
            println!(
                "  --fd-channel FD          Accept clients via fd-passing on FD (Unix only, or set BLIT_FD_CHANNEL)"
            );
            println!(
                "  --shell-flags FLAGS      Shell flags (default: li, or set BLIT_SHELL_FLAGS)"
            );
            println!("  --verbose, -v            Enable verbose logging (or set BLIT_VERBOSE=1)");
            println!(
                "  BLIT_SURFACE_ENCODERS      Comma-separated encoder priority list (default: nvenc-h265,h265-vaapi,nvenc-av1,nvenc-h264,h264-vaapi,h264-software,av1)"
            );
            println!(
                "  BLIT_VAAPI_DEVICE         VA-API render node (default: /dev/dri/renderD128)"
            );
            println!("  BLIT_CUDA_DEVICE          CUDA device ordinal for NVENC (default: 0)");
            println!("  --version, -V            Print version");
            std::process::exit(0);
        }
        if arg == "--version" || arg == "-V" {
            println!("blit-server {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }

        if let Some(value) = arg.strip_prefix("--socket=") {
            ipc_path = Some(value.to_owned());
            continue;
        }

        if arg == "--socket" {
            ipc_path = Some(args.next().unwrap_or_else(|| {
                eprintln!("missing value for --socket");
                eprintln!("{}", usage());
                std::process::exit(2);
            }));
            continue;
        }

        #[cfg(unix)]
        if let Some(value) = arg.strip_prefix("--fd-channel=") {
            fd_channel = Some(parse_fd_value(value, "--fd-channel"));
            continue;
        }

        #[cfg(unix)]
        if arg == "--fd-channel" {
            let value = args.next().unwrap_or_else(|| {
                eprintln!("missing value for --fd-channel");
                eprintln!("{}", usage());
                std::process::exit(2);
            });
            fd_channel = Some(parse_fd_value(&value, "--fd-channel"));
            continue;
        }

        if let Some(value) = arg.strip_prefix("--shell-flags=") {
            shell_flags = value.to_owned();
            continue;
        }

        if arg == "--shell-flags" {
            shell_flags = args.next().unwrap_or_else(|| {
                eprintln!("missing value for --shell-flags");
                eprintln!("{}", usage());
                std::process::exit(2);
            });
            continue;
        }

        if arg == "--verbose" || arg == "-v" {
            verbose = true;
            continue;
        }

        if arg.starts_with('-') {
            eprintln!("unrecognized argument: {arg}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }

        if ipc_path.replace(arg).is_some() {
            eprintln!("multiple socket paths provided");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }

    let max_connections = std::env::var("BLIT_MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let max_ptys = std::env::var("BLIT_MAX_PTYS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);

    blit_server::Config {
        shell,
        shell_flags,
        scrollback,
        ipc_path: ipc_path.unwrap_or_else(blit_server::default_ipc_path),
        surface_encoders,
        surface_quality,
        vaapi_device,
        #[cfg(unix)]
        fd_channel,
        verbose,
        max_connections,
        max_ptys,
    }
}

#[tokio::main]
async fn main() {
    let config = parse_config();
    blit_server::run(config).await;
}
