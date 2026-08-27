#!/usr/bin/env bash
# Linux-residue census for the mac-only fork (stage 2c).
#
# Derived from scripts/win-census.sh by swapping the predicate. Same four
# trust properties: negative control, comment-only separation, a multi-line
# paren-balancing pass, and a sweep over a DIFFERENT term space.
#
# 2c covers THREE predicate families, not one:
#   target_os = "linux"        -> delete
#   not(unix)                  -> fallback arm; delete, UN-GATE the twin
#   not(target_os = "macos")   -> reduce
#
# cfg(unix) and cfg(target_os = "macos") are the LIVE arms on macOS and are
# never counted here. macOS IS a unix.
#
# Exit status (issue #69): 0 when the STAGE 2c zero-condition is 0 AND the
# negative control is non-zero; 1 otherwise, with a `FAIL:` line on stderr
# and the offending sites listed. `--files` / `--terms` are inspection
# modes and always exit 0. run_tests.yml::platform_census runs this.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

# A grep over `path:line:content` also matches the PATH. Files under
# `src-app/src/update/linux/` therefore matched /\blinux\b/ on their
# directory name, counting every `#[cfg(unix)]` and `#[cfg(test)]` inside them
# as a Linux site -- the precise error this whole pass exists to avoid.
# Match the CONTENT only; keep the prefix for reporting.
content_match() {
  python3 -W ignore -c '
import sys, re, warnings
warnings.filterwarnings("ignore")
pat = re.compile(sys.argv[1], re.I)
for line in sys.stdin:
    m = re.match(r"^(.*?:\d+:)(.*)$", line.rstrip("\n"))
    if m and pat.search(m.group(2)):
        print(line, end="")
' "$1"
}

scan() { grep -rn --include='*.rs' -E 'cfg!?\(|cfg_attr' . 2>/dev/null | grep -v '^\./target/'; }
nocomment() { grep -vE ':[0-9]+:[[:space:]]*(//|/\*|\*)'; }
onlycomment() { grep -E ':[0-9]+:[[:space:]]*(//|/\*|\*)'; }

P_LINUX='\blinux\b|"musl"|unknown-linux'
P_NOTUNIX='not\s*\(\s*unix\s*\)'
P_NOTMAC='not\s*\(\s*target_os\s*=\s*"macos"\s*\)'
P_ALL="$P_LINUX|$P_NOTUNIX|$P_NOTMAC"

ATTR=$(scan | grep -v 'cfg!(' | content_match "$P_ALL" | nocomment)
MACRO=$(scan | grep    'cfg!(' | content_match "$P_ALL" | nocomment)
COMMENTS=$(scan | content_match "$P_ALL" | onlycomment)
TOML=$(grep -rn -E "target\.'cfg\((unix|target_os = \"linux\")\)'" --include='Cargo.toml' . 2>/dev/null | grep -v '^\./target/')

# Operator-negated cfg! macros naming a target_os (`!cfg!(target_os = "...")`).
# Invisible to P_NOTMAC, which only matches the `not(...)` predicate form.
# Requires `target_os` so `!cfg!(debug_assertions)` is not counted.
BANGCFG=$(scan | grep 'cfg!(' | content_match '!\s*cfg!\s*\(\s*target_os\s*=' | nocomment)

# Per-family breakdown (code lines only)
F_LINUX=$(printf '%s\n' "$ATTR" "$MACRO" | grep . | content_match "$P_LINUX")
F_NOTUNIX=$(printf '%s\n' "$ATTR" "$MACRO" | grep . | content_match "$P_NOTUNIX")
F_NOTMAC=$(printf '%s\n' "$ATTR" "$MACRO" | grep . | content_match "$P_NOTMAC")

# The 5 sites 2b left standing: all(unix, not(macos)) are now PURE LINUX.
UNIXMAC=$(scan | content_match '\bunix\b' | content_match "$P_NOTMAC" | nocomment)

# NEGATIVE CONTROL: cfg(unix) must stay huge. If this reads 0 the regex is broken.
CTL_UNIX=$(scan | content_match 'cfg!?\s*\(\s*unix\s*\)|cfg_attr\s*\(\s*unix' | nocomment)
CTL_MAC=$(scan | content_match 'target_os\s*=\s*"macos"' | grep -vE 'not[[:space:]]*\([[:space:]]*target_os' | nocomment)

