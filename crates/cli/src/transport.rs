use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

pub enum Transport {
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
    Ssh(tokio::process::Child),
    Share(tokio::io::DuplexStream),
}

impl Transport {
    pub fn split(
        self,
    ) -> (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ) {
        match self {
            Transport::Unix(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
            Transport::Tcp(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
            Transport::Ssh(mut child) => {
                let stdout = child.stdout.take().expect("ssh stdout");
                let stdin = child.stdin.take().expect("ssh stdin");
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                (Box::new(stdout), Box::new(stdin))
            }
            Transport::Share(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
        }
    }
}

pub fn default_local_socket() -> String {
    if let Ok(p) = std::env::var("BLIT_SOCK") {
        return p;
    }
    if let Ok(dir) = std::env::var("TMPDIR") {
        let p = format!("{dir}/blit.sock");
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }
    if let Ok(user) = std::env::var("USER") {
        let p = format!("/tmp/blit-{user}.sock");
        if std::path::Path::new(&p).exists() {
            return p;
        }
        let sys = format!("/run/blit/{user}.sock");
        if std::path::Path::new(&sys).exists() {
            return sys;
        }
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return format!("{dir}/blit.sock");
    }
    "/tmp/blit.sock".into()
}

pub async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> Option<Vec<u8>> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr).await.ok()?;
    let len = u32::from_le_bytes(hdr) as usize;
    if len == 0 {
        return Some(vec![]);
    }
    if len > MAX_FRAME_SIZE {
        return None;
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

pub fn make_frame(payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= u32::MAX as usize);
    let mut v = Vec::with_capacity(4 + payload.len());
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

pub async fn write_frame(w: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> bool {
    w.write_all(&make_frame(payload)).await.is_ok()
}

pub async fn connect(
    socket: &Option<String>,
    tcp: &Option<String>,
    ssh: &Option<String>,
    share: &Option<String>,
    hub: &str,
) -> Result<Transport, String> {
    if let Some(passphrase) = share {
        let hub = blit_webrtc_forwarder::normalize_hub(hub);
        let stream = blit_webrtc_forwarder::client::connect(passphrase, &hub)
            .await
            .map_err(|e| format!("share: {e}"))?;
        return Ok(Transport::Share(stream));
    }

    if let Some(path) = socket {
        return Ok(Transport::Unix(
            tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| format!("cannot connect to {path}: {e}"))?,
        ));
    }

    if let Some(addr) = tcp {
        let s = tokio::net::TcpStream::connect(addr.as_str())
            .await
            .map_err(|e| format!("cannot connect to {addr}: {e}"))?;
        let _ = s.set_nodelay(true);
        return Ok(Transport::Tcp(s));
    }

    if let Some(host) = ssh {
        let bridge = r#"sh -c 'if [ -n "$BLIT_SOCK" ]; then S="$BLIT_SOCK"; elif [ -n "$TMPDIR" ] && [ -S "$TMPDIR/blit.sock" ]; then S="$TMPDIR/blit.sock"; elif [ -S "/tmp/blit-$(id -un).sock" ]; then S="/tmp/blit-$(id -un).sock"; elif [ -S "/run/blit/$(id -un).sock" ]; then S="/run/blit/$(id -un).sock"; elif [ -n "$XDG_RUNTIME_DIR" ] && [ -S "$XDG_RUNTIME_DIR/blit.sock" ]; then S="$XDG_RUNTIME_DIR/blit.sock"; else S=/tmp/blit.sock; fi; if ! [ -S "$S" ]; then if command -v blit >/dev/null 2>&1; then blit server &  i=0; while ! [ -S "$S" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done; elif command -v blit-server >/dev/null 2>&1; then blit-server & i=0; while ! [ -S "$S" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done; fi; fi; exec nc -U "$S" 2>/dev/null || socat - "UNIX-CONNECT:$S"'"#;
        let child = tokio::process::Command::new("ssh")
            .arg("-T")
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPath=/tmp/blit-ssh-%r@%h:%p")
            .arg("-o")
            .arg("ControlPersist=300")
            .arg(host)
            .arg("--")
            .arg(bridge)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("ssh: {e}"))?;
        return Ok(Transport::Ssh(child));
    }

    let path = default_local_socket();
    ensure_local_server(&path).await?;
    Ok(Transport::Unix(
        tokio::net::UnixStream::connect(&path)
            .await
            .map_err(|e| format!("cannot connect to {path}: {e}"))?,
    ))
}

/// Start a local blit-server if the socket doesn't exist yet.
///
/// If the socket file exists but no server is listening (stale socket from a
/// crashed process), the file is removed and a fresh server is spawned.
pub async fn ensure_local_server(socket_path: &str) -> Result<(), String> {
    if std::path::Path::new(socket_path).exists() {
        match tokio::net::UnixStream::connect(socket_path).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                let _ = std::fs::remove_file(socket_path);
            }
        }
    }
    let config = blit_server::Config {
        shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
        shell_flags: std::env::var("BLIT_SHELL_FLAGS").unwrap_or_else(|_| "li".into()),
        scrollback: std::env::var("BLIT_SCROLLBACK")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000),
        socket_path: socket_path.to_string(),
        fd_channel: None,
        verbose: false,
    };
    tokio::spawn(blit_server::run(config));
    for _ in 0..100 {
        if std::path::Path::new(socket_path).exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err("server did not create socket in time".into())
}
