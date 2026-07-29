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
