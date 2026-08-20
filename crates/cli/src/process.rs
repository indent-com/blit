//! `blit run` — execute a native, non-PTY process on the server.

use crate::transport::{FragmentReassembly, Transport, read_message, write_frame};
use blit_remote::process::{
    FEATURE_PROCESS, PROCESS_EXIT_RETURNED, PROCESS_EXIT_SIGNALLED, PROCESS_MAX_STREAM_PAYLOAD,
    PROCESS_STDIN_ACCEPTING, ProcessChild, ProcessClientError, ProcessCommand, ProcessEvent,
    ProcessExitStatus, S2C_PROCESS_STARTED, parse_process_started,
};
use blit_remote::{S2C_QUIT, STATUS_OK, status_text};
use clap::Args;
use std::ffi::{OsStr, OsString};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const SPAWN_NONCE: u16 = 1;
const CLOSE_STDIN_NONCE: u16 = 2;
const PROCESS_ID: u32 = 1;

#[derive(Args, Clone, Debug)]
pub struct RunArgs {
    /// Working directory for the process
    #[arg(long = "in", value_name = "DIR")]
    pub directory: Option<OsString>,

    /// Set an environment variable, repeatable (--env KEY=VALUE)
    #[arg(long, value_name = "KEY=VALUE")]
    pub env: Vec<OsString>,

    /// Program to execute directly
    #[arg(value_name = "PROGRAM", allow_hyphen_values = true)]
    pub program: OsString,

    /// Arguments passed verbatim to the program
    #[arg(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub arguments: Vec<OsString>,
}

pub async fn run(transport: Transport, args: RunArgs) -> Result<i32, String> {
    run_with_stdio(
        transport,
        args,
        tokio::io::stdin(),
        tokio::io::stdout(),
        tokio::io::stderr(),
    )
    .await
}

async fn run_with_stdio<R, O, E>(
    transport: Transport,
    args: RunArgs,
    mut stdin: R,
    mut stdout: O,
    mut stderr: E,
) -> Result<i32, String>
where
    R: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    let RunArgs {
        directory,
        env,
        program,
        arguments,
    } = args;
    let mut remote_command = ProcessCommand::new(os_bytes(&program)?);
    for argument in arguments {
        remote_command = remote_command.arg(os_bytes(&argument)?);
    }
    if let Some(directory) = directory {
        remote_command = remote_command.cwd(os_bytes(&directory)?);
    }
    for assignment in env {
        let bytes = os_bytes(&assignment)?;
        let (key, value) = split_env_assignment(&bytes, &assignment)?;
        remote_command = remote_command.env(key.to_vec(), value.to_vec());
    }

    let (mut reader, mut writer) = transport.split();
    let mut fragments = FragmentReassembly::default();
    let features = crate::fs::handshake(&mut reader, &mut fragments).await?;
    if features & FEATURE_PROCESS == 0 {
        return Err("server does not support native processes (upgrade blit on the remote)".into());
    }

    let spawn = remote_command
        .spawn_packet(SPAWN_NONCE, PROCESS_ID)
        .map_err(|error| format!("invalid process command: {error:?}"))?;
    send(&mut writer, &spawn).await?;

    let started_packet = loop {
        let packet = read_message(&mut reader, &mut fragments)
            .await
            .ok_or_else(|| "connection closed while starting process".to_string())?;
        match packet.first().copied() {
            Some(S2C_PROCESS_STARTED) => break packet,
            Some(S2C_QUIT) => return Err("server is shutting down".into()),
            _ => {}
        }
    };
    let started = parse_process_started(&started_packet)
        .map_err(|error| format!("malformed process start reply: {error:?}"))?;
    if started.nonce != SPAWN_NONCE || started.process_id != PROCESS_ID {
        return Err("server returned a process start reply for another request".into());
    }
    if started.status != STATUS_OK {
        return Err(refusal(
            "process spawn failed",
            started.status,
            started.detail,
        ));
    }
    let stdin_window = started.stdin_window;
    let mut child = ProcessChild::from_started(started).map_err(client_error)?;

    let mut stdin_buffer = vec![0; PROCESS_MAX_STREAM_PAYLOAD.min(64 * 1024)];
    let mut stdin_done = stdin_window == 0;
    let mut stdin_sent = 0u64;
    let mut stdin_acked = 0u64;

    loop {
        let stdin_credit = stdin_acked
            .checked_add(stdin_window)
            .and_then(|limit| limit.checked_sub(stdin_sent))
            .unwrap_or(0);
        let stdin_read_len = usize::try_from(stdin_credit)
            .unwrap_or(usize::MAX)
            .min(stdin_buffer.len());

        tokio::select! {
            read = stdin.read(&mut stdin_buffer[..stdin_read_len]), if !stdin_done && stdin_read_len != 0 => {
                let count = read.map_err(|error| format!("cannot read stdin: {error}"))?;
                if count == 0 {
                    stdin_done = true;
                    let close = child.close_stdin_packet(CLOSE_STDIN_NONCE)
                        .map_err(|error| format!("cannot close process stdin: {error:?}"))?;
                    send(&mut writer, &close).await?;
                } else {
                    let input = child.stdin_packet(&stdin_buffer[..count]).map_err(client_error)?;
                    send(&mut writer, &input).await?;
                    stdin_sent += count as u64;
                }
            }
            packet = read_message(&mut reader, &mut fragments) => {
                let packet = packet.ok_or_else(|| "connection closed while process was running".to_string())?;
                if packet.first() == Some(&S2C_QUIT) {
                    return Err("server is shutting down".into());
                }
                let event = match child.decode_event(&packet) {
                    Ok(event) => event,
                    Err(ProcessClientError::UnexpectedPacket) => continue,
                    Err(error) => return Err(client_error(error)),
                };
                match &event {
                    ProcessEvent::Stdout { data, .. } => {
                        stdout.write_all(data).await
                            .map_err(|error| format!("cannot write stdout: {error}"))?;
                        let ack = child.acknowledge(&event).map_err(client_error)?;
                        send(&mut writer, &ack).await?;
                    }
                    ProcessEvent::Stderr { data, .. } => {
                        stderr.write_all(data).await
                            .map_err(|error| format!("cannot write stderr: {error}"))?;
                        let ack = child.acknowledge(&event).map_err(client_error)?;
                        send(&mut writer, &ack).await?;
                    }
                    ProcessEvent::StdinAck { bytes, state } => {
                        stdin_acked = *bytes;
                        if *state != PROCESS_STDIN_ACCEPTING {
                            stdin_done = true;
                        }
                    }
                    ProcessEvent::Controlled { status, detail, .. } if *status != STATUS_OK => {
                        return Err(refusal("process control failed", *status, detail));
                    }
                    ProcessEvent::Controlled { .. } => {}
                    ProcessEvent::Exit(status) => {
                        stdout.flush().await
                            .map_err(|error| format!("cannot flush stdout: {error}"))?;
                        stderr.flush().await
                            .map_err(|error| format!("cannot flush stderr: {error}"))?;
                        return exit_code(status);
                    }
                }
            }
        }
    }
}

