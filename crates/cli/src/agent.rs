use std::collections::HashMap;

use blit_remote::{
    C2S_SURFACE_CAPTURE, C2S_SURFACE_INPUT, C2S_SURFACE_LIST, C2S_SURFACE_POINTER,
    EXIT_STATUS_UNKNOWN, S2C_EXITED, S2C_HELLO, S2C_LIST, S2C_READY, S2C_SURFACE_CAPTURE,
    S2C_SURFACE_LIST, S2C_TEXT, S2C_TITLE, S2C_UPDATE, ServerMsg, TerminalState, msg_ack,
    msg_close, msg_create2, msg_input, msg_kill, msg_read, msg_resize, msg_restart, msg_subscribe,
    parse_server_msg,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::transport::{Transport, read_frame, write_frame};

struct PtyInfo {
    id: u16,
    tag: String,
    command: String,
}

struct AgentConn {
    reader: Box<dyn AsyncRead + Unpin + Send>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    ptys: Vec<PtyInfo>,
    titles: HashMap<u16, String>,
    exited: HashMap<u16, i32>,
}

impl AgentConn {
    async fn connect(transport: Transport) -> Result<Self, String> {
        let (mut reader, writer) = transport.split();

        let mut ptys = Vec::new();
        let mut titles = HashMap::new();
        let mut exited = HashMap::new();

        loop {
            let data =
                tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut reader))
                    .await
                    .map_err(|_| "timeout waiting for server".to_string())?
                    .ok_or_else(|| "server closed connection".to_string())?;

            if data.is_empty() {
                continue;
            }

            match data[0] {
                S2C_READY => break,
                S2C_HELLO => {}
                S2C_LIST => {
                    if let Some(ServerMsg::List { entries }) = parse_server_msg(&data) {
                        ptys = entries
                            .into_iter()
                            .map(|e| PtyInfo {
                                id: e.pty_id,
                                tag: e.tag.to_string(),
                                command: e.command.to_string(),
                            })
                            .collect();
                    }
                }
                S2C_TITLE => {
                    if let Some(ServerMsg::Title { pty_id, title }) = parse_server_msg(&data)
                        && let Ok(t) = std::str::from_utf8(title)
                    {
                        titles.insert(pty_id, t.to_string());
                    }
                }
                S2C_EXITED => {
                    if let Some(ServerMsg::Exited {
                        pty_id,
                        exit_status,
                    }) = parse_server_msg(&data)
                    {
                        exited.insert(pty_id, exit_status);
                    }
                }
                _ => {}
            }
        }

        Ok(Self {
            reader,
            writer,
            ptys,
            titles,
            exited,
        })
    }

    async fn send(&mut self, msg: &[u8]) -> Result<(), String> {
        write_frame(&mut self.writer, msg)
            .await
            .then_some(())
            .ok_or_else(|| "failed to send message".to_string())
    }

    async fn recv(&mut self) -> Result<Vec<u8>, String> {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_frame(&mut self.reader),
        )
        .await
        .map_err(|_| "timeout waiting for server response".to_string())?
        .ok_or_else(|| "server closed connection".to_string())
    }

    fn has_pty(&self, id: u16) -> bool {
        self.ptys.iter().any(|p| p.id == id)
    }

    async fn recv_deadline(&mut self, deadline: tokio::time::Instant) -> Result<Vec<u8>, String> {
        tokio::time::timeout_at(deadline, read_frame(&mut self.reader))
            .await
            .map_err(|_| "timeout".to_string())?
            .ok_or_else(|| "server closed connection".to_string())
    }

    async fn maybe_resize(&mut self, id: u16, size: Option<(u16, u16)>) -> Result<(), String> {
        if let Some((rows, cols)) = size {
            self.send(&msg_resize(id, rows, cols)).await?;
        }
        Ok(())
    }
}

pub async fn cmd_list(transport: Transport) -> Result<(), String> {
    let conn = AgentConn::connect(transport).await?;

    println!("ID\tTAG\tTITLE\tCOMMAND\tSTATUS");
    for pty in &conn.ptys {
        let title = conn.titles.get(&pty.id).map(|s| s.as_str()).unwrap_or("");
        let status = match conn.exited.get(&pty.id) {
            None => "running".to_string(),
            Some(&s) => format_exit_status(s),
        };
        println!(
            "{}\t{}\t{}\t{}\t{}",
            pty.id, pty.tag, title, pty.command, status
        );
    }
    Ok(())
}

pub async fn cmd_start(
    transport: Transport,
    tag: Option<String>,
    command: Vec<String>,
    rows: u16,
    cols: u16,
) -> Result<u16, String> {
    let mut conn = AgentConn::connect(transport).await?;

    let nonce: u16 = 1;
    let tag_str = tag.as_deref().unwrap_or("");
    let cmd_str = command.join("\0");
    let msg = msg_create2(nonce, rows, cols, tag_str, &cmd_str, 0);
    conn.send(&msg).await?;

    loop {
        let data = conn.recv().await?;
        if data.is_empty() {
            continue;
        }
        if let Some(ServerMsg::CreatedN {
            nonce: n, pty_id, ..
        }) = parse_server_msg(&data)
            && n == nonce
        {
            println!("{pty_id}");
            return Ok(pty_id);
        }
    }
}

pub fn capture_size(rows: Option<u16>, cols: Option<u16>) -> Option<(u16, u16)> {
    if rows.is_some() || cols.is_some() {
        Some((rows.unwrap_or(24), cols.unwrap_or(80)))
    } else {
        None
    }
}

