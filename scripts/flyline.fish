# >>> flyline start >>>
# flyline runs as a separate process from a `fish_prompt` event handler: it
# draws its TUI on the tty and returns the chosen line on fd 3. Fail-open — if
# flyline is missing, cancelled, or crashes, native fish handles the line
# unharmed.

if status is-interactive; and not set -q _flyline_loaded
    set -g _flyline_loaded 1
    set -g _flyline_script (status filename)

    # Default to flyline-standalone next to the install dir (parent of scripts/).
    if not set -q FLYLINE_BIN
        set -g FLYLINE_BIN (path resolve (status dirname)/../flyline-standalone)
    end

    # fish 4.x readline sends blocking terminal queries (cursor position,
    # background color, DA1) each prompt. Their replies land while flyline
    # owns the tty and get consumed by its TUI, leaving the query pending
    # forever — fish then dies on `assertion failed: query.is_none()`
    # (reader.rs) at the next prompt, window resize, or theme change.
    # This variable is checked before every query, so setting it here and
    # clearing it in flyline_disable is safely session-scoped. While flyline
    # renders the command line, fish loses nothing it uses.
    set -g FISH_TEST_NO_RECURRENT_QUERIES 1

    # With queries disabled, fish's reader blocks on tty input and drains
    # queued readline functions only when something wakes it — so an
    # `execute` queued from this event handler would sit until the next
    # keypress. A signal wakes the reader; its handler runs `execute` with
    # the reader active, which fish processes immediately.
    function _flyline_do_execute --on-signal SIGUSR2
        commandline -f execute
    end

    function _flyline_edit --on-event fish_prompt
        set -l last_exit $status # capture before anything clobbers $status
        test -x "$FLYLINE_BIN"; or return 0 # fail open

        # flyline reads history from the fish history file; flush this session's
        # first, and tell flyline which session file to read.
        builtin history save 2>/dev/null
        set -l hist_session $fish_history
        test -n "$hist_session"; or set hist_session fish
        set -lx FLYLINE_FISH_HISTORY $__fish_user_data_dir/{$hist_session}_history

        # fish prompts are functions: hand flyline the rendered output (ANSI),
        # covering starship/tide/etc. for free.
        set -lx PS1 (fish_prompt 2>/dev/null | string collect)
        test -n "$PS1"; or set PS1 (printf '%s@%s %s> ' $USER (prompt_hostname) (prompt_pwd))
        set -lx RPS1 ''
        functions -q fish_right_prompt
        and set RPS1 (fish_right_prompt 2>/dev/null | string collect)

        set -lx FLYLINE_HOST fish
        set -lx FLYLINE_INIT (commandline | string collect)
        set -lx FLYLINE_LAST_EXIT $last_exit

        # UI -> /dev/tty, chosen line -> fd 3 -> captured here; keys from the tty.
        set -l cmd ("$FLYLINE_BIN" </dev/tty 3>&1 1>/dev/tty 2>/dev/tty | string collect)
        set -l rc $pipestatus[1]

        if test $rc -eq 0
            # Accept even when empty, so the prompt event re-fires and
            # relaunches flyline (matches the zsh widget's behaviour).
            # Execution is signal-deferred: see _flyline_do_execute above.
            commandline -r -- $cmd
            command fish -c "sleep 0.05; kill -USR2 $fish_pid" &
            builtin disown 2>/dev/null
        else if test $rc -eq 130
            # Ctrl-C: clear to a fresh native line for this prompt.
            commandline -r ''
            commandline -f repaint
        else
            commandline -f repaint
        end
        return 0
    end

    function flyline_enable
        set -e _flyline_loaded
        source $_flyline_script
    end

    function flyline_disable
        functions -e _flyline_edit
        functions -e _flyline_do_execute
        set -e _flyline_loaded
        set -e FISH_TEST_NO_RECURRENT_QUERIES
    end

    function flyline_uninstall
        flyline_disable
        set -e FLYLINE_BIN
    end
end
# <<< flyline end <<<
