#!/usr/bin/env zsh
# The repo as a zim module (plan §10.2, README): `zmodule cmprmsd/cinelog --on-pull 'zsh
# shell/fetch.zsh'` installed and updated by a real zimfw.
#
#   zsh shell/tests/zim_test.zsh [-v]
#
# Covers zimfw 1.20.2: that exact release is downloaded and must match the sha256 below before
# it runs. CINELOG_ZIMFW=<path> runs a local zimfw.zsh instead (unverified, e.g. to try a newer
# zimfw). github.com is redirected to local bare repos (git `insteadOf` in a private
# GIT_CONFIG_GLOBAL), and the release to a local directory with a fake binary
# (CINELOG_FETCH_BASE_URL), so the only network access is that one download.
# Without zimfw.zsh the test is skipped (exit 0) unless CINELOG_ZIM_TEST_STRICT=1; a download
# with the wrong checksum always fails.
# Checks: zimfw picks cinelog.plugin.zsh as the module's only init script and adds no fpath;
# the on-pull hook installs the verified binary (install and update); the compiled module
# starts the binary in an interactive shell; nothing is left behind outside a temp dir.

emulate -R zsh
setopt extendedglob nomultios
zmodload zsh/zpty zsh/datetime zsh/zselect

typeset -g ROOT=${${(%):-%x}:A:h:h:h} VERBOSE=0
[[ $1 == -v ]] && VERBOSE=1
for tool in git sha256sum; do
  (( $+commands[$tool] )) || { print -u2 "zim_test: $tool is required"; exit 2; }
done
export LC_ALL=C.UTF-8

typeset -g TMP=$(mktemp -d ${TMPDIR:-/tmp}/cinelog-zimtest.XXXXXX)
typeset -gi PASS=0 FAIL=0 T_FAIL=0
cleanup() {
  zpty -d z 2>/dev/null
  [[ -d $TMP ]] && rm -rf -- $TMP
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

fail() { print -r -- "    FAIL: $*"; T_FAIL=1; }
eq() { [[ $1 == "$2" ]] || fail "$3"$'\n'"      expected: ${(q+)2}"$'\n'"      actual:   ${(q+)1}"; }

# ------------------------------------------------------------------------------- zimfw

typeset -gr ZIMFW_VERSION=1.20.2
typeset -gr ZIMFW_SHA256=918e8948d08346d5f1373ec628d858e4732fbbb9991b25c22d1a9b3443a2edc2
typeset -g ZIMFW=$TMP/zimfw.zsh
if [[ -n $CINELOG_ZIMFW ]]; then
  cp -- $CINELOG_ZIMFW $ZIMFW || exit 2
elif ! { (( $+commands[curl] )) &&
         curl -fsSL --connect-timeout 15 --max-time 60 -o $ZIMFW \
           https://github.com/zimfw/zimfw/releases/download/v$ZIMFW_VERSION/zimfw.zsh; }; then
  if [[ $CINELOG_ZIM_TEST_STRICT == 1 ]]; then print -u2 "zim_test: cannot get zimfw.zsh"; exit 2; fi
  print -r -- "zim tests: skipped (no zimfw.zsh; set CINELOG_ZIMFW)"
  exit 0
elif [[ ${${(z)"$(sha256sum -- $ZIMFW)"}[1]} != $ZIMFW_SHA256 ]]; then
  print -u2 "zim_test: zimfw.zsh v$ZIMFW_VERSION does not match its pinned sha256; not running it"
  exit 2
fi

# ------------------------------------------------------------------------------ fixture

# Sandbox: HOME, ZDOTDIR, XDG dirs, ZIM_HOME, a git config that maps github.com to $TMP/gh.
typeset -g H=$TMP/home ZIM=$TMP/home/.zim GH=$TMP/gh REL=$TMP/release LOG=$TMP/home/fake.log
mkdir -p $H/run $H/state $H/config $GH/cmprmsd $REL
chmod 700 $H/run
print -r -- "[url \"file://$GH/\"]
	insteadOf = https://github.com/
[protocol \"file\"]
	allow = always" >$TMP/gitconfig
typeset -ga SANDBOX=(HOME=$H ZDOTDIR=$H ZIM_HOME=$ZIM XDG_RUNTIME_DIR=$H/run
  XDG_STATE_HOME=$H/state XDG_CONFIG_HOME=$H/config GIT_CONFIG_GLOBAL=$TMP/gitconfig
  GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 CINELOG_FETCH_BASE_URL=file://$REL
  PATH=$PATH TERM=xterm-256color LC_ALL=C.UTF-8 TMPDIR=$TMP)
gitq() { git -c user.name=zim-test -c user.email=zim-test@invalid -c init.defaultBranch=main \
  -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@" >/dev/null 2>&1; }

typeset -g TARGET
case $(uname -m) in
  x86_64|amd64) TARGET=x86_64-unknown-linux-musl ;;
  aarch64|arm64) TARGET=aarch64-unknown-linux-musl ;;
  *) print -r -- "zim tests: skipped (no release target for $(uname -m))"; exit 0 ;;
esac

# Publishes a release of version $1: a fake binary that logs its argv and answers like the
# real one where the module needs it.
release() {
  local ver=$1
  mkdir -p $REL/v$ver
  print -r -- "#!/bin/sh
printf '%s\n' \"\$*\" >>'$LOG'
case \"\$1 \$2\" in
  '--version '*) echo 'cinelog $ver' ;;
  'rec --check') echo 'skip zim-test' ;;
  'rec --protocol') echo 1 ;;