pub async fn cmd_show(
    transport: Transport,
    id: u16,
    ansi: bool,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

    conn.maybe_resize(id, capture_size(rows, cols)).await?;

    conn.send(&msg_subscribe(id)).await?;

    let mut state = TerminalState::new(0, 0);

    loop {
        let data = conn.recv().await?;
        if data.is_empty() {
            continue;
        }
        if data[0] == S2C_UPDATE && data.len() >= 3 {
            let pid = u16::from_le_bytes([data[1], data[2]]);
            if pid == id {
                state.feed_compressed(&data[3..]);
                conn.send(&msg_ack()).await?;
                let text = if ansi {
                    state.get_ansi_text()
                } else {
                    state.get_all_text()
                };
                print!("{text}");
                return Ok(());
            }
        }
    }
}

pub async fn cmd_history(
    transport: Transport,
    id: u16,
    from_start: Option<u32>,
    from_end: Option<u32>,
    limit: Option<u32>,
    ansi: bool,
    size: Option<(u16, u16)>,
) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

    conn.maybe_resize(id, size).await?;

    let mut flags: u8 = 0;
    if ansi {
        flags |= blit_remote::READ_ANSI;
    }
    let offset = if let Some(n) = from_end {
        flags |= blit_remote::READ_TAIL;
        n
    } else {
        from_start.unwrap_or(0)
    };
    let nonce: u16 = 1;
    conn.send(&msg_read(nonce, id, offset, limit.unwrap_or(0), flags))
        .await?;

    loop {
        let data = conn.recv().await?;
        if data.is_empty() {
            continue;
        }
        if data[0] == S2C_TEXT
            && let Some(ServerMsg::Text { nonce: n, text, .. }) = parse_server_msg(&data)
            && n == nonce
        {
            if !text.is_empty() {
                println!("{text}");
            }
            return Ok(());
        }
    }
}

pub async fn cmd_send(transport: Transport, id: u16, text: String) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

    if conn.exited.contains_key(&id) {
        return Err(format!("pty {id} has exited"));
    }

    let bytes = parse_escapes(&text);
    conn.send(&msg_input(id, &bytes)).await?;
    Ok(())
}

pub async fn cmd_close(transport: Transport, id: u16) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

    conn.send(&msg_close(id)).await?;

    loop {
        let data = conn.recv().await?;
        if data.is_empty() {
            continue;
        }
        if let Some(ServerMsg::Closed { pty_id }) = parse_server_msg(&data)
            && pty_id == id
        {
            return Ok(());
        }
    }
}

pub async fn cmd_kill(transport: Transport, id: u16, signal: &str) -> Result<(), String> {
    let signum = parse_signal(signal)?;
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

    if conn.exited.contains_key(&id) {
        return Err(format!("pty {id} has already exited"));
    }

    conn.send(&msg_kill(id, signum)).await?;
    Ok(())
}

fn parse_signal(s: &str) -> Result<i32, String> {
    if let Ok(n) = s.parse::<i32>() {
        return Ok(n);
    }
    let name = s.strip_prefix("SIG").unwrap_or(s);
    match name {
        "HUP" => Ok(1),
        "INT" => Ok(2),
        "QUIT" => Ok(3),
        "KILL" => Ok(9),
        "USR1" => Ok(10),
        "USR2" => Ok(12),
        "TERM" => Ok(15),
        "CONT" => Ok(18),
        "STOP" => Ok(19),
        _ => Err(format!("unknown signal: {s}")),
    }
}

pub async fn cmd_restart(transport: Transport, id: u16) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

    if !conn.exited.contains_key(&id) {
        return Err(format!("pty {id} is still running"));
    }

    conn.send(&msg_restart(id)).await?;

    loop {
        let data = conn.recv().await?;
        if data.is_empty() {
            continue;
        }
        if let Some(ServerMsg::Created { pty_id, .. }) = parse_server_msg(&data)
            && pty_id == id
        {
            return Ok(());
        }
    }
}

fn format_exit_status(status: i32) -> String {
    if status == EXIT_STATUS_UNKNOWN {
        "exited".to_string()
    } else if status >= 0 {
        format!("exited({})", status)
    } else {
        format!("signal({})", -status)
    }
}

fn exit_code_from_status(status: i32) -> i32 {
    if status == EXIT_STATUS_UNKNOWN {
        1
    } else if status >= 0 {
        status
    } else {
        128 + (-status)
    }
}

