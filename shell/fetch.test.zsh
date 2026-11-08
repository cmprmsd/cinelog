#!/usr/bin/env zsh
# Tests for shell/fetch.zsh against a fake release directory (file:// base URL), fake
# binaries and a fake cargo. Usage: zsh shell/fetch.test.zsh [-v] [name-pattern]
emulate -R zsh -o extended_glob -o pipe_fail

typeset -g FETCH=${${(%):-%x}:A:h}/fetch.zsh ROOT_TMP= VERBOSE=0 FILTER='*'
[[ $1 == -v ]] && { VERBOSE=1; shift }
[[ -n $1 ]] && FILTER="*$1*"
typeset -gi PASS=0 FAIL=0
typeset -g T VER=9.9.9 TARGET=x86_64-unknown-linux-musl

ROOT_TMP=$(mktemp -d "${TMPDIR:-/tmp}/cinelog-fetch-test.XXXXXX") || exit 1
trap 'rm -rf -- $ROOT_TMP' EXIT

fail() { print -ru2 -- "    FAIL: $*"; return 1 }
assert() { "$@" || fail "assertion: $*" }
assert_eq() { [[ $1 == $2 ]] || fail "expected '$2', got '$1'${3:+ ($3)}" }
contains() { [[ $(<$1) == *$2* ]] || fail "$1 does not contain '$2'" }
not_contains() { [[ ! -e $1 || $(<$1) != *$2* ]] || fail "$1 contains '$2'" }

# A fake cinelog: prints its version and logs `install` calls (which fail with CL_REFRESH_FAIL).
fake_bin() {  # <path> <version> [tag]
  print -r -- "#!/bin/sh
# ${3:-release}
case \"\$1\" in
  --version) echo \"cinelog $2\" ;;
  install) [ -n \"\$CL_REFRESH_FAIL\" ] && exit 1; echo \"\$*\" >> \"\$CL_TEST_LOG\" ;;
esac" > $1
  chmod 755 $1
}

sha() { sha256sum $1 | read -r REPLY _; print -r -- $REPLY }

setup() {
  T=$ROOT_TMP/$1
  mkdir -p $T/{mod/shell/checksums,rel/v$VER,home,state,tmp,path}
  cp $FETCH $T/mod/shell/fetch.zsh
  print -r -- $VER > $T/mod/VERSION
  fake_bin $T/rel/v$VER/cinelog-$TARGET $VER
  fake_bin $T/rel/v$VER/cinelog-aarch64-unknown-linux-musl $VER arm
  ( cd $T/rel/v$VER && sha256sum cinelog-* > SHA256SUMS )
  cp $T/rel/v$VER/SHA256SUMS $T/mod/shell/checksums/v$VER.sha256
  : > $T/log
}

# Fake cargo: builds bin `cinelog` reporting VERSION; with CL_CARGO_NEED_OFFLINE it only
# succeeds when --offline is passed; with CL_CARGO_NEED_TOOLCHAIN only under that
# RUSTUP_TOOLCHAIN; with CL_CARGO_FAIL it always fails. Fake rustup: `rustup default` prints
# CL_RUSTUP_DEFAULT, or fails like rustup without a default toolchain.
fake_cargo() {
  print -r -- '#!/bin/sh
echo "cargo${RUSTUP_TOOLCHAIN:+ [tc=$RUSTUP_TOOLCHAIN]} $*" >> "$CL_TEST_LOG"
[ -n "$CL_CARGO_FAIL" ] && exit 101
[ -n "$CL_CARGO_NEED_TOOLCHAIN" ] && [ "$RUSTUP_TOOLCHAIN" != "$CL_CARGO_NEED_TOOLCHAIN" ] && exit 1
case " $* " in *" --offline "*) ;; *) [ -n "$CL_CARGO_NEED_OFFLINE" ] && exit 101 ;; esac
mkdir -p "$CARGO_TARGET_DIR/release"
v=$(cat VERSION)
printf "#!/bin/sh\n# built\n[ \"\$1\" = --version ] && echo \"cinelog %s\"\n[ \"\$1\" = install ] && echo \"\$*\" >> \"\$CL_TEST_LOG\"\nexit 0\n" "$v" > "$CARGO_TARGET_DIR/release/cinelog"
chmod 755 "$CARGO_TARGET_DIR/release/cinelog"' > $T/path/cargo
  chmod 755 $T/path/cargo
  print -r -- '#!/bin/sh
