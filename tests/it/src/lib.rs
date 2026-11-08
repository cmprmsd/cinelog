//! Integration-test harness for cinelog (plan §12.2).
//!
//! Every scenario gets a [`Sandbox`]: a throwaway HOME/ZDOTDIR/XDG tree with a minimal
//! `.zshrc` that sources a copy of the repo plugin, a cinelog config pointing all logs into
//! the sandbox, and an isolated `tmux -L cinelog-it-<id> -f /dev/null` server whose panes
//! start `env -i … zsh -i`. Tests drive panes with `send-keys`, poll `capture-pane`, and read
//! casts and registry files directly. Dropping the sandbox kills the tmux servers and every
//! process that carries the sandbox's environment marker, then removes the tree.
//!
//! The index scenarios (I11–I14) run `cinelog` subcommands and `serve --no-http` against
//! the sandbox ([`cli`]), build cast corpora without recording ([`synth`], and the legacy
//! generator in `tests/fixtures/legacy-gen`) and inspect `index.db` read-only ([`index`]).
//!
//! Scenarios live in `tests/`; see `README.md` for the gating environment variables.

pub mod binary;
pub mod cast;
pub mod cli;
pub mod gate;
pub mod index;
pub mod procns;
pub mod procs;
pub mod sandbox;
pub mod secret;
pub mod spoof;
pub mod sshd;
pub mod sudo;
pub mod synth;
pub mod tmux;
pub mod upgrade;
pub mod util;

pub use binary::Flavour;
pub use cast::{Cast, CmdNode, Event, Marker};
pub use cli::{CliOutput, Serve};
pub use gate::Need;
pub use sandbox::{Recorded, Sandbox, SandboxOptions, cast_timeout};
pub use tmux::{Pane, Tmux};
pub use util::{scaled, unique_id, wait_until};