pub async fn cmd_wait(
    transport: Transport,
    id: u16,
    timeout_secs: u64,
    pattern: Option<String>,
) -> Result<i32, String> {
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    if let Some(ref pat) = pattern {
        let re = regex::Regex::new(pat).map_err(|e| format!("invalid pattern: {e}"))?;

        conn.send(&msg_subscribe(id)).await?;

        let mut state = TerminalState::new(0, 0);

        loop {
            let data = match conn.recv_deadline(deadline).await {
                Ok(d) => d,
                Err(e) if e == "timeout" => {
                    eprintln!("blit: timed out waiting for pty {id}");
                    return Ok(124);
                }
                Err(e) => return Err(e),
            };
            if data.is_empty() {
                continue;
            }
            match data[0] {
                S2C_UPDATE if data.len() >= 3 => {
                    let pid = u16::from_le_bytes([data[1], data[2]]);
                    if pid == id {
                        state.feed_compressed(&data[3..]);
                        conn.send(&msg_ack()).await?;
                        let text = state.get_all_text();
                        for line in text.lines() {
                            if re.is_match(line) {
                                println!("{line}");
                                return Ok(0);
                            }
                        }
                        if let Some(&status) = conn.exited.get(&id) {
                            let code = exit_code_from_status(status);
                            println!("{}", format_exit_status(status));
                            return Ok(code);
                        }
                    }
                }
                S2C_EXITED => {
                    if let Some(ServerMsg::Exited {
                        pty_id,
                        exit_status,
                    }) = parse_server_msg(&data)
                        && pty_id == id
                    {
                        let code = exit_code_from_status(exit_status);
                        println!("{}", format_exit_status(exit_status));
                        return Ok(code);
                    }
                }
                _ => {}
            }
        }
    } else {
        if let Some(&status) = conn.exited.get(&id) {
            let code = exit_code_from_status(status);
            println!("{}", format_exit_status(status));
            return Ok(code);
        }

        loop {
            let data = match conn.recv_deadline(deadline).await {
                Ok(d) => d,
                Err(e) if e == "timeout" => {
                    eprintln!("blit: timed out waiting for pty {id}");
                    return Ok(124);
                }
                Err(e) => return Err(e),
            };
            if data.is_empty() {
                continue;
            }
            if let Some(ServerMsg::Exited {
                pty_id,
                exit_status,
            }) = parse_server_msg(&data)
                && pty_id == id
            {
                let code = exit_code_from_status(exit_status);
                println!("{}", format_exit_status(exit_status));
                return Ok(code);
            }
        }
    }
}

pub fn parse_escapes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                b'0' => {
                    out.push(0);
                    i += 2;
                }
                b'x' if i + 3 < bytes.len() => {
                    let hi = hex_digit(bytes[i + 2]);
                    let lo = hex_digit(bytes[i + 3]);
                    if let (Some(h), Some(l)) = (hi, lo) {
                        out.push(h << 4 | l);
                        i += 4;
                    } else {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub async fn cmd_surfaces(transport: Transport) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    let mut msg = vec![C2S_SURFACE_LIST];
    msg.extend_from_slice(&0u16.to_le_bytes());
    conn.send(&msg).await?;

    loop {
        let data = conn.recv().await?;
        if data.is_empty() {
            continue;
        }
        if data[0] == S2C_SURFACE_LIST {
            if let Some(ServerMsg::SurfaceList { entries }) = parse_server_msg(&data) {
                println!("ID\tTITLE\tSIZE\tAPP_ID");
                for e in &entries {
                    println!(
                        "{}\t{}\t{}x{}\t{}",
                        e.surface_id, e.title, e.width, e.height, e.app_id
                    );
                }
            }
            return Ok(());
        }
    }
}

pub async fn cmd_capture(
    transport: Transport,
    id: u16,
    output: Option<String>,
) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    let mut msg = vec![C2S_SURFACE_CAPTURE];
    msg.extend_from_slice(&0u16.to_le_bytes());
    msg.extend_from_slice(&id.to_le_bytes());
    conn.send(&msg).await?;

    loop {
        let data = conn.recv().await?;
        if data.is_empty() {
            continue;
        }
        if data[0] == S2C_SURFACE_CAPTURE {
            if let Some(ServerMsg::SurfaceCapture {
                width,
                height,
                png_data,
                ..
            }) = parse_server_msg(&data)
            {
                if width == 0 && height == 0 {
                    return Err(format!("surface {id} not found or has no buffer"));
                }
                let path = output.unwrap_or_else(|| format!("surface-{id}.png"));
                std::fs::write(&path, png_data).map_err(|e| format!("write {path}: {e}"))?;
                println!("{path}");
            }
            return Ok(());
        }
    }
}

pub async fn cmd_click(transport: Transport, id: u16, x: u16, y: u16) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    for ptype in [0u8, 1u8] {
        let mut msg = vec![C2S_SURFACE_POINTER];
        msg.extend_from_slice(&0u16.to_le_bytes());
        msg.extend_from_slice(&id.to_le_bytes());
        msg.push(ptype);
        msg.push(0);
        msg.extend_from_slice(&x.to_le_bytes());
        msg.extend_from_slice(&y.to_le_bytes());
        conn.send(&msg).await?;
    }
    Ok(())
}

pub async fn cmd_key(transport: Transport, id: u16, key: &str) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;
    let events = parse_key_combo(key)?;
    for (keycode, pressed) in &events {
        send_key_event(&mut conn, id, *keycode, *pressed).await?;
    }
    Ok(())
}

pub async fn cmd_type(transport: Transport, id: u16, text: &str) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;
    let events = parse_type_string(text)?;
    for (keycode, pressed) in &events {
        send_key_event(&mut conn, id, *keycode, *pressed).await?;
    }
    Ok(())
}

async fn send_key_event(
    conn: &mut AgentConn,
    surface_id: u16,
    keycode: u32,
    pressed: bool,
) -> Result<(), String> {
    let mut msg = vec![C2S_SURFACE_INPUT];
    msg.extend_from_slice(&0u16.to_le_bytes());
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(&keycode.to_le_bytes());
    msg.push(if pressed { 1 } else { 0 });
    conn.send(&msg).await
}

