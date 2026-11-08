# cinelog early start. Source this as the FIRST line of ~/.zshrc, before any instant
# prompt and before zim, so the outer shell execs into `cinelog rec` before doing any work:
#
#   [[ -r ${ZIM_HOME:-~/.zim}/modules/cinelog/shell/early.zsh ]] &&
#     source ${ZIM_HOME:-~/.zim}/modules/cinelog/shell/early.zsh
#
# It runs only the start block of cinelog.plugin.zsh (guards, nonce, exec). The zim module
# loads the rest later; without this file the module does the same, one .zshrc run later.
typeset -g _cl_early_only=1
source ${${(%):-%x}:A:h:h}/cinelog.plugin.zsh
