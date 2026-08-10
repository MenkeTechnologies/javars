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
# It must ALSO mean the same thing under `java T.java`, the entry point
# `tests/parity.rs` replays it through. The two are not interchangeable: the
# JDK's source-file launcher runs the FIRST top-level class in the file, while
# `java -classpath . T` runs the one named on the command line. A program whose
# first declaration is not the main class therefore runs under one and fails
# under the other, and a corpus captured through only the second would be
# replayed against output the replay's own entry point never produces.
# (Four records predating this check declare an `enum` first; see BUGS.md.)
#
# Override the toolchain with JAVAC / JAVA_ORACLE.
emulate -L zsh
set -uo pipefail

javac=${JAVAC:-$(whence -p javac)}
java=${JAVA_ORACLE:-$(whence -p java)}
src=${1:?usage: capture-parity.sh PROGRAMS-FILE}

# Being named `java` on PATH is not evidence of being a JVM launcher: the first
# hit is routinely a version-manager shim, and a broken one exits non-zero
# without a banner. Run each tool before trusting it — a corpus captured from a
# shim that never launched anything would freeze empty output as the reference.
for tool in $javac $java; do
    [[ -x $tool ]] || { print -u2 "capture-parity: $tool is not executable"; exit 2 }
    if ! banner=$($tool --version 2>&1 | head -1) || [[ -z $banner ]]; then
        print -u2 "capture-parity: $tool does not run — \`--version\` failed: $banner"
        print -u2 "capture-parity: set JAVAC / JAVA_ORACLE to a real JDK"
        exit 2
    fi
    print -u2 "capture-parity: $tool — $banner"
done

# Running is not enough either: JDK 19 replaced `Double.toString` with the
# shortest round-tripping decimal, so an older JVM answers `1.0e23` with
# `9.999999999999999E22`. Freezing a corpus from one would bless the old
# rendering as the reference, silently, for every double-valued record.
probe=$(mktemp -d) || exit 2
print 'public class D { public static void main(String[] a) { System.out.println(1.0e23); } }' > $probe/D.java
render=$($java $probe/D.java 2>&1)
rm -rf -- $probe
if [[ $render != '1.0E23' ]]; then
    print -u2 "capture-parity: $java renders 1.0e23 as '$render' — pre-JDK-19 Double.toString"
    print -u2 "capture-parity: JAVA_HOME=${JAVA_HOME:-<unset>}"
    print -u2 "capture-parity: set JAVA_ORACLE to a JDK 19 or newer \`java\`"
    exit 2
fi

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
    # Both entry points, one answer, or the record does not go in.
    src_out=$( (cd $work && $java T.java) 2>/dev/null )
    if (( $? != 0 )) || [[ $src_out != $out ]]; then
        print -u2 "capture-parity: entry points disagree — \`java T.java\` gave '${src_out}', \`java -cp . T\` gave '${out}': $line"
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
