# Keep LF line endings: this Bash 3.2-compatible bootstrap is embedded verbatim.
# Profiles run in an interactive shell. Disable terminal job control afterwards
# so background jobs stay in the process group that the daemon owns and reaps.
builtin set +m
# fd 3 is protocol input; fd 4 and fd 5 are private stdout/stderr frame channels.
exec 3<&0 4>&1 5>&2
builtin cd -- "$LOCALSHELLD_INITIAL_CWD" || exit
builtin unset LOCALSHELLD_INITIAL_CWD
trap '' USR1
# `builtin` bypasses any read/printf/eval a command may later define as a function.
while IFS='|' builtin read -r __localshelld_marker __localshelld_encoded __localshelld_input_encoded <&3; do
    builtin printf -v __localshelld_script_path '%b' "$__localshelld_encoded"
    builtin printf -v __localshelld_input '%b' "$__localshelld_input_encoded"
    # Route stdout/stderr separately through fd 4/fd 5, then close fd 3/4/5
    # for its duration: `exec >...` inside a command only rebinds its own fd 1, so
    # the next command still starts attached to the pipe, and the command cannot
    # forge or break the completion frame.
    # A sourced command runs in this same shell (including declare/functions),
    # but gives the interrupt a return boundary that skips the rest of this command.
    __localshelld_interrupted=0
    # A plain return would leave only the innermost user function. After USR1,
    # unwind every remaining sourced/function context before another statement
    # can run; remove this temporary DEBUG trap back in the protocol loop.
    # Bash 3.2 can discard a pending SIGINT when its foreground child dies from
    # SIGKILL instead. USR1 is an internal control signal without that special case.
    builtin trap '__localshelld_interrupted=1; builtin trap '\''if (( ${#BASH_SOURCE[@]} )); then return 130; fi'\'' DEBUG; return 130' USR1
    builtin source "$__localshelld_script_path" <"$__localshelld_input" >&4 2>&5 3<&- 4>&- 5>&-
    __localshelld_ec=$?
    if (( __localshelld_interrupted )); then builtin trap - DEBUG; __localshelld_ec=130; fi
    builtin trap '' USR1
    __localshelld_cwd=$(builtin printf '%s' "$PWD" | /usr/bin/base64 | /usr/bin/tr -d '\r\n')
    builtin printf '\036%s|%s|%s\037' "$__localshelld_marker" "$__localshelld_ec" "$__localshelld_cwd" >&4
    builtin printf '\036%s|%s|%s\037' "$__localshelld_marker" "$__localshelld_ec" "$__localshelld_cwd" >&5
done
