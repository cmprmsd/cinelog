#!/usr/bin/env zsh
# cinelog binary fetcher (plan §10.2). Run from zim's `--on-pull 'zsh shell/fetch.zsh'` or
# through the plugin's `cinelog-fetch`.
#
#  1. Read VERSION and map `uname -m` to a release target.
#  2. Expected sha256 comes from shell/checksums/v<VERSION>.sha256 in this repo (the trust
#     anchor); when that file does not exist yet, the release's SHA256SUMS is used with a warning.
#  3. Download to bin/cinelog.new, verify, run `--version`, then rename onto bin/cinelog.
#     The old binary stays in place until the new one is verified.
#  4. Fallback: `cargo build --release --locked` (then offline, then with the user's default
#     rustup toolchain in place of rust-toolchain.toml).
#  5. `bin/cinelog install --refresh` (non-root only) after a change, and on every later run
#     until it has succeeded once.
#
# Exit status: 0 = bin/cinelog is a working binary of VERSION; 1 = it is not (any old binary
# is kept); 2 = usage error. Sourcing this file only defines the _clf_* functions (tests).

typeset -g _clf_self=${${(%):-%x}:A}

_clf_usage() {
  print -r -- "usage: zsh shell/fetch.zsh [options]
  -q, --quiet         only print warnings and errors
  --force             download even if bin/cinelog is current
  --source            skip the download and build with cargo
  --no-cargo          never fall back to cargo
  --no-refresh        do not run 'cinelog install --refresh' now (the next run does)
  --background        detached, at most once a day, download only (non-root)
  --dir DIR           module directory (default: the parent of shell/)
  --base-url URL      release base (https://, file:// or an absolute path)
  --arch MACHINE      override uname -m
environment: CINELOG_FETCH_BASE_URL, CARGO_TARGET_DIR, RUSTUP_TOOLCHAIN"
}

_clf_info() { (( ${_clf_quiet:-0} )) || print -ru2 -- "cinelog-fetch: $*" }
_clf_warn() { print -ru2 -- "cinelog-fetch: warning: $*" }
_clf_err()  { print -ru2 -- "cinelog-fetch: error: $*" }

# Release target triple for a machine/kernel pair; fails for unsupported platforms.
_clf_target() {
  local machine=${1:-$(uname -m)} os=${2:-$(uname -s)}
  [[ $os == Linux ]] || return 1
  case $machine in
    x86_64|amd64)  REPLY=x86_64-unknown-linux-musl ;;
    aarch64|arm64) REPLY=aarch64-unknown-linux-musl ;;
    *) return 1 ;;
  esac
}

