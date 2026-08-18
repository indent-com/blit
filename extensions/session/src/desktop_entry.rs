//! XDG desktop-entry parsing, enough to launch an application.
//!
//! Deliberately not a general implementation: this reads the `[Desktop Entry]`
//! group of a `.desktop` file and answers one question — what argv would start
//! this application. Localised names, actions, and MIME associations are not
//! read because nothing here needs them.
//!
//! Kept free of any host or protocol dependency so it can be unit-tested
//! natively, which is the only way to get real coverage of the field-code and
//! quoting rules without a running session.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// One installed application, as far as launching it is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopEntry {
    /// Basename without `.desktop` — the id `@session enable <id>` matches.
    pub id: String,
    /// Human-readable `Name`, or the id when absent.
    pub name: String,
    /// `Exec` with field codes removed and quoting resolved.
    pub argv: Vec<String>,
    /// `Icon`, when set: either a bare name to look up on the icon path or an
    /// absolute path to a file. Kept exactly as written, because deciding which
    /// of the two it is belongs to [`super::icon`] rather than to the parser.
    pub icon: Option<String>,
    /// `StartupWMClass`, when set. Not used for identity — a stamped socket is
    /// the source of truth — but useful to show next to it when they disagree.
    pub startup_wm_class: Option<String>,
    /// `TryExec`, which the caller resolves against PATH before offering the
    /// entry. Kept rather than acted on here so the parser stays pure.
    pub try_exec: Option<String>,
    /// `Terminal=true` — needs a terminal emulator, so it is not launchable as
    /// a bare GUI child.
    pub terminal: bool,
    /// `NoDisplay=true` or `Hidden=true`: present but not something a user
    /// should be offered.
    pub hidden: bool,
}

/// Parse the `[Desktop Entry]` group of one file.
///
/// Returns `None` when the file has no such group, or no `Exec` to run — an
/// entry that cannot be started is not a candidate, whatever else it says.
pub fn parse(id: &str, contents: &str) -> Option<DesktopEntry> {
    let mut in_group = false;
    let mut name = None;
    let mut exec = None;
    let mut try_exec = None;
    let mut icon = None;
    let mut startup_wm_class = None;
    let mut terminal = false;
    let mut hidden = false;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            // Any later group (an action, a MIME block) ends the one we want.
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            // Bare keys only: a `Name[fr]` is a localisation, and taking it
            // would depend on file order rather than on the locale.
            "Name" => name = Some(value.to_string()),
            "Exec" => exec = Some(value.to_string()),
            "TryExec" => try_exec = Some(value.to_string()),
            "Icon" if !value.is_empty() => icon = Some(value.to_string()),
            "StartupWMClass" => startup_wm_class = Some(value.to_string()),
            "Terminal" => terminal = value == "true",
            "NoDisplay" | "Hidden" => hidden |= value == "true",
            _ => {}
        }
    }

    let argv = split_exec(&exec?);
    if argv.is_empty() {
        return None;
    }
    Some(DesktopEntry {
        id: id.to_string(),
        name: name.unwrap_or_else(|| id.to_string()),
        argv,
        icon,
        startup_wm_class,
        try_exec,
        terminal,
        hidden,
    })
}

