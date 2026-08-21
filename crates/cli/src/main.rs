mod agent;
mod attach;
mod cli;
mod completion;
mod events;
mod extension;
mod forward;
mod fs;
mod generate;
mod git;
mod grep;
mod interactive;
mod journal;
mod kv;
mod lsp;
mod process;
mod relay;
mod socks;
mod transport;
mod uplink;

use clap::Parser;
use cli::{
    Cli, ClientCommand, ClipboardCommand, Command, EventsCommand, EventsConfigCommand,
    EventsFileCommand, FsCommand, GitCommand, KvCommand, LspCommand, RemoteCommand, SurfaceCommand,
    TerminalCommand,
};

// glibc malloc retains freed memory in per-thread arenas (up to 8 per core);
// with one tokio worker per core this inflates RSS by hundreds of MB under
// streaming load. mimalloc returns memory far more aggressively.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Bound arenas used by native libraries that bypass Rust's allocator.
///
/// NVENC/CUDA, notify, and other C dependencies still call glibc malloc.
/// Its default permits up to eight arenas per CPU, and a heavily threaded
/// server can leave hundreds of 64 MiB arena mappings resident after bursts.
/// Rust allocations use mimalloc, so four native arenas provide concurrency
/// without multiplying retained memory by the server's thread count. An
/// explicit `MALLOC_ARENA_MAX` remains authoritative.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn limit_native_malloc_arenas() {
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        // SAFETY: `mallopt` is process-global and this is the first action in
        // main, before blit creates any worker threads.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, 4);
        }
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn limit_native_malloc_arenas() {}

fn main() {
    limit_native_malloc_arenas();

    // ProxyDaemon must run synchronously — blit_proxy::run() builds its own
    // tokio runtime, which panics if called from within an existing one.
    // Detect this subcommand before entering the async runtime. Account for
    // global option values, but stop at the actual subcommand: a verbatim
    // argument after `@name` must never launch the daemon.
    if proxy_daemon_requested(std::env::args().skip(1)) {
        blit_proxy::run(false);
        return;
    }

    // `--license` works like `--help`: a bare top-level flag, handled before
    // clap since every other invocation requires a subcommand.
    if std::env::args().nth(1).as_deref() == Some("--license") {
        print!("{}", cli::license_text());
        return;
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async_main());
}

fn proxy_daemon_requested<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        let argument = argument.as_ref();
        if matches!(argument, "--on" | "--hub") {
            let _ = args.next();
            continue;
        }
        if argument.starts_with("--on=") || argument.starts_with("--hub=") {
            continue;
        }
        if argument == "proxy-daemon" {
            return true;
        }
        if !argument.starts_with('-') {
            return false;
        }
    }
    false
}