fn parse_key_combo(key: &str) -> Result<Vec<(u32, bool)>, String> {
    let parts: Vec<&str> = key.split('+').collect();
    let mut modifiers = Vec::new();
    let main_key = parts.last().ok_or("empty key")?;

    for &part in &parts[..parts.len() - 1] {
        let kc = modifier_keycode(part).ok_or_else(|| format!("unknown modifier: {part}"))?;
        modifiers.push(kc);
    }

    let main_kc =
        key_name_to_keycode(main_key).ok_or_else(|| format!("unknown key: {main_key}"))?;

    let mut events = Vec::new();
    for &kc in &modifiers {
        events.push((kc, true));
    }
    events.push((main_kc, true));
    events.push((main_kc, false));
    for &kc in modifiers.iter().rev() {
        events.push((kc, false));
    }
    Ok(events)
}

fn parse_type_string(text: &str) -> Result<Vec<(u32, bool)>, String> {
    let mut events = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            let end = chars[i..]
                .iter()
                .position(|&c| c == '}')
                .ok_or("unclosed { in type string")?;
            let inner: String = chars[i + 1..i + end].iter().collect();
            let combo = parse_key_combo(&inner)?;
            events.extend(combo);
            i += end + 1;
        } else {
            let ch = chars[i];
            let (kc, need_shift) =
                char_to_keycode(ch).ok_or_else(|| format!("unsupported character: {ch}"))?;
            if need_shift {
                events.push((KEY_LEFTSHIFT, true));
            }
            events.push((kc, true));
            events.push((kc, false));
            if need_shift {
                events.push((KEY_LEFTSHIFT, false));
            }
            i += 1;
        }
    }
    Ok(events)
}

fn modifier_keycode(name: &str) -> Option<u32> {
    match name.to_lowercase().as_str() {
        "ctrl" | "control" => Some(KEY_LEFTCTRL),
        "shift" => Some(KEY_LEFTSHIFT),
        "alt" => Some(KEY_LEFTALT),
        "super" | "meta" => Some(KEY_LEFTMETA),
        _ => None,
    }
}

fn letter_to_keycode(ch: char) -> u32 {
    match ch {
        'a' => KEY_A,
        'b' => KEY_B,
        'c' => KEY_C,
        'd' => KEY_D,
        'e' => KEY_E,
        'f' => KEY_F,
        'g' => KEY_G,
        'h' => KEY_H,
        'i' => KEY_I,
        'j' => KEY_J,
        'k' => KEY_K,
        'l' => KEY_L,
        'm' => KEY_M,
        'n' => KEY_N,
        'o' => KEY_O,
        'p' => KEY_P,
        'q' => KEY_Q,
        'r' => KEY_R,
        's' => KEY_S,
        't' => KEY_T,
        'u' => KEY_U,
        'v' => KEY_V,
        'w' => KEY_W,
        'x' => KEY_X,
        'y' => KEY_Y,
        'z' => KEY_Z,
        _ => KEY_SPACE,
    }
}

fn char_to_keycode(ch: char) -> Option<(u32, bool)> {
    let (kc, shift) = match ch {
        'a'..='z' => (letter_to_keycode(ch), false),
        'A'..='Z' => (letter_to_keycode(ch.to_ascii_lowercase()), true),
        '0' => (KEY_0, false),
        '1'..='9' => (KEY_1 + (ch as u32 - '1' as u32), false),
        ' ' => (KEY_SPACE, false),
        '-' => (KEY_MINUS, false),
        '=' => (KEY_EQUAL, false),
        '[' => (KEY_LEFTBRACE, false),
        ']' => (KEY_RIGHTBRACE, false),
        ';' => (KEY_SEMICOLON, false),
        '\'' => (KEY_APOSTROPHE, false),
        ',' => (KEY_COMMA, false),
        '.' => (KEY_DOT, false),
        '/' => (KEY_SLASH, false),
        '\\' => (KEY_BACKSLASH, false),
        '`' => (KEY_GRAVE, false),
        '\t' => (KEY_TAB, false),
        '\n' => (KEY_ENTER, false),
        '!' => (KEY_1, true),
        '@' => (KEY_2, true),
        '#' => (KEY_3, true),
        '$' => (KEY_4, true),
        '%' => (KEY_5, true),
        '^' => (KEY_6, true),
        '&' => (KEY_7, true),
        '*' => (KEY_8, true),
        '(' => (KEY_9, true),
        ')' => (KEY_0, true),
        '_' => (KEY_MINUS, true),
        '+' => (KEY_EQUAL, true),
        '{' => (KEY_LEFTBRACE, true),
        '}' => (KEY_RIGHTBRACE, true),
        ':' => (KEY_SEMICOLON, true),
        '"' => (KEY_APOSTROPHE, true),
        '<' => (KEY_COMMA, true),
        '>' => (KEY_DOT, true),
        '?' => (KEY_SLASH, true),
        '|' => (KEY_BACKSLASH, true),
        '~' => (KEY_GRAVE, true),
        _ => return None,
    };
    Some((kc, shift))
}

