#!/usr/bin/env bash
# Headless-nvim format-then-diagnostics probe.
#
# Spawns a real nvim, attaches tfls (with or without lspmux
# wrapping), opens the fixture, lets the server publish initial
# diagnostics, runs `vim.lsp.buf.format()`, waits for re-publish,
# asserts every diagnostic still points at the right line.
#
# Usage:
#   scripts/nvim-format-probe/run.sh --mode direct --tfls-path ./target/debug/tfls
#   scripts/nvim-format-probe/run.sh --mode lspmux --tfls-path ./target/debug/tfls --lspmux-path /nix/store/...
#
# Pass --nvim-path to override the nvim binary; defaults to the one
# from the neovim-nix-flake's `nix build .#nvim` output if available,
# else `nvim` on PATH.

set -euo pipefail

mode="direct"
tfls_path=""
lspmux_path=""
nvim_path=""
fixture=""
keep_workdir=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode) mode="$2"; shift 2 ;;
    --tfls-path) tfls_path="$2"; shift 2 ;;
    --lspmux-path) lspmux_path="$2"; shift 2 ;;
    --nvim-path) nvim_path="$2"; shift 2 ;;
    --fixture) fixture="$2"; shift 2 ;;
    --keep-workdir) keep_workdir=1; shift ;;
    -h|--help)
      sed -n '2,/^$/p' "$0" | sed 's/^# \?//' >&2
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

if [[ -z "$tfls_path" ]]; then
  echo "--tfls-path is required" >&2
  exit 2
fi
if [[ "$mode" == "lspmux" && -z "$lspmux_path" ]]; then
  echo "--mode lspmux requires --lspmux-path" >&2
  exit 2
fi

# Resolve nvim binary. Prefer flake build output (consistent with
# user's actual setup); fall back to PATH; fall back to a clear
# error.
if [[ -z "$nvim_path" ]]; then
  if command -v nvim >/dev/null 2>&1; then
    nvim_path="$(command -v nvim)"
  else
    echo "no nvim on PATH and --nvim-path not provided" >&2
    exit 2
  fi
fi

tfls_abs="$(cd "$(dirname "$tfls_path")" && pwd)/$(basename "$tfls_path")"
if [[ ! -x "$tfls_abs" ]]; then
  echo "tfls binary not executable: $tfls_abs" >&2
  exit 2
fi

probe_dir="$(cd "$(dirname "$0")" && pwd)"
if [[ -z "$fixture" ]]; then
  fixture="$probe_dir/test.tf"
fi
if [[ ! -f "$fixture" ]]; then
  echo "fixture not found: $fixture" >&2
  exit 2
fi

workdir="$(mktemp -d -t "tfls-nvim-probe-$mode-XXXXXX")"
# shellcheck disable=SC2329  # invoked indirectly via `trap EXIT`.
cleanup() {
  if [[ "$keep_workdir" -eq 0 ]]; then
    rm -rf "$workdir"
  else
    echo "[$mode] kept workdir: $workdir" >&2
  fi
}
trap cleanup EXIT

cp "$fixture" "$workdir/test.tf"

# Choose tfls command shape.
case "$mode" in
  direct)
    tfls_cmd="$tfls_abs"
    ;;
  lspmux)
    lspmux_abs="$(cd "$(dirname "$lspmux_path")" && pwd)/$(basename "$lspmux_path")"
    if [[ ! -x "$lspmux_abs" ]]; then
      echo "lspmux binary not executable: $lspmux_abs" >&2
      exit 2
    fi
    # `lspmux client --server-path tfls` wraps the server with
    # multiplexer indirection — same shape the user-facing
    # neovim-nix-flake plugin generates per-buffer.
    tfls_cmd="$lspmux_abs client --server-path $tfls_abs"
    ;;
  *)
    echo "unknown --mode: $mode" >&2
    exit 2
    ;;
esac

# lspmux mode needs an isolated daemon; spawn one on a free port and
# point lspmux's config at it via XDG_CONFIG_HOME so the test doesn't
# clash with a daemon the user already has running.
daemon_pid=""
xdg_home=""
if [[ "$mode" == "lspmux" ]]; then
  port=0
  # Steal a free port via python (`nvim --headless` envs are typically
  # nix-shells where python is available; fall back gracefully).
  if command -v python3 >/dev/null 2>&1; then
    port=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
  fi
  if [[ "$port" -eq 0 ]]; then
    echo "could not pick free port (need python3)" >&2
    exit 2
  fi
  xdg_home="$workdir/xdg-config"
  mkdir -p "$xdg_home/lspmux" "$workdir/lspmux-app-support/lspmux"
  cfg=$(printf 'instance_timeout = 300\ngc_interval = 10\nlisten = ["127.0.0.1", %d]\nconnect = ["127.0.0.1", %d]\nlog_filters = "info"\npass_environment = ["RUST_LOG", "TFLS_LOG_FILE"]\n' "$port" "$port")
  printf '%s' "$cfg" >"$xdg_home/lspmux/config.toml"
  printf '%s' "$cfg" >"$workdir/lspmux-app-support/lspmux/config.toml"
  HOME_OVERRIDE="$workdir"
  XDG_CONFIG_HOME_OVERRIDE="$xdg_home"
  HOME="$HOME_OVERRIDE" XDG_CONFIG_HOME="$XDG_CONFIG_HOME_OVERRIDE" \
    "$lspmux_abs" server >"$workdir/lspmux.stderr.log" 2>&1 &
  daemon_pid=$!
  # Wait up to 5s for the port.
  for _ in $(seq 1 50); do
    if (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
      break
    fi
    sleep 0.1
  done
  trap 'kill -9 '"$daemon_pid"' 2>/dev/null || true; cleanup' EXIT
fi

echo "[$mode] nvim=$nvim_path" >&2
echo "[$mode] tfls cmd=$tfls_cmd" >&2
echo "[$mode] workdir=$workdir" >&2

# Run the headless nvim. `-i NONE -n` skips shada / swapfile so the
# probe is hermetic. Env values are forwarded explicitly because
# `--clean` clears most of nvim's environment-derived state.
status=0
env \
  TFLS_CMD="$tfls_cmd" \
  TFLS_FIXTURE="$workdir/test.tf" \
  TFLS_FORMAT_STYLE="opinionated" \
  HOME="${HOME_OVERRIDE:-$HOME}" \
  XDG_CONFIG_HOME="${XDG_CONFIG_HOME_OVERRIDE:-${XDG_CONFIG_HOME:-$HOME/.config}}" \
  RUST_LOG="${RUST_LOG:-tfls_lsp=info}" \
  "$nvim_path" --clean --headless -n -i NONE \
    -u "$probe_dir/init.lua" \
    +qa || status=$?

if [[ "$status" -eq 0 ]]; then
  echo "[$mode] PASS" >&2
else
  echo "[$mode] FAIL (exit $status)" >&2
fi
exit "$status"
