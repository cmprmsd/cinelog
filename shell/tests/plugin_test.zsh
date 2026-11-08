#!/usr/bin/env zsh
# Tests for cinelog.plugin.zsh and shell/early.zsh (docs/PROTOCOL.md §3, §6).
#
#   zsh shell/tests/plugin_test.zsh [-v] [NAME-PATTERN]
#
# Unit tests call the plugin functions directly; session tests run real interactive zsh
# instances on private pseudo-terminals (zsh/zpty) with a throwaway HOME, ZDOTDIR and XDG
# dirs, a fake `cinelog` binary, and the nonce handed over on an fd exactly as the recorder
# does. Needs zsh >= 5.8, jq and coreutils base64. Nothing outside a temp dir is touched.
# CINELOG_PERF_STRICT=1 turns the marker-encoding budget (§9.2) into a failure.

emulate -R zsh
setopt extendedglob nomultios
zmodload zsh/zpty zsh/datetime zsh/zselect zsh/parameter zsh/system
zmodload -F zsh/stat b:zstat

typeset -g ROOT=${${(%):-%x}:A:h:h:h} VERBOSE=0 FILTER='*'
[[ $1 == -v ]] && { VERBOSE=1; shift; }
[[ -n $1 ]] && FILTER=$1
for tool in jq base64; do
  (( $+commands[$tool] )) || { print -u2 "plugin_test: $tool is required"; exit 2; }
done
export LC_ALL=C.UTF-8

typeset -g TMP=$(mktemp -d ${TMPDIR:-/tmp}/cinelog-shtest.XXXXXX)
typeset -gi PASS=0 FAIL=0 SKIP=0 T_FAIL=0
typeset -gA SP_BUF
typeset -g NONCE=0123456789abcdef0123456789abcdef
typeset -g E=$'\e' A=$'\a'

cleanup() {
  local s
  for s in ${(k)SP_BUF}; do zpty -d $s 2>/dev/null; done
  [[ -d $TMP ]] && rm -rf -- $TMP
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

# ------------------------------------------------------------------------------ helpers

fail() { print -r -- "    FAIL: $*"; T_FAIL=1; }
ok() { local msg=$1; shift; "$@" || fail "$msg"; }
eq() {   # eq ACTUAL EXPECTED MESSAGE
  [[ $1 == "$2" ]] || fail "$3"$'\n'"      expected: ${(q+)2}"$'\n'"      actual:   ${(q+)1}"
}

# Decodes every cinelog marker in $1 into the array `markers` (JSON strings) and the array
# `marker_enc` (b/j); markers with a nonce other than $2 go to `foreign`.
decode_markers() {
  local buf=$1 want=${2:-$NONCE} n enc p pad
  typeset -ga markers=() marker_enc=() foreign=()
  local re="${E}\\]7777;cinelog;1;([0-9a-f]{32}|-);([bj]);([^${A}]*)${A}"
  while [[ $buf =~ $re ]]; do
    n=$match[1] enc=$match[2] p=$match[3]
    buf=${buf[MEND+1,-1]}
    if [[ $enc == b ]]; then
      pad=${(l:$(( (4 - ${#p} % 4) % 4 ))::=:)}
      p=$(print -rn -- "$p$pad" | base64 -d 2>/dev/null) || p='<bad base64>'
    fi
    if [[ $n == $want ]]; then markers+=("$p"); marker_enc+=($enc); else foreign+=("$p"); fi
  done
}
jget() { print -rn -- "$1" | jq -j "$2" 2>/dev/null; }   # jget JSON FILTER
kinds() { local m; REPLY=; for m in $markers; do REPLY+="$(jget $m .k) "; done; REPLY=${REPLY% }; }

# A throwaway module dir: the plugin, early.zsh and a fake binary (unless $1 == nobin).
mkmodule() {
  local mod=$TMP/mod.$RANDOM$RANDOM
  mkdir -p $mod/shell $mod/bin
  cp $ROOT/cinelog.plugin.zsh $mod/
  cp $ROOT/shell/early.zsh $mod/shell/
  if [[ $1 != nobin ]]; then
    cp $TMP/fake-cinelog $mod/bin/cinelog
    chmod 755 $mod/bin/cinelog
  fi
  REPLY=$mod
}

# A fake `cinelog`: logs its argv; `rec --check` prints $FAKE_CHECK and, for `recorded`,
# writes a registry entry for the calling tty; `rec -- SHELL` simulates an init failure
# and execs SHELL unrecorded, exactly like the real recorder's fallback; `doctor --skew`
# prints $FAKE_SKEW split at '|', or hangs with FAKE_SKEW_HANG=1.
cat >$TMP/fake-cinelog <<'EOF'
#!/bin/zsh -f
print -r -- "$*" >>$FAKE_LOG
if [[ -n $FAKE_TTYLOG ]]; then   # which of fds 0-2 are terminals
  ttys=; for fd in 0 1 2; do [[ -t $fd ]] && ttys+=$fd; done; print -r -- "ttys:$ttys" >>$FAKE_LOG
fi
case "$1 $2" in
  ('rec --protocol') print ${FAKE_PROTOCOL:-1} ;;
  ('doctor --skew')
    [[ $FAKE_SKEW_HANG == 1 ]] && sleep 30
    [[ -n $FAKE_SKEW ]] && print -rl -- ${(s:|:)FAKE_SKEW}; exit 0 ;;
  ('rec --check')
    if [[ $FAKE_CHECK == recorded* ]]; then
      t=$(tty); dir=$XDG_RUNTIME_DIR/cinelog
      mkdir -p -m 700 $dir
      f=$dir/pts-${t#/dev/pts/}.json
      print -rn -- "{\"format\":1,\"pid\":1,\"starttime\":1,\"boot_id\":\"x\",\"uuid\":\"${FAKE_CHECK#* }\",\"nonce\":\"$FAKE_NONCE\",\"scope\":\"personal\",\"cast\":\"/logs/a \\\"b\\\".cast\",\"pts\":\"$t\",\"state\":\"live\"${FAKE_REG_OPTS:+,\"opts\":\"$FAKE_REG_OPTS\"}}" >$f
      chmod ${FAKE_REG_MODE:-600} $f
    fi
    print -r -- $FAKE_CHECK ;;
  ('rec --')
    export CINELOG_FAILED='fake init failure' CINELOG_FAILED_TTY=$(tty) CINELOG_FAILED_PID=$$
    exec $3 ;;
  (*) exit 2 ;;
esac
EOF

# Starts an interactive zsh on a new pty. Options: -m MODULE, -n (hand over the nonce on fd 3
# as the recorder does; -N for a wrong PPID), -e (.zshrc sources early.zsh first),
# -r RCLINES (extra .zshrc lines before the plugin), -a RCLINES (after it), -c CMD (run
# `zsh -i -c CMD` instead), -i (stdin from /dev/null). Remaining args: VAR=value for env.
sp_start() {
  local name=$1; shift
  local mod= nonce=0 early=0 pre= post= cmd= devnull=0 o OPTIND=1 OPTARG
  while getopts "m:nNer:a:c:i" o; do
    case $o in
      m) mod=$OPTARG ;; n) nonce=1 ;; N) nonce=2 ;; e) early=1 ;;
      r) pre=$OPTARG ;; a) post=$OPTARG ;; c) cmd=$OPTARG ;; i) devnull=1 ;;
    esac
  done
  shift $(( OPTIND - 1 ))
  [[ -n $mod ]] || { mkmodule; mod=$REPLY; }
  local home=$TMP/home.$name.$RANDOM$RANDOM
  mkdir -p $home/zd $home/run $home/config $home/state
  chmod 700 $home/run
  # Ubuntu's /etc/zsh/zshrc runs compinit, which on CI's group-writable workspace
  # directories blocks every spawned shell on the insecure-directories y/n prompt.
  # The documented guard is skip_global_compinit=1 in $ZDOTDIR/.zshenv; the harness
  # must stay hermetic regardless of the host's global rc files.
  print -r -- 'skip_global_compinit=1' >$home/zd/.zshenv
  {
    print -r -- 'PS1="PROMPT%!> "; unsetopt PROMPT_SP; HISTSIZE=100'
    print -r -- $pre
    (( early )) && print -r -- "source ${(q)mod}/shell/early.zsh"
    print -r -- "source ${(q)mod}/cinelog.plugin.zsh"
    print -r -- $post
  } >$home/zd/.zshrc
  : >$home/fake.log
  local -a envv=(PATH=$PATH HOME=$home ZDOTDIR=$home/zd TERM=xterm-256color LC_ALL=C.UTF-8
    XDG_RUNTIME_DIR=$home/run XDG_CONFIG_HOME=$home/config XDG_STATE_HOME=$home/state
    FAKE_LOG=$home/fake.log "FAKE_CHECK=skip error" "$@")
  local pre_exec=
  if (( nonce )); then
    print -r -- $NONCE >$home/nonce
    pre_exec="exec 3<${(q)home}/nonce; "
    local ppid='$me'; (( nonce == 2 )) && ppid=1
    envv+=(CINELOG_NONCE_FD=3 "CINELOG_REC_PID=$ppid" 'CINELOG_TTY=${${:-/proc/self/fd/0}:A}'
      CINELOG_SESSION=0190f0e0-1111-7222-8333-123456789abc
      'CINELOG_OPTS=motd=0,banners=1,check_free_space=0,external_ip=0,title=1,title_link=0,url=https://h:1/s/x%2Cy')
  fi
  local me=$$ line="exec env -i"
  for o in $envv; do
    if [[ $o == *'${'* || $o == *'$me'* ]]; then line+=" ${o%%=*}=\"${o#*=}\""; else line+=" ${(q)o}"; fi
  done
  line+=" zsh -i"
  [[ -n $cmd ]] && line+=" -c ${(q)cmd}"
  (( devnull )) && line+=" </dev/null"
  SP_BUF[$name]=
  typeset -g "SP_HOME_$name=$home"
  zpty $name "$pre_exec$line"
}

