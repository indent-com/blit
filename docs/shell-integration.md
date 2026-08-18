# Shell integration

blit consumes **OSC 7** working-directory reports from shells running in its
PTYs ([protocol.md § Working directory tracking](protocol.md#working-directory-tracking)):
the server stores the reported cwd per PTY, pushes `TERM_CWD_EVENT` to clients
when it changes, and answers `TERM_CWD` polls from the stored value instead of
querying the kernel. Everything below is about making the shell _emit_ that
sequence; without it, cwd tracking falls back to a per-poll kernel query
against the PTY child, which misses `cd`s in nested shells and costs a syscall
per poll.

The sequence is `ESC ] 7 ; file://<hostname><absolute-path> BEL` with the path
percent-encoded. Reports naming a foreign hostname are ignored by design — a
shell reached over ssh reports the _remote_ machine's path, which is not a
local path.

## fish

Nothing to do. fish 4.x emits OSC 7 natively at every prompt and directory
change (`man fish-terminal-compatibility`). fish 3.1+ emits it from
`__update_cwd_osc` when it recognizes the terminal.

## zsh

Add to `~/.zshrc`:

```zsh
# Report the working directory to the terminal (OSC 7).
_blit_osc7() {
  local url="file://${HOST}"
  local c ch
  for ((i = 1; i <= ${#PWD}; i++)); do
    ch="${PWD[i]}"
    case "$ch" in
      [-A-Za-z0-9_./~]) url+="$ch" ;;
      *) printf -v c '%%%02X' "'$ch"; url+="$c" ;;
    esac
  done
  printf '\e]7;%s\a' "$url"
}
autoload -Uz add-zsh-hook
add-zsh-hook chpwd _blit_osc7
_blit_osc7
```

## bash

Add to `~/.bashrc`:

```bash
# Report the working directory to the terminal (OSC 7).
_blit_osc7() {
  local url="file://${HOSTNAME}" i ch
  for ((i = 0; i < ${#PWD}; i++)); do
    ch="${PWD:i:1}"
    case "$ch" in
      [-A-Za-z0-9_./~]) url+="$ch" ;;
      *) printf -v ch '%%%02X' "'$ch"; url+="$ch" ;;
    esac
  done
  printf '\e]7;%s\a' "$url"
}
PROMPT_COMMAND="_blit_osc7${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
```

Both snippets are also what other OSC 7 consumers (kitty, foot, WezTerm,
Terminal.app) expect, so they are safe to keep in dotfiles used outside blit.

## OSC 133 — command journal

blit also consumes **OSC 133** semantic-prompt markers (and VS Code's OSC 633
superset) to record each command a shell runs: the command line, when it
started and finished, its exit status, and the region of output that belongs
to it. Agents then ask `blit terminal journal` / `output` / `history --since`
instead of scraping scrollback. Contract:
[design/term-journal.md](design/term-journal.md).

blit does not inject these hooks itself. Starship, kitty, WezTerm, VS Code,
and oh-my-zsh already emit them when their own integration is on; if any of
those is in the PTY, there is nothing to add. Otherwise:

### fish

```fish
function __blit_preexec --on-event fish_preexec
    printf '\e]133;C\a'
end
function __blit_postexec --on-event fish_postexec
    printf '\e]133;D;%s\a' $status
end
function __blit_prompt --on-event fish_prompt
    printf '\e]133;A\a'
    printf '\e]133;B\a'
end
```

### zsh

Add to `~/.zshrc`:

```zsh
_blit_precmd() {
  printf '\e]133;D;%s\a' $?
  printf '\e]133;A\a'
}
_blit_preexec() {
  printf '\e]133;C\a'
}
autoload -Uz add-zsh-hook
add-zsh-hook precmd _blit_precmd
add-zsh-hook preexec _blit_preexec
PS1=$'%{\e]133;B\a%}'$PS1
```

### bash

Add to `~/.bashrc`:

```bash
_blit_prompt() {
  local s=$?
  printf '\e]133;D;%s\a' "$s"
  printf '\e]133;A\a'
}
_blit_preexec() {
  [ -n "${COMP_LINE-}" ] && return
  [[ "$BASH_COMMAND" == "$PROMPT_COMMAND" ]] && return
  printf '\e]133;C\a'
}
PROMPT_COMMAND="_blit_prompt${PROMPT_COMMAND:+;}$PROMPT_COMMAND"
PS1='\[\e]133;B\a\]'$PS1
trap '_blit_preexec' DEBUG
```

The first prompt's `D` is dropped (nothing is running yet). A terminal whose
shell emits nothing keeps an empty journal; `blit terminal journal` says so.