# Target-triple STRING checks -- not cfg constructs, invisible to any cfg regex.
STRCHK=$(grep -rn --include='*.rs' -E '"[^"]*(linux|musl|gnueabi)[^"]*"' . 2>/dev/null | grep -v '^\./target/' | content_match '"[^"]*(linux|musl|gnueabi)[^"]*"' \
         | grep -E '\.contains\(|\.starts_with\(|\.ends_with\(|==|!=' | grep -v 'cfg!')

n() { [ -z "$1" ] && echo 0 || printf '%s\n' "$1" | wc -l | tr -d ' '; }

MULTILINE=$(python3 - <<'PYEOF'
import re,os
pat = re.compile(r'\blinux\b|"musl"|unknown-linux|not\s*\(\s*unix\s*\)|not\s*\(\s*target_os\s*=\s*"macos"\s*\)')
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
            if '\n' in expr and pat.search(expr):
                out.append(f"{p}:{s[:m.start()].count(chr(10))+1}: {' '.join(expr.split())[:120]}")
print('\n'.join(out))
PYEOF
)

# DIFFERENT TERM SPACE. Re-running the cfg grep only reproduces its own blind
# spots. This is what found the orphaned window_chrome/backdrop.rs in 2b.
TERMS=$(grep -rnE --include='*.rs' \
  '/proc/|/sys/|XDG_[A-Z]|\.desktop\b|AppImage|appimage|dpkg|\brpm\b|apt-get|\bdnf\b|zypper|pkexec|polkit|flatpak|Flatpak|\bsnap\b|Snapd|zsync|systemd|wayland|Wayland|X11|dbus|DBus|notify-rust|ostree|gtk|Gtk' \
  . 2>/dev/null | grep -v '^\./target/' | nocomment)

# UNGATED PLATFORM STRINGS (issue #103).
# A STAGE 2c ZERO-CONDITION of 0 is cfg-predicates only and is blind to this
# class: Windows executable-suffix / PowerShell / AppData path handling that
# arrives with no #[cfg] at all. Reported separately; not added to the
# zero-condition integer. Suffixes use a trailing word boundary so `.execute`
# is not counted as `.exe`.
UNGATED_STR=$(grep -rn --include='*.rs' -E \
  'powershell|\.exe\b|\.cmd\b|\.bat\b|\.ps1\b|\\\\\?\\|%APPDATA%' \
  . 2>/dev/null | grep -v '^\./target/' | nocomment)

# ORPHAN CHECK: a .rs file on disk that no `mod` declaration reaches is
# invisible to every cfg scan precisely because it no longer compiles.
ORPHANS=$(python3 - <<'PYEOF'
import os,re
mods=set()
for root,dirs,files in os.walk('.'):
    dirs[:] = [d for d in dirs if d not in ('target','.git')]
    for f in files:
        if not f.endswith('.rs'): continue
        s=open(os.path.join(root,f),encoding='utf-8',errors='replace').read()
        for m in re.finditer(r'\bmod\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*[;{]', s):
            mods.add(m.group(1))
out=[]
for root,dirs,files in os.walk('.'):
    dirs[:] = [d for d in dirs if d not in ('target','.git')]
    for f in files:
        if not f.endswith('.rs'): continue
        stem=f[:-3]
        if stem in ('mod','lib','main','build'): continue
        p=os.path.join(root,f)
        # crate roots / integration tests are reached without a `mod` line
        if '/tests/' in p or '/benches/' in p or '/examples/' in p: continue
        if stem not in mods and os.path.basename(root) not in mods:
            out.append(p)
print('\n'.join(sorted(out)))
PYEOF
)

MODE="${1:-}"
TOTAL=$(( $(n "$ATTR") + $(n "$MACRO") + $(n "$TOML") + $(n "$STRCHK") + $(n "$MULTILINE") + $(n "$BANGCFG") ))

