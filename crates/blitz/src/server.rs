use sha2::{Digest, Sha256};

pub async fn start_embedded(passphrase: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(passphrase.as_bytes());
    hasher.update(b"socket-path");
    let hash: [u8; 32] = hasher.finalize().into();
    let suffix: String = hash[..4].iter().map(|b| format!("{b:02x}")).collect();

    let sock_path = format!(
        "{}/blitz-{}.sock",
        std::env::var("TMPDIR")
            .or_else(|_| std::env::var("XDG_RUNTIME_DIR"))
            .unwrap_or_else(|_| "/tmp".into()),
        suffix,
    );

    let config = blit_server::Config {
        shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
        shell_flags: "li".into(),
        scrollback: 10_000,
        socket_path: sock_path.clone(),
        fd_channel: None,
    };

    let path = sock_path.clone();
    tokio::spawn(async move {
        blit_server::run(config).await;
    });

    for _ in 0..50 {
        if std::path::Path::new(&sock_path).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let cleanup_path = sock_path.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        let _ = std::fs::remove_file(&cleanup_path);
    });

    path
}
