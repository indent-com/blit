use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

pub enum Transport {
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
    Ssh(tokio::process::Child),
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
) -> Result<Transport, String> {
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
        let bridge = r#"sh -c 'if [ -n "$BLIT_SOCK" ]; then S="$BLIT_SOCK"; elif [ -n "$TMPDIR" ] && [ -S "$TMPDIR/blit.sock" ]; then S="$TMPDIR/blit.sock"; elif [ -S "/tmp/blit-$(id -un).sock" ]; then S="/tmp/blit-$(id -un).sock"; elif [ -S "/run/blit/$(id -un).sock" ]; then S="/run/blit/$(id -un).sock"; elif [ -n "$XDG_RUNTIME_DIR" ] && [ -S "$XDG_RUNTIME_DIR/blit.sock" ]; then S="$XDG_RUNTIME_DIR/blit.sock"; else S=/tmp/blit.sock; fi; exec nc -U "$S" 2>/dev/null || socat - "UNIX-CONNECT:$S"'"#;
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
    Ok(Transport::Unix(
        tokio::net::UnixStream::connect(&path)
            .await
            .map_err(|e| format!("cannot connect to {path}: {e}"))?,
    ))
}