if [ -z "$MODE" ] || [ "$MODE" = "--list" ]; then
echo "── NEGATIVE CONTROL (must stay LARGE; 0 means the regex is broken) ──"
echo "cfg(unix) live sites ......... $(n "$CTL_UNIX")"
echo "cfg(macos) live sites ........ $(n "$CTL_MAC")"
echo
echo "── ZERO-CONDITION COMPONENTS ──"
echo "attribute cfg gates .......... $(n "$ATTR")"
echo "runtime cfg!() expressions ... $(n "$MACRO")"
echo "Cargo target tables .......... $(n "$TOML")"
echo "target-triple string checks .. $(n "$STRCHK")"
echo "multi-line cfg expressions ... $(n "$MULTILINE")"
echo "negated cfg!(target_os) ....... $(n "$BANGCFG")"
echo
echo "  by family (attr+macro):"
echo "    target_os=linux .......... $(n "$F_LINUX")"
echo "    not(unix) ................ $(n "$F_NOTUNIX")"
echo "    not(macos) ............... $(n "$F_NOTMAC")"
echo "    all(unix,not(macos)) ..... $(n "$UNIXMAC")   [2b left these standing; 2c kills them]"
echo
echo "── NON-BLOCKING, REVIEW ──"
echo "comment-only references ...... $(n "$COMMENTS")"
echo "different-term-space hits .... $(n "$TERMS")"
echo "orphaned .rs files ........... $(n "$ORPHANS")"
echo "files touched ................ $(printf '%s\n' "$ATTR" "$MACRO" | grep . | cut -d: -f1 | sort -u | wc -l | tr -d ' ')"
echo "---"
echo "STAGE 2c ZERO-CONDITION: $TOTAL   (must reach 0)"
echo
echo "── UNGATED PLATFORM STRINGS ──"
echo "issue #103 (not in STAGE 2c integer) ... $(n "$UNGATED_STR")"
fi

if [ "$MODE" = "--list" ]; then
  echo; echo "=== attribute gates ==="; printf '%s\n' "$ATTR"
  echo; echo "=== runtime cfg!() ===";  printf '%s\n' "$MACRO"
  echo; echo "=== negated cfg!(target_os) ==="; printf '%s\n' "$BANGCFG"
  echo; echo "=== Cargo tables ===";    printf '%s\n' "$TOML"
  echo; echo "=== triple string checks ==="; printf '%s\n' "$STRCHK"
  echo; echo "=== multi-line cfg ==="; printf '%s\n' "$MULTILINE"
  echo; echo "=== orphans ==="; printf '%s\n' "$ORPHANS"
  echo; echo "=== ungated platform strings ==="; printf '%s\n' "$UNGATED_STR"
fi

# Inspection modes stay exit 0 so they remain usable interactively.
if [ "$MODE" = "--files" ]; then
  printf '%s\n' "$ATTR" "$MACRO" | grep . | cut -d: -f1 | sort | uniq -c | sort -rn
  exit 0
fi
if [ "$MODE" = "--terms" ]; then printf '%s\n' "$TERMS"; exit 0; fi

# ENFORCEMENT (issue #69). The zero-condition total is the exit status so
# CI (run_tests.yml::platform_census) can block a PR. The negative control
# is checked FIRST: a census that reads 0 because its regex broke must not
# look like a clean tree. The ungated-string report (issue #103) is
# deliberately not part of the exit status: it is not in the STAGE 2c
# integer and no expected count is tracked for it.
dump() { if [ -n "$2" ]; then { echo; echo "=== $1 ==="; printf '%s\n' "$2"; } >&2; fi; }
if [ "$(n "$CTL_UNIX")" -eq 0 ] || [ "$(n "$CTL_MAC")" -eq 0 ]; then
  echo "FAIL: negative control collapsed to 0 (cfg(unix)=$(n "$CTL_UNIX"), cfg(macos)=$(n "$CTL_MAC")) - the regex is broken, not the tree clean" >&2
  exit 1
fi
if [ "$TOTAL" -ne 0 ]; then
  echo "FAIL: STAGE 2c ZERO-CONDITION is $TOTAL, must be 0" >&2
  dump "attribute gates" "$ATTR"
  dump "runtime cfg!()" "$MACRO"
  dump "negated cfg!(target_os)" "$BANGCFG"
  dump "Cargo tables" "$TOML"
  dump "triple string checks" "$STRCHK"
  dump "multi-line cfg" "$MULTILINE"
  exit 1
fi
exit 0
