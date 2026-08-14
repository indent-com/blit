//! Hidden runtime completion query used only by generated shell scripts.

use crate::{extension, transport};
use std::time::Duration;

pub(crate) const RUNTIME_COMMAND: &str = "__complete-extension";
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_WORDS: usize = 4_096;
const MAX_WORD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Request {
    current: String,
    words: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Target {
    on: Option<String>,
    hub: String,
}

enum Parsed {
    Other,
    Invalid,
    Request(Request),
}

/// Handle the private query before clap sees it. Invalid input, an offline
/// server, unsupported features, and malformed advertised data all produce no
/// output and a successful process exit, as shell completion requires.
pub(crate) async fn run_if_requested<I, S>(arguments: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let request = match parse(arguments) {
        Parsed::Other => return false,
        Parsed::Invalid => return true,
        Parsed::Request(request) => request,
    };
    let default_hub = std::env::var("BLIT_HUB")
        .unwrap_or_else(|_| blit_webrtc_forwarder::DEFAULT_HUB_URL.to_string());
    let Some(target) = selected_target(&request, &default_hub) else {
        return true;
    };

    let query = async {
        let transport = transport::connect_for_completion(&target.on, &target.hub).await?;
        extension::complete_advertised_commands(transport, &request.words, &request.current).await
    };
    if let Ok(Ok(candidates)) = tokio::time::timeout(COMPLETION_TIMEOUT, query).await {
        for candidate in candidates {
            println!("{candidate}");
        }
    }
    true
}

fn parse<I, S>(arguments: I) -> Parsed
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut arguments = arguments.into_iter().map(Into::into);
    if arguments.next().as_deref() != Some(RUNTIME_COMMAND) {
        return Parsed::Other;
    }

    let Some(current_argument) = arguments.next() else {
        return Parsed::Invalid;
    };
    let current = if let Some(current) = current_argument.strip_prefix("--current=") {
        current.to_string()
    } else if current_argument == "--current" {
        let Some(current) = arguments.next() else {
            return Parsed::Invalid;
        };
        current
    } else {
        return Parsed::Invalid;
    };
    if arguments.next().as_deref() != Some("--") {
        return Parsed::Invalid;
    }
    let words = arguments.collect::<Vec<_>>();
    let total_bytes = words
        .iter()
        .try_fold(current.len(), |total, word| total.checked_add(word.len()));
    if words.len() > MAX_WORDS || total_bytes.is_none_or(|total| total > MAX_WORD_BYTES) {
        return Parsed::Invalid;
    }
    Parsed::Request(Request { current, words })
}

/// Select connection flags only before the `@name` boundary. Everything after
/// that boundary belongs to the extension command and is never reinterpreted.
fn selected_target(request: &Request, default_hub: &str) -> Option<Target> {
    let mut on = None;
    let mut hub = default_hub.to_string();
    let mut index = 0usize;
    while index < request.words.len() {
        let word = &request.words[index];
        match word.as_str() {
            "--json" => {}
            "--on" => {
                index += 1;
                on = Some(request.words.get(index)?.clone());
            }
            "--hub" => {
                index += 1;
                hub = request.words.get(index)?.clone();
            }
            _ if word.starts_with("--on=") => {
                on = Some(word[5..].to_string());
            }
            _ if word.starts_with("--hub=") => {
                hub = word[6..].to_string();
            }
            _ if valid_namespace_word(word) => {
                return Some(Target { on, hub });
            }
            _ => return None,
        }
        index += 1;
    }

    (request.current.is_empty() || request.current.starts_with('@')).then_some(Target { on, hub })
}

fn valid_namespace_word(word: &str) -> bool {
    let Some(name) = word.strip_prefix('@') else {
        return false;
    };
    !name.is_empty()
        && name.len() <= blit_remote::extension::EXT_MAX_NAME
        && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(current: &str, words: &[&str]) -> Request {
        Request {
            current: current.into(),
            words: words.iter().map(|word| (*word).into()).collect(),
        }
    }

    #[test]
    fn hidden_query_parser_is_exact_and_bounded() {
        let Parsed::Request(parsed) = parse([
            RUNTIME_COMMAND,
            "--current=bu",
            "--",
            "--on",
            "prod",
            "@builder",
        ]) else {
            panic!("valid hidden query was not parsed");
        };
        assert_eq!(parsed, request("bu", &["--on", "prod", "@builder"]));
        assert!(matches!(parse(["terminal", "list"]), Parsed::Other));
        assert!(matches!(
            parse([RUNTIME_COMMAND, "--current=x"]),
            Parsed::Invalid
        ));
        let too_many = std::iter::once(RUNTIME_COMMAND.to_string())
            .chain(["--current=".into(), "--".into()])
            .chain(std::iter::repeat_n("x".to_string(), MAX_WORDS + 1));
        assert!(matches!(parse(too_many), Parsed::Invalid));
    }

    #[test]
    fn target_parser_stops_at_extension_namespace() {
        let target = selected_target(
            &request(
                "--remote-option",
                &[
                    "--on=prod",
                    "--json",
                    "--hub",
                    "https://hub.example",
                    "@builder",
                    "deploy",
                    "--on",
                    "guest-value",
                ],
            ),
            "default-hub",
        )
        .unwrap();
        assert_eq!(target.on.as_deref(), Some("prod"));
        assert_eq!(target.hub, "https://hub.example");

        assert!(selected_target(&request("", &["terminal"]), "hub").is_none());
        assert!(selected_target(&request("prod", &["--on"]), "hub").is_none());
        assert!(selected_target(&request("@bu", &[]), "hub").is_some());
    }
}