fn key_name_to_keycode(name: &str) -> Option<u32> {
    match name.to_lowercase().as_str() {
        "a" => Some(KEY_A),
        "b" => Some(KEY_B),
        "c" => Some(KEY_C),
        "d" => Some(KEY_D),
        "e" => Some(KEY_E),
        "f" => Some(KEY_F),
        "g" => Some(KEY_G),
        "h" => Some(KEY_H),
        "i" => Some(KEY_I),
        "j" => Some(KEY_J),
        "k" => Some(KEY_K),
        "l" => Some(KEY_L),
        "m" => Some(KEY_M),
        "n" => Some(KEY_N),
        "o" => Some(KEY_O),
        "p" => Some(KEY_P),
        "q" => Some(KEY_Q),
        "r" => Some(KEY_R),
        "s" => Some(KEY_S),
        "t" => Some(KEY_T),
        "u" => Some(KEY_U),
        "v" => Some(KEY_V),
        "w" => Some(KEY_W),
        "x" => Some(KEY_X),
        "y" => Some(KEY_Y),
        "z" => Some(KEY_Z),
        "0" => Some(KEY_0),
        "1" => Some(KEY_1),
        "2" => Some(KEY_2),
        "3" => Some(KEY_3),
        "4" => Some(KEY_4),
        "5" => Some(KEY_5),
        "6" => Some(KEY_6),
        "7" => Some(KEY_7),
        "8" => Some(KEY_8),
        "9" => Some(KEY_9),
        "return" | "enter" => Some(KEY_ENTER),
        "escape" | "esc" => Some(KEY_ESC),
        "tab" => Some(KEY_TAB),
        "backspace" | "bs" => Some(KEY_BACKSPACE),
        "space" => Some(KEY_SPACE),
        "up" => Some(KEY_UP),
        "down" => Some(KEY_DOWN),
        "left" => Some(KEY_LEFT),
        "right" => Some(KEY_RIGHT),
        "home" => Some(KEY_HOME),
        "end" => Some(KEY_END),
        "pageup" | "page_up" => Some(KEY_PAGEUP),
        "pagedown" | "page_down" => Some(KEY_PAGEDOWN),
        "insert" => Some(KEY_INSERT),
        "delete" | "del" => Some(KEY_DELETE),
        "f1" => Some(KEY_F1),
        "f2" => Some(KEY_F2),
        "f3" => Some(KEY_F3),
        "f4" => Some(KEY_F4),
        "f5" => Some(KEY_F5),
        "f6" => Some(KEY_F6),
        "f7" => Some(KEY_F7),
        "f8" => Some(KEY_F8),
        "f9" => Some(KEY_F9),
        "f10" => Some(KEY_F10),
        "f11" => Some(KEY_F11),
        "f12" => Some(KEY_F12),
        "minus" => Some(KEY_MINUS),
        "equal" => Some(KEY_EQUAL),
        "ctrl" | "control" => Some(KEY_LEFTCTRL),
        "shift" => Some(KEY_LEFTSHIFT),
        "alt" => Some(KEY_LEFTALT),
        "super" | "meta" => Some(KEY_LEFTMETA),
        _ => None,
    }
}

const KEY_ESC: u32 = 1;
const KEY_1: u32 = 2;
const KEY_2: u32 = 3;
const KEY_3: u32 = 4;
const KEY_4: u32 = 5;
const KEY_5: u32 = 6;
const KEY_6: u32 = 7;
const KEY_7: u32 = 8;
const KEY_8: u32 = 9;
const KEY_9: u32 = 10;
const KEY_0: u32 = 11;
const KEY_MINUS: u32 = 12;
const KEY_EQUAL: u32 = 13;
const KEY_BACKSPACE: u32 = 14;
const KEY_TAB: u32 = 15;
const KEY_Q: u32 = 16;
const KEY_W: u32 = 17;
const KEY_E: u32 = 18;
const KEY_R: u32 = 19;
const KEY_T: u32 = 20;
const KEY_Y: u32 = 21;
const KEY_U: u32 = 22;
const KEY_I: u32 = 23;
const KEY_O: u32 = 24;
const KEY_P: u32 = 25;
const KEY_LEFTBRACE: u32 = 26;
const KEY_RIGHTBRACE: u32 = 27;
const KEY_ENTER: u32 = 28;
const KEY_LEFTCTRL: u32 = 29;
const KEY_A: u32 = 30;
const KEY_S: u32 = 31;
const KEY_D: u32 = 32;
const KEY_F: u32 = 33;
const KEY_G: u32 = 34;
const KEY_H: u32 = 35;
const KEY_J: u32 = 36;
const KEY_K: u32 = 37;
const KEY_L: u32 = 38;
const KEY_SEMICOLON: u32 = 39;
const KEY_APOSTROPHE: u32 = 40;
const KEY_GRAVE: u32 = 41;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_BACKSLASH: u32 = 43;
const KEY_Z: u32 = 44;
const KEY_X: u32 = 45;
const KEY_C: u32 = 46;
const KEY_V: u32 = 47;
const KEY_B: u32 = 48;
const KEY_N: u32 = 49;
const KEY_M: u32 = 50;
const KEY_COMMA: u32 = 51;
const KEY_DOT: u32 = 52;
const KEY_SLASH: u32 = 53;
const KEY_LEFTALT: u32 = 56;
const KEY_SPACE: u32 = 57;
const KEY_F1: u32 = 59;
const KEY_F2: u32 = 60;
const KEY_F3: u32 = 61;
const KEY_F4: u32 = 62;
const KEY_F5: u32 = 63;
const KEY_F6: u32 = 64;
const KEY_F7: u32 = 65;
const KEY_F8: u32 = 66;
const KEY_F9: u32 = 67;
const KEY_F10: u32 = 68;
const KEY_F11: u32 = 87;
const KEY_F12: u32 = 88;
const KEY_HOME: u32 = 102;
const KEY_UP: u32 = 103;
const KEY_PAGEUP: u32 = 104;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_END: u32 = 107;
const KEY_DOWN: u32 = 108;
const KEY_PAGEDOWN: u32 = 109;
const KEY_INSERT: u32 = 110;
const KEY_DELETE: u32 = 111;
const KEY_LEFTMETA: u32 = 125;

