use std::collections::HashMap;

use blit_remote::{
    msg_ack, msg_close, msg_create_n_command, msg_input, msg_read, msg_resize, msg_subscribe,
    parse_server_msg, ServerMsg, TerminalState, S2C_EXITED, S2C_HELLO, S2C_LIST, S2C_READY,
    S2C_TEXT, S2C_TITLE, S2C_UPDATE,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::transport::{read_frame, write_frame, Transport};

struct PtyInfo {
    id: u16,
    tag: String,
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
                            })
                            .collect();
                    }
                }
                S2C_TITLE => {
                    if let Some(ServerMsg::Title { pty_id, title }) = parse_server_msg(&data) {
                        if let Ok(t) = std::str::from_utf8(title) {
                            titles.insert(pty_id, t.to_string());
                        }
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
}

pub async fn cmd_list(transport: Transport) -> Result<(), String> {
    let conn = AgentConn::connect(transport).await?;

    println!("ID\tTAG\tTITLE\tSTATUS");
    for pty in &conn.ptys {
        let title = conn.titles.get(&pty.id).map(|s| s.as_str()).unwrap_or("");
        let status = match conn.exited.get(&pty.id) {
            None => "running".to_string(),
            Some(&s) if s == blit_remote::EXIT_STATUS_UNKNOWN => "exited".to_string(),
            Some(&s) if s >= 0 => format!("exited({})", s),
            Some(&s) => format!("signal({})", -s),
        };
        println!("{}\t{}\t{}\t{}", pty.id, pty.tag, title, status);
    }
    Ok(())
}

pub async fn cmd_start(
    transport: Transport,
    tag: Option<String>,
    command: Vec<String>,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    let nonce: u16 = 1;
    let tag_str = tag.as_deref().unwrap_or("");
    let cmd_str = command.join("\0");
    let msg = msg_create_n_command(nonce, rows, cols, tag_str, &cmd_str);
    conn.send(&msg).await?;

    loop {
        let data = conn.recv().await?;
        if data.is_empty() {
            continue;
        }
        if let Some(ServerMsg::CreatedN {
            nonce: n, pty_id, ..
        }) = parse_server_msg(&data)
        {
            if n == nonce {
                println!("{pty_id}");
                return Ok(());
            }
        }
    }
}

pub async fn cmd_show(transport: Transport, id: u16, ansi: bool) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

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
) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

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
        if data[0] == S2C_TEXT {
            if let Some(ServerMsg::Text { nonce: n, text, .. }) = parse_server_msg(&data) {
                if n == nonce {
                    if !text.is_empty() {
                        println!("{text}");
                    }
                    return Ok(());
                }
            }
        }
    }
}

pub async fn cmd_send(transport: Transport, id: u16, text: String) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
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
        if let Some(ServerMsg::Closed { pty_id }) = parse_server_msg(&data) {
            if pty_id == id {
                return Ok(());
            }
        }
    }
}

pub async fn cmd_resize(transport: Transport, id: u16, rows: u16, cols: u16) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;

    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

    conn.send(&msg_resize(id, rows, cols)).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use blit_remote::{
        build_update_msg, msg_hello, CellStyle, FrameState, FEATURE_CREATE_NONCE,
        FEATURE_RESIZE_BATCH, FEATURE_RESTART, S2C_CLOSED, S2C_CREATED_N,
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
            assert_eq!(data[0], blit_remote::C2S_CREATE_N);
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            assert_eq!(nonce, 1);

            mock.send_created_n(nonce, 5, "").await;
        });

        let transport = Transport::Unix(client);
        let mut conn = AgentConn::connect(transport).await.unwrap();

        let msg = msg_create_n_command(1, 24, 80, "", "echo\0hello");
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
            assert_eq!(data[0], blit_remote::C2S_CREATE_N);
            let nonce = u16::from_le_bytes([data[1], data[2]]);

            let rows = u16::from_le_bytes([data[3], data[4]]);
            let cols = u16::from_le_bytes([data[5], data[6]]);
            assert_eq!(rows, 24);
            assert_eq!(cols, 80);

            let tag_len = u16::from_le_bytes([data[7], data[8]]) as usize;
            let tag = std::str::from_utf8(&data[9..9 + tag_len]).unwrap();
            assert_eq!(tag, "mytag");

            mock.send_created_n(nonce, 7, "mytag").await;
        });

        let transport = Transport::Unix(client);
        let mut conn = AgentConn::connect(transport).await.unwrap();

        let msg = msg_create_n_command(1, 24, 80, "mytag", "bash");
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
        let result = cmd_show(transport, 99, false).await;
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
}
