use crate::cli;
use clap::{Arg, Command, CommandFactory};
use clap_complete::Shell;
use std::fs;
use std::path::Path;

const BASH_RUNTIME_COMPLETION: &str = r#"

# Advertised extension commands are discovered only during explicit completion.
_blit_with_extension_commands() {
    _blit "$@"
    local _blit_current="${COMP_WORDS[COMP_CWORD]}"
    local -a _blit_previous=()
    local _blit_index _blit_candidate
    for ((_blit_index = 1; _blit_index < COMP_CWORD; _blit_index++)); do
        _blit_previous+=("${COMP_WORDS[_blit_index]}")
    done
    while IFS= read -r _blit_candidate; do
        if [[ -n "$_blit_candidate" ]]; then
            COMPREPLY+=("$_blit_candidate")
        fi
    done < <(command "${COMP_WORDS[0]}" __complete-extension \
        "--current=$_blit_current" -- "${_blit_previous[@]}" 2>/dev/null)
}
complete -F _blit_with_extension_commands -o bashdefault -o default blit
"#;

const ZSH_RUNTIME_COMPLETION: &str = r#"

# Advertised extension commands are discovered only during explicit completion.
functions[_blit_static]=$functions[_blit]
_blit() {
    local -a _blit_original_words _blit_previous _blit_dynamic
    local _blit_original_current _blit_index _blit_static_result
    _blit_original_words=("${words[@]}")
    _blit_original_current=$CURRENT
    _blit_static "$@"
    _blit_static_result=$?
    for ((_blit_index = 2; _blit_index < _blit_original_current; _blit_index++)); do
        _blit_previous+=("${_blit_original_words[_blit_index]}")
    done
    _blit_dynamic=("${(@f)$(command "${_blit_original_words[1]}" \
        __complete-extension \
        "--current=${_blit_original_words[_blit_original_current]}" -- \
        "${_blit_previous[@]}" 2>/dev/null)}")
    if (( ${#_blit_dynamic} )); then
        compadd -- "${_blit_dynamic[@]}"
        return 0
    fi
    return $_blit_static_result
}
"#;

const FISH_RUNTIME_COMPLETION: &str = r#"

# Advertised extension commands are discovered only during explicit completion.
function __fish_blit_extension_commands
    set -l _blit_words (commandline -opc)
    set -l _blit_executable $_blit_words[1]
    set -e _blit_words[1]
    command $_blit_executable __complete-extension \
        "--current="(commandline -ct) -- $_blit_words 2>/dev/null
end
complete -c blit -f -a '(__fish_blit_extension_commands)'
"#;

/// Build a clap Command for blit-gateway (mirrors its env-var config).
fn blit_gateway_cmd() -> Command {
    Command::new("blit-gateway")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Terminal streaming WebSocket gateway")
        .long_about(
            "blit-gateway serves the browser UI and proxies WebSocket traffic to one or \
             more blit server Unix sockets. It handles passphrase authentication and \
             serves static web assets.\n\n\
             Use it for always-on deployments behind a reverse proxy or as a systemd \
             service. For local and SSH use, the blit(1) CLI embeds equivalent gateway \
             functionality and is simpler to run.\n\n\
             When BLIT_QUIC=1, the gateway also listens for WebTransport (HTTP/3) \
             connections on the same address, requiring TLS certificates.\n\n\
             All configuration is via environment variables.",
        )
        .after_help(
            "ENVIRONMENT:\n    \
             BLIT_PASSPHRASE    Browser passphrase, or argon2 PHC hash from blit hash-passphrase (required)\n    \
             BLIT_ADDR          Listen address (default: 0.0.0.0:3264)\n    \
             BLIT_REMOTES       Path to remotes file (default: ~/.config/blit/blit.remotes)\n    \
             BLIT_FONT_DIRS     Colon-separated extra font directories\n    \
             BLIT_CORS          CORS origin for font routes (* or specific origin)\n    \
             BLIT_QUIC          Set to 1 to enable WebTransport (QUIC/HTTP3)\n    \
             BLIT_QUIC_PUBLIC_ADDR Browser-facing hostname:port or :port advertised to clients\n    \
             BLIT_TLS_CERT      PEM certificate file (for WebTransport)\n    \
             BLIT_TLS_KEY       PEM private key file (for WebTransport)\n    \
             BLIT_STORE_CONFIG  Set to 1 to sync browser settings to ~/.config/blit/blit.conf",
        )
}

/// Build a clap Command for blit-webrtc-forwarder.
fn blit_webrtc_forwarder_cmd() -> Command {
    Command::new("blit-webrtc-forwarder")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Forward a blit server terminal over WebRTC")
        .long_about(
            "blit-webrtc-forwarder connects to a blit server Unix socket and \
             bridges it to browsers over WebRTC data channels. It handles signaling, \
             STUN/TURN NAT traversal, and peer-to-peer connections.\n\n\
             For most use cases, blit share is simpler -- it runs the forwarder \
             in-process and auto-starts a server if needed. The standalone binary is \
             for custom deployments where the server is managed separately.",
        )
        .arg(
            Arg::new("socket")
                .long("socket")
                .value_name("PATH")
                .env("BLIT_SOCK")
                .required(true)
                .help("Path to the blit server Unix socket"),
        )
        .arg(
            Arg::new("passphrase")
                .long("passphrase")
                .value_name("PASSPHRASE")
                .env("BLIT_PASSPHRASE")
                .required(true)
                .help("Share passphrase"),
        )
        .arg(
            Arg::new("hub")
                .long("hub")
                .value_name("URL")
                .env("BLIT_HUB")
                .default_value("https://hub.blit.sh")
                .help("Signaling hub URL"),
        )
        .arg(
            Arg::new("message")
                .long("message")
                .value_name("TEMPLATE")
                .help("Override the message template (use {secret} as placeholder)"),
        )
        .arg(
            Arg::new("quiet")
                .long("quiet")
                .action(clap::ArgAction::SetTrue)
                .help("Don't print the sharing URL"),
        )
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .action(clap::ArgAction::SetTrue)
                .help("Print detailed connection diagnostics to stderr"),
        )
}

fn generate_man_page(cmd: Command, out_dir: &Path) {
    let name = cmd.get_name().to_string();
    let man = clap_mangen::Man::new(cmd);
    let mut buf = Vec::new();
    man.render(&mut buf).expect("failed to render man page");
    let path = out_dir.join(format!("{name}.1"));
    fs::write(&path, buf).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

fn generate_completions(mut cmd: Command, out_dir: &Path, name: &str) {
    for shell in [Shell::Fish, Shell::Bash, Shell::Zsh] {
        let dir = match shell {
            Shell::Fish => out_dir.join("fish/vendor_completions.d"),
            Shell::Bash => out_dir.join("bash-completion/completions"),
            Shell::Zsh => out_dir.join("zsh/site-functions"),
            _ => unreachable!(),
        };
        fs::create_dir_all(&dir).unwrap();
        let path = clap_complete::generate_to(shell, &mut cmd, name, &dir).unwrap();
        let hook = match shell {
            Shell::Bash => BASH_RUNTIME_COMPLETION,
            Shell::Zsh => ZSH_RUNTIME_COMPLETION,
            Shell::Fish => FISH_RUNTIME_COMPLETION,
            _ => unreachable!(),
        };
        let mut generated = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        generated.push_str(hook);
        fs::write(&path, generated)
            .unwrap_or_else(|error| panic!("failed to extend {}: {error}", path.display()));
    }
}

pub fn run(output: &str) {
    let base = Path::new(output);

    // Man pages
    let man_dir = base.join("man/man1");
    fs::create_dir_all(&man_dir).unwrap();

    clap_mangen::generate_to(cli::Cli::command(), &man_dir).expect("failed to generate man pages");
    generate_man_page(blit_gateway_cmd(), &man_dir);
    generate_man_page(blit_webrtc_forwarder_cmd(), &man_dir);

    // Shell completions (for the main blit CLI only)
    generate_completions(cli::Cli::command(), base, "blit");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn generated_shells_call_the_hidden_runtime_query() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output = std::env::temp_dir().join(format!(
            "blit-completion-test-{}-{unique}",
            std::process::id()
        ));
        generate_completions(cli::Cli::command(), &output, "blit");

        let bash =
            fs::read_to_string(output.join("bash-completion/completions/blit.bash")).unwrap();
        let zsh = fs::read_to_string(output.join("zsh/site-functions/_blit")).unwrap();
        let fish = fs::read_to_string(output.join("fish/vendor_completions.d/blit.fish")).unwrap();
        assert!(bash.contains("_blit_with_extension_commands"));
        assert!(bash.contains("__complete-extension"));
        assert!(zsh.contains("functions[_blit_static]=$functions[_blit]"));
        assert!(zsh.contains("__complete-extension"));
        assert!(fish.contains("__fish_blit_extension_commands"));
        assert!(fish.contains("__complete-extension"));

        fs::remove_dir_all(output).unwrap();
    }

    #[test]
    fn hidden_query_is_absent_from_normal_root_help() {
        let help = cli::Cli::command().render_long_help().to_string();
        assert!(!help.contains("__complete-extension"));
    }
}