async fn async_main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    if completion::run_if_requested(std::env::args().skip(1)).await {
        return;
    }

    let cli = Cli::parse();

    if cli.advertised_command_json && !matches!(&cli.command, Command::External(_)) {
        eprintln!("blit: root --json is only valid before an extension command namespace (@name)");
        std::process::exit(2);
    }

    match cli.command {
        Command::Terminal { command } => {
            let cmd = command.unwrap_or(TerminalCommand::List);
            // All terminal commands except Quit need a server connection.
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            };
            let result = match cmd {
                TerminalCommand::List => agent::cmd_list(transport).await,
                TerminalCommand::Start {
                    command,
                    shell,
                    cwd,
                    env,
                    tag,
                    rows,
                    cols,
                    wait,
                    timeout,
                    deadline,
                } => {
                    let start_result = agent::cmd_start(
                        transport,
                        agent::StartRequest {
                            tag,
                            command,
                            shell,
                            cwd,
                            env,
                            rows,
                            cols,
                            deadline,
                        },
                    )
                    .await;
                    if wait {
                        let pty_id = match start_result {
                            Ok(id) => id,
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(1);
                            }
                        };
                        let transport2 = match transport::connect(&conn.on, &conn.hub).await {
                            Ok(t) => t,
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(1);
                            }
                        };
                        match agent::cmd_wait(transport2, pty_id, timeout.unwrap(), None).await {
                            Ok(code) => std::process::exit(code),
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                    start_result.map(|_| ())
                }
                TerminalCommand::Show {
                    id,
                    ansi,
                    rows,
                    cols,
                } => agent::cmd_show(transport, id, ansi, rows, cols).await,
                TerminalCommand::History {
                    id,
                    from_start,
                    from_end,
                    limit,
                    since,
                    max_bytes,
                    json,
                    ansi,
                    rows,
                    cols,
                } => {
                    if let Some(since) = since {
                        let cursor = match journal::parse_cursor(&since) {
                            Ok(c) => c,
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(2);
                            }
                        };
                        let max_bytes = max_bytes.unwrap_or(journal::OUTPUT_MAX_BYTES);
                        match journal::cmd_since(transport, id, cursor, max_bytes, json).await {
                            Ok(code) => std::process::exit(code),
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                    let size = agent::capture_size(rows, cols);
                    agent::cmd_history(transport, id, from_start, from_end, limit, ansi, size).await
                }
                TerminalCommand::Journal {
                    id,
                    from,
                    limit,
                    json,
                } => match journal::cmd_journal(transport, id, from, limit, json).await {
                    Ok(code) => std::process::exit(code),
                    Err(e) => {
                        eprintln!("blit: {e}");
                        std::process::exit(1);
                    }
                },
                TerminalCommand::Output {
                    id,
                    index,
                    wait,
                    max_bytes,
                    json,
                } => match journal::cmd_output(transport, id, index, wait, max_bytes, json).await {
                    Ok(code) => std::process::exit(code),
                    Err(e) => {
                        eprintln!("blit: {e}");
                        std::process::exit(1);
                    }
                },
                TerminalCommand::Send { id, text } => {
                    let text = if text == "-" {
                        use std::io::Read;
                        let mut buf = String::new();
                        std::io::stdin().read_to_string(&mut buf).unwrap_or(0);
                        buf
                    } else {
                        text
                    };
                    agent::cmd_send(transport, id, text).await
                }
                TerminalCommand::Mouse {
                    id,
                    event,
                    col,
                    row,
                    button,
                } => agent::cmd_mouse(transport, id, &event, col, row, &button).await,
                TerminalCommand::Click {
                    id,
                    col,
                    row,
                    button,
                } => agent::cmd_terminal_click(transport, id, col, row, &button).await,
                TerminalCommand::Wait {
                    id,
                    timeout,
                    pattern,
                } => match agent::cmd_wait(transport, id, timeout, pattern).await {
                    Ok(code) => std::process::exit(code),
                    Err(e) => {
                        eprintln!("blit: {e}");
                        std::process::exit(1);
                    }
                },
                TerminalCommand::Restart { id } => agent::cmd_restart(transport, id).await,
                TerminalCommand::Deadline { id, seconds } => {
                    agent::cmd_deadline(transport, id, seconds).await
                }
                TerminalCommand::Kill { id, signal } => {
                    agent::cmd_kill(transport, id, &signal).await
                }
                TerminalCommand::Close { id } => agent::cmd_close(transport, id).await,
                TerminalCommand::Attach { id } => match attach::cmd_attach(transport, id).await {
                    Ok(code) => std::process::exit(code),
                    Err(e) => Err(e),
                },
                TerminalCommand::Resize { id, cols, rows } => {
                    agent::cmd_resize(transport, id, cols, rows).await
                }
                TerminalCommand::Grep {
                    pattern,
                    ids,
                    regexps,
                    pattern_files,
                    fixed_strings,
                    word_regexp,
                    line_regexp,
                    ignore_case,
                    case_sensitive,
                    smart_case,
                    invert_match,
                    multiline,
                    multiline_dotall,
                    after_context,
                    before_context,
                    context,
                    context_separator,
                    no_context_separator,
                    line_number,
                    no_line_number,
                    with_filename,
                    no_filename,
                    heading,
                    no_heading,
                    column,
                    count,
                    count_matches,
                    files_with_matches,
                    files_without_match,
                    only_matching,
                    max_count,
                    passthru,
                    vimgrep,
                    json,
                    pretty,
                    null,
                    color,
                    field_context_separator,
                    field_match_separator,
                    quiet,
                    no_messages,
                    stats,
                    stop_on_nonmatch,
                    sort,
                    sortr,
                    tag,
                    title,
                    running,
                    exited,
                    all,
                } => {
                    let opts = match grep::Opts::from_cli(
                        pattern,
                        ids,
                        regexps,
                        pattern_files,
                        fixed_strings,
                        word_regexp,
                        line_regexp,
                        ignore_case,
                        case_sensitive,
                        smart_case,
                        multiline,
                        multiline_dotall,
                        after_context,
                        before_context,
                        context,
                        context_separator,
                        no_context_separator,
                        line_number,
                        no_line_number,
                        with_filename,
                        no_filename,
                        heading,
                        no_heading,
                        column,
                        count,
                        count_matches,
                        files_with_matches,
                        files_without_match,
                        only_matching,
                        max_count,
                        passthru,
                        vimgrep,
                        json,
                        pretty,
                        null,
                        color,
                        field_context_separator,
                        field_match_separator,
                        quiet,
                        no_messages,
                        stats,
                        stop_on_nonmatch,
                        invert_match,
                        sort,
                        sortr,
                        tag,
                        title,
                        running,
                        exited,
                        all,
                    ) {
                        Ok(o) => o,
                        Err(e) => {
                            eprintln!("blit: {e}");
                            std::process::exit(2);
                        }
                    };
                    match grep::run(transport, opts).await {
                        Ok(code) => std::process::exit(code),
                        Err(e) => {
                            eprintln!("blit: {e}");
                            std::process::exit(2);
                        }
                    }
                }
                TerminalCommand::Record {
                    id,
                    output,
                    frames,
                    duration,
                } => {
                    agent::cmd_record(
                        transport,
                        id,
                        output,
                        frames,
                        duration,
                        agent::RecordSource::Pty,
                        None,
                    )
                    .await
                }
            };
            if let Err(e) = result {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Client { command } => {
            let cmd = command.unwrap_or(ClientCommand::List);
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            };
            let result = match cmd {
                ClientCommand::List => agent::cmd_clients(transport).await,
                ClientCommand::Kick { id, reason } => {
                    agent::cmd_kick_client(transport, id, reason.as_deref().unwrap_or("")).await
                }
            };
            if let Err(e) = result {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Surface { command } => {
            let cmd = command.unwrap_or(SurfaceCommand::List);
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            };
            let result = match cmd {
                SurfaceCommand::List => agent::cmd_surfaces(transport).await,
                SurfaceCommand::Close { id } => agent::cmd_close_surface(transport, id).await,
                SurfaceCommand::Capture {
                    id,
                    output,
                    format,
                    quality,
                    width,
                    height,
                    scale,
                } => {
                    agent::cmd_capture(transport, id, output, format, quality, width, height, scale)
                        .await
                }
                SurfaceCommand::Click { id, x, y, button } => {
                    agent::cmd_click(transport, id, x, y, &button).await
                }
                SurfaceCommand::Key { id, key } => agent::cmd_key(transport, id, &key).await,
                SurfaceCommand::Scroll {
                    id,
                    amount,
                    horizontal,
                    smooth,
                } => agent::cmd_scroll(transport, id, amount, horizontal, smooth).await,
                SurfaceCommand::Focus { id } => agent::cmd_focus_surface(transport, id).await,
                SurfaceCommand::Text { id, text } => agent::cmd_text(transport, id, &text).await,
                SurfaceCommand::Type { id, text } => agent::cmd_type(transport, id, &text).await,
                SurfaceCommand::Record {
                    id,
                    output,
                    frames,
                    duration,
                    codec,
                    size,
                    encode_size,
                    fps,
                    timing,
                } => {
                    agent::cmd_record(
                        transport,
                        id,
                        output,
                        frames,
                        duration,
                        agent::RecordSource::Surface {
                            codecs: codec,
                            size,
                            encode_size,
                            fps,
                        },
                        timing,
                    )
                    .await
                }
            };
            if let Err(e) = result {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Clipboard { command } => {
            let cmd = command.unwrap_or(ClipboardCommand::List);
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            };
            let result = match cmd {
                ClipboardCommand::List => agent::cmd_clipboard_list(transport).await,
                ClipboardCommand::Get { mime } => agent::cmd_clipboard_get(transport, &mime).await,
                ClipboardCommand::Set {
                    mime,
                    primary,
                    text,
                } => agent::cmd_clipboard_set(transport, &mime, primary, text).await,
            };
            if let Err(e) = result {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Fs { command } => {
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            };
            let result: Result<i32, String> = match command {
                FsCommand::Sync {
                    path,
                    content,
                    no_recursive,
                    gitignore,
                    dot_ignore,
                    ignore,
                    exclude_git,
                    exclude,
                    once,
                    json,
                } => fs::cmd_sync(
                    transport,
                    path,
                    fs::SyncArgs {
                        content,
                        no_recursive,
                        gitignore,
                        dot_ignore,
                        ignore,
                        exclude_git,
                        exclude,
                        once,
                        json,
                    },
                )
                .await
                .map(|()| 0),
                FsCommand::Write {
                    path,
                    root,
                    if_hash,
                    create,
                    force,
                    parents,
                    durable,
                    mode,
                    json,
                } => {
                    fs::cmd_write(
                        transport, path, root, if_hash, create, force, parents, durable, mode, json,
                    )
                    .await
                }
                FsCommand::Mkdir {
                    path,
                    root,
                    parents,
                    mode,
                    json,
                } => fs::cmd_mkdir(transport, path, root, parents, mode, json).await,
                FsCommand::Rm {
                    path,
                    root,
                    if_hash,
                    json,
                } => fs::cmd_rm(transport, path, root, if_hash, json).await,
                FsCommand::Mv {
                    from,
                    to,
                    root,
                    parents,
                    json,
                } => fs::cmd_mv(transport, from, to, root, parents, json).await,
                FsCommand::Ln {
                    target,
                    link,
                    symlink,
                    root,
                    if_hash,
                    force,
                    parents,
                    json,
                } => {
                    fs::cmd_ln(
                        transport, target, link, symlink, root, if_hash, force, parents, json,
                    )
                    .await
                }
                FsCommand::Grep {
                    pattern,
                    root,
                    regex,
                    case_sensitive,
                    word,
                    no_ignore,
                    max_matches,
                    files_with_matches,
                    json,
                } => {
                    fs::cmd_grep(
                        transport,
                        pattern,
                        root,
                        regex,
                        case_sensitive,
                        word,
                        no_ignore,
                        max_matches,
                        files_with_matches,
                        json,
                    )
                    .await
                }
                FsCommand::Cat { path, root } => fs::cmd_cat(transport, path, root).await,
                FsCommand::Find {
                    query,
                    root,
                    limit,
                    json,
                } => fs::cmd_find(transport, query, root, limit, json).await,
            };
            match result {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Git { command } => {
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            };
            let result = match command {
                GitCommand::Status { repo, watch, json } => {
                    git::cmd_status(transport, repo, watch, json).await
                }
                GitCommand::Log {
                    rev,
                    pathspec,
                    repo,
                    limit,
                    watch,
                    follow,
                    first_parent,
                    full_message,
                    topo,
                    json,
                } => {
                    if pathspec.len() > 1 {
                        Err("only one path filter is supported".to_string())
                    } else {
                        let opts = git::LogOpts {
                            rev,
                            path: pathspec.into_iter().next(),
                            limit,
                            watch,
                            follow,
                            first_parent,
                            full_message,
                            topo,
                            json,
                        };
                        git::cmd_log(transport, repo, opts).await
                    }
                }
                GitCommand::Diff {
                    revs,
                    pathspec,
                    repo,
                    staged,
                    merge_base,
                    patch,
                    binary,
                    json,
                } => {
                    if pathspec.len() > 1 {
                        Err("only one path filter is supported".to_string())
                    } else {
                        let opts = git::DiffOpts {
                            revs,
                            staged,
                            merge_base,
                            patch,
                            binary,
                            path: pathspec.into_iter().next(),
                            json,
                        };
                        git::cmd_diff(transport, repo, opts).await
                    }
                }
                GitCommand::Show {
                    spec,
                    repo,
                    max_len,
                } => git::cmd_show(transport, repo, spec, max_len).await,
                GitCommand::LsTree { spec, repo, json } => {
                    git::cmd_ls_tree(transport, repo, spec, json).await
                }
                GitCommand::LsFiles { path, repo, json } => {
                    git::cmd_ls_files(transport, repo, path, json).await
                }
                GitCommand::Blame {
                    path,
                    repo,
                    rev,
                    start,
                    lines,
                    follow,
                    json,
                } => git::cmd_blame(transport, repo, path, rev, start, lines, follow, json).await,
                GitCommand::Reflog {
                    ref_name,
                    repo,
                    limit,
                    reverse,
                    json,
                } => git::cmd_reflog(transport, repo, ref_name, limit, reverse, json).await,
                GitCommand::Discover {
                    path,
                    depth,
                    nested,
                    bare,
                    json,
                } => git::cmd_discover(transport, path, depth, nested, bare, json).await,
                GitCommand::Fetch {
                    remote,
                    refspecs,
                    repo,
                    prune,
                    anchor,
                    timeout,
                    json,
                } => {
                    // Exits non-zero when a ref was refused: `git fetch`
                    // can exit 0 having refused one refspec of several,
                    // which is the trap this avoids.
                    match git::cmd_fetch(
                        transport, repo, remote, refspecs, prune, anchor, timeout, json,
                    )
                    .await
                    {
                        Ok(code) => std::process::exit(code),
                        Err(e) => Err(e),
                    }
                }
                GitCommand::MergeBase { revs, repo, json } => {
                    // The only git command with a meaningful non-zero exit
                    // (1 = unrelated histories), so it exits directly
                    // rather than flattening to this block's Ok(()).
                    match git::cmd_merge_base(transport, repo, revs, json).await {
                        Ok(code) => std::process::exit(code),
                        Err(e) => Err(e),
                    }
                }
            };
            if let Err(e) = result {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Kv { command } => {
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            };
            let result: Result<i32, String> = match command {
                KvCommand::Get { key } => kv::cmd_get(transport, key).await,
                KvCommand::Put {
                    key,
                    value,
                    if_hash,
                    force,
                    durable,
                    json,
                } => kv::cmd_put(transport, key, value, false, if_hash, force, durable, json).await,
                KvCommand::Rm {
                    key,
                    if_hash,
                    force,
                    durable,
                    json,
                } => kv::cmd_put(transport, key, None, true, if_hash, force, durable, json).await,
                KvCommand::Ls {
                    prefix,
                    watch,
                    values,
                    json,
                } => kv::cmd_ls(transport, prefix, watch, values, json).await,
            };
            match result {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Events { command } => {
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(transport) => transport,
                Err(error) => {
                    eprintln!("blit: {error}");
                    std::process::exit(1);
                }
            };
            let result = match command {
                EventsCommand::Config { command, json } => match command {
                    None => events::cmd_config(transport, json).await,
                    Some(EventsConfigCommand::Set { bytes, active }) => {
                        events::cmd_config_set(transport, bytes, active, json).await
                    }
                },
                EventsCommand::Dump {
                    since,
                    limit,
                    output,
                } => events::cmd_dump(transport, since, limit, output).await,
                EventsCommand::Stream { since, output } => {
                    events::cmd_stream(transport, since, output).await
                }
                EventsCommand::File { command } => match command {
                    EventsFileCommand::Start {
                        path,
                        append,
                        sync,
                        id,
                        json,
                    } => events::cmd_file_start(transport, path, append, sync, id, json).await,
                    EventsFileCommand::Stop { id, json } => {
                        events::cmd_file_stop(transport, id, json).await
                    }
                },
            };
            if let Err(error) = result {
                eprintln!("blit: {error}");
                std::process::exit(1);
            }
        }
        Command::Lsp { command } => {
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    // Exit 2 (error): the 0/1/2 contract reserves 1 for
                    // "no result", so a connect failure must not look
                    // like a clean/empty answer.
                    std::process::exit(2);
                }
            };
            let result = match command {
                LspCommand::Def { spec, root, json } => {
                    lsp::cmd_position(
                        transport,
                        root,
                        lsp::KIND_DEF,
                        spec,
                        String::new(),
                        false,
                        json,
                    )
                    .await
                }
                LspCommand::Refs {
                    spec,
                    declaration,
                    root,
                    json,
                } => {
                    lsp::cmd_position(
                        transport,
                        root,
                        lsp::KIND_REFS,
                        spec,
                        String::new(),
                        declaration,
                        json,
                    )
                    .await
                }
                LspCommand::Hover { spec, root, json } => {
                    lsp::cmd_position(
                        transport,
                        root,
                        lsp::KIND_HOVER,
                        spec,
                        String::new(),
                        false,
                        json,
                    )
                    .await
                }
                LspCommand::Complete { spec, root, json } => {
                    lsp::cmd_position(
                        transport,
                        root,
                        lsp::KIND_COMPLETE,
                        spec,
                        String::new(),
                        false,
                        json,
                    )
                    .await
                }
                LspCommand::Signature { spec, root, json } => {
                    lsp::cmd_position(
                        transport,
                        root,
                        lsp::KIND_SIGNATURE,
                        spec,
                        String::new(),
                        false,
                        json,
                    )
                    .await
                }
                LspCommand::Symbols {
                    query,
                    file,
                    root,
                    json,
                } => lsp::cmd_symbols(transport, root, query, file, json).await,
                LspCommand::Diagnostics {
                    path,
                    watch,
                    wait,
                    root,
                    json,
                } => lsp::cmd_diagnostics(transport, root, path, watch, wait, json).await,
                LspCommand::Rename {
                    spec,
                    new_name,
                    root,
                    json,
                } => {
                    lsp::cmd_position(
                        transport,
                        root,
                        lsp::KIND_RENAME,
                        spec,
                        new_name,
                        false,
                        json,
                    )
                    .await
                }
                LspCommand::Wait { root, timeout } => lsp::cmd_wait(transport, root, timeout).await,
                LspCommand::List { json } => lsp::cmd_list(transport, json).await,
                LspCommand::Stop { server_ref } => lsp::cmd_stop(transport, server_ref).await,
            };
            match result {
                // 0 found/clean, 1 no result/diagnostics present, 2 error.
                Ok(0) => {}
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(2);
                }
            }
        }
        Command::Extension { command } => {
            let conn = &cli.connect;
            let result = match transport::connect(&conn.on, &conn.hub).await {
                Ok(transport) => extension::dispatch(transport, command).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Run(args) => {
            let conn = &cli.connect;
            let result = match transport::connect(&conn.on, &conn.hub).await {
                Ok(transport) => process::run(transport, args).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::External(tokens) => {
            // Reject unknown commands and oversized argument vectors before a
            // bad invocation can cause a remote connection.
            let result = match extension::parse_advertised_command(tokens) {
                Ok((name, args)) => {
                    let conn = &cli.connect;
                    match transport::connect(&conn.on, &conn.hub).await {
                        Ok(transport) => {
                            extension::dispatch_advertised_command(
                                transport,
                                name,
                                args,
                                cli.advertised_command_json,
                            )
                            .await
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Remote { command } => {
            let cmd = command.unwrap_or(RemoteCommand::List { reveal: false });
            cmd_remote(cmd);
        }
        Command::Quit => {
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.on, &conn.hub).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = agent::cmd_quit(transport).await {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Server {
            name,
            socket,
            shell_flags,
            scrollback,
            #[cfg(unix)]
            fd_channel,
            export_sock,
            inject_path,
            max_ptys,
            surface_encoders,
            camera_codecs,
            microphone_codecs,
            allow_forward,
            allow_forward_insecure,
            no_persistent_extensions,
            deployment,
            verbose,
            no_processes,
        } => {
            let deployment = match deployment.into_overrides() {
                Ok(deployment) => deployment,
                Err(error) => {
                    eprintln!("blit server: {error}");
                    std::process::exit(2);
                }
            };
            if let Err(error) = blit_server::configure_deployment(deployment) {
                eprintln!("blit server: {error}");
                std::process::exit(2);
            }
            // A typed list that does not parse is a mistake worth stopping
            // for; the environment fallbacks inside `defaults()` stay lenient,
            // since a stale export should not make the server unbootable.
            let fail = |error: String| -> ! {
                eprintln!("blit server: {error}");
                std::process::exit(2);
            };
            let surface_encoders = match surface_encoders {
                Some(list) => blit_server::SurfaceEncoderPreference::parse_list(&list)
                    .unwrap_or_else(|error| fail(error)),
                None => blit_server::SurfaceEncoderPreference::defaults(),
            };
            let media_codecs = {
                let defaults = blit_server::MediaCodecPolicy::defaults();
                blit_server::MediaCodecPolicy {
                    camera: match camera_codecs {
                        Some(list) => blit_server::MediaCodecPolicy::parse_camera(&list)
                            .unwrap_or_else(|error| fail(error)),
                        None => defaults.camera,
                    },
                    microphone: match microphone_codecs {
                        Some(list) => blit_server::MediaCodecPolicy::parse_microphone(&list)
                            .unwrap_or_else(|error| fail(error)),
                        None => defaults.microphone,
                    },
                }
            };
            let ipc_path = socket
                .or_else(|| std::env::var("BLIT_SOCK").ok())
                .unwrap_or_else(|| blit_server::default_ipc_path_for(&name));

            #[cfg(unix)]
            let shell_default = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            #[cfg(windows)]
            let shell_default = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());

            #[cfg(unix)]
            let flags_default = "li";
            #[cfg(windows)]
            let flags_default = "";

            let config = blit_server::Config {
                name,
                shell: shell_default,
                shell_flags: shell_flags
                    .or_else(|| std::env::var("BLIT_SHELL_FLAGS").ok())
                    .unwrap_or_else(|| flags_default.into()),
                scrollback: scrollback
                    .or_else(|| {
                        std::env::var("BLIT_SCROLLBACK")
                            .ok()
                            .and_then(|s| s.parse().ok())
                    })
                    .unwrap_or(10_000),
                ipc_path,
                surface_encoders,
                surface_encoding: blit_server::SurfaceEncoding {
                    bandwidth: std::env::var("BLIT_SURFACE_BANDWIDTH")
                        .ok()
                        .and_then(|v| blit_server::SurfaceBandwidth::parse(&v))
                        .unwrap_or_default(),
                    speed: std::env::var("BLIT_SURFACE_SPEED")
                        .ok()
                        .and_then(|v| blit_server::SurfaceSpeed::parse(&v))
                        .unwrap_or_default(),
                },
                chroma: blit_server::ChromaSubsampling::from_env(),
                media_codecs,
                vaapi_device: std::env::var("BLIT_VAAPI_DEVICE")
                    .unwrap_or_else(|_| "/dev/dri/renderD128".into()),
                #[cfg(unix)]
                fd_channel: fd_channel.or_else(|| {
                    std::env::var("BLIT_FD_CHANNEL")
                        .ok()
                        .and_then(|s| s.parse().ok())
                }),
                verbose: verbose
                    || std::env::var("BLIT_VERBOSE")
                        .ok()
                        .map(|v| v == "1")
                        .unwrap_or(false),
                processes: !no_processes
                    && !std::env::var("BLIT_PROCESS").is_ok_and(|value| value == "0"),
                // Both default to 0 (unlimited), which is the right default:
                // a client that can open a PTY can already spend the machine's
                // resources from inside it, so these are an operator sanity
                // bound against runaway automation, not a security control.
                // They were hardcoded to 0 with no way to set them, which made
                // the enforcement in blit-server dead code.
                max_connections: env_usize("BLIT_MAX_CONNECTIONS"),
                // The flag takes precedence over the env var; absent both, the
                // env default of 0 stands.
                max_ptys: max_ptys.unwrap_or_else(|| env_usize("BLIT_MAX_PTYS")),
                ping_interval: std::time::Duration::from_secs(10),
                skip_compositor: std::env::var("BLIT_SKIP_COMPOSITOR")
                    .ok()
                    .map(|v| v == "1")
                    .unwrap_or(false),
                export_sock: export_sock
                    || std::env::var("BLIT_EXPORT_SOCK")
                        .ok()
                        .map(|v| v == "1")
                        .unwrap_or(false),
                inject_path: inject_path
                    || std::env::var("BLIT_INJECT_PATH")
                        .ok()
                        .map(|v| v == "1")
                        .unwrap_or(false),
                allow_forward,
                allow_forward_insecure,
                allow_persistent_extensions: !no_persistent_extensions
                    && !std::env::var("BLIT_ALLOW_EXT_PERSIST").is_ok_and(|value| value == "0"),
            };
            blit_server::run(config).await;
        }
        Command::Uplink { url } => {
            if let Err(e) = uplink::cmd_uplink(url).await {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Share { quiet, verbose } => {
            let signal_url = blit_webrtc_forwarder::normalize_hub(&cli.connect.hub);
            let passphrase = std::env::var("BLIT_PASSPHRASE").ok().unwrap_or_else(|| {
                use rand::RngExt as _;
                const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
                let mut rng = rand::rng();
                let bytes: [u8; 26] = rng.random();
                bytes
                    .iter()
                    .map(|b| ALPHABET[(b & 0x1f) as usize] as char)
                    .collect()
            });

            let sock_path = transport::default_local_socket();
            if let Err(e) = transport::ensure_local_server(&sock_path).await {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }

            // Route per-peer IPC connections through blit-proxy when enabled.
            let proxy_sock = if transport::proxy_enabled() {
                match transport::ensure_proxy().await {
                    Ok(sock) => Some(sock),
                    Err(e) => {
                        eprintln!("blit share: proxy auto-start failed: {e}");
                        None
                    }
                }
            } else {
                None
            };

            // Provide a callback to restart the proxy if it dies mid-session.
            let proxy_ensure: Option<blit_webrtc_forwarder::ProxyEnsureFn> = if proxy_sock.is_some()
            {
                let exe = blit_proxy::blit_exe();
                Some(std::sync::Arc::new(move || {
                    let exe = exe.clone();
                    Box::pin(async move { blit_proxy::ensure_proxy(&exe, true).await })
                }))
            } else {
                None
            };

            blit_webrtc_forwarder::run(blit_webrtc_forwarder::Config {
                sock_path,
                signal_url,
                passphrase,
                message_override: None,
                quiet,
                verbose,
                proxy_sock,
                proxy_ensure,
            })
            .await;
        }
        Command::Install { host } => match host {
            Some(host) => {
                if let Err(e) = cmd_install(&host).await {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            }
            None => {
                println!("# Linux / macOS");
                println!("curl -sf https://install.blit.sh | sh");
                println!();
                println!("# Windows (PowerShell)");
                println!("irm https://install.blit.sh/install.ps1 | iex");
            }
        },
        Command::Upgrade => {
            if let Err(e) = cmd_upgrade().await {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::HashPassphrase { value } => {
            if let Err(e) = cmd_hash_passphrase(value) {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Open { port } => {
            let hub = blit_webrtc_forwarder::normalize_hub(&cli.connect.hub);
            interactive::run_browser(port, &hub).await;
        }
        Command::Gateway => {
            blit_gateway::run().await;
        }
        Command::Forward {
            specs,
            all,
            alpn,
            insecure,
        } => {
            // The management verbs share the positional slot with specs: no
            // spec can be a bare word (they all carry colons), so the first
            // argument is unambiguous.
            let verb = specs.first().map(String::as_str).unwrap_or("");
            let rest = specs.get(1..).unwrap_or(&[]);
            let result: Result<i32, String> = match verb {
                "add" => match rest {
                    [name, spec] => forward::cmd_add(name, spec),
                    _ => Err("usage: blit forward add NAME SPEC".into()),
                },
                "list" | "ls" => forward::cmd_list(),
                "rm" | "remove" => match rest {
                    [name] => forward::cmd_rm(name),
                    _ => Err("usage: blit forward rm NAME".into()),
                },
                "toggle" => match rest {
                    [name] => forward::cmd_toggle(name),
                    _ => Err("usage: blit forward toggle NAME".into()),
                },
                _ => match forward::resolve_specs(&specs, all) {
                    // Connect only once there is something to forward, and
                    // only after every spec has parsed.
                    Ok(resolved) => {
                        let conn = &cli.connect;
                        match transport::connect(&conn.on, &conn.hub).await {
                            Ok(transport) => {
                                let tls = forward::TlsOpts { alpn, insecure };
                                forward::cmd_forward(transport, resolved, tls).await
                            }
                            Err(e) => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                },
            };
            match result {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Socks { listen } => {
            // Parse before connecting, so a bad listen address costs nothing.
            let result: Result<i32, String> = match socks::parse_listen(&listen) {
                Ok(listen) => {
                    let conn = &cli.connect;
                    match transport::connect(&conn.on, &conn.hub).await {
                        Ok(transport) => socks::cmd_socks(transport, listen).await,
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Learn => {
            print!("{}", include_str!("learn.md"));
        }
        Command::Generate { output } => {
            generate::run(&output);
        }
        Command::ProxyDaemon => {
            blit_proxy::run(false);
        }
    }
}

/// Read a `usize` limit from the environment. Unset, unparseable or 0 all
/// mean "no limit", which is what the server's `> 0` guards already expect.
fn env_usize(key: &str) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn cmd_hash_passphrase(value: Option<String>) -> Result<(), String> {
    use std::io::Read;

    let passphrase = match value.as_deref() {
        Some("-") | None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("failed to read passphrase from stdin: {e}"))?;
            buf.trim_end_matches(['\r', '\n']).to_string()
        }
        Some(value) => value.to_string(),
    };

    if passphrase.is_empty() {
        return Err("passphrase must be non-empty".to_string());
    }

    let hash = blit_webserver::passphrase::hash(&passphrase)?;
    println!("{hash}");
    Ok(())
}

/// Replace the passphrase in a `share:PASSPHRASE` URI with `****`.
/// URIs with an optional `?hub=...` query string are handled correctly.
/// Non-share URIs are returned unchanged.
fn mask_share_passphrase(uri: &str) -> String {
    let rest = match uri.strip_prefix("share:") {
        Some(r) => r,
        None => return uri.to_string(),
    };
    // Preserve any query string (e.g. ?hub=...)
    if let Some(q_pos) = rest.find('?') {
        format!("share:****{}", &rest[q_pos..])
    } else {
        "share:****".to_string()
    }
}

fn cmd_remote(cmd: RemoteCommand) {
    match cmd {
        RemoteCommand::List { reveal } => {
            let entries = blit_webserver::config::read_remotes_full();
            if entries.is_empty() {
                eprintln!("blit: no remotes configured (blit.remotes is empty or missing)");
            } else {
                for e in &entries {
                    let display_uri = if !reveal {
                        mask_share_passphrase(&e.uri)
                    } else {
                        e.uri.clone()
                    };
                    if e.disabled {
                        println!("{}\t{}\t(disabled)", e.name, display_uri);
                    } else {
                        println!("{}\t{}", e.name, display_uri);
                    }
                }
            }
        }
        RemoteCommand::Add { name, uri } => {
            // The same rule the file parser enforces. Checking a laxer one
            // here meant `blit remote add 'my remote' ssh:host` printed
            // success, wrote the line, and the next read dropped it.
            if !blit_webserver::config::valid_entry_name(&name) {
                eprintln!(
                    "blit: invalid remote name '{name}' — no whitespace, '=', or leading '#'"
                );
                std::process::exit(1);
            }
            let uri = match uri {
                Some(u) => u,
                None => {
                    eprint!("URI for '{name}' (ssh:host, tcp:h:p, socket:/path, local): ");
                    let mut input = String::new();
                    if std::io::stdin().read_line(&mut input).is_err() || input.trim().is_empty() {
                        eprintln!("\nblit: no URI provided");
                        std::process::exit(1);
                    }
                    input.trim().to_string()
                }
            };
            blit_webserver::config::modify_remotes(|entries| {
                if let Some(pos) = entries.iter().position(|e| e.name == name) {
                    entries[pos].uri = uri.clone();
                    entries[pos].disabled = false;
                } else {
                    entries.push(blit_webserver::config::RemoteEntry {
                        name: name.clone(),
                        uri: uri.clone(),
                        disabled: false,
                    });
                }
            });
            eprintln!("blit: remote '{name}' set to '{uri}'");
        }
        RemoteCommand::Remove { name } => {
            let mut found = false;
            blit_webserver::config::modify_remotes(|entries| {
                let before = entries.len();
                entries.retain(|e| e.name != name);
                found = entries.len() < before;
            });
            if !found {
                eprintln!("blit: no remote named '{name}'");
                std::process::exit(1);
            }
            eprintln!("blit: remote '{name}' removed");
        }
        RemoteCommand::Toggle { name } => {
            let mut new_state: Option<bool> = None;
            blit_webserver::config::modify_remotes(|entries| {
                if let Some(pos) = entries.iter().position(|e| e.name == name) {
                    entries[pos].disabled = !entries[pos].disabled;
                    new_state = Some(entries[pos].disabled);
                }
            });
            match new_state {
                None => {
                    eprintln!("blit: no remote named '{name}'");
                    std::process::exit(1);
                }
                Some(true) => eprintln!("blit: remote '{name}' disabled"),
                Some(false) => eprintln!("blit: remote '{name}' enabled"),
            }
        }
        RemoteCommand::SetDefault { target } => {
            blit_webserver::config::modify_config(|config| {
                if target.is_empty() || target == "local" {
                    config.remove("blit.target");
                } else {
                    config.insert("blit.target".into(), target.clone());
                }
            });
            if target.is_empty() || target == "local" {
                eprintln!("blit: default target cleared (using local)");
            } else {
                eprintln!("blit: default target set to '{target}'");
            }
        }
    }
}

async fn cmd_install(host: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Reject hosts starting with '-' to prevent SSH option injection.
    let host_check = host.split('@').next_back().unwrap_or(host);
    if host_check.starts_with('-') {
        return Err(format!("invalid ssh host '{host}': must not start with '-'").into());
    }
    let ssh_base = |host: &str| {
        let mut cmd = std::process::Command::new("ssh");
        cmd.arg("-T")
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPath=/tmp/blit-ssh-%r@%h:%p")
            .arg("-o")
            .arg("ControlPersist=300")
            .arg(host);
        cmd
    };

    let detect = ssh_base(host)
        .arg("--")
        .arg("uname -s 2>/dev/null || echo WINDOWS")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()?;

    if !detect.status.success() {
        return Err("ssh failed to detect remote OS".into());
    }

    let os = String::from_utf8_lossy(&detect.stdout)
        .trim()
        .to_uppercase();

    let install_cmd = if os.contains("WINDOWS")
        || os.contains("MINGW")
        || os.contains("MSYS")
        || os.contains("CYGWIN")
    {
        r#"powershell -ExecutionPolicy Bypass -Command "irm https://install.blit.sh/install.ps1 | iex""#.to_string()
    } else {
        r#"sh -c 'if command -v curl >/dev/null 2>&1; then curl -sf https://install.blit.sh | sh; elif command -v wget >/dev/null 2>&1; then wget -qO- https://install.blit.sh | sh; else echo "error: neither curl nor wget found" >&2; exit 1; fi'"#.to_string()
    };

    eprintln!("installing blit on {host} ({os})...");

    let status = ssh_base(host)
        .arg("--")
        .arg(&install_cmd)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;

    if !status.success() {
        return Err(format!("remote install exited with {status}").into());
    }

    Ok(())
}

async fn cmd_upgrade() -> Result<(), Box<dyn std::error::Error>> {
    let exe_path = blit_proxy::blit_exe();
    let bin_dir = exe_path
        .parent()
        .ok_or("cannot determine binary directory")?;
    #[cfg(not(windows))]
    let prefix = bin_dir.parent().unwrap_or(bin_dir);

    let install_url = if cfg!(windows) {
        "https://install.blit.sh/install.ps1"
    } else {
        "https://install.blit.sh"
    };
    let script = reqwest::get(install_url)
        .await?
        .error_for_status()?
        .text()
        .await?;

    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let tmp = std::env::temp_dir().join(format!("blit-install-{}.{}", std::process::id(), ext));
    std::fs::write(&tmp, &script)?;

    #[cfg(unix)]
    {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg(&tmp).env("BLIT_PREFIX", prefix);
        // Upgrades keep the flavor: a GPL binary (x264 compiled in) fetches
        // the GPL flavor again.  An explicit BLIT_GPL in the environment
        // wins, so users can switch flavors through `blit upgrade`.
        if std::env::var_os("BLIT_GPL").is_none() {
            cmd.env(
                "BLIT_GPL",
                if cfg!(all(target_os = "linux", feature = "x264")) {
                    "1"
                } else {
                    "0"
                },
            );
        }
        let status = cmd.status()?;
        if status.success() {
            transport::stop_proxy().await;
        }
        std::process::exit(status.code().unwrap_or(1));
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-File"])
            .arg(&tmp)
            .env("BLIT_INSTALL_DIR", bin_dir)
            .status()?;
        if status.success() {
            transport::stop_proxy().await;
        }
        std::process::exit(status.code().unwrap_or(1));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let status = std::process::Command::new("sh")
            .arg(&tmp)
            .env("BLIT_PREFIX", prefix)
            .status()?;
        if status.success() {
            transport::stop_proxy().await;
        }
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::{mask_share_passphrase, proxy_daemon_requested};

    #[test]
    fn test_mask_share_passphrase() {
        assert_eq!(mask_share_passphrase("share:mysecret"), "share:****");
        assert_eq!(
            mask_share_passphrase("share:mysecret?hub=hub.blit.sh"),
            "share:****?hub=hub.blit.sh"
        );
        assert_eq!(
            mask_share_passphrase("ssh:alice@prod.co"),
            "ssh:alice@prod.co"
        );
        assert_eq!(mask_share_passphrase("local"), "local");
        assert_eq!(mask_share_passphrase("share:"), "share:****");
    }

    #[test]
    fn proxy_daemon_detection_stops_at_extension_command_arguments() {
        assert!(proxy_daemon_requested(["--on", "prod", "proxy-daemon"]));
        assert!(proxy_daemon_requested([
            "--hub=https://hub",
            "proxy-daemon"
        ]));
        assert!(!proxy_daemon_requested([
            "--on",
            "prod",
            "@builder",
            "proxy-daemon"
        ]));
        assert!(!proxy_daemon_requested([
            "--on",
            "proxy-daemon",
            "@builder"
        ]));
    }
}