# First line of VERSION, validated (it becomes part of a URL and a file name).
_clf_read_version() {
  emulate -L zsh -o extended_glob
  local file=$1/VERSION line
  [[ -r $file ]] || { _clf_err "missing $file"; return 1 }
  IFS= read -r line < $file || [[ -n $line ]] || { _clf_err "empty $file"; return 1 }
  line=${${line##[[:space:]]#}%%[[:space:]]#}
  [[ $line == <->.<->.<->(-[[:alnum:].-]##|) ]] || { _clf_err "invalid version '$line' in $file"; return 1 }
  REPLY=$line
}

# Looks up <name> in sha256sum-format text on stdin. Conflicting entries are an error.
_clf_sum_for() {
  emulate -L zsh -o extended_glob
  local name=$1 line hex= h
  local -a w
  while IFS= read -r line || [[ -n $line ]]; do
    w=(${=line})
    (( $#w == 2 )) || continue
    [[ ${w[2]#\*} == $name && $w[1] == [[:xdigit:]](#c64) ]] || continue
    h=${(L)w[1]}
    if [[ -n $hex && $hex != $h ]]; then
      _clf_err "conflicting checksums for $name"; return 1
    fi
    hex=$h
  done
  [[ -n $hex ]] || return 1
  REPLY=$hex
}

_clf_sha256() {
  local out
  if (( $+commands[sha256sum] )); then out=$(sha256sum -- $1) || return 1
  elif (( $+commands[shasum] )); then out=$(shasum -a 256 -- $1) || return 1
  elif (( $+commands[openssl] )); then out=$(openssl dgst -sha256 -r -- $1) || return 1
  else _clf_err "no sha256 tool (sha256sum, shasum or openssl)"; return 1
  fi
  REPLY=${(L)${out%%[[:space:]]*}}
}

# Allowed release locations: https, file:// and absolute paths (tests, mirrors).
_clf_check_base() {
  case $1 in
    https://?*|file:///?*|/?*) return 0 ;;
    *) _clf_err "refusing release base '$1' (https://, file:// or an absolute path only)"; return 1 ;;
  esac
}

# Fetches <url> to <dest>; at most <max> bytes.
_clf_download() {
  local url=$1 dest=$2 max=${3:-268435456}
  case $url in
    https://*)
      if (( $+commands[curl] )); then
        curl -fsSL --proto '=https' --tlsv1.2 --retry 2 --connect-timeout 15 --max-time 900 \
          --max-filesize $max -o $dest -- $url
      elif (( $+commands[wget] )); then
        wget -q --https-only --secure-protocol=TLSv1_2 --tries=2 --timeout=15 -O $dest -- $url
      else
        _clf_err "neither curl nor wget is installed"; return 1
      fi ;;
    file://*) _clf_copy_local ${url#file://} $dest $max ;;
    /*)       _clf_copy_local $url $dest $max ;;
    *)        return 1 ;;
  esac
}

_clf_copy_local() {
  zmodload -F zsh/stat b:zstat || return 1
  local -A st
  [[ -f $1 && -r $1 ]] && zstat -H st -- $1 2>/dev/null || return 1
  (( st[size] <= $3 )) || { _clf_err "$1 is larger than $3 bytes"; return 1 }
  command cp -f -- $1 $2
}

# True if <path> and every parent directory are owned by <uid> and not group/other-writable.
_clf_tree_secure() {
  emulate -L zsh
  zmodload -F zsh/stat b:zstat || return 1
  local p=${1:A} want=$2
  local -A st
  while :; do
    zstat -L -H st -- $p 2>/dev/null || return 1
    (( st[uid] == want && (st[mode] & 8#022) == 0 )) || return 1
    [[ $p == / ]] && return 0
    p=${p:h}
  done
}

# Same, for files inside the module that root trusts (VERSION, checksum files, the binary).
_clf_file_secure() {
  emulate -L zsh
  zmodload -F zsh/stat b:zstat || return 1
  local -A st
  zstat -L -H st -- $1 2>/dev/null || return 1
  (( st[uid] == $2 && (st[mode] & 8#022) == 0 ))
}

# True if <bin> runs and reports exactly `cinelog <version>`.
_clf_version_ok() {
  local bin=$1 want=$2 out
  [[ -f $bin && -x $bin ]] || return 1
  if (( $+commands[timeout] )); then out=$(timeout 15 $bin --version </dev/null 2>/dev/null)
  else out=$($bin --version </dev/null 2>/dev/null)
  fi || return 1
  [[ $out == "cinelog $want" ]]
}

# Sets REPLY to the expected sha256 of cinelog-<target> and _clf_sum_src to repo|release.
# Returns 1 if no checksum could be obtained, 2 if the repo file exists but is unusable.
_clf_expected_sum() {
  local dir=$1 ver=$2 target=$3 base=$4 repo=$1/shell/checksums/v$2.sha256
  if [[ -e $repo ]]; then
    if (( EUID == 0 )) && ! _clf_file_secure $repo 0; then
      _clf_err "$repo is not root-owned or is writable by others"; return 2
    fi
    if ! _clf_sum_for cinelog-$target < $repo; then
      _clf_err "$repo has no valid entry for cinelog-$target"; return 2
    fi
    _clf_sum_src=repo
    return 0
  fi
  local tmp=${_clf_tmp:?}/SHA256SUMS
  if ! _clf_download $base/v$ver/SHA256SUMS $tmp 65536; then
    _clf_warn "no checksum for v$ver (repo file missing, release SHA256SUMS unreachable)"
    return 1
  fi
  _clf_sum_for cinelog-$target < $tmp || { _clf_err "release SHA256SUMS has no entry for cinelog-$target"; return 1 }
  _clf_warn "shell/checksums/v$ver.sha256 not in the repo yet; verified against the release's SHA256SUMS"
  _clf_sum_src=release
}

# Moves a verified candidate onto bin/cinelog (same directory, so rename(2) is atomic).
_clf_activate() {
  local new=$1 bin=$2 ver=$3
  command chmod 0755 -- $new || return 1
  if ! _clf_version_ok $new $ver; then
    _clf_err "new binary does not report 'cinelog $ver'; keeping the old one"
    return 1
  fi
  command mv -f -- $new $bin
}

_clf_from_release() {
  local dir=$1 ver=$2 target=$3 base=$4 new=$1/bin/cinelog.new expected
  _clf_info "downloading cinelog $ver ($target)"
  if ! _clf_download $base/v$ver/cinelog-$target $new; then
    _clf_warn "download of $base/v$ver/cinelog-$target failed"
    return 1
  fi
  expected=$5
  _clf_sha256 $new || return 1
  if [[ $REPLY != $expected ]]; then
    _clf_err "checksum mismatch for cinelog-$target ($_clf_sum_src): expected $expected, got $REPLY"
    return 3
  fi
  _clf_activate $new $dir/bin/cinelog $ver
}

# Runs one `cargo build` of the recorder in <dir>; args: <dir> <cargo> <target-dir> <toolchain|""> [cargo args].
_clf_cargo_build() {
  local dir=$1 cargo=$2 tdir=$3 tc=$4
  shift 4
  local -a q
  (( ${_clf_quiet:-0} )) && q=(--quiet)
  ( builtin cd -q -- $dir || exit 1
    export CARGO_TARGET_DIR=$tdir
    [[ -n $tc ]] && export RUSTUP_TOOLCHAIN=$tc
    $cargo build $q --release --locked --bin cinelog "$@" >&2 )
}

# The user's default rustup toolchain. rust-toolchain.toml can name a channel or components
# the user has not installed, and rustup cannot download them offline; RUSTUP_TOOLCHAIN
# overrides the file. A user-set RUSTUP_TOOLCHAIN is left alone.
_clf_default_toolchain() {
  emulate -L zsh -o extended_glob
  local cargo=$1 rustup=${commands[rustup]:-} out
  [[ -z $rustup && -x ${cargo:h}/rustup ]] && rustup=${cargo:h}/rustup
  [[ -n $rustup && -z ${RUSTUP_TOOLCHAIN:-} && -f $2/rust-toolchain.toml ]] || return 1
  out=$(builtin cd -q / && $rustup default 2>/dev/null </dev/null) || return 1
  local -a lines=("${(@f)out}")
  out=${lines[1]%% *}
  [[ -n $out && $out != *[^[:alnum:]._-]* ]] || return 1
  REPLY=$out
}

_clf_from_source() {
  local dir=$1 ver=$2 new=$1/bin/cinelog.new tdir=${CARGO_TARGET_DIR:-$1/target}
  local cargo=${commands[cargo]:-}
  [[ -z $cargo && -x $HOME/.cargo/bin/cargo ]] && cargo=$HOME/.cargo/bin/cargo
  [[ -n $cargo ]] || { _clf_warn "cargo not found; cannot build from source"; return 1 }
  [[ -f $dir/Cargo.toml && -f $dir/Cargo.lock ]] || { _clf_warn "no Cargo.toml/Cargo.lock in $dir"; return 1 }
  [[ $tdir == /* ]] || tdir=$dir/$tdir
  _clf_info "building cinelog $ver with cargo (this takes a few minutes)"
  if ! _clf_cargo_build $dir $cargo $tdir ''; then
    _clf_warn "cargo build failed; retrying offline"
    if ! _clf_cargo_build $dir $cargo $tdir '' --offline; then
      if ! _clf_default_toolchain $cargo $dir; then
        _clf_err "cargo build failed"; return 1
      fi
      local tc=$REPLY
      _clf_warn "retrying with your default toolchain $tc instead of rust-toolchain.toml"
      _clf_cargo_build $dir $cargo $tdir $tc || _clf_cargo_build $dir $cargo $tdir $tc --offline ||
        { _clf_err "cargo build failed"; return 1 }
    fi
  fi
  command cp -f -- $tdir/release/cinelog $new || return 1
  _clf_activate $new $dir/bin/cinelog $ver
}

_clf_state_dir() { REPLY=${XDG_STATE_HOME:-$HOME/.local/state}/cinelog }

# Starts a detached fetch at most once a day (the plugin calls this when the binary is missing).
_clf_background() {
  emulate -L zsh
  local dir=$1 base=$2 arch=$3
  if (( EUID == 0 )); then _clf_err "background fetch is not allowed for root"; return 1; fi
  zmodload -F zsh/stat b:zstat; zmodload -F zsh/datetime p:EPOCHSECONDS
  _clf_state_dir; local sd=$REPLY
  command mkdir -p -m 0700 -- $sd || return 1
  local stamp=$sd/fetch.stamp log=$sd/fetch.log
  local -A st
  if zstat -H st -- $stamp 2>/dev/null && (( EPOCHSECONDS - st[mtime] < 86400 )); then
    return 0
  fi
  : >| $stamp || return 1
  if zstat -H st -- $log 2>/dev/null && (( st[size] > 65536 )); then : >| $log; fi
  local -a args=(--quiet --no-cargo --dir $dir)
  [[ -n $base ]] && args+=(--base-url $base)
  [[ -n $arch ]] && args+=(--arch $arch)
  zsh -f -- $_clf_self --bg-child $args </dev/null >>$log 2>&1 &!
  return 0
}

_clf_main() {
  emulate -L zsh -o extended_glob
  local -a o_help o_quiet o_force o_source o_nocargo o_norefresh o_bg o_nowait o_dir o_base o_arch
  zparseopts -D -E -F -- h=o_help -help=o_help q=o_quiet -quiet=o_quiet -force=o_force \
    -source=o_source -no-cargo=o_nocargo -no-refresh=o_norefresh -background=o_bg \
    -bg-child=o_nowait -dir:=o_dir -base-url:=o_base -arch:=o_arch 2>/dev/null ||
    { _clf_usage >&2; return 2 }
  (( $# == 0 )) || { _clf_usage >&2; return 2 }
  (( $#o_help )) && { _clf_usage; return 0 }
  local -i _clf_quiet=$#o_quiet
  local dir=${${o_dir[-1]:-${_clf_self:h:h}}:A}
  local base=${o_base[-1]:-${CINELOG_FETCH_BASE_URL:-https://github.com/cmprmsd/cinelog/releases/download}}
  base=${base%/}
  _clf_check_base $base || return 2

  if (( $#o_bg )); then
    _clf_background "$dir" "${o_base[-1]}" "${o_arch[-1]}"
    return
  fi

  if (( EUID == 0 )); then
    if ! _clf_tree_secure $dir 0 || { [[ -e $dir/bin ]] && ! _clf_tree_secure $dir/bin 0 } ||
       ! _clf_file_secure $dir/VERSION 0; then
      _clf_err "refusing to run as root: $dir (or a parent, bin/ or VERSION) is not root-owned or is group/world-writable"
      return 1
    fi
  fi

  _clf_read_version $dir || return 1
  local ver=$REPLY bin=$dir/bin/cinelog
  umask 022
  command mkdir -p -- $dir/bin || return 1

  # One fetch at a time per user; the background child gives up instead of waiting.
  zmodload zsh/system || return 1
  _clf_state_dir; local sd=$REPLY lockfd
  command mkdir -p -m 0700 -- $sd || return 1
  : >> $sd/fetch.lock || return 1
  if ! zsystem flock -t ${${o_nowait:+0}:-600} -f lockfd -- $sd/fetch.lock 2>/dev/null; then
    (( $#o_nowait )) && return 0
    _clf_err "another fetch holds $sd/fetch.lock"; return 1
  fi

  local _clf_tmp _clf_sum_src
  _clf_tmp=$(command mktemp -d "${TMPDIR:-/tmp}/cinelog-fetch.XXXXXX") || return 1
  local -i _clf_changed=0
  {
    _clf_run "$dir" "$ver" "$bin" "$base" "${o_arch[-1]}" $#o_force $#o_source $#o_nocargo &&
      _clf_refresh $bin $_clf_changed $#o_norefresh
  } always {
    command rm -rf -- $_clf_tmp $dir/bin/cinelog.new
    exec {lockfd}>&-
  }
}

# Puts a verified binary of <ver> at <bin>; sets _clf_changed=1 if it replaced the old one.
_clf_run() {
  local dir=$1 ver=$2 bin=$3 base=$4 arch=$5 force=$6 source=$7 nocargo=$8
  local target= expected= have=0 changed=0 rc
  _clf_version_ok $bin $ver && have=1
  _clf_target $arch && target=$REPLY

  if (( ! source )) && [[ -n $target ]]; then
    _clf_expected_sum $dir $ver $target $base; rc=$?
    (( rc == 2 )) && return 1
    if (( rc == 0 )); then
      expected=$REPLY
      if (( have && ! force )) && _clf_sha256 $bin && [[ $REPLY == $expected ]]; then
        _clf_info "cinelog $ver is up to date"
        return 0
      fi
      _clf_from_release $dir $ver $target $base $expected; rc=$?
      (( rc == 3 )) && return 1
      (( rc == 0 )) && changed=1
    elif (( have && ! force )); then
      _clf_warn "keeping the existing cinelog $ver binary (not re-verified)"
      return 0
    fi
  elif (( ! source )); then
    _clf_warn "no release binary for $(uname -s)/${arch:-$(uname -m)}"
  fi

  if (( ! changed )); then
    if (( have && ! force && ! source )); then
      _clf_warn "keeping the existing cinelog $ver binary"
      return 0
    fi
    if (( ! nocargo )) && _clf_from_source $dir $ver; then
      changed=1
    elif [[ -x $bin ]]; then
      _clf_err "no verified cinelog $ver; keeping the old binary at $bin"
      return 1
    else
      _clf_err "no cinelog binary installed; see README (cargo build --release --locked)"
      return 1
    fi
  fi

  _clf_info "installed cinelog $ver at $bin"
  _clf_changed=1
  return 0
}

# `install --refresh` after a change. A failed or skipped refresh leaves a marker in the state
# dir, so later runs retry it even when the binary is already up to date (plan §10.2 step 5).
_clf_refresh() {
  local bin=$1 changed=$2 norefresh=$3 marker
  # A bare `cinelog` under sudo's secure_path is usually the old team binary: name this one.
  if (( EUID == 0 )); then
    (( changed )) && _clf_info "team binary: run '$bin install --team --upgrade' to update /usr/local/bin/cinelog"
    return 0
  fi
  (( changed )) && [[ -x /usr/local/bin/cinelog ]] &&
    _clf_info "team binary: run 'sudo $bin install --team --upgrade' to update /usr/local/bin/cinelog"
  _clf_state_dir; marker=$REPLY/refresh.pending
  (( changed )) || [[ -e $marker ]] || return 0
  if (( norefresh )); then
    : >| $marker
    return 0
  fi
  if $bin install --refresh </dev/null; then
    command rm -f -- $marker
  else
    _clf_warn "'cinelog install --refresh' failed; the next fetch retries it"
    : >| $marker
  fi
  return 0
}

if [[ $ZSH_EVAL_CONTEXT == toplevel ]]; then
  _clf_main "$@"
  exit
fi
