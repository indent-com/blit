use std::io;
use std::time::Duration;

use blit_remote::C2S_QUIT;
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

pub type IpcStream = NamedPipeServer;

pub fn default_ipc_path() -> String {
    default_ipc_path_for(&crate::ServerName::default())
}

pub fn default_ipc_path_for(name: &crate::ServerName) -> String {
    if let Ok(user) = std::env::var("USERNAME") {
        format!(r"\\.\pipe\blit-{user}-{}", name.as_str())
    } else {
        format!(r"\\.\pipe\blit-{}", name.as_str())
    }
}

pub struct IpcListener {
    pipe_name: String,
    current: NamedPipeServer,
}

impl IpcListener {
    pub async fn bind(pipe_name: &str, verbose: bool) -> Self {
        let server = bind_replacing_existing(pipe_name)
            .await
            .unwrap_or_else(|e| {
                eprintln!("blit-server: cannot create named pipe {pipe_name}: {e}");
                std::process::exit(1);
            });
        if verbose {
            eprintln!("listening on {pipe_name}");
        }
        Self {
            pipe_name: pipe_name.to_string(),
            current: server,
        }
    }

    pub async fn accept(&mut self) -> std::io::Result<IpcStream> {
        self.current.connect().await?;
        let connected = std::mem::replace(
            &mut self.current,
            ServerOptions::new().create(&self.pipe_name)?,
        );
        Ok(connected)
    }
}

/// Claim the pipe name, gracefully stopping the previous server if one owns it.
///
/// `FILE_FLAG_FIRST_PIPE_INSTANCE` reports an existing instance as
/// `PermissionDenied` (`ERROR_ACCESS_DENIED`). Ordinary CLI commands auto-start
/// a detached server, so an explicit `blit server` commonly encounters one.
/// Match the Unix listener's replacement semantics by asking that server to
/// shut down, then waiting for Windows to release the pipe name.
async fn bind_replacing_existing(pipe_name: &str) -> io::Result<NamedPipeServer> {
    const REPLACE_ATTEMPTS: usize = 30;
    const REPLACE_INTERVAL: Duration = Duration::from_millis(100);

    let mut shutdown_requested = false;

    for attempt in 0..=REPLACE_ATTEMPTS {
        let in_use = match ServerOptions::new()
            .first_pipe_instance(true)
            .create(pipe_name)
        {
            Ok(server) => return Ok(server),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => error,
            Err(error) => return Err(error),
        };
        if attempt == REPLACE_ATTEMPTS {
            return Err(in_use);
        }

        if !shutdown_requested && let Ok(mut previous) = ClientOptions::new().open(pipe_name) {
            let frame = [1, 0, 0, 0, C2S_QUIT];
            if previous.write_all(&frame).await.is_ok() {
                eprintln!("blit-server: requesting previous server shutdown");
                shutdown_requested = true;
            }
        }

        tokio::time::sleep(REPLACE_INTERVAL).await;
    }

    unreachable!("replacement loop always returns on its final attempt")
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test]
    async fn bind_replaces_an_existing_server() {
        let pipe_name = format!(r"\\.\pipe\blit-test-replace-{}", std::process::id());
        let mut previous = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .unwrap();

        let previous_task = tokio::spawn(async move {
            previous.connect().await.unwrap();
            let mut frame = [0; 5];
            previous.read_exact(&mut frame).await.unwrap();
            assert_eq!(frame, [1, 0, 0, 0, C2S_QUIT]);
        });

        let replacement = bind_replacing_existing(&pipe_name).await.unwrap();
        previous_task.await.unwrap();
        drop(replacement);
    }
}
