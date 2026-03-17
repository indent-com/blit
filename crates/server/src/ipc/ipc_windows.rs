use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

pub type IpcStream = NamedPipeServer;

pub fn default_ipc_path() -> String {
    if let Ok(user) = std::env::var("USERNAME") {
        format!(r"\\.\pipe\blit-{user}")
    } else {
        r"\\.\pipe\blit".to_string()
    }
}

pub struct IpcListener {
    pipe_name: String,
    current: NamedPipeServer,
}

impl IpcListener {
    pub fn bind(pipe_name: &str, verbose: bool) -> Self {
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(pipe_name)
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
