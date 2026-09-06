# verterm zsh bootstrap. zsh was started with ZDOTDIR pointing here; restore the user's real
# ZDOTDIR so .zprofile/.zshrc/.zlogin load untouched, replay their .zshenv, then add hooks.
if [[ -n "${VERTERM_ORIG_ZDOTDIR:-}" ]]; then
    export ZDOTDIR="$VERTERM_ORIG_ZDOTDIR"
else
    unset ZDOTDIR
fi
unset VERTERM_ORIG_ZDOTDIR

if [[ -r "${ZDOTDIR:-$HOME}/.zshenv" ]]; then
    source "${ZDOTDIR:-$HOME}/.zshenv"
fi

if [[ -o interactive && -r "${VERTERM_SHELL_INTEGRATION_DIR:-}/verterm.zsh" ]]; then
    source "$VERTERM_SHELL_INTEGRATION_DIR/verterm.zsh"
fi
