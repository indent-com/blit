/// blit-proxy standalone binary — thin wrapper around the `blit-proxy` library.
///
/// All logic lives in `lib.rs`; this file only handles CLI argument parsing
/// and delegates to [`blit_proxy::run`].
fn main() {
    let mut verbose = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "blit-proxy {} — multi-target connection pool and transparent proxy",
                    env!("CARGO_PKG_VERSION")
                );
                println!();
                println!("A single persistent process.  Each connecting client declares its");
                println!("upstream target via a one-line handshake:");
                println!();
                println!("  target <uri>\\n  →  ok\\n  (then blit protocol begins)");
                println!("                   or  error <msg>\\n");
                println!();
                println!("Options:");
                println!("  -v, --verbose    Log connections and lifecycle events to stderr");
                println!();
                println!("Configuration is via environment variables:");
                println!("  BLIT_PROXY_SOCK   Downstream listen socket");
                println!("                   (default: $XDG_RUNTIME_DIR/blit-proxy.sock)");
                println!("  BLIT_PROXY_POOL  Pre-warmed idle connections per target (default: 4)");
                println!("  BLIT_PROXY_IDLE  Total idle seconds before self-exit (default: never)");
                println!();
                println!("Upstream URI formats:");
                println!("  socket:/path/to/blit.sock");
                println!("  tcp:host:port");
                println!("  ws://host:port/?passphrase=secret");
                println!("  wss://host:port/?passphrase=secret");
                println!("  wt://host:port/?passphrase=secret&certHash=aabbcc\u{2026}");
                println!("  share:passphrase");
                println!("  share:passphrase?hub=wss://custom.hub.example.com");
                println!("  ssh:host  or  ssh:user@host  or  ssh:host:/path/to/socket");
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("blit-proxy {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--verbose" | "-v" => {
                verbose = true;
            }
            other => {
                eprintln!("blit-proxy: unknown argument: {other}");
                std::process::exit(1);
            }
        }
    }

    blit_proxy::run(verbose);
}