async fn send(writer: &mut (impl AsyncWrite + Unpin), packet: &[u8]) -> Result<(), String> {
    if write_frame(writer, packet).await {
        Ok(())
    } else {
        Err("connection closed".into())
    }
}

fn refusal(context: &str, status: u8, detail: &str) -> String {
    if detail.is_empty() {
        format!("{context}: {}", status_text(status))
    } else {
        format!("{context}: {detail} ({})", status_text(status))
    }
}

fn client_error(error: ProcessClientError) -> String {
    match error {
        ProcessClientError::Refused { status, detail } => {
            refusal("process refused", status, &detail)
        }
        other => format!("invalid process stream: {other:?}"),
    }
}

fn exit_code(status: &ProcessExitStatus) -> Result<i32, String> {
    match status.reason {
        PROCESS_EXIT_RETURNED => Ok(status.code as i32),
        PROCESS_EXIT_SIGNALLED => Ok(128u32.saturating_add(status.code).min(255) as i32),
        _ => {
            let detail = if status.detail.is_empty() {
                format!("process ended abnormally (reason {})", status.reason)
            } else {
                status.detail.clone()
            };
            Err(detail)
        }
    }
}

fn split_env_assignment<'a>(
    bytes: &'a [u8],
    original: &OsStr,
) -> Result<(&'a [u8], &'a [u8]), String> {
    match bytes.iter().position(|byte| *byte == b'=') {
        Some(0) => Err(format!("--env needs a name before the '=': {original:?}")),
        Some(index) => Ok((&bytes[..index], &bytes[index + 1..])),
        None => Err(format!("--env needs KEY=VALUE, got {original:?}")),
    }
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Result<Vec<u8>, String> {
    use std::os::unix::ffi::OsStrExt;
    Ok(value.as_bytes().to_vec())
}

#[cfg(windows)]
fn os_bytes(value: &OsStr) -> Result<Vec<u8>, String> {
    value
        .to_str()
        .map(|value| value.as_bytes().to_vec())
        .ok_or_else(|| format!("process arguments must be valid UTF-8 on Windows: {value:?}"))
}

#[cfg(not(any(unix, windows)))]
fn os_bytes(value: &OsStr) -> Result<Vec<u8>, String> {
    Ok(value.as_encoded_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use crate::transport::read_frame;
    use blit_remote::process::{
        PROCESS_STREAM_STDERR, PROCESS_STREAM_STDOUT, ProcessControlled, ProcessExit,
        ProcessOutput, ProcessStarted, ProcessStdinAck, msg_process_controlled, msg_process_exit,
        msg_process_stderr, msg_process_stdin_ack, msg_process_stdout, parse_process_control,
        parse_process_output_ack, parse_process_spawn, parse_process_stdin,
    };
    use blit_remote::{S2C_HELLO, S2C_READY};
    use clap::Parser;

    #[test]
    fn cli_accepts_directory_repeated_env_and_verbatim_arguments() {
        let cli = Cli::try_parse_from([
            "blit", "run", "--in", "/example", "--env", "A=1", "--env", "B=2", "printenv",
            "--null", "A", "B",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.directory.as_deref(), Some(OsStr::new("/example")));
        assert_eq!(args.env, [OsString::from("A=1"), OsString::from("B=2")]);
        assert_eq!(args.program, OsString::from("printenv"));
        assert_eq!(args.arguments, ["--null", "A", "B"].map(OsString::from));
    }

    #[tokio::test]
    async fn stdio_is_forwarded_and_return_code_is_preserved() {
        let (client_io, mut server_io) = tokio::io::duplex(1024 * 1024);
        let server = tokio::spawn(async move {
            let mut hello = vec![S2C_HELLO, 0, 0];
            hello.extend_from_slice(&FEATURE_PROCESS.to_le_bytes());
            assert!(write_frame(&mut server_io, &hello).await);
            assert!(write_frame(&mut server_io, &[S2C_READY]).await);

            let spawn = read_frame(&mut server_io).await.unwrap();
            let spawn = parse_process_spawn(&spawn).unwrap();
            assert_eq!(spawn.cwd, b"/example");
            assert_eq!(spawn.argv, [b"command".as_slice(), b"arg".as_slice()]);
            assert_eq!(
                spawn.env,
                [
                    (b"A".as_slice(), b"1".as_slice()),
                    (b"B".as_slice(), b"2".as_slice())
                ]
            );
            assert!(
                write_frame(
                    &mut server_io,
                    &blit_remote::process::msg_process_started(ProcessStarted {
                        nonce: SPAWN_NONCE,
                        status: STATUS_OK,
                        process_id: PROCESS_ID,
                        process_ref: 9,
                        stdin_window: 1024,
                        stdout_window: 1024,
                        stderr_window: 1024,
                        detail: "",
                    })
                    .unwrap(),
                )
                .await
            );

            let mut got_input = false;
            let mut got_close = false;
            while !got_input || !got_close {
                let packet = read_frame(&mut server_io).await.unwrap();
                match packet.first().copied() {
                    Some(blit_remote::process::C2S_PROCESS_STDIN) => {
                        let input = parse_process_stdin(&packet).unwrap();
                        assert_eq!(input.offset, 0);
                        assert_eq!(input.data, b"input");
                        got_input = true;
                        assert!(
                            write_frame(
                                &mut server_io,
                                &msg_process_stdin_ack(ProcessStdinAck {
                                    process_id: PROCESS_ID,
                                    bytes: 5,
                                    stdin_state: PROCESS_STDIN_ACCEPTING,
                                })
                                .unwrap(),
                            )
                            .await
                        );
                    }
                    Some(blit_remote::process::C2S_PROCESS_CONTROL) => {
                        let control = parse_process_control(&packet).unwrap();
                        assert_eq!(control.nonce, CLOSE_STDIN_NONCE);
                        got_close = true;
                        assert!(
                            write_frame(
                                &mut server_io,
                                &msg_process_controlled(ProcessControlled {
                                    nonce: CLOSE_STDIN_NONCE,
                                    status: STATUS_OK,
                                    process_id: PROCESS_ID,
                                    detail: "",
                                }),
                            )
                            .await
                        );
                    }
                    opcode => panic!("unexpected client packet {opcode:?}"),
                }
            }

            assert!(
                write_frame(
                    &mut server_io,
                    &msg_process_stdout(ProcessOutput {
                        process_id: PROCESS_ID,
                        offset: 0,
                        data: b"output",
                    })
                    .unwrap(),
                )
                .await
            );
            assert!(
                write_frame(
                    &mut server_io,
                    &msg_process_stderr(ProcessOutput {
                        process_id: PROCESS_ID,
                        offset: 0,
                        data: b"error",
                    })
                    .unwrap(),
                )
                .await
            );

            for expected_stream in [PROCESS_STREAM_STDOUT, PROCESS_STREAM_STDERR] {
                let packet = read_frame(&mut server_io).await.unwrap();
                let ack = parse_process_output_ack(&packet).unwrap();
                assert_eq!(ack.stream, expected_stream);
                assert_eq!(
                    ack.bytes,
                    if expected_stream == PROCESS_STREAM_STDOUT {
                        6
                    } else {
                        5
                    }
                );
            }
            assert!(
                write_frame(
                    &mut server_io,
                    &msg_process_exit(ProcessExit {
                        process_id: PROCESS_ID,
                        reason: PROCESS_EXIT_RETURNED,
                        kill_cause: 0,
                        code: 23,
                        detail: "",
                    })
                    .unwrap(),
                )
                .await
            );
        });

        let args = RunArgs {
            directory: Some("/example".into()),
            env: vec!["A=1".into(), "B=2".into()],
            program: "command".into(),
            arguments: vec!["arg".into()],
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with_stdio(
            Transport::Duplex(client_io),
            args,
            &b"input"[..],
            &mut stdout,
            &mut stderr,
        )
        .await
        .unwrap();
        assert_eq!(code, 23);
        assert_eq!(stdout, b"output");
        assert_eq!(stderr, b"error");
        server.await.unwrap();
    }
}
