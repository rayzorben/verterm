# verterm shell integration for fish: OSC 7 (cwd) + OSC 133 (prompt / command / exit code).
if not set -q __verterm_fish_loaded
    set -g __verterm_fish_loaded 1

    function __verterm_osc
        printf '\e]%s\a' $argv[1]
    end

    function __verterm_precmd --on-event fish_prompt
        set -l ec $status
        __verterm_osc "133;D;$ec"
        __verterm_osc "7;file://$hostname$PWD"
        __verterm_osc "133;A"
    end

    # $argv[1] is the command line about to run. Control characters are collapsed so a
    # command containing one cannot terminate the OSC early.
    function __verterm_preexec --on-event fish_preexec
        __verterm_osc "133;C;"(string replace -ra '[[:cntrl:]]' ' ' -- "$argv[1]")
    end
end
