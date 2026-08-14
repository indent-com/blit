use blit_guest::terminal::{Error, UpdateOutcome};

fn extension(mut client: blit_guest::Client) -> Result<(), Error> {
    let pty_id = client
        .context()
        .args
        .first()
        .and_then(|argument| argument.parse::<u16>().ok())
        .unwrap_or(1);
    let mut terminals = client.terminal_subscriptions();
    terminals.subscribe(&mut client, pty_id, 24, 80)?;

    // `next_update` deliberately sends no ACK. Applying the logical update
    // advances TerminalState first and sends exactly one ACK afterward.
    let update = terminals.next_update(&mut client)?;
    if let UpdateOutcome::Applied { .. } = terminals.apply_update(&mut client, update)? {
        let state = terminals
            .subscription(pty_id)
            .expect("the subscription remains registered")
            .state();
        let _snapshot = state.get_all_text();
    }
    Ok(())
}

blit_guest::entry!(extension);

// Cargo examples are binaries; Wasmi calls the exported `blit_main`.
fn main() {}
