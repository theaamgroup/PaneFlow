#!/usr/bin/env bash
# Windows-residue census for the mac-only fork (stage 2b).
#
# There is no compiler worklist for this pass: `target_os` is a real cfg value,
# so it never triggers `unexpected_cfgs` the way the Ghostty feature removal did.
# This script is the substitute acceptance test. It must print 0 when 2b is done.
#
# It deliberately matches BOTH spellings. `#[cfg(windows)]` and
# `#[cfg(target_os = "windows")]` are the same thing, and a grep for only the
# latter misses 228 of 396 sites across 25 files.
set -uo pipefail
cd "$(dirname "$0")/.."

scan() { grep -rn --include='*.rs' -E 'cfg!?\(|cfg_attr' . 2>/dev/null | grep -v '^\./target/'; }

# Strip pure-comment lines: a doc comment explaining WHY an item is
# `#[cfg(unix)]`-gated legitimately mentions Windows and must not block the
# zero-condition. They are counted separately for review.
nocomment() { grep -vE ':[0-9]+:[[:space:]]*(//|/\*|\*)'; }
onlycomment() { grep -E ':[0-9]+:[[:space:]]*(//|/\*|\*)'; }

ATTR=$(scan | grep -v 'cfg!(' | grep -Ei '\bwindows\b|"msvc"' | nocomment)
MACRO=$(scan | grep    'cfg!(' | grep -Ei '\bwindows\b|"msvc"' | nocomment)
COMMENTS=$(scan | grep -Ei '\bwindows\b|"msvc"' | onlycomment)
TOML=$(grep -rn "target\.'cfg(windows)'" --include='Cargo.toml' . 2>/dev/null | grep -v '^\./target/')
IDENT=$(grep -rn --include='*.rs' 'windows_app_identity' . 2>/dev/null | grep -v '^\./target/')
UNIXMAC=$(scan | grep -E '\bunix\b' | grep -E 'not\s*\(\s*target_os\s*=\s*"macos"')
# Target-triple STRING checks. These are not cfg constructs at all, so no cfg
# regex can see them -- found only by searching a different term space.
STRCHK=$(grep -rn --include='*.rs' -E '"[^"]*(windows|msvc)[^"]*"' . 2>/dev/null | grep -v '^\./target/' \
         | grep -E '\.contains\(|\.starts_with\(|\.ends_with\(|==|!=' | grep -v 'cfg!')

n() { [ -z "$1" ] && echo 0 || printf '%s\n' "$1" | wc -l | tr -d ' '; }

# Line-oriented greps CANNOT see a cfg expression split across newlines --
# `cfg!(` and the `target_os = "windows"` arm land on different lines. This
# python pass balances parens across newlines and caught a real site in
# terminal/view.rs that every grep above missed.
MULTILINE=$(python3 - <<'PYEOF'
import re,os
out=[]
for root,dirs,files in os.walk('.'):
    dirs[:] = [d for d in dirs if d not in ('target','.git')]
    for f in files:
        if not f.endswith('.rs'): continue
        p=os.path.join(root,f); s=open(p,encoding='utf-8',errors='replace').read()
        for m in re.finditer(r'cfg!?\s*\(|cfg_attr\s*\(', s):
            i=m.end()-1; depth=0; j=i
            while j < len(s):
                if s[j]=='(': depth+=1
                elif s[j]==')':
                    depth-=1
                    if depth==0: break
                j+=1
            expr=s[m.start():j+1]
            if '\n' in expr and re.search(r'\bwindows\b|"msvc"', expr):
                out.append(f"{p}:{s[:m.start()].count(chr(10))+1}: {' '.join(expr.split())[:120]}")
print('\n'.join(out))
PYEOF
)

echo "attribute cfg gates .......... $(n "$ATTR")"
echo "runtime cfg!() expressions ... $(n "$MACRO")"
echo "Cargo target tables .......... $(n "$TOML")"
echo "target-triple string checks .. $(n "$STRCHK")"
echo "multi-line cfg expressions ... $(n "$MULTILINE")"
echo "comment-only references ...... $(n "$COMMENTS")   [review, not blocking]"
echo "windows_app_identity refs .... $(n "$IDENT")   [expected 2 until stage 2c]"
echo "all(unix, not(macos)) ........ $(n "$UNIXMAC")   [expected 6 until stage 2c]"
echo "files touched ................ $(printf '%s\n' "$ATTR" "$MACRO" | grep -c . >/dev/null; printf '%s\n' "$ATTR" "$MACRO" | grep . | cut -d: -f1 | sort -u | wc -l | tr -d ' ')"
echo "---"
TOTAL=$(( $(n "$ATTR") + $(n "$MACRO") + $(n "$TOML") + $(n "$STRCHK") + $(n "$MULTILINE") ))
echo "STAGE 2b ZERO-CONDITION: $TOTAL   (must reach 0)"

if [ "${1:-}" = "--list" ]; then
  echo; echo "=== attribute gates ==="; printf '%s\n' "$ATTR"
  echo; echo "=== runtime cfg!() ===";  printf '%s\n' "$MACRO"
  echo; echo "=== Cargo tables ===";    printf '%s\n' "$TOML"
  echo; echo "=== triple string checks ==="; printf '%s\n' "$STRCHK"
  echo; echo "=== multi-line cfg ==="; printf '%s\n' "$MULTILINE"
fi
exit 0
