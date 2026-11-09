# cinelog: zsh integration (zim module). Records every interactive shell exactly once.
#
# Start: if this is not yet the recorded shell of its tty, exec `cinelog rec`, which runs
# a fresh inner zsh on its own PTY. The inner shell proves it is recorded with the nonce
# its parent recorder passes on an fd, then emits in-band markers from preexec/precmd.
# Wire format and binary interface: docs/PROTOCOL.md. `shell/early.zsh` runs only the start
# block, from the top of ~/.zshrc, so the outer shell does almost nothing before the exec.

(( ${+_cl_loaded} )) && return 0   # a root-owned copy (system zshrc) or an earlier load won

zmodload zsh/datetime zsh/parameter 2>/dev/null
zmodload -F zsh/stat b:zstat 2>/dev/null

# ------------------------------------------------------------------------------- start

_cl_warn() {   # at most one warning per shell, on stderr
  (( ${+_cl_warned} )) && return 0
  typeset -g _cl_warned=1
  print -ru2 -- "cinelog: $1"
}

# The file and every parent directory are root-owned and not writable by group or others
# (a sticky directory such as /tmp is fine as a parent: nobody else can rename our entry).
_cl_trusted() {
  emulate -L zsh
  local p=${1:A}
  local -A st
  zstat -H st -- $p 2>/dev/null || return 1
  (( st[uid] == 0 && !(st[mode] & 8#022) )) || return 1
  while [[ $p != / ]]; do
    p=${p:h}
    zstat -H st -- $p 2>/dev/null || return 1
    (( st[uid] == 0 && (!(st[mode] & 8#022) || (st[mode] & 8#1000)) )) || return 1
  done
}

# REPLY = the first trusted candidate, resolved: the path that was verified is the one
# exec'd, never a symlink in a directory someone else may write to.
_cl_pick_trusted() {
  emulate -L zsh
  local b
  for b; do
    [[ -f $b && -x $b ]] && _cl_trusted $b && { REPLY=${b:A}; return 0; }
  done
  return 1
}

# Sets REPLY to the cinelog binary to use. Root accepts only root-owned, non-writable paths
# and never looks at an inherited $HOME.
_cl_pick_bin() {
  emulate -L zsh
  local b
  REPLY=
  if (( EUID == 0 )); then
    local -a cand=(/usr/local/bin/cinelog)
    [[ $_cl_dir == */share/cinelog ]] && cand=(${_cl_dir:h:h}/bin/cinelog $cand)
    cand+=($_cl_dir/bin/cinelog ${commands[cinelog]})
    _cl_pick_trusted $cand && return 0
  else
    for b in $_cl_dir/bin/cinelog ${HOME:+$HOME/.local/bin/cinelog} ${commands[cinelog]}; do
      [[ -f $b && -x $b ]] && { REPLY=$b; return 0; }
    done
  fi
  return 1
}

# REPLY = what to do without a usable binary. Root never runs a binary from a module it
# does not trust (a member's ~/.zim checkout sourced by root's zsh), and cinelog-fetch
# refuses to work there as root: root shells are recorded by the team install, which that
# member runs once from their module.
_cl_no_bin() {
  emulate -L zsh
  REPLY="cinelog binary missing - run: cinelog-fetch"
  (( EUID == 0 )) && [[ -n $_cl_dir ]] && ! _cl_trusted $_cl_dir || return 0
  local -A st
  local b=${_cl_dir//[[:cntrl:]]/}/bin/cinelog pre=
  if [[ ! -x $b ]] && zstat -s -H st -- $_cl_dir 2>/dev/null; then
    pre="as ${st[uid]//[[:cntrl:]]/} run cinelog-fetch, then: "
  fi
  REPLY="root shells are recorded by the team install - ${pre:-run: }sudo ${(q-)b} install --team --system-zshrc"
}

# Registry entry of the recorder of this tty (PROTOCOL.md §6.5).
_cl_regfile() {
  emulate -L zsh
  local dir key
  if (( EUID == 0 )); then dir=/run/cinelog
  elif [[ -n $XDG_RUNTIME_DIR ]]; then dir=$XDG_RUNTIME_DIR/cinelog
  else dir=${XDG_STATE_HOME:-$HOME/.local/state}/cinelog/run
  fi
  if [[ $TTY == /dev/pts/<-> ]]; then key=pts-${TTY#/dev/pts/}
  else key=tty-${${TTY#/dev/}//\//-}
  fi
  REPLY=$dir/$key.json
}

# Reads the registry entry into REPLY if it is a regular 0600-style file of our euid.
# _cl_regread_sig identifies the version read: writers replace the file by rename, so
# every write has a new inode even within the same (whole) second of mtime.
_cl_regread() {
  emulate -L zsh
  local -A st
  zstat -L -H st -- $1 2>/dev/null || return 1
  (( st[uid] == EUID && (st[mode] & 8#170000) == 8#100000 && !(st[mode] & 8#077) )) || return 1
  typeset -g _cl_regread_sig=$st[inode]:$st[mtime]:$st[size]
  REPLY=$(<$1)
}

# Subshell of a recorded shell (`rec --check` said `recorded <uuid>`): take the nonce from
# the registry instead of an fd.
_cl_attach() {
  emulate -L zsh
  _cl_regfile; local f=$REPLY
  _cl_regread $f || return 1
  local j=$REPLY
  [[ $j =~ '"nonce":"([0-9a-f]{32})"' ]] || return 1
  local nonce=$match[1]
  [[ $j =~ '"uuid":"([^"]+)"' && $match[1] == $1 ]] || return 1
  # The recorder's CINELOG_OPTS string, so [shell] config applies here too.
  [[ $j =~ '"opts":"(([^"\\]|\\.)*)"' ]] && _cl_parse_opts "${${match[1]//\\\"/\"}//\\\\/\\}"
  _cl_nonce=$nonce _cl_session=$1 _cl_reg=$f
}

_cl_parse_opts() {   # CINELOG_OPTS: k=v pairs joined by ',', see PROTOCOL.md §6.6
  emulate -L zsh
  local kv v
  for kv in ${(s:,:)1}; do
    [[ $kv == *=* ]] || continue
    v=${kv#*=}; v=${v//\%3D/=}; v=${v//\%2C/,}; v=${v//\%25/%}
    _cl_opt[${kv%%=*}]=$v
  done
}

# Disabled, yet maybe started by a recorder (the switch appeared after its check): the nonce
# fd and the hand-over variables must still not reach children (PROTOCOL.md §6.2).
# CINELOG_DISABLE itself stays exported for them.
_cl_drop_handover() {
  emulate -L zsh
  if [[ $CINELOG_NONCE_FD == <-> && $CINELOG_REC_PID == $PPID ]] && (( CINELOG_NONCE_FD > 2 )) &&
     [[ -e /proc/$$/fd/$CINELOG_NONCE_FD ]]; then
    local fd=$CINELOG_NONCE_FD
    exec {fd}<&-
  fi
  local -a v=(${(k)parameters[(I)CINELOG_*]:#CINELOG_DISABLE})
  (( $#v )) && unset $v
  return 0
}

_cl_start() {
  emulate -L zsh
  local off=0
  if (( EUID == 0 )); then
    [[ -e /etc/cinelog/disable ]] && off=1
  elif [[ -n $CINELOG_DISABLE || -e ${XDG_CONFIG_HOME:-$HOME/.config}/cinelog/disable ]]; then
    off=1
  fi
  if (( off )); then
    _cl_drop_handover
    _cl_state=disabled; return 0
  fi

  # 1. The recorded inner shell: our direct parent passed the nonce on an fd for our tty.
  if [[ $CINELOG_NONCE_FD == <-> && $CINELOG_REC_PID == $PPID && $CINELOG_TTY == $TTY ]]; then
    local fd=$CINELOG_NONCE_FD nonce=
    if [[ -e /proc/$$/fd/$fd ]]; then
      read -r -t 1 -u $fd nonce
      exec {fd}<&-
    fi
    _cl_parse_opts "$CINELOG_OPTS"
    if [[ ${#nonce} == 32 && $nonce != *[^0-9a-f]* ]]; then
      _cl_nonce=$nonce _cl_session=$CINELOG_SESSION _cl_state=recorded
      _cl_regfile; _cl_reg=$REPLY
    else
      _cl_state=unrecorded; _cl_warn "not recording: no valid nonce from the recorder"
    fi
    unset -m 'CINELOG_*'
    return 0
  fi

  # 2. The recorder fell back to this unrecorded shell for this tty: never try again here.
  if [[ -n $CINELOG_FAILED && $CINELOG_FAILED_TTY == $TTY && $CINELOG_FAILED_PID == $$ ]]; then
    _cl_state=unrecorded; _cl_warn "not recording: $CINELOG_FAILED"
    unset -m 'CINELOG_*'
    return 0
  fi

  unset -m 'CINELOG_*'   # anything else is a stale value inherited from an ancestor

  # powerlevel10k instant prompt (zim-only setups load us after it) parks stdin on /dev/null
  # and stdout/stderr on a file until the first prompt; the terminal is on its saved fds.
  local -i ifd=0 ofd=1
  if (( ${+__p9k_instant_prompt_active} )) && [[ $__p9k_fd_0 == <-> && $__p9k_fd_1 == <-> ]]; then
    ifd=$__p9k_fd_0 ofd=$__p9k_fd_1
  fi

  # 3. Guards: only real interactive terminals; never `zsh -i -c`, editors resolving env, dumb.
  if [[ ! -o interactive || ! -t $ifd || ! -t $ofd || -n $ZSH_EXECUTION_STRING || $TERM == dumb
        || -n $VSCODE_RESOLVING_ENVIRONMENT || -n $INSIDE_EMACS ]]; then
    _cl_state=guarded; return 0
  fi
  _cl_state=unrecorded
  if ! _cl_pick_bin; then
    _cl_no_bin; _cl_warn $REPLY
    _cl_autofetch
    return 0
  fi
  local bin=$REPLY st
  st=$($bin rec --check <&$ifd 2>/dev/null)   # it identifies the tty from stdin
  case ${st%% *} in
    start)
      local sh=${${:-/proc/$$/exe}:A}
      [[ -x $sh ]] || sh=${commands[zsh]:-/bin/zsh}
      # A failed exec would end this shell (zsh _exits, no hook runs): exec only a binary that
      # still exists and answered just now. Updates replace it by rename, never in place.
      if [[ -f $bin && -x $bin ]]; then
        (( ifd )) && _cl_p10k_release
        # VS Code's injection points ZDOTDIR at the user's directory before it sources this
        # .zshrc; point it back so the recorded shell runs the injection (OSC 633) again.
        [[ $VSCODE_INJECTION == 1 && -f $VSCODE_ZDOTDIR/.zshrc ]] && export ZDOTDIR=$VSCODE_ZDOTDIR
        exec $bin rec -- $sh
      fi
      _cl_warn "not recording: $bin vanished before it could start" ;;
    recorded)
      if _cl_attach ${st#* }; then _cl_state=recorded
      else _cl_warn "not recording: cannot read the registry of this tty"
      fi ;;
    skip)
      [[ $st == 'skip disabled' ]] || _cl_warn "not recording: ${st#* }" ;;
    *)
      _cl_warn "not recording: '$bin rec --check' failed" ;;
  esac
}

# Ends a powerlevel10k instant prompt before the exec: its own cleanup erases the painted
# prompt, replays captured output and restores the terminal fds; else restore them here.
_cl_p10k_release() {
  if (( $+functions[_p9k_instant_prompt_cleanup] )); then
    _p9k_instant_prompt_cleanup 0
  fi
  if (( ${+__p9k_instant_prompt_active} )) && [[ $__p9k_fd_0 == <-> && $__p9k_fd_1 == <-> && $__p9k_fd_2 == <-> ]]; then
    exec 0<&$__p9k_fd_0 1>&$__p9k_fd_1 2>&$__p9k_fd_2 {__p9k_fd_0}<&- {__p9k_fd_1}>&- {__p9k_fd_2}>&-
    unset __p9k_fd_0 __p9k_fd_1 __p9k_fd_2 __p9k_instant_prompt_active
  fi
}

# Missing binary: `fetch.zsh --background` detaches a download, at most once a day, never
# for root. Opt out with `zstyle ':cinelog:' auto-fetch no`.
_cl_autofetch() {
  emulate -L zsh
  (( EUID )) && [[ -r $_cl_dir/shell/fetch.zsh ]] && zstyle -T ':cinelog:' auto-fetch || return 0
  zsh -f $_cl_dir/shell/fetch.zsh --background </dev/null >/dev/null 2>&1
}

if (( ! ${+_cl_state} )); then
  typeset -g _cl_state= _cl_dir=${${(%):-%x}:A:h} _cl_nonce= _cl_session= _cl_reg=
  typeset -g _cl_regsig= _cl_regread_sig= _cl_degraded=0
  typeset -gi _cl_n=0 _cl_open=0
  typeset -gA _cl_opt=(motd 1 banners 1 check_free_space 1 external_ip 0 title 1 title_link 0 url '' web '')
  _cl_start
fi
if [[ $_cl_state == (disabled|guarded) ]]; then
  typeset -g _cl_loaded=1
  unset _cl_early_only
  return 0
fi
if (( ${+_cl_early_only} )); then   # sourced by shell/early.zsh: start only
  unset _cl_early_only
  return 0
fi
typeset -g _cl_loaded=1

# ------------------------------------------------------------------------------ markers

() {
  emulate -L zsh
  # _cl_b12[v+1]: the two base64 characters of the 12-bit value v (a 64x64 product).
  local -a s=(${(s::):-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/})
  typeset -ga _cl_b12=(${^s}${^s})
  # JSON escapes of the C0 bytes and DEL; _cl_ctl lists them, the common ones first.
  typeset -gA _cl_esc=()
  typeset -ga _cl_ctl=($'\n' $'\t' $'\r' $'\e')
  local -i i
  for (( i = 0; i < 32; i++ )); do
    _cl_esc[${(#)i}]=\\u00${(l:2::0:)$(( [##16] i ))}
    (( i == 9 || i == 10 || i == 13 || i == 27 )) || _cl_ctl+=(${(#)i})
  done
  local del=$'\x7f'
  _cl_esc[$del]='\u007F'; _cl_ctl+=($del)
}

# JSON string body of $1 in REPLY: \ and " escaped, every byte 0x00-0x1f and 0x7f as \u00XX.
# Split and join instead of ${s//x/y}, whose cost grows with the square of the matches.
_cl_jstr() {
  emulate -L zsh; setopt nomultibyte
  local s=$1 c e
  if [[ $s == *[\\\"]* ]]; then
    c='\' e='\\'; s=${(pj:$e:)"${(@ps:$c:)s}"}
    c='"' e='\"'; s=${(pj:$e:)"${(@ps:$c:)s}"}
  fi
  for c in "${_cl_ctl[@]}"; do
    [[ $s == *[[:cntrl:]]* ]] || break
    [[ $s == *$c* ]] || continue
    e=$_cl_esc[$c]; s=${(pj:$e:)"${(@ps:$c:)s}"}
  done
  REPLY=$s
}

# Base64 (standard alphabet, no padding) of the bytes of $1 in REPLY. One command per three
# bytes: missing bytes at the end read as 0 and the surplus characters are cut off.
_cl_b64() {
  emulate -L zsh; setopt nomultibyte
  local o= x y z
  for x y z in "${(@s::)1}"; do
    o+=${_cl_b12[1+(#x<<4|#y>>4)]}${_cl_b12[1+((#y&15)<<8|#z)]}
  done
  REPLY=${o[1,(4*${#1}+2)/3]}
}

# Cuts $1 to at most $2 bytes on a UTF-8 character boundary; REPLY = result. Returns 1 if
# nothing was cut.
_cl_cut() {
  emulate -L zsh; setopt nomultibyte
  REPLY=$1
  (( ${#REPLY} > $2 )) || return 1
  REPLY=${REPLY[1,$2]}
  local c
  local -i i b k
  for (( i = 0; i < 4 && i < ${#REPLY}; i++ )); do
    c=${REPLY[-1-i]}; b=$(( #c & 255 ))
    (( b >= 0x80 && b < 0xc0 )) && continue                    # continuation byte
    if (( b >= 0xc0 )); then
      (( k = b >= 0xf0 ? 4 : b >= 0xe0 ? 3 : 2 ))
      (( i + 1 < k )) && REPLY=${REPLY[1,-2-i]}                  # incomplete sequence
    fi
    break
  done
  return 0
}

_cl_now() {   # $EPOCHREALTIME with at most 6 decimals
  emulate -L zsh
  local t=${EPOCHREALTIME/,/.}
  REPLY=${t%%.*}.${${t#*.}[1,6]}
}

# Emits one marker for the JSON members in $1 (after "v":1).
_cl_emit() {
  emulate -L zsh; setopt nomultibyte
  local j="{\"v\":1,$1}"
  if (( ${#j} <= 1536 )); then
    _cl_b64 $j
    print -rn -- $'\e]7777;cinelog;1;'$_cl_nonce$';b;'$REPLY$'\a'
  else
    print -rn -- $'\e]7777;cinelog;1;'$_cl_nonce$';j;'$j$'\a'
  fi
}

_cl_hello() {
  emulate -L zsh
  local stat f j
  local -a fs
  stat=$(</proc/$$/stat) 2>/dev/null && fs=(${=${stat##*\) }})
  _cl_now; j="\"k\":\"hello\",\"sh\":$$,\"sst\":${fs[20]:-0},\"ppid\":$PPID,\"uid\":$UID,\"euid\":$EUID"
  local t=$REPLY k
  for k f in user "$USERNAME" host "$HOST" tty "$TTY" cwd "$PWD" zsh "$ZSH_VERSION"; do
    _cl_jstr $f; j+=",\"$k\":\"$REPLY\""
  done
  if [[ -n $TMUX ]]; then
    local -a tm=("${(@s:,:)TMUX}")
    local sock=${(j:,:)tm[1,-3]} spid=$tm[-2] sid=$tm[-1]
    [[ $spid == <-> ]] || spid=0
    _cl_jstr $sock; j+=",\"tmux\":{\"socket\":\"$REPLY\",\"server_pid\":$spid"
    _cl_jstr $sid; j+=",\"session_id\":\"$REPLY\""
    _cl_jstr $TMUX_PANE; j+=",\"pane\":\"$REPLY\"}"
  fi
  _cl_emit "$j,\"t\":$t"
}

_cl_banner() {   # right-aligned "Begin [YYYY-MM-DD HH:MM:SS]" / "End   [...]"
  emulate -L zsh
  local d
  strftime -s d '%Y-%m-%d %H:%M:%S' $EPOCHSECONDS
  printf '%*s\n' $(( COLUMNS > 0 ? COLUMNS : 80 )) "$1 [$d]"
}

_cl_title() {   # OSC 0: user@host: cwd [command] [session link]
  emulate -L zsh
  local t=${(%):-%n@%m: %~}
  [[ -n $1 ]] && t+=" ${1[1,200]}"
  if [[ $_cl_opt[title_link] == 1 && -n $_cl_session ]]; then
    t+=" [${_cl_opt[url]:-cl:${${_cl_session//-/}[-8,-1]}}]"
  fi
  setopt extendedglob
  print -rn -- $'\e]0;'${t//[[:cntrl:]]##/ }$'\a'
}

# The members of every cmd marker that are fixed for the life of the shell, escaped once.
_cl_ident() {
  emulate -L zsh
  local u
  _cl_jstr "$USERNAME"; u=$REPLY
  _cl_jstr "$HOST"
  typeset -g _cl_jid=",\"user\":\"$u\",\"euid\":$EUID,\"host\":\"$REPLY\""
  [[ -n $TMUX_PANE ]] && { _cl_jstr "$TMUX_PANE"; _cl_jid+=",\"pane\":\"$REPLY\""; }
  return 0
}

_cl_preexec() {
  emulate -L zsh; setopt nomultibyte   # ${#...} counts bytes
  [[ -n $_cl_nonce ]] || return 0
  _cl_now; local t=$REPLY
  if [[ $1 == ' '* ]]; then   # leading space: no command text, only a visible skip marker
    print -rn -- $'\e]133;C\a'
    _cl_emit "\"k\":\"skip\",\"sh\":$$,\"t\":$t"
    _cl_open=2
    return 0
  fi
  local rest typed=${1:-$3} expanded=$3 trunc= et ee
  _cl_jstr "$PWD"; rest=",\"cwd\":\"$REPLY\"$_cl_jid,\"t\":$t"
  # typed and expanded are each capped at 16 KiB; cut further if escaping would push the
  # payload past the recorder's 64 KiB limit (an oversized marker would reach the screen).
  # The second round scales the cap by the overflow; escaping grows a byte to at most 6, so
  # the third round's cap always fits.
  local -i lim=16384 budget=$(( 65000 - ${#rest} )) size round=0
  while :; do
    _cl_cut "$typed" $lim && { typed=$REPLY; trunc=1; }
    _cl_cut "$expanded" $lim && { expanded=$REPLY; trunc=1; }
    _cl_jstr "$typed"; et=$REPLY
    if [[ $expanded == "$typed" ]]; then ee=$et; else _cl_jstr "$expanded"; ee=$REPLY; fi
    (( size = ${#et} + ${#ee}, size < budget )) && break
    (( lim = ++round == 1 ? lim * (budget - 1) / size : (budget - 1) / 12 ))
  done
  [[ -n $trunc ]] && rest+=",\"trunc\":true"
  (( _cl_n++ ))
  print -rn -- $'\e]133;C\a'
  _cl_emit "\"k\":\"cmd\",\"sh\":$$,\"n\":$_cl_n,\"typed\":\"$et\",\"expanded\":\"$ee\"$rest"
  _cl_open=1
  [[ $_cl_opt[banners] == 1 ]] && _cl_banner Begin
  [[ $_cl_opt[title] == 1 ]] && _cl_title "$typed"
  return 0
}

_cl_precmd() {
  local ex=$? ps=${(j:,:)pipestatus}   # one statement: both still describe the command
  emulate -L zsh
  [[ -n $_cl_nonce ]] || return 0
  # Loaded before a powerlevel10k instant prompt began: wait once for its cleanup (see
  # _cl_after_instant_prompt).
  if (( ${+__p9k_instant_prompt_active} && ! ${+_cl_waited} )) && zmodload zsh/sched 2>/dev/null; then
    typeset -g _cl_waited=1
    sched +0 _cl_precmd
    return 0
  fi
  (( ${+_cl_said_hello} )) || { typeset -g _cl_said_hello=1; _cl_hello; }
  if (( _cl_open == 1 )); then
    print -rn -- $'\e]133;D;'$ex$'\a'
    _cl_now; local t=$REPLY
    _cl_jstr "$PWD"
    _cl_emit "\"k\":\"done\",\"sh\":$$,\"n\":$_cl_n,\"exit\":$ex,\"pipestatus\":[$ps],\"cwd\":\"$REPLY\",\"t\":$t"
    [[ $_cl_opt[banners] == 1 ]] && _cl_banner 'End  '
  elif (( _cl_open == 2 )); then
    print -rn -- $'\e]133;D;'$ex$'\a'
  fi
  _cl_open=0
  setopt localoptions extendedglob nomultibyte
  local p=${PWD//(#m)[^A-Za-z0-9\/._~-]/%${(l:2::0:)$(( [##16] #MATCH & 255 ))}}
  print -rn -- $'\e]133;A\a\e]7;file://'$HOST$p$'\a'
  [[ $_cl_opt[title_link] == 1 && $_cl_opt[title] == 1 ]] && _cl_title
  _cl_check_degraded
  return 0
}

_cl_zshexit() {   # `exit` typed at the prompt: close its command
  local ex=$?
  emulate -L zsh
  (( _cl_open == 1 && ! ZSH_SUBSHELL )) || return 0   # zsh also runs it for `( exit )`
  print -rn -- $'\e]133;D;'$ex$'\a'
  _cl_now
  _cl_emit "\"k\":\"done\",\"sh\":$$,\"n\":$_cl_n,\"exit\":$ex,\"pipestatus\":[$ex],\"t\":$REPLY"
  _cl_open=0
}

# One stat per prompt, no fork; the entry is re-read only when the recorder replaced it.
_cl_check_degraded() {
  emulate -L zsh
  [[ -n $_cl_reg ]] || return 0
  local -A st
  zstat -L -H st -- $_cl_reg 2>/dev/null || return 0
  [[ $st[inode]:$st[mtime]:$st[size] == $_cl_regsig ]] && return 0
  _cl_regread $_cl_reg || return 0
  _cl_regsig=$_cl_regread_sig
  if [[ $REPLY == *'"state":"degraded"'* ]]; then
    (( _cl_degraded )) && return 0
    _cl_degraded=1
    local why=; [[ $REPLY =~ '"state_reason":"([^"]*)"' ]] && why=" ($match[1])"
    print -ru2 -- "cinelog: recording stopped$why - output is no longer saved; run: cinelog resume"
  else
    _cl_degraded=0
  fi
}

# Once per recorded shell, independent of motd and of earlier warnings (PROTOCOL.md §7).
_cl_check_protocol() {
  emulate -L zsh
  (( ${+_cl_proto_checked} )) && return 0
  typeset -g _cl_proto_checked=1
  _cl_pick_bin || return 0
  local v=$($REPLY rec --protocol 2>/dev/null)
  [[ ${v%%.*} == 1 ]] || print -ru2 -- "cinelog: $REPLY does not speak marker protocol 1 - run: cinelog-fetch"
}

autoload -Uz add-zsh-hook
if [[ $_cl_state == recorded ]]; then
  _cl_ident
  add-zsh-hook preexec _cl_preexec
  # Under a powerlevel10k instant prompt the first precmd waits (see _cl_after_instant_prompt).
  (( ${+__p9k_instant_prompt_active} )) || add-zsh-hook precmd _cl_precmd
  add-zsh-hook zshexit _cl_zshexit
  _cl_check_protocol
fi

# ---------------------------------------------------------------------- user interface

typeset -gA _cl_c=(
  reset $'\e[0m' bold $'\e[1m' red $'\e[31m' green $'\e[32m' cyan $'\e[36m'
  pred $'\e[38;2;255;105;97m' pgreen $'\e[38;2;119;221;119m' pyellow $'\e[38;2;253;253;150m'
  pmagenta $'\e[38;2;244;154;194m'
)

_cl_uptime() {   # like `uptime -p`, from /proc/uptime without a fork
  emulate -L zsh
  local up _
  read -r up _ </proc/uptime 2>/dev/null || return 1
  local -i s=${up%%.*} n
  local -a parts
  local u
  for u n in week $(( s / 604800 )) day $(( s % 604800 / 86400 )) hour $(( s % 86400 / 3600 )) \
             minute $(( s % 3600 / 60 )); do
    (( n )) && parts+=("$n $u${${n:#1}:+s}")
  done
  (( $#parts )) || parts=('0 minutes')
  REPLY="up ${(j:, :)parts}"
}

function load_motd {
  emulate -L zsh
  local -A c=("${(@kv)_cl_c}")
  if [[ -n $_cl_session ]]; then
    local cast=
    [[ -n $_cl_reg ]] && _cl_regread $_cl_reg && [[ $REPLY =~ '"cast":"(([^"\\]|\\.)*)"' ]] &&
      cast=${${match[1]//\\\"/\"}//\\\\/\\}
    print -r -- "$c[pgreen]cinelog: session ${${_cl_session//-/}[-8,-1]}${cast:+ → $cast}${_cl_opt[url]:+ · $_cl_opt[url]}$c[reset]"
    # The recorder's view of the personal web UI (PROTOCOL.md §6.6 `web`).
    case $_cl_opt[web] in
      (install)
        print -r -- "$c[pyellow]cinelog: web UI not set up yet - run: cinelog install (prints your login token)$c[reset]" ;;
      (starting)
        print -r -- "$c[pyellow]cinelog: web UI is starting - cinelog link prints this session's address$c[reset]" ;;
    esac
  fi
  [[ $_cl_opt[motd] == 1 ]] || return 0
  _cl_skew
  _cl_old_viewer
  _cl_uptime && print -r -- "Uptime: $REPLY"
  if (( $+commands[ip] )); then
    local -a nets=(${(f)"$(ip -brief -color -4 a 2>/dev/null)"})
    print -rl -- Networks: ${nets:#*UNKNOWN*}
  fi
  if [[ $_cl_opt[external_ip] == 1 ]] && (( $+commands[curl] )); then
    local ip gw dev
    local -a route=(${=${(M)${(f)"$(ip route 2>/dev/null)"}:#default *}[1]})
    gw=${route[3]} dev=${route[5]}
    if ip=$(curl --connect-timeout 1 -m 1 -4 -s https://my.ip.fi/ 2>/dev/null) && [[ -n $ip ]]; then
      print -r -- "$c[cyan]External IP:$c[reset]	 $c[green]UP		$c[pmagenta]$ip $c[reset]GW:$c[pmagenta] $gw$c[reset] ($dev)"
    else
      print -r -- "$c[cyan]External IP:$c[reset]	 $c[red]DOWN$c[reset]	        $c[pmagenta]Proxy or Offline$c[reset]"
    fi
  else
    print
  fi
  [[ $_cl_opt[check_free_space] == 1 ]] && _cl_free_space
  return 0
}

# Version skew between this binary, the plugin checkout, the team binary and the running
# server (`cinelog doctor --skew`, PROTOCOL.md §6.1). Asked once per shell; `cs` reprints the
# answer. Only its `cinelog: ` lines count, so an older binary without the flag prints nothing.
# It runs the team binary's --version, so a hung binary or mount is cut off after 1 s.
_cl_skew() {
  emulate -L zsh
  if (( ! ${+_cl_skew_lines} )); then
    typeset -ga _cl_skew_lines=()
    local -a tmo
    (( $+commands[timeout] )) && tmo=(timeout -k 1 1)
    _cl_pick_bin &&
      _cl_skew_lines=(${(M)${(f)"$($tmo $REPLY doctor --skew </dev/null 2>/dev/null)"}:#cinelog: *})
  fi
  local l
  for l in $_cl_skew_lines; do
    print -r -- "$_cl_c[pyellow]${l//[[:cntrl:]]/}$_cl_c[reset]"
  done
  return 0
}

# Checkouts from before 1.0 keep the old viewer's session pages (recorded output) in the
# git-ignored asciinema-player/; nothing uses them any more, so ask once per MOTD to remove them.
_cl_old_viewer() {
  emulate -L zsh
  local d=$_cl_dir/asciinema-player
  [[ -n $_cl_dir && -d $d ]] || return 0
  print -r -- "$_cl_c[pyellow]cinelog: ${d//[[:cntrl:]]/} holds the old viewer's pages of recorded output; remove it: rm -rf ${(q-)d//[[:cntrl:]]/}$_cl_c[reset]"
}

# Local mounts at 90 % or more. df right-aligns the percentage (" 95% /"); a hung mount
# cannot block the prompt for more than 2 s.
_cl_free_space() {
  emulate -L zsh; setopt extendedglob
  (( $+commands[df] )) || return 0
  local -a tmo
  (( $+commands[timeout] )) && tmo=(timeout -k 2 2)
  local l pct mnt
  for l in ${${(f)"$($tmo df -H -l --output=pcent,target 2>/dev/null)"}[2,-1]}; do
    l=${l##[[:space:]]#}; pct=${l%%\%*}; mnt=${${l#*%}##[[:space:]]#}
    [[ $pct == <-> && $mnt != /tmp/com.freerdp* ]] || continue
    (( pct >= 90 )) &&
      print -r -- "$_cl_c[pyellow]Running out of disk space $_cl_c[bold]$_cl_c[pyellow]\"$mnt ($_cl_c[pred]$pct%$_cl_c[pyellow])\"$_cl_c[reset]"
  done
  return 0
}

_cl_run() {
  _cl_pick_bin || { _cl_no_bin; print -ru2 -- "cinelog: $REPLY"; return 127; }
  $REPLY "$@"
}
# `function` syntax: a user alias of the same name must not break these.
(( $+commands[cinelog] )) || function cinelog { _cl_run "$@" }
function cinelog-link { _cl_run link "$@" }
function cinelog-fetch { zsh $_cl_dir/shell/fetch.zsh "$@" }
alias cs='clear; load_motd'

fpath=($_cl_dir/shell/completions $fpath)
if (( $+functions[compdef] )); then   # compinit already ran
  autoload -Uz _cinelog && compdef _cinelog cinelog
fi

# powerlevel10k's instant prompt sends stdout to a capture file until its cleanup, which its
# own first precmd (ahead of ours) queues with `sched +0`. The MOTD and the first precmd of a
# recorded shell are queued behind it, so they reach the terminal instead of that file.
_cl_after_instant_prompt() {
  emulate -L zsh
  add-zsh-hook -d precmd _cl_after_instant_prompt
  if (( ${+__p9k_instant_prompt_active} )) && zmodload zsh/sched 2>/dev/null; then
    sched +0 _cl_first_prompt
  else
    _cl_first_prompt
  fi
}
_cl_first_prompt() {
  emulate -L zsh
  [[ -t 1 ]] && load_motd
  if [[ $_cl_state == recorded ]]; then
    add-zsh-hook precmd _cl_precmd
    _cl_precmd
  fi
  return 0
}

if (( ${+__p9k_instant_prompt_active} )); then
  add-zsh-hook precmd _cl_after_instant_prompt
elif [[ -t 1 ]]; then
  load_motd
fi
