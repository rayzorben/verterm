# verterm shell integration for zsh: OSC 7 (cwd) + OSC 133 (prompt / command / exit code).
if [[ -n "${VERTERM_ZSH_INTEGRATION_LOADED:-}" ]]; then return 0; fi
VERTERM_ZSH_INTEGRATION_LOADED=1

autoload -Uz add-zsh-hook

__verterm_osc() { printf '\e]%s\a' "$1" }

__verterm_precmd() {
    local ec=$?
    __verterm_osc "133;D;$ec"
    __verterm_osc "7;file://${HOST}${PWD}"
    __verterm_osc "133;A"
}

# $1 is the command line about to run. Control characters are collapsed so a command
# containing one cannot terminate the OSC early.
__verterm_preexec() { __verterm_osc "133;C;${1//[[:cntrl:]]/ }" }

add-zsh-hook precmd __verterm_precmd
add-zsh-hook preexec __verterm_preexec