#[cfg(test)]
mod tests {
    use super::*;
    use blit_remote::{
        CellStyle, FEATURE_CREATE_NONCE, FEATURE_RESIZE_BATCH, FEATURE_RESTART, FrameState,
        S2C_CLOSED, S2C_CREATED_N, build_update_msg, msg_hello,
    };

    // ── Escape parsing unit tests ────────────────────────────────────────

    #[test]
    fn parse_escapes_plain() {
        assert_eq!(parse_escapes("hello"), b"hello");
    }

    #[test]
    fn parse_escapes_newline() {
        assert_eq!(parse_escapes("a\\nb"), b"a\nb");
    }

    #[test]
    fn parse_escapes_tab() {
        assert_eq!(parse_escapes("a\\tb"), b"a\tb");
    }

    #[test]
    fn parse_escapes_backslash() {
        assert_eq!(parse_escapes("a\\\\b"), b"a\\b");
    }

    #[test]
    fn parse_escapes_hex() {
        assert_eq!(parse_escapes("\\x1b[A"), &[0x1b, b'[', b'A']);
    }

    #[test]
    fn parse_escapes_carriage_return() {
        assert_eq!(parse_escapes("\\r"), b"\r");
    }

    #[test]
    fn parse_escapes_nul() {
        assert_eq!(parse_escapes("\\0"), &[0]);
    }

    #[test]
    fn parse_escapes_mixed() {
        assert_eq!(parse_escapes("echo hello\\n"), b"echo hello\n");
    }

    // ── Mock server infrastructure ───────────────────────────────────────

    struct MockPty {
        id: u16,
        tag: String,
        title: String,
        exited: bool,
        exit_status: i32,
        frame: FrameState,
    }

    struct MockServer {
        reader: Box<dyn AsyncRead + Unpin + Send>,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        ptys: Vec<MockPty>,
    }

    impl MockServer {
        fn new(stream: tokio::net::UnixStream) -> Self {
            let (r, w) = tokio::io::split(stream);
            Self {
                reader: Box::new(r),
                writer: Box::new(w),
                ptys: Vec::new(),
            }
        }

        fn add_pty(&mut self, id: u16, tag: &str, title: &str, exited: bool, text: &str) {
            let mut frame = FrameState::new(24, 80);
            if !text.is_empty() {
                for (row, line) in text.lines().enumerate() {
                    if row >= 24 {
                        break;
                    }
                    frame.write_text(row as u16, 0, line, CellStyle::default());
                }
            }
            self.ptys.push(MockPty {
                id,
                tag: tag.to_string(),
                title: title.to_string(),
                exited,
                exit_status: blit_remote::EXIT_STATUS_UNKNOWN,
                frame,
            });
        }

        async fn send_initial_burst(&mut self) {
            let hello = msg_hello(
                1,
                FEATURE_CREATE_NONCE | FEATURE_RESTART | FEATURE_RESIZE_BATCH,
            );
            write_frame(&mut self.writer, &hello).await;

            let mut list_msg = vec![blit_remote::S2C_LIST];
            let count = self.ptys.len() as u16;
            list_msg.extend_from_slice(&count.to_le_bytes());
            for pty in &self.ptys {
                list_msg.extend_from_slice(&pty.id.to_le_bytes());
                let tag_bytes = pty.tag.as_bytes();
                list_msg.extend_from_slice(&(tag_bytes.len() as u16).to_le_bytes());
                list_msg.extend_from_slice(tag_bytes);
                list_msg.extend_from_slice(&0u16.to_le_bytes());
            }
            write_frame(&mut self.writer, &list_msg).await;

            for pty in &self.ptys {
                if !pty.title.is_empty() {
                    let mut title_msg = vec![S2C_TITLE];
                    title_msg.extend_from_slice(&pty.id.to_le_bytes());
                    title_msg.extend_from_slice(pty.title.as_bytes());
                    write_frame(&mut self.writer, &title_msg).await;
                }
                if pty.exited {
                    write_frame(
                        &mut self.writer,
                        &blit_remote::msg_exited(pty.id, pty.exit_status),
                    )
                    .await;
                }
            }

            write_frame(&mut self.writer, &[S2C_READY]).await;
        }

        async fn recv(&mut self) -> Option<Vec<u8>> {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                read_frame(&mut self.reader),
            )
            .await
            .ok()?
        }

        async fn send_update_for(&mut self, pty_id: u16) {
            if let Some(pty) = self.ptys.iter().find(|p| p.id == pty_id) {
                let empty = FrameState::default();
                if let Some(msg) = build_update_msg(pty_id, &pty.frame, &empty) {
                    write_frame(&mut self.writer, &msg).await;
                }
            }
        }

        async fn send_created_n(&mut self, nonce: u16, pty_id: u16, tag: &str) {
            let mut msg = vec![S2C_CREATED_N];
            msg.extend_from_slice(&nonce.to_le_bytes());
            msg.extend_from_slice(&pty_id.to_le_bytes());
            msg.extend_from_slice(tag.as_bytes());
            write_frame(&mut self.writer, &msg).await;
        }

