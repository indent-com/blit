mod app;
mod atlas;
#[allow(dead_code)]
mod bsp;
mod connection;
mod input;
mod overlay;
mod palette;
#[allow(dead_code)]
mod remotes;
mod renderer;
mod statusbar;
mod terminal;
mod transport;

use std::path::PathBuf;

use clap::Parser;
use winit::event_loop::{ControlFlow, EventLoop};

use crate::app::App;
use crate::remotes::load_remotes;

#[derive(Parser)]
#[command(name = "blit-desktop", about = "blit native desktop terminal")]
struct Cli {
    #[arg(long, default_value = "wss://blit.dev")]
    hub: String,

    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let _guard = rt.enter();

    let cli = Cli::parse();
    let remotes = load_remotes(cli.config.as_deref());

    let event_loop = EventLoop::new().expect("failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::new(remotes, cli.hub);
    event_loop.run_app(&mut app).expect("event loop error");
}
