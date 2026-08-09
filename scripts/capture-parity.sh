#!/bin/zsh
# Capture frozen parity records from the REAL Java toolchain.
#
# Reads one program per line from the file named by $1, in the same
# backslash-n encoding `tests/data/parity_expected.txt` uses, compiles and runs
# each through `javac` + `java`, and writes `program<TAB>output` records to
# stdout in the same encoding.
#
# This script NEVER invokes the javars binary: the corpus is a record of what
# the reference toolchain does, so anything javars produced would make it a
# record of our own output instead. Append its stdout to the corpus; never
# rewrite existing lines.
#
#   scripts/capture-parity.sh new-programs.txt >> tests/data/parity_expected.txt
#
# A program is recorded only when `javac` accepted it AND `java` exited 0 with
# non-empty stdout; anything else is rejected and reported on stderr, so a probe
# that the reference toolchain itself refuses can never enter the corpus.
#
# Override the toolchain with JAVAC / JAVA_ORACLE.
emulate -L zsh
set -uo pipefail

javac=${JAVAC:-$(whence -p javac)}
java=${JAVA_ORACLE:-$(whence -p java)}
src=${1:?usage: capture-parity.sh PROGRAMS-FILE}

for tool in $javac $java; do
    [[ -x $tool ]] || { print -u2 "capture-parity: $tool is not executable"; exit 2 }
done

work=$(mktemp -d) || exit 2
trap 'rm -rf -- $work' EXIT

typeset -i n=0 bad=0
while IFS= read -r line; do
    [[ -z ${line// } ]] && continue
    # The single-file launcher requires the file to be named for its public
    # class, and every corpus program declares `public class T`.
    rm -f -- $work/*.class(N)
    printf '%s' "$line" | command perl -pe 's/\\n/\n/g' > $work/T.java
    if ! (cd $work && $javac T.java) >/dev/null 2>&1; then
        print -u2 "capture-parity: javac rejected: $line"
        (( bad++ ))
        continue
    fi
    out=$( (cd $work && $java -classpath . T) 2>/dev/null )
    rc=$?
    if (( rc != 0 )); then
        print -u2 "capture-parity: program exited $rc: $line"
        (( bad++ ))
        continue
    fi
    if [[ -z $out ]]; then
        print -u2 "capture-parity: program printed nothing: $line"
        (( bad++ ))
        continue
    fi
    # `$(...)` strips trailing newlines; every probe here ends in `println`, so
    # put exactly one back and encode it the way the corpus does.
    printf '%s\t%s\\n\n' "$line" "${out//$'\n'/\\n}"
    (( n++ ))
done < $src

print -u2 "capture-parity: $n record(s) captured, $bad rejected"
(( bad == 0 ))
