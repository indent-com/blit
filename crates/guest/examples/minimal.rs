fn extension(mut client: blit_guest::Client) -> Result<(), blit_guest::Error> {
    let now = client.monotonic_now();
    let _ = client.wait_until(now)?;
    let mut nonce = [0; 16];
    client.random(&mut nonce)?;
    // C2S_PING; use `blit_guest::remote::C2S_PING` with the default protocol feature.
    client.send(&[0x08])
}

blit_guest::entry!(extension);

// Cargo examples are binaries. Wasmi invokes only the separately exported
// `blit_main`; this empty Rust binary entry is not exported from the module.
fn main() {}
