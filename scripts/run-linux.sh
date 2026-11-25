#!/usr/bin/env bash
# Launch or precompile Xbox Series titles with r4v3n on Linux via umu-run + Proton.
#
# Usage:
#   ./scripts/run-linux.sh precompile /path/to/game_dir [--pipelines]
#   ./scripts/run-linux.sh run /path/to/game_dir/game.exe [extra victory run args...]
#
# Environment overrides:
#   PROTONPATH            Path to Proton (Experimental, GE-Proton, or Proton-CachyOS).
#                         Defaults to Proton Experimental (ships newest vkd3d-proton 3.2.0).
#   WINEPREFIX            Wine prefix path (default: ~/.local/share/r4v3n/prefix).
#   DIRECTMESH_DIR        User-space DirectMesh RADV dir for GFX1013 mesh shaders
#                         (default: ~/.local/share/directmesh). Set DIRECTMESH_DIR=0 to disable.
#   CONSOLE               series-s (default, recommended for 16 GB RAM/VRAM) or series-x.
#   VICTORY_RENDER_SCALE  Internal render scale 0.2..=1.0 (e.g. 0.5 or 0.67).
#   VICTORY_EXE           Path to victory.exe (default: diag build in target/).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE="${1:-}"
TARGET_PATH="${2:-}"

if [[ -z "$MODE" || -z "$TARGET_PATH" ]] || [[ "$MODE" != "run" && "$MODE" != "precompile" ]]; then
  echo "Usage:" >&2
  echo "  $0 precompile <game_dir_or_file> [--pipelines] [--only vs|ps|ms|cs] [--jobs N]" >&2
  echo "  $0 run <title.exe> [--console series-s|series-x] [extra args...]" >&2
  exit 2
fi
shift 2

VICTORY_EXE="${VICTORY_EXE:-$REPO_ROOT/target/x86_64-pc-windows-msvc/diag/victory.exe}"
if [[ ! -f "$VICTORY_EXE" ]]; then
  echo "Error: victory.exe not found at $VICTORY_EXE" >&2
  echo "Build it first with:" >&2
  echo "  cargo xwin build --profile diag -p victory --features diagnostics --target x86_64-pc-windows-msvc" >&2
  exit 1
fi

# Proton selection: prefer Proton-GE Latest (GE-Proton11-14 ships newer
# bleeding-edge Wine + vkd3d-proton + DualSense BT), else Experimental/CachyOS.
if [[ -z "${PROTONPATH:-}" ]]; then
  for candidate in \
    "$HOME/.steam/root/compatibilitytools.d/Proton-GE Latest" \
    "$HOME/.steam/root/steamapps/common/Proton - Experimental" \
    "$HOME/.steam/root/compatibilitytools.d/Proton-CachyOS Latest"; do
    if [[ -d "$candidate" ]]; then
      PROTONPATH="$candidate"
      break
    fi
  done
fi
if [[ -z "${PROTONPATH:-}" || ! -d "$PROTONPATH" ]]; then
  echo "Error: no Proton installation found; set PROTONPATH=/path/to/Proton" >&2
  exit 1
fi

export WINEPREFIX="${WINEPREFIX:-$HOME/.local/share/r4v3n/prefix}"
export PROTONPATH
export GAMEID="${GAMEID:-umu-default}"
export UMU_RUNTIME_UPDATE="${UMU_RUNTIME_UPDATE:-0}"
export MESA_SHADER_CACHE_MAX_SIZE="${MESA_SHADER_CACHE_MAX_SIZE:-8G}"

# Enable DirectMesh RADV (VK_EXT_mesh_shader on GFX1013 / PS5 Oberon) if installed.
DIRECTMESH_DIR="${DIRECTMESH_DIR:-$HOME/.local/share/directmesh}"
if [[ "$DIRECTMESH_DIR" != "0" && -f "$DIRECTMESH_DIR/icd.d/radeon_icd.x86_64.json" ]]; then
  export RADV_DIRECTMESH="${RADV_DIRECTMESH:-1}"
  export VK_DRIVER_FILES="$DIRECTMESH_DIR/icd.d/radeon_icd.x86_64.json"
  RO_PATHS="$DIRECTMESH_DIR"
  [[ -d /home/linuxbrew/.linuxbrew ]] && RO_PATHS="$RO_PATHS:/home/linuxbrew/.linuxbrew"
  export PRESSURE_VESSEL_FILESYSTEMS_RO="${PRESSURE_VESSEL_FILESYSTEMS_RO:+$PRESSURE_VESSEL_FILESYSTEMS_RO:}$RO_PATHS"
fi

# 16 GB unified RAM/VRAM & Linux/Proton stability defaults (overridable via env).
export VICTORY_LOW_MEM="${VICTORY_LOW_MEM:-1}"
export VICTORY_NO_WATCHDOG="${VICTORY_NO_WATCHDOG:-1}"
export VICTORY_OWN_UPSCALER="${VICTORY_OWN_UPSCALER:-1}"
export VICTORY_LOG="${VICTORY_LOG:-info}"

to_win_path() {
  local abs
  abs="$(realpath -m "$1")"
  printf 'Z:%s' "${abs//\//\\}"
}

WIN_EXE="$(to_win_path "$VICTORY_EXE")"
WIN_TARGET="$(to_win_path "$TARGET_PATH")"

mkdir -p "$WINEPREFIX"

if [[ "$MODE" == "precompile" ]]; then
  exec umu-run "$WIN_EXE" precompile "$WIN_TARGET" "$@"
else
  CONSOLE="${CONSOLE:-series-s}"
  exec umu-run "$WIN_EXE" run "$WIN_TARGET" --execute --console "$CONSOLE" "$@"
fi
