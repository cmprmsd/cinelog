# >>> cinelog >>> managed by `cinelog install --team --system-zshrc`; remove with `--uninstall`
# Root shells (sudo -s, sudo -i, su, root logins) load the root-owned plugin copy before any
# user dotfile runs, so they are recorded in TEAM scope without a zim setup of their own and
# a user-writable plugin sourced later returns at once.
if (( EUID == 0 )) && [[ -r /usr/local/share/cinelog/cinelog.plugin.zsh && -O /usr/local/share/cinelog/cinelog.plugin.zsh ]]; then
  source /usr/local/share/cinelog/cinelog.plugin.zsh
fi
# <<< cinelog <<<