esac
exit 0" >$REL/v$ver/cinelog-$TARGET
  ( cd $REL/v$ver && sha256sum cinelog-$TARGET >SHA256SUMS )
}

# Commits the module files of this checkout with VERSION $1 and its checksum file (the trust
# anchor the release workflow commits) to the bare repo github.com/cmprmsd/cinelog.
typeset -g WORK=$TMP/work
publish() {
  local ver=$1
  if [[ ! -d $WORK ]]; then
    gitq init $WORK && gitq -C $WORK remote add origin $GH/cmprmsd/cinelog.git
  fi
  cp -- $ROOT/cinelog.plugin.zsh $ROOT/LICENSE $ROOT/.gitignore $WORK/
  rm -rf -- $WORK/shell; cp -R -- $ROOT/shell $WORK/shell
  rm -f -- $WORK/shell/**/*.zwc(N)
  print -r -- $ver >$WORK/VERSION
  cp -- $REL/v$ver/SHA256SUMS $WORK/shell/checksums/v$ver.sha256
  gitq -C $WORK add -A && gitq -C $WORK commit -m "v$ver" && gitq -C $WORK push origin main
}

zimfw() { env -i $SANDBOX zsh $ZIMFW "$@" 2>&1; }

gitq init --bare $GH/cmprmsd/cinelog.git || { print -u2 "zim_test: git init failed"; exit 2; }
typeset -g VER=$(<$ROOT/VERSION)
release $VER
publish $VER
print -r -- "zstyle ':zim' disable-version-check yes" >$H/.zshenv
print -r -- "zmodule cmprmsd/cinelog --on-pull 'zsh shell/fetch.zsh'" >$H/.zimrc
print -r -- 'PS1="PROMPT%!> "; unsetopt PROMPT_SP
source $ZIM_HOME/init.zsh' >$H/.zshrc
typeset -g MOD=$ZIM/modules/cinelog

# ------------------------------------------------------------------------------- tests

t_install_runs_the_fetch_hook() {
  local out
  out=$(zimfw install -v) || fail "zimfw install failed: $out"
  (( VERBOSE )) && print -r -- "${(F)${(f)out}/#/      }"
  [[ -x $MOD/bin/cinelog ]] || { fail "on-pull did not install bin/cinelog: $out"; return; }
  eq "$($MOD/bin/cinelog --version)" "cinelog $VER" "the released binary"
  [[ -e $MOD/bin/cinelog.new ]] && fail "no leftover download"
  [[ $out == *'not in the repo yet'* ]] && fail "verified against the repo checksum, not the release"
  local log=$(<$LOG)
  [[ $log == *'install --refresh'* ]] || fail "fetch ran 'install --refresh': ${(q+)log}"
  eq "$(git -C $MOD status --porcelain --ignored -- bin)" '!! bin/' "bin/ is ignored by git"
  eq "$(git -C $MOD status --porcelain)" '' "the checkout stays clean (compiled .zwc ignored too)"
}

