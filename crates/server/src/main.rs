use std::os::unix::io::RawFd;

fn usage() -> &'static str {
    "usage: blit-server [--socket PATH] [--fd-channel FD] [--shell-flags FLAGS] [PATH]"
}

fn parse_fd_value(s: &str, label: &str) -> RawFd {
    s.parse::<RawFd>().unwrap_or_else(|_| {
        eprintln!("invalid fd number for {label}: {s}");
        std::process::exit(2);
    })
}

fn parse_config() -> blit_server::Config {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut shell_flags = std::env::var("BLIT_SHELL_FLAGS").unwrap_or_else(|_| "li".into());
    let scrollback = std::env::var("BLIT_SCROLLBACK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10_000);
    let mut socket_path = std::env::var("BLIT_SOCK").ok();
    let mut fd_channel: Option<RawFd> = std::env::var("BLIT_FD_CHANNEL")
        .ok()
        .map(|s| parse_fd_value(&s, "BLIT_FD_CHANNEL"));

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("{}", usage());
            println!("  --socket PATH            Unix socket path (or set BLIT_SOCK)");
            println!("  --fd-channel FD          Accept clients via fd-passing on FD (or set BLIT_FD_CHANNEL)");
            println!(
                "  --shell-flags FLAGS      Shell flags (default: li, or set BLIT_SHELL_FLAGS)"
            );
            println!("  --version, -V            Print version");
            std::process::exit(0);
        }
        if arg == "--version" || arg == "-V" {
            println!("blit-server {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }

        if let Some(value) = arg.strip_prefix("--socket=") {
            socket_path = Some(value.to_owned());
            continue;
        }

        if arg == "--socket" {
            socket_path = Some(args.next().unwrap_or_else(|| {
                eprintln!("missing value for --socket");
                eprintln!("{}", usage());
                std::process::exit(2);
            }));
            continue;
        }

        if let Some(value) = arg.strip_prefix("--fd-channel=") {
            fd_channel = Some(parse_fd_value(value, "--fd-channel"));
            continue;
        }

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

        if arg.starts_with('-') {
            eprintln!("unrecognized argument: {arg}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }

        if socket_path.replace(arg).is_some() {
            eprintln!("multiple socket paths provided");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }

    blit_server::Config {
        shell,
        shell_flags,
        scrollback,
        socket_path: socket_path.unwrap_or_else(blit_server::default_socket_path),
        fd_channel,
    }
}

#[tokio::main]
async fn main() {
    let config = parse_config();
    blit_server::run(config).await;
}
