use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

pub enum Transport {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    NamedPipe(tokio::net::windows::named_pipe::NamedPipeClient),
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
            #[cfg(unix)]
            Transport::Unix(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
            #[cfg(windows)]
            Transport::NamedPipe(s) => {
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

#[cfg(unix)]
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

#[cfg(windows)]
pub fn default_local_socket() -> String {
    if let Ok(p) = std::env::var("BLIT_SOCK") {
        return p;
    }
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
    format!(r"\\.\pipe\blit-{user}")
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

pub fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\"'\"'");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

pub const SSH_AUTOSTART: &str = "if ! [ -S \"$S\" ]; then if command -v blit >/dev/null 2>&1; then blit server & i=0; while ! [ -S \"$S\" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done; elif command -v blit-server >/dev/null 2>&1; then blit-server & i=0; while ! [ -S \"$S\" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done; fi; fi";

pub const SSH_SOCK_SEARCH: &str = r#"if [ -n "$BLIT_SOCK" ]; then S="$BLIT_SOCK"; elif [ -n "$TMPDIR" ] && [ -S "$TMPDIR/blit.sock" ]; then S="$TMPDIR/blit.sock"; elif [ -S "/tmp/blit-$(id -un).sock" ]; then S="/tmp/blit-$(id -un).sock"; elif [ -S "/run/blit/$(id -un).sock" ]; then S="/run/blit/$(id -un).sock"; elif [ -n "$XDG_RUNTIME_DIR" ] && [ -S "$XDG_RUNTIME_DIR/blit.sock" ]; then S="$XDG_RUNTIME_DIR/blit.sock"; else S=/tmp/blit.sock; fi"#;

pub fn ssh_bridge_script(remote_socket: Option<&str>) -> String {
    let resolve = match remote_socket {
        Some(path) => format!("S={}", shell_escape(path)),
        None => SSH_SOCK_SEARCH.to_string(),
    };
    format!(
        "sh -c '{resolve}; {SSH_AUTOSTART}; if command -v nc >/dev/null 2>&1; then exec nc -U \"$S\"; else exec socat - \"UNIX-CONNECT:$S\"; fi'"
    )
}

pub async fn connect_ssh(host: &str, remote_socket: Option<&str>) -> Result<Transport, String> {
    let bridge = ssh_bridge_script(remote_socket);
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
    Ok(Transport::Ssh(child))
}

pub async fn connect_ipc(path: &str) -> Result<Transport, String> {
    #[cfg(unix)]
    {
        Ok(Transport::Unix(
            tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| format!("cannot connect to {path}: {e}"))?,
        ))
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        Ok(Transport::NamedPipe(
            ClientOptions::new()
                .open(path)
                .map_err(|e| format!("cannot connect to {path}: {e}"))?,
        ))
    }
}

pub async fn connect(
    socket: &Option<String>,
    tcp: &Option<String>,
    ssh: &Option<String>,
    passphrase: &Option<String>,
    hub: &str,
) -> Result<Transport, String> {
    if let Some(passphrase) = passphrase {
        let hub = blit_webrtc_forwarder::normalize_hub(hub);
        let stream = blit_webrtc_forwarder::client::connect(passphrase, &hub)
            .await
            .map_err(|e| format!("passphrase: {e}"))?;
        return Ok(Transport::Share(stream));
    }

    if let Some(host) = ssh {
        return connect_ssh(host, socket.as_deref()).await;
    }

    if let Some(path) = socket {
        return connect_ipc(path).await;
    }

    if let Some(addr) = tcp {
        let s = tokio::net::TcpStream::connect(addr.as_str())
            .await
            .map_err(|e| format!("cannot connect to {addr}: {e}"))?;
        let _ = s.set_nodelay(true);
        return Ok(Transport::Tcp(s));
    }

    let path = default_local_socket();
    ensure_local_server(&path).await?;
    connect_ipc(&path).await
}

#[cfg(unix)]
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
        ipc_path: socket_path.to_string(),
        #[cfg(unix)]
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

#[cfg(windows)]
pub async fn ensure_local_server(pipe_path: &str) -> Result<(), String> {
    if connect_ipc(pipe_path).await.is_ok() {
        return Ok(());
    }
    let config = blit_server::Config {
        shell: std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
        shell_flags: String::new(),
        scrollback: std::env::var("BLIT_SCROLLBACK")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000),
        ipc_path: pipe_path.to_string(),
        verbose: false,
    };
    tokio::spawn(blit_server::run(config));
    for _ in 0..100 {
        if connect_ipc(pipe_path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err("server did not create pipe in time".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    // ── make_frame ──

    #[test]
    fn make_frame_empty_payload() {
        let frame = make_frame(&[]);
        assert_eq!(frame, vec![0, 0, 0, 0]);
    }

    #[test]
    fn make_frame_known_payload() {
        let frame = make_frame(b"hello");
        assert_eq!(frame.len(), 9);
        assert_eq!(&frame[0..4], &5u32.to_le_bytes());
        assert_eq!(&frame[4..], b"hello");
    }

    #[test]
    fn make_frame_single_byte() {
        let frame = make_frame(&[0xff]);
        assert_eq!(&frame[0..4], &1u32.to_le_bytes());
        assert_eq!(frame[4], 0xff);
    }

    // ── read_frame + make_frame round-trip ──

    #[tokio::test]
    async fn read_frame_round_trip() {
        let payload = b"blit protocol test";
        let frame = make_frame(payload);
        let mut cursor = std::io::Cursor::new(frame);
        let result = read_frame(&mut cursor).await.unwrap();
        assert_eq!(result, payload);
    }

    #[tokio::test]
    async fn read_frame_empty_payload() {
        let frame = make_frame(&[]);
        let mut cursor = std::io::Cursor::new(frame);
        let result = read_frame(&mut cursor).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized() {
        let len = (MAX_FRAME_SIZE as u32 + 1).to_le_bytes();
        let mut cursor = std::io::Cursor::new(len.to_vec());
        let result = read_frame(&mut cursor).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn read_frame_eof_during_header() {
        let mut cursor = std::io::Cursor::new(vec![0x01, 0x00]);
        let result = read_frame(&mut cursor).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn read_frame_eof_during_body() {
        let mut data = 10u32.to_le_bytes().to_vec();
        data.extend_from_slice(b"short");
        let mut cursor = std::io::Cursor::new(data);
        let result = read_frame(&mut cursor).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn read_frame_multiple_frames() {
        let mut data = make_frame(b"first");
        data.extend_from_slice(&make_frame(b"second"));
        let mut cursor = std::io::Cursor::new(data);
        let f1 = read_frame(&mut cursor).await.unwrap();
        let f2 = read_frame(&mut cursor).await.unwrap();
        assert_eq!(f1, b"first");
        assert_eq!(f2, b"second");
    }

    #[tokio::test]
    async fn write_frame_round_trip() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let payload = b"write-test";
        let ok = write_frame(&mut client, payload).await;
        assert!(ok);
        drop(client);
        let result = read_frame(&mut server).await.unwrap();
        assert_eq!(result, payload);
    }

    // ── shell_escape ──

    #[test]
    fn shell_escape_plain() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_single_quote() {
        assert_eq!(shell_escape("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn shell_escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_spaces_and_special_chars() {
        assert_eq!(shell_escape("a b"), "'a b'");
        assert_eq!(shell_escape("a;b"), "'a;b'");
    }

    #[test]
    fn shell_escape_multiple_quotes() {
        let escaped = shell_escape("a'b'c");
        assert_eq!(escaped, "'a'\"'\"'b'\"'\"'c'");
    }

    #[test]
    fn shell_escape_unicode() {
        assert_eq!(shell_escape("日本語"), "'日本語'");
    }

    // ── ssh_bridge_script ──

    #[test]
    fn ssh_bridge_script_no_socket() {
        let script = ssh_bridge_script(None);
        assert!(script.contains(SSH_SOCK_SEARCH));
        assert!(script.contains(SSH_AUTOSTART));
        assert!(script.contains("nc -U"));
    }

    #[test]
    fn ssh_bridge_script_custom_socket() {
        let script = ssh_bridge_script(Some("/tmp/custom.sock"));
        assert!(script.contains("S='/tmp/custom.sock'"));
        assert!(!script.contains(SSH_SOCK_SEARCH));
    }

    #[test]
    fn ssh_bridge_script_socket_with_quotes() {
        let script = ssh_bridge_script(Some("/tmp/it's.sock"));
        assert!(script.contains("'\"'\"'"));
    }
}