[ "$1" = default ] || exit 2
[ -n "$CL_RUSTUP_DEFAULT" ] || { echo "error: no default toolchain is configured" >&2; exit 1; }
echo "$CL_RUSTUP_DEFAULT (default)"' > $T/path/rustup
  chmod 755 $T/path/rustup
  : > $T/mod/Cargo.toml; : > $T/mod/Cargo.lock
}

run_fetch() {
  env -i PATH=$T/path:/usr/bin:/bin HOME=$T/home XDG_STATE_HOME=$T/state TMPDIR=$T/tmp \
    CL_TEST_LOG=$T/log ${CL_ENV[@]} \
    zsh -f $T/mod/shell/fetch.zsh --base-url file://$T/rel --arch x86_64 "$@" 2>$T/err
}

leftovers() { [[ ! -e $T/mod/bin/cinelog.new ]] || fail "bin/cinelog.new left behind"
  [[ -z $(print -l $T/tmp/*(N)) ]] || fail "temp files left in TMPDIR" }

# --- pure functions (sourced) -------------------------------------------------------------

t_target_mapping() {
  source $FETCH
  _clf_target x86_64 Linux && assert_eq $REPLY x86_64-unknown-linux-musl || return 1
  _clf_target amd64 Linux && assert_eq $REPLY x86_64-unknown-linux-musl || return 1
  _clf_target aarch64 Linux && assert_eq $REPLY aarch64-unknown-linux-musl || return 1
  _clf_target arm64 Linux && assert_eq $REPLY aarch64-unknown-linux-musl || return 1
  ! _clf_target riscv64 Linux || fail "riscv64 mapped"
  ! _clf_target x86_64 Darwin || fail "Darwin mapped"
}

t_sum_parsing() {
  source $FETCH
  local a=${(l:64::a:)} b=${(l:64::B:)}
  _clf_sum_for cinelog-x <<< "$a  cinelog-x" && assert_eq $REPLY $a || return 1
  _clf_sum_for cinelog-x <<< "$b *cinelog-x" && assert_eq $REPLY ${(L)b} || return 1
  _clf_sum_for cinelog-y <<< $'junk\n'"$a  cinelog-x"$'\n'"$b  cinelog-y" && assert_eq $REPLY ${(L)b} || return 1
  ! _clf_sum_for cinelog-x <<< "$a  cinelog-x.sha256" || fail "suffix matched"
  ! _clf_sum_for cinelog-x <<< "${a[1,63]}  cinelog-x" || fail "short hash accepted"
  ! _clf_sum_for cinelog-x <<< "${a[1,63]}g  cinelog-x" || fail "non-hex accepted"
  ! _clf_sum_for cinelog-x 2>/dev/null <<< "$a  cinelog-x"$'\n'"$b  cinelog-x" || fail "conflict accepted"
  _clf_sum_for cinelog-x <<< "$a  cinelog-x"$'\n'"$a  cinelog-x" && assert_eq $REPLY $a
}

t_version_file() {
  source $FETCH
  setup version_file
  print -r -- "  1.2.3-rc.1 " > $T/mod/VERSION
  _clf_read_version $T/mod && assert_eq $REPLY 1.2.3-rc.1 || return 1
  print -r -- "1.2.3/../../x" > $T/mod/VERSION
  ! _clf_read_version $T/mod 2>/dev/null || fail "path-like version accepted"
  : > $T/mod/VERSION
  ! _clf_read_version $T/mod 2>/dev/null || fail "empty version accepted"
}

t_tree_secure() {
  source $FETCH
  setup tree_secure
  mkdir -p $T/sec/a/b && chmod 755 $T/sec $T/sec/a $T/sec/a/b
  # Our own uid passes only if every parent is ours: true for none of / … so check a leaf rule.
  ! _clf_tree_secure $T/sec/a/b 0 || fail "user-owned tree accepted for root"
  _clf_file_secure $T/mod/VERSION $UID || fail "own 0644 file rejected"
  chmod g+w $T/mod/VERSION
  ! _clf_file_secure $T/mod/VERSION $UID || fail "group-writable file accepted"
  _clf_tree_secure / 0 || fail "/ is not root-owned?"
  ! _clf_tree_secure /tmp 0 || fail "world-writable /tmp accepted"
}

t_base_url_policy() {
  source $FETCH
  _clf_check_base https://example.org/x || fail https
  _clf_check_base file:///srv/rel || fail file
  _clf_check_base /srv/rel || fail abs
  ! _clf_check_base http://example.org 2>/dev/null || fail "http accepted"
  ! _clf_check_base rel/dir 2>/dev/null || fail "relative accepted"
}

# --- end to end (executed like zim's --on-pull) -------------------------------------------

t_fresh_install() {
  setup fresh_install
  run_fetch || fail "exit $? $(<$T/err)" || return 1
  assert_eq $(sha $T/mod/bin/cinelog) $(sha $T/rel/v$VER/cinelog-$TARGET) || return 1
  [[ -x $T/mod/bin/cinelog ]] || fail "not: -x $T/mod/bin/cinelog" || return 1
  assert_eq "$(<$T/log)" "install --refresh" || return 1
  not_contains $T/err warning || return 1
  leftovers
}

t_up_to_date_is_noop() {
  setup up_to_date
  run_fetch || return 1
  : > $T/log
  local before=$(stat -c %i $T/mod/bin/cinelog)
  run_fetch || fail "second run failed: $(<$T/err)" || return 1
  assert_eq $(stat -c %i $T/mod/bin/cinelog) $before "binary replaced" || return 1
  assert_eq "$(<$T/log)" "" "refresh ran without a change" || return 1
  contains $T/err "up to date"
}

t_quiet_up_to_date_prints_nothing() {
  setup quiet
  run_fetch -q || return 1
  run_fetch -q || return 1
  assert_eq "$(<$T/err)" ""
}

t_force_redownloads() {
  setup force
  run_fetch || return 1
  local before=$(stat -c %i $T/mod/bin/cinelog)
  run_fetch --force || return 1
  [[ $(stat -c %i $T/mod/bin/cinelog) != $before ]] || fail "not replaced"
}

t_upgrade_swaps() {
  setup upgrade
  mkdir -p $T/mod/bin && fake_bin $T/mod/bin/cinelog 9.9.8 old
  run_fetch || fail "$(<$T/err)" || return 1
  assert_eq $(sha $T/mod/bin/cinelog) $(sha $T/rel/v$VER/cinelog-$TARGET)
}

t_checksum_mismatch_keeps_old() {
  setup mismatch
  mkdir -p $T/mod/bin && fake_bin $T/mod/bin/cinelog 9.9.8 old
  local old=$(sha $T/mod/bin/cinelog)
  print -r -- "#!/bin/sh
echo evil" > $T/rel/v$VER/cinelog-$TARGET          # tampered asset, repo checksum unchanged
  fake_cargo
  run_fetch && fail "succeeded on mismatch"
  assert_eq $(sha $T/mod/bin/cinelog) $old "old binary changed" || return 1
  contains $T/err "checksum mismatch" || return 1
  not_contains $T/log cargo || return 1
  leftovers
}

t_repo_checksum_is_authoritative() {
  setup authoritative
  # The release SHA256SUMS matches a tampered asset; the repo checksum does not.
  print -r -- "#!/bin/sh
echo cinelog $VER" > $T/rel/v$VER/cinelog-$TARGET
  ( cd $T/rel/v$VER && sha256sum cinelog-* > SHA256SUMS )
  run_fetch --no-cargo && fail "accepted asset that only matches release SHA256SUMS"
  [[ ! -e $T/mod/bin/cinelog ]] || fail "not: ! -e $T/mod/bin/cinelog" || return 1
  contains $T/err "checksum mismatch"
}

t_release_sums_fallback() {
  setup release_sums
  rm $T/mod/shell/checksums/v$VER.sha256
  run_fetch || fail "$(<$T/err)" || return 1
  assert_eq $(sha $T/mod/bin/cinelog) $(sha $T/rel/v$VER/cinelog-$TARGET) || return 1
  contains $T/err "not in the repo yet"
}

t_repo_file_without_entry_fails() {
  setup no_entry
  grep -v $TARGET $T/rel/v$VER/SHA256SUMS > $T/mod/shell/checksums/v$VER.sha256
  fake_cargo
  run_fetch && fail "succeeded without a repo entry"
  contains $T/err "no valid entry" || return 1
  not_contains $T/log cargo
}

t_no_checksum_anywhere_uses_cargo() {
  setup no_sums
  rm $T/mod/shell/checksums/v$VER.sha256 $T/rel/v$VER/SHA256SUMS
  fake_cargo
  run_fetch || fail "$(<$T/err)" || return 1
  contains $T/log "cargo build --release --locked --bin cinelog" || return 1
  contains $T/mod/bin/cinelog "# built" || return 1
  contains $T/log "install --refresh" || return 1
  leftovers
}

t_offline_keeps_current_binary() {
  setup offline_keep
  run_fetch || return 1
  rm -r $T/rel $T/mod/shell/checksums/v$VER.sha256
  fake_cargo
  : > $T/log
  run_fetch || fail "offline run failed: $(<$T/err)" || return 1
  not_contains $T/log cargo || return 1
  contains $T/err "keeping the existing"
}

t_missing_release_builds_from_source() {
  setup missing_release
  rm -r $T/rel/v$VER
  rm $T/mod/shell/checksums/v$VER.sha256
  fake_cargo
  run_fetch || fail "$(<$T/err)" || return 1
  contains $T/mod/bin/cinelog "# built"
}

t_cargo_offline_retry() {
  setup cargo_offline
  rm -r $T/rel/v$VER $T/mod/shell/checksums/v$VER.sha256
  fake_cargo
  CL_ENV=(CL_CARGO_NEED_OFFLINE=1) run_fetch || fail "$(<$T/err)" || return 1
  contains $T/log "--offline" || return 1
  contains $T/mod/bin/cinelog "# built"
}

t_cargo_toolchain_file_falls_back_to_default_toolchain() {
  setup toolchain_default
  rm -r $T/rel/v$VER $T/mod/shell/checksums/v$VER.sha256
  fake_cargo
  print -r -- $'[toolchain]\nchannel = "stable"\ncomponents = ["clippy"]' > $T/mod/rust-toolchain.toml
  CL_ENV=(CL_CARGO_NEED_TOOLCHAIN=nightly-test CL_RUSTUP_DEFAULT=nightly-test) run_fetch ||
    fail "$(<$T/err)" || return 1
  contains $T/log "cargo build --release --locked --bin cinelog --offline" || return 1
  contains $T/log "cargo [tc=nightly-test] build --release --locked --bin cinelog" || return 1
  contains $T/err "default toolchain nightly-test" || return 1
  contains $T/mod/bin/cinelog "# built" || return 1
  leftovers
}

t_cargo_default_toolchain_retry_offline() {
  setup toolchain_offline
  rm -r $T/rel/v$VER $T/mod/shell/checksums/v$VER.sha256
  fake_cargo
  print -r -- $'[toolchain]\nchannel = "stable"' > $T/mod/rust-toolchain.toml
  CL_ENV=(CL_CARGO_NEED_TOOLCHAIN=nightly-test CL_CARGO_NEED_OFFLINE=1 CL_RUSTUP_DEFAULT=nightly-test) \
    run_fetch || fail "$(<$T/err)" || return 1
  contains $T/log "cargo [tc=nightly-test] build --release --locked --bin cinelog --offline" || return 1
  contains $T/mod/bin/cinelog "# built"
}

t_cargo_user_toolchain_override_is_kept() {
  setup toolchain_user
  rm -r $T/rel/v$VER $T/mod/shell/checksums/v$VER.sha256
  fake_cargo
  print -r -- $'[toolchain]\nchannel = "stable"' > $T/mod/rust-toolchain.toml
  CL_ENV=(RUSTUP_TOOLCHAIN=mine CL_CARGO_NEED_TOOLCHAIN=nightly-test CL_RUSTUP_DEFAULT=nightly-test) \
    run_fetch && fail "succeeded although the user's toolchain fails"
  not_contains $T/log "tc=nightly-test" || return 1
  assert_eq "$(grep -c 'tc=mine' $T/log)" 2 "expected the normal and the offline attempt"
}

t_cargo_no_default_toolchain_fails() {
  setup toolchain_none
  rm -r $T/rel/v$VER $T/mod/shell/checksums/v$VER.sha256
  fake_cargo
  print -r -- $'[toolchain]\nchannel = "stable"' > $T/mod/rust-toolchain.toml
  CL_ENV=(CL_CARGO_NEED_TOOLCHAIN=nightly-test) run_fetch && fail "succeeded without a toolchain"
  assert_eq "$(grep -c cargo $T/log)" 2 "expected exactly two cargo runs" || return 1
  contains $T/err "cargo build failed"
}

t_cargo_failure_keeps_old() {
  setup cargo_fail
  rm -r $T/rel/v$VER $T/mod/shell/checksums/v$VER.sha256
  mkdir -p $T/mod/bin && fake_bin $T/mod/bin/cinelog 9.9.8 old
  fake_cargo
  CL_ENV=(CL_CARGO_FAIL=1) run_fetch && fail "succeeded although cargo failed"
  contains $T/mod/bin/cinelog "# old" || return 1
  contains $T/err "keeping the old binary" || return 1
  leftovers
}

t_no_binary_no_cargo_fails() {
  setup nothing
  rm -r $T/rel/v$VER $T/mod/shell/checksums/v$VER.sha256
  run_fetch && fail "succeeded with nothing"
  [[ ! -e $T/mod/bin/cinelog ]] || fail "not: ! -e $T/mod/bin/cinelog"
}

t_unsupported_arch_builds_from_source() {
  setup arch
  fake_cargo
  run_fetch --arch riscv64 || fail "$(<$T/err)" || return 1   # last --arch wins
  contains $T/mod/bin/cinelog "# built" || return 1
  contains $T/err "no release binary"
}

t_source_flag_skips_download() {
  setup source
  fake_cargo
  run_fetch --source || fail "$(<$T/err)" || return 1
  contains $T/mod/bin/cinelog "# built"
}

t_wrong_version_rejected() {
  setup wrong_version
  fake_bin $T/rel/v$VER/cinelog-$TARGET 1.0.0
  ( cd $T/rel/v$VER && sha256sum cinelog-* > SHA256SUMS )
  cp $T/rel/v$VER/SHA256SUMS $T/mod/shell/checksums/v$VER.sha256
  run_fetch --no-cargo && fail "accepted a binary reporting another version"
  [[ ! -e $T/mod/bin/cinelog ]] || fail "not: ! -e $T/mod/bin/cinelog" || return 1
  contains $T/err "does not report"
}

t_non_executable_payload_rejected() {
  setup garbage
  print -rn -- "not an elf" > $T/rel/v$VER/cinelog-$TARGET
  ( cd $T/rel/v$VER && sha256sum cinelog-* > SHA256SUMS )
  cp $T/rel/v$VER/SHA256SUMS $T/mod/shell/checksums/v$VER.sha256
  run_fetch --no-cargo && fail "accepted garbage"
  [[ ! -e $T/mod/bin/cinelog ]] || fail "not: ! -e $T/mod/bin/cinelog" || return 1
  leftovers
}

t_aarch64_asset() {
  setup aarch64
  run_fetch --arch aarch64 || fail "$(<$T/err)" || return 1
  contains $T/mod/bin/cinelog "# arm"
}

t_no_refresh_flag_defers_refresh() {
  setup no_refresh
  run_fetch --no-refresh || return 1
  assert_eq "$(<$T/log)" "" || return 1
  run_fetch || return 1
  assert_eq "$(<$T/log)" "install --refresh" "deferred refresh did not run" || return 1
  [[ ! -e $T/state/cinelog/refresh.pending ]] || fail "marker not removed"
}

# Regression: a failed refresh used to be lost, because later runs saw "up to date" and
# returned before refreshing.
t_failed_refresh_is_retried_when_up_to_date() {
  setup refresh_retry
  CL_ENV=(CL_REFRESH_FAIL=1) run_fetch || fail "exit $?: $(<$T/err)" || return 1
  contains $T/err "the next fetch retries it" || return 1
  [[ -e $T/state/cinelog/refresh.pending ]] || fail "no refresh.pending marker" || return 1
  local before=$(stat -c %i $T/mod/bin/cinelog)
  CL_ENV=(CL_REFRESH_FAIL=1) run_fetch || return 1
  [[ -e $T/state/cinelog/refresh.pending ]] || fail "marker removed by a failed retry" || return 1
  assert_eq "$(<$T/log)" "" || return 1
  run_fetch || fail "$(<$T/err)" || return 1
  contains $T/err "up to date" || return 1
  assert_eq "$(<$T/log)" "install --refresh" "refresh not retried" || return 1
  assert_eq $(stat -c %i $T/mod/bin/cinelog) $before "binary replaced" || return 1
  [[ ! -e $T/state/cinelog/refresh.pending ]] || fail "marker not removed" || return 1
  run_fetch || return 1
  assert_eq "$(<$T/log)" "install --refresh" "refresh ran again after it had succeeded"
}

t_refresh_retried_when_keeping_unverified_binary() {
  setup refresh_keep
  CL_ENV=(CL_REFRESH_FAIL=1) run_fetch || return 1
  rm -r $T/rel $T/mod/shell/checksums/v$VER.sha256
  run_fetch || fail "$(<$T/err)" || return 1
  contains $T/err "keeping the existing" || return 1
  assert_eq "$(<$T/log)" "install --refresh"
}

t_refresh_failure_is_a_warning() {
  setup refresh_fails
  print -r -- "#!/bin/sh
[ \"\$1\" = --version ] && echo cinelog $VER && exit 0
exit 2" > $T/rel/v$VER/cinelog-$TARGET
  ( cd $T/rel/v$VER && sha256sum cinelog-* > SHA256SUMS )
  cp $T/rel/v$VER/SHA256SUMS $T/mod/shell/checksums/v$VER.sha256
  run_fetch || fail "refresh failure made fetch fail"
  contains $T/err "install --refresh' failed"
}

t_usage_errors() {
  setup usage
  run_fetch --bogus; assert_eq $? 2 || return 1
  run_fetch extra-arg; assert_eq $? 2 || return 1
  run_fetch --base-url http://example.org; assert_eq $? 2
}

t_lock_held_background_child_gives_up() {
  setup lock
  mkdir -p $T/state/cinelog && : > $T/state/cinelog/fetch.lock
  zmodload zsh/system
  local fd
  zsystem flock -f fd $T/state/cinelog/fetch.lock || return 1
  run_fetch --bg-child; assert_eq $? 0 || return 1
  exec {fd}>&-
  [[ ! -e $T/mod/bin/cinelog ]] || fail "not: ! -e $T/mod/bin/cinelog"
}

t_background_once_a_day() {
  setup background
  run_fetch --background || fail "$(<$T/err)" || return 1
  local i
  for i in {1..100}; do [[ -x $T/mod/bin/cinelog ]] && break; sleep 0.1; done
  [[ -x $T/mod/bin/cinelog ]] || fail "not: -x $T/mod/bin/cinelog" || return 1
  [[ -e $T/state/cinelog/fetch.stamp ]] || fail "not: -e $T/state/cinelog/fetch.stamp" || return 1
  for i in {1..50}; do [[ -s $T/log ]] && break; sleep 0.1; done   # child's refresh
  rm $T/mod/bin/cinelog
  run_fetch --background || return 1
  sleep 0.5
  [[ ! -e $T/mod/bin/cinelog ]] || fail "second background run within a day fetched again"
}

t_background_never_builds() {
  setup background_nocargo
  rm -r $T/rel/v$VER $T/mod/shell/checksums/v$VER.sha256
  fake_cargo
  run_fetch --background || return 1
  sleep 1
  not_contains $T/log cargo
}

t_concurrent_runs_serialize() {
  setup concurrent
  local -a pids
  local i
  for i in {1..4}; do run_fetch -q & pids+=($!); done
  for i in $pids; do wait $i || fail "a concurrent run failed"; done
  assert_eq $(sha $T/mod/bin/cinelog) $(sha $T/rel/v$VER/cinelog-$TARGET) || return 1
  assert_eq "$(grep -c refresh $T/log)" 1 "refresh ran more than once"
}

# ------------------------------------------------------------------------------------------

typeset -ga CL_ENV=()
for t in ${(ok)functions[(I)t_*]}; do
  [[ ${t#t_} == $~FILTER ]] || continue
  if ( $t ) 2>$ROOT_TMP/.stderr; then
    (( PASS++ )); (( VERBOSE )) && print -r -- "ok   ${t#t_}"
  else
    (( FAIL++ )); print -r -- "FAIL ${t#t_}"; sed 's/^/     /' $ROOT_TMP/.stderr
  fi
done
print -r -- "fetch.zsh: $PASS passed, $FAIL failed"
(( FAIL == 0 ))