/// Split an `Exec` value into argv, honouring the spec's quoting and dropping
/// field codes.
///
/// Field codes expand to the files or URLs a launcher was asked to open. There
/// are none here — a supervised application is started with no arguments — so
/// they are removed rather than expanded. `%%` is a literal percent.
fn split_exec(exec: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut has_current = false;
    let mut quoted = false;
    let mut chars = exec.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                // An empty quoted string is still an argument.
                has_current = true;
            }
            '\\' if quoted => {
                // Inside quotes the spec escapes `"`, `\``, `$` and `\`.
                if let Some(next) = chars.next() {
                    current.push(next);
                    has_current = true;
                }
            }
            '%' => match chars.peek() {
                Some('%') => {
                    chars.next();
                    current.push('%');
                    has_current = true;
                }
                // A field code is dropped whole. `%c`/`%k` would expand to the
                // name and path, which a supervised launch has no use for.
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            c if c.is_whitespace() && !quoted => {
                if has_current {
                    argv.push(core::mem::take(&mut current));
                    has_current = false;
                }
            }
            c => {
                current.push(c);
                has_current = true;
            }
        }
    }
    if has_current {
        argv.push(current);
    }
    // A field code alone in an argument leaves it empty — `foo %U` must not
    // become `["foo", ""]`.
    argv.retain(|arg| !arg.is_empty());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_plain_entry_parses() {
        let entry = parse(
            "legcord",
            "[Desktop Entry]\nName=Legcord\nExec=legcord\nType=Application\n",
        )
        .expect("parses");
        assert_eq!(entry.id, "legcord");
        assert_eq!(entry.name, "Legcord");
        assert_eq!(entry.argv, vec!["legcord".to_string()]);
        assert!(!entry.hidden);
        assert!(!entry.terminal);
    }

    /// Field codes are what a launcher substitutes files into. A supervised app
    /// gets no files, so they must vanish rather than reach argv as literals or
    /// as empty strings.
    #[test]
    fn field_codes_are_dropped_without_leaving_empty_arguments() {
        let entry = parse("b", "[Desktop Entry]\nExec=brave %U\n").expect("parses");
        assert_eq!(entry.argv, vec!["brave".to_string()]);

        let entry = parse("c", "[Desktop Entry]\nExec=app --file=%f --url %u\n").expect("parses");
        assert_eq!(
            entry.argv,
            vec![
                "app".to_string(),
                "--file=".to_string(),
                "--url".to_string()
            ]
        );

        // %% is a literal percent, not a field code.
        let entry = parse("d", "[Desktop Entry]\nExec=app 50%%\n").expect("parses");
        assert_eq!(entry.argv, vec!["app".to_string(), "50%".to_string()]);
    }

    #[test]
    fn quoted_arguments_survive_with_their_spaces() {
        let entry = parse(
            "e",
            "[Desktop Entry]\nExec=\"/opt/my app/bin\" --flag \"two words\"\n",
        )
        .expect("parses");
        assert_eq!(
            entry.argv,
            vec![
                "/opt/my app/bin".to_string(),
                "--flag".to_string(),
                "two words".to_string()
            ]
        );
    }

    #[test]
    fn escapes_inside_quotes_are_unwrapped() {
        let entry = parse("f", "[Desktop Entry]\nExec=app \"a\\\"b\"\n").expect("parses");
        assert_eq!(entry.argv, vec!["app".to_string(), "a\"b".to_string()]);
    }

    /// Only the `[Desktop Entry]` group counts: an action group further down
    /// carries its own Exec, and taking it would launch the wrong thing.
    #[test]
    fn a_later_group_does_not_override_the_main_exec() {
        let entry = parse(
            "g",
            "[Desktop Entry]\nExec=real\n\n[Desktop Action new]\nName=New\nExec=wrong --new\n",
        )
        .expect("parses");
        assert_eq!(entry.argv, vec!["real".to_string()]);
    }

    #[test]
    fn hidden_and_nodisplay_both_mark_an_entry_hidden() {
        assert!(
            parse("h", "[Desktop Entry]\nExec=x\nNoDisplay=true\n")
                .expect("parses")
                .hidden
        );
        assert!(
            parse("i", "[Desktop Entry]\nExec=x\nHidden=true\n")
                .expect("parses")
                .hidden
        );
        assert!(
            !parse("j", "[Desktop Entry]\nExec=x\nNoDisplay=false\n")
                .expect("parses")
                .hidden
        );
    }

    #[test]
    fn localised_names_do_not_win_over_the_bare_one() {
        let entry =
            parse("k", "[Desktop Entry]\nName=Real\nName[fr]=Faux\nExec=x\n").expect("parses");
        assert_eq!(entry.name, "Real");
    }

    #[test]
    fn an_entry_without_exec_is_not_a_candidate() {
        assert!(parse("l", "[Desktop Entry]\nName=Link\nURL=https://x\n").is_none());
        // No group at all.
        assert!(parse("m", "Name=stray\nExec=x\n").is_none());
        // Exec present but empty once field codes go.
        assert!(parse("n", "[Desktop Entry]\nExec=%U\n").is_none());
    }

    #[test]
    fn terminal_and_wm_class_and_try_exec_are_reported() {
        let entry = parse(
            "o",
            "[Desktop Entry]\nExec=htop\nTerminal=true\nTryExec=/usr/bin/htop\nStartupWMClass=htop\n",
        )
        .expect("parses");
        assert!(entry.terminal);
        assert_eq!(entry.try_exec.as_deref(), Some("/usr/bin/htop"));
        assert_eq!(entry.startup_wm_class.as_deref(), Some("htop"));
    }

    /// An empty `Icon=` is a key that says nothing, and treating it as a name
    /// would send the lookup after a file called `.png`.
    #[test]
    fn an_icon_is_read_but_an_empty_one_is_not() {
        let entry =
            parse("q", "[Desktop Entry]\nExec=x\nIcon=org.gnome.Nautilus\n").expect("parses");
        assert_eq!(entry.icon.as_deref(), Some("org.gnome.Nautilus"));

        let entry =
            parse("r", "[Desktop Entry]\nExec=x\nIcon=/opt/app/logo.png\n").expect("parses");
        assert_eq!(entry.icon.as_deref(), Some("/opt/app/logo.png"));

        assert!(
            parse("s", "[Desktop Entry]\nExec=x\nIcon=\n")
                .expect("parses")
                .icon
                .is_none()
        );
        assert!(
            parse("t", "[Desktop Entry]\nExec=x\n")
                .expect("parses")
                .icon
                .is_none()
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let entry = parse(
            "p",
            "# a comment\n\n[Desktop Entry]\n# another\nExec=app\n\n",
        )
        .expect("parses");
        assert_eq!(entry.argv, vec!["app".to_string()]);
    }
}