t_module_init_is_the_plugin() {
  local init=$(<$ZIM/init.zsh)
  local -a src=(${(M)${(f)init}:#*/modules/cinelog/*})
  eq "${#src}" 1 "one line for the module in init.zsh: ${(q+)init}"
  [[ $src[1] == 'source '*'/modules/cinelog/cinelog.plugin.zsh"' ]] || fail "it sources cinelog.plugin.zsh: $src"
  [[ $init == *fpath*cinelog* ]] && fail "no functions/ dir, so no fpath entry from zim"
  [[ -e $MOD/cinelog.plugin.zsh.zwc ]] || fail "zimfw compiled the plugin"
}

t_shell_loads_the_module() {
  : >$LOG
  local -a envq=(${(q)SANDBOX})
  zpty z "exec env -i $envq zsh -i"
  local buf= chunk
  local -F deadline=$(( EPOCHREALTIME + 10 ))
  while [[ $buf != *'PROMPT'<->'> '* ]] && (( EPOCHREALTIME < deadline )); do
    if zpty -rt z chunk 2>/dev/null; then buf+=$chunk; else zselect -t 2; fi
  done
  [[ $buf == *'PROMPT'<->'> '* ]] || fail "no prompt: ${(q+)buf[-400,-1]}"
  zpty -w z 'print -r -- "<${+functions[cinelog-fetch]} ${+functions[_cl_preexec]} ${+functions[hist]} $_cl_dir>"'
  deadline=$(( EPOCHREALTIME + 5 ))
  while [[ $buf != *'<'<->' '<->' '<->' '*'>'* ]] && (( EPOCHREALTIME < deadline )); do
    if zpty -rt z chunk 2>/dev/null; then buf+=$chunk; else zselect -t 2; fi
  done
  zpty -d z
  [[ $buf == *"<1 1 0 ${MOD:A}>"* ]] || fail "plugin loaded from the module dir: ${(q+)buf[-300,-1]}"
  [[ $buf == *'cinelog: not recording: zim-test'* ]] || fail "start block ran the module's binary"
  [[ $(<$LOG) == 'rec --check'* ]] || fail "module binary asked: ${(q+)$(<$LOG)}"
}

t_update_swaps_the_binary() {
  local next=${VER%.*}.$(( ${VER##*.} + 1 )) out
  release $next
  publish $next
  out=$(zimfw update -v) || fail "zimfw update failed: $out"
  (( VERBOSE )) && print -r -- "${(F)${(f)out}/#/      }"
  eq "$($MOD/bin/cinelog --version)" "cinelog $next" "on-pull swapped in the new release"
  eq "$(<$MOD/VERSION)" $next "module updated"
  # A release whose binary does not match the committed checksum keeps the old binary.
  local bad=${next%.*}.$(( ${next##*.} + 1 ))
  release $bad
  publish $bad
  print -r -- '# tampered' >>$REL/v$bad/cinelog-$TARGET
  out=$(zimfw update -v)
  [[ $out == *'checksum mismatch'* ]] || fail "mismatch reported: $out"
  eq "$($MOD/bin/cinelog --version)" "cinelog $next" "the verified binary stays"
}

for fn in t_install_runs_the_fetch_hook t_module_init_is_the_plugin t_shell_loads_the_module \
          t_update_swaps_the_binary; do
  T_FAIL=0
  $fn
  if (( T_FAIL )); then (( FAIL++ )); print -r -- "not ok  $fn"; else (( PASS++ )); print -r -- "ok      $fn"; fi
done
print -r -- "zim tests: $PASS passed, $FAIL failed"
(( FAIL == 0 ))
