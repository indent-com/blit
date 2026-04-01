function fish_greeting
    set -l cols (tput cols 2>/dev/null; or echo 80)
    set -l lines \
        ' ____  _     ___ _____' \
        '| __ )| |   |_ _|_   _|' \
        '|  _ \| |    | |  | |' \
        '| |_) | |___ | |  | |' \
        '|____/|_____|___| |_| https://blit.sh' \
        '' \
        'What would be fun to test' \
        'a remote terminal instead?' \
        '' \
        'With ❤️ from https://indent.com'
    for line in $lines
        set -l len (string length -- $line)
        set -l pad (math "$cols - $len")
        if test $pad -gt 0
            printf '%*s%s\n' $pad '' $line
        else
            echo $line
        end
    end
end
