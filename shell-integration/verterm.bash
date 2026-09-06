# verterm shell integration for bash.
# Loaded via `bash --rcfile`, so first replay what an interactive bash would have sourced,
# then install OSC 7 (cwd) and OSC 133 (prompt / command / exit-code) hooks.
if [[ -n "${VERTERM_BASH_INTEGRATION_LOADED:-}" ]]; then return 0; fi
VERTERM_BASH_INTEGRATION_LOADED=1

if [[ -r /etc/bash.bashrc ]]; then source /etc/bash.bashrc; fi
if [[ -r "$HOME/.bashrc" ]]; then source "$HOME/.bashrc"; fi

__verterm_osc() { printf '\e]%s\a' "$1"; }

__verterm_precmd() {
    local ec=$?
    __verterm_osc "133;D;$ec"
    __verterm_osc "7;file://${HOSTNAME:-$(hostname)}${PWD}"
    __verterm_osc "133;A"
    return $ec
}

# PROMPT_COMMAND may be an array (bash >= 5.1) or a string.
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
    PROMPT_COMMAND=(__verterm_precmd "${PROMPT_COMMAND[@]}")
else
    PROMPT_COMMAND="__verterm_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
fi

# PS0 is expanded after a command line is read and before it executes → command start.
# bash has no preexec hook, so the command text comes from the history entry that reading the
# line just appended. `promptvars` (on by default) is what makes the command substitution in
# PS0 run; without it the literal `$(...)` would be printed at every prompt, so fall back to
# the bare marker. Note: with `HISTCONTROL=ignorespace` a command typed with a leading space
# is never added, and `history 1` then reports the previous one — bash cannot do better.
__verterm_ps0() {
    local line
    line=$(HISTTIMEFORMAT= builtin history 1)
    line=${line#"${line%%[![:space:]]*}"}   # drop leading blanks
    line=${line#* }                          # drop the history number
    line=${line#"${line%%[![:space:]]*}"}   # drop the blanks after it
    printf '\e]133;C;%s\a' "${line//[[:cntrl:]]/ }"
}
if shopt -q promptvars; then
    PS0='$(__verterm_ps0)'"${PS0:-}"
else
    PS0='\e]133;C\a'"${PS0:-}"
fi
# Mark the end of the prompt so verterm knows where input begins.
PS1="${PS1}"'\[\e]133;B\a\]'