# Waits until the session output matches the glob $2 (default: a prompt), max $3 s.
# The prompt carries the history number, so a redrawn prompt is never mistaken for the next.
sp_expect() {
  local name=$1 pat=${2:-'*PROMPT<->> *'} chunk
  local -F deadline=$(( EPOCHREALTIME + ${3:-5} ))
  while [[ ${SP_BUF[$name]} != ${~pat} ]]; do
    if (( EPOCHREALTIME > deadline )); then
      (( VERBOSE )) && print -r -- "    output so far: ${(q+)SP_BUF[$name][-600,-1]}"
      return 1
    fi
    if zpty -rt $name chunk 2>/dev/null; then SP_BUF[$name]+=$chunk; else zselect -t 2; fi
  done
}
# Runs a command line and waits for the next prompt; REPLY = output since the command.
sp_run() {
  local name=$1 before no=${${SP_BUF[$1]##*PROMPT}%%>*}
  before=${#SP_BUF[$name]}
  zpty -w $name "$2"
  sp_expect $name "*PROMPT$(( no + 1 ))> *" ${3:-5} || { fail "no prompt after: $2"; return 1; }
  REPLY=${SP_BUF[$name][before+1,-1]}
}
sp_stop() { zpty -d $1 2>/dev/null; unset "SP_BUF[$1]"; }
sp_log() { local h=SP_HOME_$1; REPLY=$(<${(P)h}/fake.log); }
argv0s() { local -a l=("${(@f)REPLY}"); REPLY=${(F)${(@)l%% /*}}; }   # log lines without paths

# Unit tests run against the plugin loaded into this shell as a recorded shell.
unit_load() {
  typeset -g _cl_state=recorded _cl_dir=$ROOT _cl_nonce=$NONCE _cl_reg= _cl_regsig=
  typeset -g _cl_regread_sig= _cl_degraded=0 _cl_proto_checked=1 _cl_session=0190f0e0-1111-7222-8333-123456789abc
  typeset -gi _cl_n=0 _cl_open=0 COLUMNS=80
  typeset -gA _cl_opt=(motd 0 banners 1 check_free_space 0 external_ip 0 title 0 title_link 0 url '')
  unset _cl_loaded
  source $ROOT/cinelog.plugin.zsh >/dev/null
  add-zsh-hook -d preexec _cl_preexec
  add-zsh-hook -d precmd _cl_precmd
  add-zsh-hook -d zshexit _cl_zshexit
}
capture() { "$@" >$TMP/cap; REPLY=$(<$TMP/cap); }   # keeps global side effects

# --------------------------------------------------------------------------- unit tests

t_unit_base64_matches_coreutils() {
  local s ref all=
  integer i
  for (( i = 32; i < 127; i++ )); do all+=${(#)i}; done
  local -a samples=('' a ab abc abcd abcde abcdef 'ls -la ~' $'äöü€😀' $all $'\x80\xff\xfe\0\x01')
  samples+=("$(head -c 999 /dev/urandom | base64 -w0)" "$(head -c 1600 /dev/urandom)")
  for s in $samples; do
    _cl_b64 $s
    ref=$(print -rn -- $s | base64 -w0); ref=${ref%%=*}
    eq "$REPLY" "$ref" "base64 of ${(q+)s[1,20]}"
  done
}

t_unit_json_escaping_round_trips() {
  local s back ctl= qb='"\\'
  integer i
  for (( i = 1; i < 32; i++ )); do ctl+=${(#)i}; done
  local -a samples=($'a"b\\c' $'tab\there\nnl\rcr' $'\x01\x1f\x7f\e[31m' 'C:\temp\new' 'foo\cbar'
    "grep '\\d+'" $'äöü €' $ctl $'\x7f' $'x\0y' '\' '"' '\\""' $'\n\n' $'a\n' $'\nb' '$e' '%s'
    ${(pl:5000::$qb:)} ${(pl:3000::\x01:)} "${(pl:999::\n:)}${(l:999::😀:)}")
  for s in $samples; do
    _cl_jstr $s
    [[ $REPLY == *[$'\0'-$'\x1f'$'\x7f']* ]] && fail "raw control byte in ${(q+)REPLY}"
    back=$(print -rn -- "{\"s\":\"$REPLY\"}" | jq -j '.s + "."'); back=${back%.}
    eq "$back" "$s" "JSON round trip of ${(q+)s[1,40]}"
  done
}

t_unit_cut_respects_utf8_boundaries() {
  _cl_cut abc 5; eq $? 1 "short strings are not cut"
  _cl_cut abcdef 4; eq "$REPLY" abcd "plain cut"
  _cl_cut 'aä' 2; eq "$REPLY" a "2-byte char not split"
  _cl_cut 'ab€' 4; eq "$REPLY" ab "3-byte char not split"
  _cl_cut 'ab€' 5; eq "$REPLY" 'ab€' "complete 3-byte char kept"
  _cl_cut 'a😀' 4; eq "$REPLY" a "4-byte char not split"
}

t_unit_cmd_marker_fields() {
  _cl_n=0 _cl_open=0
  local typed=$'printf "a\\nb" | grep \'\\d+\'\necho C:\\temp\\new'
  capture _cl_preexec "$typed" x "$typed --color"
  decode_markers $REPLY
  eq ${#markers} 1 "one marker"
  local m=$markers[1]
  eq "$(jget $m '[.v,.k,.sh,.n]|map(tostring)|join(" ")')" "1 cmd $$ 1" "v k sh n"
  eq "$(jget $m .typed)" "$typed" "typed is byte-exact"
  eq "$(jget $m .expanded)" "$typed --color" "expanded is \$3"
  eq "$(jget $m .cwd)" "$PWD" cwd
  eq "$(jget $m .euid)" "$EUID" euid
  eq "$(jget $m 'keys_unsorted[0:2]|join(",")')" "v,k" "v and k come first"
  eq "$(jget $m 'has("trunc")')" false "no trunc flag"
  [[ $(jget $m .t) == <->.<-> ]] || fail "t is a decimal epoch"
  eq $_cl_open 1 "command is open"
  [[ $REPLY == ${E}\]133\;C${A}${E}\]7777* ]] || fail "OSC 133;C precedes the marker"
  [[ $REPLY == *${A}' '(#c53)'Begin ['<->-<->-<->' '<->:<->:<->']' ]] ||
    fail "right-aligned Begin banner"
}

t_unit_leading_space_emits_skip_only() {
  _cl_n=4 _cl_open=0 _cl_opt[title]=1
  capture _cl_preexec ' secret --token x' x ' secret --token x'
  [[ $REPLY == *secret* || $REPLY == *Begin* ]] && fail "skip leaks text or banner: ${(q+)REPLY}"
  decode_markers $REPLY
  eq "$(jget $markers[1] 'keys_unsorted|join(",")')" "v,k,sh,t" "skip carries no text"
  eq "$(jget $markers[1] .k)" skip kind
  eq $_cl_n 4 "n not incremented"
  capture _cl_precmd
  decode_markers $REPLY
  kinds; eq "$REPLY" "" "no done after a skip"
  _cl_opt[title]=0
}

t_unit_done_marker_and_banner() {
  _cl_n=0 _cl_open=0
  typeset -g _cl_said_hello=1
  capture _cl_preexec 'false' x 'false'
  capture eval '(exit 3) | (exit 4); _cl_precmd'
  local out=$REPLY
  decode_markers $out
  local m=$markers[1]
  eq "$(jget $m '[.k,.n,.exit]|map(tostring)|join(" ")')" "done 1 4" "done fields"
  eq "$(jget $m '.pipestatus|map(tostring)|join(",")')" "3,4" pipestatus
  [[ $out == ${E}\]133\;D\;4${A}* ]] || fail "OSC 133;D;exit first"
  [[ $out == *${A}' '(#c53)'End   ['<->-<->-<->' '<->:<->:<->']'$'\n'* ]] || fail "End banner with three spaces"
  [[ $out == *${E}\]133\;A${A}${E}\]7\;file://${HOST}* ]] || fail "OSC 133;A and OSC 7"
  capture _cl_precmd
  decode_markers $REPLY
  eq ${#markers} 0 "no second done"
}

t_unit_hello_fields() {
  capture _cl_hello
  decode_markers $REPLY
  local m=$markers[1] st
  st=$(</proc/$$/stat); st=${${=${st##*\) }}[20]}
  eq "$(jget $m '[.k,.sh,.sst,.ppid,.uid,.euid]|map(tostring)|join(" ")')" "hello $$ $st $PPID $UID $EUID" "hello ids"
  eq "$(jget $m .zsh)" $ZSH_VERSION "zsh version"
  eq "$(jget $m 'has("tmux")')" false "no tmux object outside tmux"
  TMUX=/tmp/tmux-1/de,fault,4242,7 TMUX_PANE=%3 capture _cl_hello
  decode_markers $REPLY
  eq "$(jget $markers[1] '.tmux|[.socket,.server_pid,.session_id,.pane]|map(tostring)|join(" ")')" \
    "/tmp/tmux-1/de,fault 4242 7 %3" "tmux hint"
}

t_unit_encoding_threshold_and_limits() {
  # The JSON carries $PWD; a short cwd keeps the 1300-byte case below the threshold.
  local here=$PWD
  builtin cd -q /
  _cl_n=0
  capture _cl_preexec ${(l:1300::x:)} x x
  decode_markers $REPLY; eq "$marker_enc" b "≤ 1536 bytes JSON: base64"
  capture _cl_preexec ${(l:1700::x:)} x x
  decode_markers $REPLY; eq "$marker_enc" j "> 1536 bytes JSON: escaped JSON"
  eq "$(jget $markers[1] .typed)" ${(l:1700::x:)} "j payload parses"
  local big=${(l:20000::ä:)}
  capture _cl_preexec $big x $big
  decode_markers $REPLY
  eq "$(jget $markers[1] .trunc)" true "trunc set"
  local typed=$(jget $markers[1] .typed)
  setopt localoptions nomultibyte
  (( ${#typed} <= 16384 && ${#typed} >= 16380 )) || fail "typed is ${#typed} bytes"
  unsetopt nomultibyte
  local ctl=${(pl:16384::\x01:)}
  capture _cl_preexec $ctl x $ctl
  decode_markers $REPLY
  setopt nomultibyte
  local raw=$REPLY
  (( ${#raw} < 65536 )) || fail "escaped worst case stays under the 64 KiB payload limit (${#raw})"
  eq "$(jget $markers[1] .trunc)" true "worst case is truncated, not dropped"
  # Regression: the budget counts bytes. C0 bytes (6 escaped bytes each) mixed with 4-byte
  # characters are far more bytes than characters: 72253 bytes when counted in characters.
  unsetopt nomultibyte
  local mixed=${(pl:4000::\n:)}${(l:3000::😀:)}
  capture _cl_preexec $mixed x $mixed
  setopt nomultibyte
  raw=$REPLY
  (( ${#raw} < 65536 )) || fail "mixed C0 + 4-byte UTF-8 stays under the 64 KiB limit (${#raw} bytes)"
  decode_markers $raw
  eq ${#markers} 1 "mixed worst case: one marker"
  eq "$(jget $markers[1] .trunc)" true "mixed worst case is truncated"
  [[ $(jget $markers[1] .typed) == $'\n'(#c4000)* ]] || fail "mixed: kept the leading newlines"
  builtin cd -q $here
}

t_unit_opts_parsing() {
  typeset -gA _cl_opt=()
  _cl_parse_opts 'motd=0,banners=1,url=https://h:1/s/a%2Cb%3Dc%25d,unknown=7,url2'
  eq "$_cl_opt[motd] $_cl_opt[banners]" "0 1" booleans
  eq "$_cl_opt[url]" 'https://h:1/s/a,b=c%d' "url percent-decoding"
  _cl_parse_opts 'motd=1,url='
  eq "$_cl_opt[url]" '' "empty url"
  typeset -gA _cl_opt=(motd 0 banners 1 check_free_space 0 external_ip 0 title 0 title_link 0 url '')
}

t_unit_trust_rules() {
  ok "/usr/bin/zsh path is root-owned" _cl_trusted /usr/bin/zsh
  local f=$TMP/mine; : >$f
  (( EUID == 0 )) || ! _cl_trusted $f || fail "user-owned file is not trusted"
  mkdir -p $TMP/ww/sub; chmod 777 $TMP/ww
  ! _cl_trusted $TMP/ww/sub || fail "non-root, world-writable parent rejected"
  # Regression: a trusted target behind a symlink in an untrusted dir: the resolved path is
  # what gets exec'd, so re-pointing the link after the check changes nothing.
  local real=${commands[zsh]:A}
  ln -sf $real $TMP/link-to-zsh
  _cl_pick_trusted $TMP/mine $TMP/link-to-zsh || fail "symlink to a trusted binary accepted"
  eq "$REPLY" "$real" "the verified, resolved path is returned, not the symlink"
}

t_unit_osc7_percent_encoding() {
  local d=$TMP/'dir with space'/äö; mkdir -p $d
  ( cd $d && _cl_open=0 && _cl_precmd ) >$TMP/cap
  REPLY=$(<$TMP/cap)
  [[ $REPLY == *"file://$HOST${TMP}/dir%20with%20space/%C3%A4%C3%B6"${A}* ]] || fail "OSC 7 encoding: ${(q+)REPLY}"
}

t_unit_free_space_warnings() {
  # Regression: df right-aligns the percentage; " 95% /" was never parsed (no extendedglob).
  mkdir -p $TMP/fakedf
  print -r -- '#!/bin/sh
printf "%s\n" "Use% Mounted on" " 95% /" "100% /x" " 50% /home" " 99% /tmp/com.freerdp.1" \
  " 90% /mnt/with space" "  9% /boot"' >$TMP/fakedf/df
  chmod 755 $TMP/fakedf/df
  local -a opath=($path)
  path=($TMP/fakedf $path); rehash
  capture _cl_free_space
  path=($opath); rehash
  local plain=${REPLY//$'\e'\[[0-9;]#m}
  local -a want=('Running out of disk space "/ (95%)"' 'Running out of disk space "/x (100%)"'
    'Running out of disk space "/mnt/with space (90%)"')
  eq "$plain" "${(F)want}" "warnings for / /x and the mount with a space, none for freerdp or below 90%"
}

# The §9.2 budget covers the marker alone: _cl_preexec with banners and title off. Each figure
# is the best of several runs, which filters scheduler noise, so CINELOG_PERF_STRICT=1 can gate
# a benchmark job. Banner and title are timed apart, for information.
t_unit_perf_budget() {
  local ob=$_cl_opt[banners] ot=$_cl_opt[title]
  _cl_opt[banners]=0 _cl_opt[title]=0
  local cmd="${(l:180::x:)} | grep -v 'y' > /dev/null" w
  local -F t0
  local -i us p95=999999999 worst dense best run i
  local -a ts
  for (( run = 0; run < 5; run++ )); do
    ts=()
    for (( i = 0; i < 100; i++ )); do
      t0=$EPOCHREALTIME; _cl_preexec $cmd x $cmd >/dev/null; (( us = (EPOCHREALTIME - t0) * 1e6 ))
      ts+=($us)
    done
    ts=(${(on)ts})
    (( ts[95] < p95 )) && p95=$ts[95]
  done
  # Any command: just below the 1536-byte base64 limit is the slowest size, then 16 KiB of
  # text (plain, and a heredoc-like paste with newlines and quotes) and multibyte text.
  local line='printf "%s\\n" "$HOME/some/path" --flag=value | tee -a /tmp/out.log # comment'$'\n'
  local -a big=()
  for (( i = 600; i <= 760; i += 10 )); do big+=(${(l:i::z:)}); done
  big+=(${(l:16384::y:)} ${(pl:16384::$line:)} ${(l:8000::ä:)})
  _t_perf_worst $big; worst=$REPLY
  # Escaping-dense input past the 64 KiB payload limit (16 KiB of control bytes, quotes or
  # newlines) costs up to three escaping rounds; bounded separately.
  local qb='"\\'
  _t_perf_worst ${(pl:16384::$qb:)} ${(pl:16384::\x01:)} ${(pl:16384::\n:)} "${(pl:4000::\n:)}${(l:3000::😀:)}"
  dense=$REPLY
  _cl_opt[banners]=1 _cl_opt[title]=1
  best=999999999
  for (( run = 0; run < 20; run++ )); do
    t0=$EPOCHREALTIME; _cl_banner Begin >/dev/null; _cl_title $cmd >/dev/null; (( us = (EPOCHREALTIME - t0) * 1e6 ))
    (( us < best )) && best=us
  done
  _cl_opt[banners]=$ob _cl_opt[title]=$ot
  printf '    marker p95 (200 B): %.2f ms, worst command: %.2f ms, escaping-dense 16 KiB: %.2f ms; banner + title: %.2f ms\n' \
    $(( p95 / 1e3 )) $(( worst / 1e3 )) $(( dense / 1e3 )) $(( best / 1e3 ))
  if (( p95 >= 2000 || worst >= 5000 || dense >= 20000 )); then
    if [[ -n $CINELOG_PERF_STRICT ]]; then fail "over budget (2 ms / 5 ms / 20 ms)"; else print "    (over budget, not strict)"; fi
  fi
}
# REPLY = the slowest of the commands $@ in µs, each the best of three runs.
_t_perf_worst() {
  local w
  local -F t0
  local -i us best run worst=0
  for w; do
    best=999999999
    for (( run = 0; run < 3; run++ )); do
      t0=$EPOCHREALTIME; _cl_preexec $w x $w >/dev/null; (( us = (EPOCHREALTIME - t0) * 1e6 ))
      (( us < best )) && best=us
    done
    (( best > worst )) && worst=best
  done
  REPLY=$worst
}

# ------------------------------------------------------------------------ session tests

t_session_recorded_shell_emits_markers() {
  sp_start s -n || return
  sp_expect s || { fail "no first prompt"; sp_stop s; return; }
  sp_run s 'echo hi; (exit 3)'
  sp_run s ' echo hidden-cmd'
  sp_run s 'printf "%s\n" "a b" | grep -c b'
  decode_markers ${SP_BUF[s]}
  kinds
  eq "$REPLY" "hello cmd done skip cmd done" "marker sequence"
  eq "$(jget $markers[2] .typed)" 'echo hi; (exit 3)' "typed from preexec \$1"
  eq "$(jget $markers[3] .exit)" 3 "exit status"
  eq "$(jget $markers[1] .sh)" "$(jget $markers[2] .sh)" "same shell pid"
  eq "$(jget $markers[5] .n)" 2 "skip does not consume n"
  [[ ${SP_BUF[s]} == *hidden-cmd* ]] || fail "skipped command still runs"
  [[ ${SP_BUF[s]} == *' End   ['* && ${SP_BUF[s]} == *' Begin ['* ]] || fail "banners shown (banners=1)"
  eq ${#foreign} 0 "no foreign markers"
  sp_stop s
}

t_session_exit_closes_command() {
  sp_start s -n || return
  sp_expect s || { fail "no first prompt"; sp_stop s; return; }
  zpty -w s 'exit 5'
  sp_expect s "*${E}]133;D;5${A}${E}]7777*${A}" 3 || fail "no done after exit"
  decode_markers ${SP_BUF[s]}
  kinds; eq "$REPLY" "hello cmd done" "exit gets a done"
  eq "$(jget $markers[3] .exit)" 5 "exit status of exit"
  sp_stop s
}

t_session_nonce_never_in_environment() {
  sp_start s -n || return
  sp_expect s || { fail "no first prompt"; sp_stop s; return; }
  sp_run s 'print -r "<$(env | grep -c CINELOG_)>"'
  [[ $REPLY == *'<0>'* ]] || fail "CINELOG_* exported: ${(q+)REPLY}"
  sp_run s 'print -r "<$(grep -c '$NONCE' /proc/self/environ /proc/$$/environ | grep -vc :0)>"'
  [[ $REPLY == *'<0>'* ]] || fail "nonce in an environ: ${(q+)REPLY}"
  sp_run s '[[ -e /proc/$$/fd/3 ]] && print "<fd3-open>" || print "<fd3-closed>"'
  [[ $REPLY == *'<fd3-closed>'* ]] || fail "nonce fd left open"
  sp_run s 'print -r -- "<${(t)_cl_nonce}>"'
  [[ $REPLY == *'<scalar>'* ]] || fail "_cl_nonce must not be exported: ${(q+)REPLY}"
  sp_stop s
}

t_session_title_and_url_from_opts() {
  sp_start s -n -r 'precmd_functions+=(_t_reset); _t_reset() { false }' || return
  sp_expect s || { fail "no first prompt"; sp_stop s; return; }
  sp_run s '(exit 7)'
  decode_markers ${SP_BUF[s]}
  eq "$(jget $markers[3] .exit)" 7 "exit status survives an earlier precmd hook"
  [[ $REPLY == *${E}\]0\;*@*': '*'(exit 7)'${A}* ]] || fail "title set (title=1): ${(q+)REPLY}"
  sp_stop s
}

t_session_wrong_parent_is_not_trusted() {
  sp_start s -N "FAKE_CHECK=skip error" || return
  sp_expect s || { fail "no first prompt"; sp_stop s; return; }
  decode_markers ${SP_BUF[s]}
  eq ${#markers} 0 "no markers without proof"
  sp_log s; eq "$REPLY" $'rec --check\ndoctor --skew' "stale env ignored, start decision asked"
  [[ ${SP_BUF[s]} == *'cinelog: not recording: error'* ]] || fail "one warning"
  sp_run s 'print -r "<$(env | grep -c CINELOG_)>"'
  [[ $REPLY == *'<0>'* ]] || fail "stale CINELOG_* unset"
  sp_stop s
}

t_session_fallback_does_not_loop() {
  sp_start s "FAKE_CHECK=start personal terminal" || return
  sp_expect s || { fail "no first prompt"; sp_stop s; return; }
  sp_log s; local log=$REPLY
  argv0s; eq "$REPLY" $'rec --check\nrec --\ndoctor --skew' "one check, one exec, no loop"
  [[ $log == *"rec -- ${commands[zsh]:A}"* || $log == *"rec -- /"*zsh* ]] || fail "execs the running zsh"
  local n=${#${(M)${(f)SP_BUF[s]}:#*not recording: fake init failure*}}
  eq $n 1 "exactly one warning"
  sp_run s 'print -r "<$(env | grep -c CINELOG_)>"'
  [[ $REPLY == *'<0>'* ]] || fail "CINELOG_FAILED* unset"
  sp_run s 'zsh -i -c "print inner-ok"'
  [[ $REPLY == *inner-ok* ]] || fail "zsh -i -c inside works"
  sp_stop s
}

t_session_binary_vanishing_after_check_keeps_shell() {
  mkmodule; local mod=$REPLY
  print -r -- '#!/bin/sh
[ "$1 $2" = "rec --check" ] && { rm -f "$0"; echo start personal terminal; }' >| $mod/bin/cinelog
  sp_start s -m $mod || return
  sp_expect s || { fail "shell died after the binary vanished"; sp_stop s; return; }
  [[ ${SP_BUF[s]} == *'vanished before it could start'* ]] || fail "warning"
  sp_stop s
}

t_session_guards_skip_noninteractive() {
  local -F t0=$EPOCHREALTIME
  sp_start s -c 'print X-OUT' "FAKE_CHECK=start personal terminal" || return
  sp_expect s '*X-OUT*' 3 || fail "zsh -i -c did not print"
  (( EPOCHREALTIME - t0 < 1 )) || fail "zsh -i -c took over 1 s"
  sp_log s; eq "$REPLY" "" "zsh -i -c never asks the recorder"
  sp_stop s
  local v
  for v in VSCODE_RESOLVING_ENVIRONMENT=1 TERM=dumb INSIDE_EMACS=29; do
    sp_start s "FAKE_CHECK=start personal terminal" $v || return
    sp_expect s || { fail "no prompt with $v"; sp_stop s; continue; }
    sp_log s; eq "$REPLY" "" "$v never asks the recorder"
    sp_stop s
  done
  sp_start s -i "FAKE_CHECK=start personal terminal" || return
  zselect -t 50
  sp_log s; eq "$REPLY" "" "stdin not a tty never asks the recorder"
  sp_stop s
}

t_session_kill_switches() {
  sp_start s "FAKE_CHECK=start personal terminal" CINELOG_DISABLE=1 || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  sp_log s; eq "$REPLY" "" "CINELOG_DISABLE"
  sp_run s 'print "<${+aliases[cs]} ${+functions[_cl_preexec]}>"'
  [[ $REPLY == *'<0 0>'* ]] || fail "nothing loaded when disabled"
  sp_stop s
  sp_start s -r 'mkdir -p $XDG_CONFIG_HOME/cinelog; : >$XDG_CONFIG_HOME/cinelog/disable' \
    "FAKE_CHECK=start personal terminal" || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  sp_log s; eq "$REPLY" "" "disable file"
  sp_stop s
}

t_session_kill_switch_drops_handover() {
  # Regression: a kill switch that appears after the recorder's check returned before the
  # hand-over cleanup, leaving the nonce fd open and CINELOG_* exported to every child.
  sp_start s -n -r 'mkdir -p $XDG_CONFIG_HOME/cinelog; : >$XDG_CONFIG_HOME/cinelog/disable' || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  sp_run s 'print -r "<$(env | grep -c CINELOG_)>"'
  [[ $REPLY == *'<0>'* ]] || fail "disable file: CINELOG_* still exported: ${(q+)REPLY}"
  sp_run s '[[ -e /proc/$$/fd/3 ]] && print "<fd3-open>" || print "<fd3-closed>"'
  [[ $REPLY == *'<fd3-closed>'* ]] || fail "disable file: nonce fd left open"
  decode_markers ${SP_BUF[s]}
  eq ${#markers} 0 "disabled: no markers"
  sp_stop s
  sp_start s -n CINELOG_DISABLE=1 || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  sp_run s 'print -r "<${(j:,:)${(f)$(env | grep ^CINELOG_)}}>"'
  [[ $REPLY == *'<CINELOG_DISABLE=1>'* ]] || fail "CINELOG_DISABLE: only the switch itself stays: ${(q+)REPLY}"
  sp_run s '[[ -e /proc/$$/fd/3 ]] && print "<fd3-open>" || print "<fd3-closed>"'
  [[ $REPLY == *'<fd3-closed>'* ]] || fail "CINELOG_DISABLE: nonce fd left open"
  sp_stop s
}

t_session_missing_binary_warns() {
  mkmodule nobin; local mod=$REPLY
  print -r -- 'print -r -- "fetch $*" >>$FAKE_LOG' >$mod/shell/fetch.zsh
  sp_start s -m $mod || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  [[ ${SP_BUF[s]} == *'cinelog binary missing - run: cinelog-fetch'* ]] || fail "missing-binary warning"
  sp_run s 'print "<${+functions[cinelog-fetch]} ${+functions[cinelog-link]} ${+aliases[cs]} ${+functions[here]} ${+functions[hist]} ${+aliases[hist]}>"'
  [[ $REPLY == *'<1 1 1 0 0 0>'* ]] || fail "helpers present, here and the hist shim dropped: ${(q+)REPLY}"
  sp_log s; eq "$REPLY" "fetch --background" "background fetch requested"
  sp_stop s
  sp_start s -m $mod -r "zstyle ':cinelog:' auto-fetch no" || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  sp_log s; eq "$REPLY" "" "auto-fetch can be turned off"
  sp_stop s
}

t_session_subshell_attaches_via_registry() {
  sp_start s FAKE_NONCE=$NONCE "FAKE_CHECK=recorded 0190f0e0-1111-7222-8333-123456789abc" || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  sp_run s 'echo sub'
  decode_markers ${SP_BUF[s]}
  kinds; eq "$REPLY" "hello cmd done" "markers with the registry nonce"
  [[ ${SP_BUF[s]} == *'cinelog: session 56789abc → /logs/a "b".cast'* ]] ||
    fail "MOTD session line: ${(q+)SP_BUF[s]}"
  sp_stop s
  sp_start s FAKE_NONCE=$NONCE "FAKE_CHECK=recorded 0190f0e0-1111-7222-8333-123456789abc" FAKE_REG_MODE=644 || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  sp_run s 'echo sub'
  decode_markers ${SP_BUF[s]}
  eq ${#markers} 0 "a group/other-readable registry entry is refused"
  [[ ${SP_BUF[s]} == *'cannot read the registry'* ]] || fail "registry warning"
  sp_stop s
}

t_session_degraded_warning_once() {
  sp_start s FAKE_NONCE=$NONCE "FAKE_CHECK=recorded 0190f0e0-1111-7222-8333-123456789abc" || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  local h=SP_HOME_s f
  f=(${(P)h}/run/cinelog/pts-*.json(N))
  # Regression: no sleep. A replacement within the same (whole) second as the last read must
  # be seen: rename gives the new entry a new inode.
  print -rn -- "$(<$f[1])" | sed 's/"state":"live"/"state":"degraded","state_reason":"enospc"/' >$f[1].new
  chmod 600 $f[1].new; mv $f[1].new $f[1]
  sp_run s 'true'; sp_run s 'true'
  local n=${#${(M)${(f)SP_BUF[s]}:#*'recording stopped (enospc)'*}}
  eq $n 1 "degraded warning printed once"
  sp_stop s
}

t_session_early_and_module_load_once() {
  sp_start s -n -e || return
  sp_expect s || { fail "no first prompt"; sp_stop s; return; }
  sp_run s 'echo once'
  decode_markers ${SP_BUF[s]}
  kinds; eq "$REPLY" "hello cmd done" "hooks registered once"
  sp_run s 'print "<${#${(@M)preexec_functions:#_cl_preexec}}>"'
  [[ $REPLY == *'<1>'* ]] || fail "one preexec hook: ${(q+)REPLY}"
  sp_stop s
  sp_start s -e "FAKE_CHECK=start personal terminal" || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  sp_log s; argv0s; eq "$REPLY" $'rec --check\nrec --\ndoctor --skew' "early.zsh starts the recorder before the module"
  sp_stop s
}

t_session_loaded_copy_wins() {
  # A root-owned copy loaded from the system zshrc sets _cl_loaded; a second plugin file
  # sourced later (the user's module) must return before defining anything.
  mkmodule; local other=$REPLY
  print -r -- '_cl_preexec() { print HIJACKED }' >>$other/cinelog.plugin.zsh
  sp_start s -n -a "source ${(q)other}/cinelog.plugin.zsh" || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  sp_run s 'echo x'
  [[ $REPLY == *HIJACKED* ]] && fail "second plugin redefined functions"
  sp_stop s
}

t_session_hostile_user_options() {
  # zim sources modules after the user's options; aliases of our names must not break us.
  local opts='setopt ksharrays shwordsplit nounset noclobber rcexpandparam; alias cinelog-link="echo old-link"'
  sp_start s -n -r $opts || return
  sp_expect s || { fail "no first prompt"; sp_stop s; return; }
  sp_run s 'echo opts-ok'
  sp_run s ' echo quiet'
  decode_markers ${SP_BUF[s]}
  kinds; eq "$REPLY" "hello cmd done skip" "markers under ksharrays/shwordsplit/nounset"
  eq "$(jget $markers[2] .typed)" 'echo opts-ok' "typed"
  [[ ${SP_BUF[s]} == *' Begin ['<->-<->-<->' '<->:<->:<->']'* ]] || fail "banner"
  [[ ${SP_BUF[s]} == *'parameter not set'* || ${SP_BUF[s]} == *'bad substitution'* ]] && fail "errors in output"
  sp_stop s
}

t_session_protocol_mismatch_warns_without_motd() {
  # motd=0 (from CINELOG_OPTS) and an earlier warning must not hide the version check.
  sp_start s -n -a '_cl_warn "earlier warning"' FAKE_PROTOCOL=2 || return
  sp_expect s || { fail "no first prompt"; sp_stop s; return; }
  sp_run s 'true'
  local n=${#${(M)${(f)SP_BUF[s]}:#*'does not speak marker protocol 1'*}}
  eq $n 1 "protocol mismatch warned once with motd=0"
  sp_stop s
  sp_start s -n || return
  sp_expect s || { fail "no first prompt"; sp_stop s; return; }
  [[ ${SP_BUF[s]} == *'marker protocol'* ]] && fail "no warning when the protocol matches"
  sp_log s; eq "$REPLY" "rec --protocol" "one protocol query per recorded shell"
  sp_stop s
}

t_session_attached_shell_takes_opts_from_registry() {
  # A nested shell attaches through the registry; the recorder's opts must apply there too.
  sp_start s FAKE_NONCE=$NONCE "FAKE_CHECK=recorded 0190f0e0-1111-7222-8333-123456789abc" \
    FAKE_REG_OPTS='motd=0,banners=0,title=0,url=https://h:1/s/x%2Cy' || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  sp_run s 'echo sub; print -r -- "<$_cl_opt[url]>"'
  [[ $REPLY == *'<https://h:1/s/x,y>'* ]] || fail "url from the registry opts: ${(q+)REPLY}"
  [[ ${SP_BUF[s]} == *' Begin ['* || ${SP_BUF[s]} == *'Uptime:'* ]] && fail "banners/motd despite banners=0 motd=0"
  decode_markers ${SP_BUF[s]}
  kinds; eq "$REPLY" "hello cmd done" "still recorded"
  sp_stop s
}

t_session_motd_prints_skew_warnings() {
  local skew='cinelog: binary 1.0.0 but plugin 0.13.0 in /m: run cinelog-fetch|not a warning|cinelog: /usr/local/bin/cinelog is 0.13.0, older than 1.0.0: sudo x'
  sp_start s FAKE_NONCE=$NONCE "FAKE_CHECK=recorded 0190f0e0-1111-7222-8333-123456789abc" "FAKE_SKEW=$skew" || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  local plain=${SP_BUF[s]//$'\e'\[[0-9;]#m}
  [[ $plain == *'cinelog: binary 1.0.0 but plugin 0.13.0 in /m: run cinelog-fetch'$'\r'#$'\n''cinelog: /usr/local/bin/cinelog is 0.13.0, older than 1.0.0: sudo x'* ]] ||
    fail "both skew lines in the MOTD: ${(q+)plain[1,600]}"
  [[ $plain == *'not a warning'* ]] && fail "only 'cinelog: ' lines are printed"
  [[ ${plain%%Uptime:*} == *'cinelog: session 56789abc'*'older than 1.0.0'* ]] ||
    fail "after the session line, before the rest of the MOTD"
  sp_run s cs
  [[ ${REPLY//$'\e'\[[0-9;]#m} == *'cinelog: binary 1.0.0 but plugin 0.13.0'* ]] || fail "cs reprints the skew lines"
  sp_log s; local n=${#${(M)${(f)REPLY}:#doctor --skew}}
  eq $n 1 "doctor --skew asked once per shell"
  sp_stop s
  sp_start s FAKE_NONCE=$NONCE "FAKE_CHECK=recorded 0190f0e0-1111-7222-8333-123456789abc" "FAKE_SKEW=$skew" \
    FAKE_REG_OPTS='motd=0' || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  [[ ${SP_BUF[s]} == *'older than'* ]] && fail "no skew lines with motd=0"
  sp_log s; [[ $REPLY == *'doctor --skew'* ]] && fail "motd=0 does not run doctor --skew"
  sp_stop s
}

# Escape sequences from the binary must not reach the terminal: ESC, BEL and CR are stripped
# and only the plugin's own colour codes remain around the line.
t_session_motd_skew_strips_control_chars() {
  local skew=$'cinelog: x\e]0;pwn\a\r|cinelog: y\e[2Jz'
  sp_start s FAKE_NONCE=$NONCE "FAKE_CHECK=recorded 0190f0e0-1111-7222-8333-123456789abc" "FAKE_SKEW=$skew" || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  local buf=${SP_BUF[s]}
  [[ $buf == *$'\e[38;2;253;253;150mcinelog: x]0;pwn\e[0m\r\n'* ]] || fail "OSC line printed as plain text: ${(q+)buf[1,600]}"
  [[ $buf == *$'\e[38;2;253;253;150mcinelog: y[2Jz\e[0m\r\n'* ]] || fail "CSI line printed as plain text: ${(q+)buf[1,600]}"
  [[ $buf == *$'\e]0;pwn'* || $buf == *$'pwn\a'* ]] && fail "raw OSC/BEL from the binary reached the terminal"
  [[ $buf == *$'\e[2J'* ]] && fail "raw CSI from the binary reached the terminal"
  sp_stop s
}

# A pre-1.0 checkout's asciinema-player/ (old viewer pages with recorded output) is reported
# in the MOTD with the command that removes it, and nothing is said once it is gone.
t_session_motd_reports_old_viewer_pages() {
  mkmodule; local mod=$REPLY
  mkdir -p $mod/asciinema-player/tmp/h8 && print x >$mod/asciinema-player/tmp/h8/index.html
  sp_start s -m $mod FAKE_NONCE=$NONCE "FAKE_CHECK=recorded 0190f0e0-1111-7222-8333-123456789abc" || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  local plain=${SP_BUF[s]//$'\e'\[[0-9;]#m}
  [[ $plain == *"cinelog: $mod/asciinema-player holds the old viewer's pages of recorded output; remove it: rm -rf $mod/asciinema-player"* ]] ||
    fail "old viewer pages reported: ${(q+)plain[1,600]}"
  [[ -f $mod/asciinema-player/tmp/h8/index.html ]] || fail "the plugin itself deletes nothing"
  sp_stop s
  rm -rf $mod/asciinema-player
  sp_start s -m $mod FAKE_NONCE=$NONCE "FAKE_CHECK=recorded 0190f0e0-1111-7222-8333-123456789abc" || return
  sp_expect s || { fail "no prompt"; sp_stop s; return; }
  [[ ${SP_BUF[s]} == *'old viewer'* ]] && fail "no notice without asciinema-player/"
  sp_stop s
}

# A hung `doctor --skew` (e.g. the team binary on a dead mount) must not keep the prompt away.
t_session_motd_skew_hang_is_bounded() {
  (( $+commands[timeout] )) || { print -r -- "    (skipped: no timeout(1))"; return; }
  local -F t0=$EPOCHREALTIME
  sp_start s FAKE_NONCE=$NONCE "FAKE_CHECK=recorded 0190f0e0-1111-7222-8333-123456789abc" \
    'FAKE_SKEW=cinelog: late' FAKE_SKEW_HANG=1 || return
  sp_expect s '*PROMPT<->> *' 8 || { fail "no prompt while doctor --skew hangs"; sp_stop s; return; }
  (( EPOCHREALTIME - t0 < 6 )) || fail "prompt took $(( EPOCHREALTIME - t0 )) s"
  [[ ${SP_BUF[s]} == *'cinelog: late'* ]] && fail "a timed-out skew answer is not printed"
  sp_stop s
}

# A stand-in for powerlevel10k's instant prompt as zim users have it above the module:
# stdin from /dev/null, stdout and stderr into a file, the terminal kept on saved fds.
P10K_SIM='exec {__p9k_fd_0}<&0 {__p9k_fd_1}>&1 {__p9k_fd_2}>&2 0</dev/null 1>>$HOME/p10k.out; exec 2>&1
typeset -gi __p9k_instant_prompt_active=1'
P10K_CLEANUP='_p9k_instant_prompt_cleanup() {
  unset __p9k_instant_prompt_active
  exec 0<&$__p9k_fd_0 1>&$__p9k_fd_1 2>&$__p9k_fd_2 {__p9k_fd_0}>&- {__p9k_fd_1}>&- {__p9k_fd_2}>&-
  unset __p9k_fd_0 __p9k_fd_1 __p9k_fd_2
  print -r -- p10k-cleanup-ran >>$FAKE_LOG
}'

t_session_p10k_instant_prompt_still_records() {
  local variant pre want
  for variant in with-cleanup without-cleanup; do
    pre=$P10K_SIM
    [[ $variant == with-cleanup ]] && pre+=$'\n'$P10K_CLEANUP
    sp_start s -r $pre "FAKE_CHECK=start personal terminal" FAKE_TTYLOG=1 || return
    local -F deadline=$(( EPOCHREALTIME + 5 ))
    while sp_log s; [[ $REPLY != *'rec -- '* ]] && (( EPOCHREALTIME < deadline )); do zselect -t 5; done
    # The check reads the tty from stdin (stdout is the command substitution); the exec
    # gets the terminal on all three fds.
    want=$'rec --check\nttys:0\n'
    [[ $variant == with-cleanup ]] && want+=$'p10k-cleanup-ran\n'
    want+=$'rec --\nttys:012'
    # The fake falls back to an unrecorded shell that sources the same .zshrc: only the
    # first start matters here.
    local -a got=("${(@f)REPLY}")
    got=("${(@)got[1,${#${(f)want}}]%% /*}")
    eq "${(F)got}" "$want" "$variant: guards see the terminal, check and exec on the tty"
    sp_stop s
  done
}

# The rest of the instant prompt as powerlevel10k has it: the first precmd (put ahead of all
# others) queues the cleanup with `sched +0`; the cleanup replays the capture file.
P10K_FULL=$P10K_SIM$'\n''_p9k_instant_prompt_cleanup() {
  (( ${+__p9k_instant_prompt_active} )) || return 0
  unset __p9k_instant_prompt_active
  exec 0<&$__p9k_fd_0 1>&$__p9k_fd_1 2>&$__p9k_fd_2 {__p9k_fd_0}>&- {__p9k_fd_1}>&- {__p9k_fd_2}>&-
  unset __p9k_fd_0 __p9k_fd_1 __p9k_fd_2
  print -r -- p10k-cleanup-ran >>$FAKE_LOG
  command cat $HOME/p10k.out
}
_p9k_instant_prompt_precmd_first() {
  zmodload zsh/sched
  sched +0 _p9k_instant_prompt_cleanup
  precmd_functions=(${(@)precmd_functions:#_p9k_instant_prompt_precmd_first})
}
precmd_functions=(_p9k_instant_prompt_precmd_first $precmd_functions)'

t_session_p10k_recorded_shell_output_reaches_terminal() {
  # Regression: in a recorded shell under an instant prompt the session line was dropped
  # (stdout is the capture file, so not a tty) and the first precmd wrote its markers there.
  # Both orders: the module after the instant prompt preamble (usual) and before it.
  local order
  for order in -r -a; do
    sp_start s -n $order $P10K_FULL || return
    sp_expect s || { fail "$order: no first prompt"; sp_stop s; return; }
    sp_run s 'echo after-p10k'
    local h=SP_HOME_s
    local captured=$(<${(P)h}/p10k.out)
    eq "${(q+)captured}" "''" "$order: nothing written into the instant prompt capture file"
    sp_log s; [[ $REPLY == *p10k-cleanup-ran* ]] || fail "$order: the p10k cleanup ran"
    local before=${SP_BUF[s]%%${E}\]7777*}
    [[ $before == *'cinelog: session 56789abc · https://h:1/s/x,y'* ]] ||
      fail "$order: session line on the terminal, before the first marker: ${(q+)SP_BUF[s][1,400]}"
    decode_markers ${SP_BUF[s]}
    kinds; eq "$REPLY" "hello cmd done" "$order: markers reach the terminal"
    sp_run s 'print "<${#${(@M)precmd_functions:#_cl_precmd}} ${#${(@M)precmd_functions:#_cl_after_instant_prompt}}>"'
    [[ $REPLY == *'<1 0>'* ]] || fail "$order: precmd hook registered once, deferral hook gone: ${(q+)REPLY}"
    sp_stop s
  done
}

# ------------------------------------------------------------------------------- runner

unit_load
for fn in ${(ok)functions[(I)t_*]}; do
  [[ $fn == ${~FILTER} || $fn == *${~FILTER}* ]] || continue
  T_FAIL=0
  (( VERBOSE )) && print -r -- "  $fn"
  $fn
  if (( T_FAIL )); then (( FAIL++ )); print -r -- "not ok  $fn"; else (( PASS++ )); print -r -- "ok      $fn"; fi
done
print -r -- "plugin tests: $PASS passed, $FAIL failed"
(( FAIL == 0 ))