        async fn send_closed(&mut self, pty_id: u16) {
            let mut msg = vec![S2C_CLOSED];
            msg.extend_from_slice(&pty_id.to_le_bytes());
            write_frame(&mut self.writer, &msg).await;
        }

        async fn send_exited(&mut self, pty_id: u16, exit_status: i32) {
            write_frame(
                &mut self.writer,
                &blit_remote::msg_exited(pty_id, exit_status),
            )
            .await;
        }

        async fn send_text(
            &mut self,
            nonce: u16,
            pty_id: u16,
            total_lines: u32,
            offset: u32,
            text: &str,
        ) {
            let mut msg = Vec::with_capacity(13 + text.len());
            msg.push(S2C_TEXT);
            msg.extend_from_slice(&nonce.to_le_bytes());
            msg.extend_from_slice(&pty_id.to_le_bytes());
            msg.extend_from_slice(&total_lines.to_le_bytes());
            msg.extend_from_slice(&offset.to_le_bytes());
            msg.extend_from_slice(text.as_bytes());
            write_frame(&mut self.writer, &msg).await;
        }
    }

    // ── Helper to capture stdout ─────────────────────────────────────────

    // ── Integration tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_list_empty() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.send_initial_burst().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let transport = Transport::Unix(client);
        let conn = AgentConn::connect(transport).await.unwrap();

        assert!(conn.ptys.is_empty());
        assert!(conn.titles.is_empty());
        assert!(conn.exited.is_empty());

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_list_with_ptys() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "shell", "user@host:~", false, "");
            mock.add_pty(2, "build", "make", true, "");
            mock.add_pty(3, "htop", "htop", false, "");
            mock.send_initial_burst().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let transport = Transport::Unix(client);
        let conn = AgentConn::connect(transport).await.unwrap();

        assert_eq!(conn.ptys.len(), 3);
        assert_eq!(conn.ptys[0].id, 1);
        assert_eq!(conn.ptys[0].tag, "shell");
        assert_eq!(conn.ptys[1].id, 2);
        assert_eq!(conn.ptys[1].tag, "build");
        assert_eq!(conn.ptys[2].id, 3);
        assert_eq!(conn.ptys[2].tag, "htop");

        assert_eq!(conn.titles.get(&1).unwrap(), "user@host:~");
        assert_eq!(conn.titles.get(&2).unwrap(), "make");
        assert_eq!(conn.titles.get(&3).unwrap(), "htop");

        assert!(conn.exited.contains_key(&2));
        assert!(!conn.exited.contains_key(&1));
        assert!(!conn.exited.contains_key(&3));

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_start() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_CREATE2);
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            assert_eq!(nonce, 1);

            mock.send_created_n(nonce, 5, "").await;
        });

        let transport = Transport::Unix(client);
        let mut conn = AgentConn::connect(transport).await.unwrap();

        let msg = msg_create2(1, 24, 80, "", "echo\0hello", 0);
        conn.send(&msg).await.unwrap();

        loop {
            let data = conn.recv().await.unwrap();
            if data.is_empty() {
                continue;
            }
            if let Some(ServerMsg::CreatedN {
                nonce: n, pty_id, ..
            }) = parse_server_msg(&data)
            {
                if n == 1 {
                    assert_eq!(pty_id, 5);
                    break;
                }
            }
        }

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_start_with_tag() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_CREATE2);
            let nonce = u16::from_le_bytes([data[1], data[2]]);

            let rows = u16::from_le_bytes([data[3], data[4]]);
            let cols = u16::from_le_bytes([data[5], data[6]]);
            assert_eq!(rows, 24);
            assert_eq!(cols, 80);

            let tag_len = u16::from_le_bytes([data[8], data[9]]) as usize;
            let tag = std::str::from_utf8(&data[10..10 + tag_len]).unwrap();
            assert_eq!(tag, "mytag");

            mock.send_created_n(nonce, 7, "mytag").await;
        });

        let transport = Transport::Unix(client);
        let mut conn = AgentConn::connect(transport).await.unwrap();

        let msg = msg_create2(1, 24, 80, "mytag", "bash", 0);
        conn.send(&msg).await.unwrap();

        loop {
            let data = conn.recv().await.unwrap();
            if let Some(ServerMsg::CreatedN {
                nonce: n, pty_id, ..
            }) = parse_server_msg(&data)
            {
                if n == 1 {
                    assert_eq!(pty_id, 7);
                    break;
                }
            }
        }

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_show() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "shell", "bash", false, "hello world");
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_SUBSCRIBE);
            let pid = u16::from_le_bytes([data[1], data[2]]);
            assert_eq!(pid, 1);

            mock.send_update_for(1).await;

            let ack = mock.recv().await.unwrap();
            assert_eq!(ack[0], blit_remote::C2S_ACK);
        });

        let transport = Transport::Unix(client);
        let mut conn = AgentConn::connect(transport).await.unwrap();

        assert!(conn.has_pty(1));
        conn.send(&msg_subscribe(1)).await.unwrap();

        let mut state = TerminalState::new(0, 0);
        loop {
            let data = conn.recv().await.unwrap();
            if data[0] == S2C_UPDATE && data.len() >= 3 {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                if pid == 1 {
                    state.feed_compressed(&data[3..]);
                    conn.send(&msg_ack()).await.unwrap();
                    break;
                }
            }
        }

        let text = state.get_all_text();
        assert!(text.contains("hello world"), "got: {text}");

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_show_nonexistent() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "shell", "bash", false, "");
            mock.send_initial_burst().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_show(transport, 99, false, None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_close() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(2, "build", "make", false, "");
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_CLOSE);
            let pid = u16::from_le_bytes([data[1], data[2]]);
            assert_eq!(pid, 2);

            mock.send_closed(2).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_close(transport, 2).await;
        assert!(result.is_ok());

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_close_nonexistent() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.send_initial_burst().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_close(transport, 99).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_plain() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "shell", "bash", false, "");
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_INPUT);
            let pid = u16::from_le_bytes([data[1], data[2]]);
            assert_eq!(pid, 1);
            assert_eq!(&data[3..], b"hello");
        });

        let transport = Transport::Unix(client);
        let result = cmd_send(transport, 1, "hello".to_string()).await;
        assert!(result.is_ok());

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_escapes() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "shell", "bash", false, "");
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_INPUT);
            assert_eq!(&data[3..], b"line1\nline2\ttab");
        });

        let transport = Transport::Unix(client);
        let result = cmd_send(transport, 1, "line1\\nline2\\ttab".to_string()).await;
        assert!(result.is_ok());

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_hex_escape() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "shell", "bash", false, "");
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_INPUT);
            assert_eq!(&data[3..], &[0x1b, b'[', b'A']);
        });

        let transport = Transport::Unix(client);
        let result = cmd_send(transport, 1, "\\x1b[A".to_string()).await;
        assert!(result.is_ok());

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_history() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "shell", "bash", false, "");
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_READ);
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            let pid = u16::from_le_bytes([data[3], data[4]]);
            assert_eq!(pid, 1);

            mock.send_text(nonce, pid, 3, 0, "line1\nline2\nline3")
                .await;
        });

        let transport = Transport::Unix(client);
        let mut conn = AgentConn::connect(transport).await.unwrap();

        assert!(conn.has_pty(1));
        conn.send(&msg_read(1, 1, 0, 0, 0)).await.unwrap();

        loop {
            let data = conn.recv().await.unwrap();
            if data[0] == S2C_TEXT {
                if let Some(ServerMsg::Text {
                    nonce,
                    text,
                    total_lines,
                    offset,
                    ..
                }) = parse_server_msg(&data)
                {
                    if nonce == 1 {
                        assert_eq!(total_lines, 3);
                        assert_eq!(offset, 0);
                        assert!(text.contains("line1"), "got: {text}");
                        assert!(text.contains("line3"), "got: {text}");
                        break;
                    }
                }
            }
        }

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_to_exited_session() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "shell", "bash", true, "");
            mock.send_initial_burst().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_send(transport, 1, "hello".to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("has exited"));

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_already_exited() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "build", "make", true, "");
            mock.ptys[0].exit_status = 0;
            mock.send_initial_burst().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_wait(transport, 1, 5, None).await;
        assert_eq!(result.unwrap(), 0);

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_exits_later() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "build", "make", false, "");
            mock.send_initial_burst().await;

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            mock.send_exited(1, 42).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_wait(transport, 1, 5, None).await;
        assert_eq!(result.unwrap(), 42);

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_timeout() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "build", "make", false, "");
            mock.send_initial_burst().await;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_wait(transport, 1, 1, None).await;
        assert_eq!(result.unwrap(), 124);

        mock.abort();
    }

    #[tokio::test]
    async fn test_wait_not_found() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.send_initial_burst().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_wait(transport, 99, 5, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_signal_exit() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "build", "make", false, "");
            mock.send_initial_burst().await;

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            mock.send_exited(1, -9).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_wait(transport, 1, 5, None).await;
        assert_eq!(result.unwrap(), 137);

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_pattern_match() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "build", "make", false, "BUILD SUCCESS");
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_SUBSCRIBE);

            mock.send_update_for(1).await;

            let ack = mock.recv().await.unwrap();
            assert_eq!(ack[0], blit_remote::C2S_ACK);
        });

        let transport = Transport::Unix(client);
        let result = cmd_wait(transport, 1, 5, Some("BUILD (SUCCESS|FAILURE)".to_string())).await;
        assert_eq!(result.unwrap(), 0);

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_pattern_exits_before_match() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "build", "make", false, "compiling...");
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_SUBSCRIBE);

            mock.send_update_for(1).await;

            let _ack = mock.recv().await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            mock.send_exited(1, 1).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_wait(transport, 1, 5, Some("BUILD (SUCCESS|FAILURE)".to_string())).await;
        assert_eq!(result.unwrap(), 1);

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_invalid_pattern() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "build", "make", false, "");
            mock.send_initial_burst().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_wait(transport, 1, 5, Some("[invalid".to_string())).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid pattern"));

        mock.await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_pattern_already_exited_no_match() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let mock = tokio::spawn(async move {
            let mut mock = MockServer::new(server);
            mock.add_pty(1, "build", "make", true, "compiling done");
            mock.ptys[0].exit_status = 0;
            mock.send_initial_burst().await;

            let data = mock.recv().await.unwrap();
            assert_eq!(data[0], blit_remote::C2S_SUBSCRIBE);

            mock.send_update_for(1).await;

            let _ack = mock.recv().await;
        });

        let transport = Transport::Unix(client);
        let result = cmd_wait(transport, 1, 5, Some("BUILD (SUCCESS|FAILURE)".to_string())).await;
        assert_eq!(result.unwrap(), 0);

        mock.await.unwrap();
    }
}
