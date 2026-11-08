//! Integration-test harness for cinelog (plan §12.2): throwaway HOME/ZDOTDIR, isolated
//! `tmux -L cinelog-it-<rand>` servers, real zsh. Scenarios live in `tests/`.
//! Root-only scenarios need `--features root` and `CINELOG_IT_ROOT=1`.
