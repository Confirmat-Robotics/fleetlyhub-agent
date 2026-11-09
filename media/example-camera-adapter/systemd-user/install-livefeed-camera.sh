#!/usr/bin/env bash
set -euo pipefail

if [[ $(id -u) == 0 ]]; then
  echo "Run this script as the camera user, without sudo." >&2
  exit 1
fi
device="${1:-/dev/video0}"
camera="${FLEETLY_CAMERA_BIN:-/usr/bin/fleetlyhub-camera}"
# These values enter a systemd command and sed replacements, so accept plain paths only.
for value in "$device" "$camera"; do
  if [[ ! $value =~ ^/[A-Za-z0-9_./-]+$ || $value == *..* ]]; then
    echo "Use an absolute path without spaces or special characters: $value" >&2
    exit 1
  fi
done
if [[ ! -x "$camera" ]]; then
  echo "Camera helper not found: $camera. Install fleetlyhub-agent first." >&2
  exit 1
fi
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
unit_file="$unit_dir/livefeed_camera.service"
temporary_unit="$(mktemp)"
trap 'rm -f -- "$temporary_unit"' EXIT
sed -e "s|/usr/bin/fleetlyhub-camera|$camera|g" \
    -e "s|/dev/video0|$device|g" \
    "$script_dir/livefeed_camera.service" > "$temporary_unit"
mkdir -p -- "$unit_dir"
if [[ -e "$unit_file" ]] && ! cmp -s -- "$temporary_unit" "$unit_file"; then
  echo "Existing service differs; preserving it. Edit $unit_file manually." >&2
  exit 1
fi
install -m 0644 -- "$temporary_unit" "$unit_file"
systemctl --user daemon-reload
systemctl --user enable --now livefeed_camera.service
systemctl --user --no-pager status livefeed_camera.service
if [[ $(loginctl show-user "$(id -un)" -p Linger --value) != yes ]]; then
  echo "For startup at boot and operation after logout, run once:"
  printf 'sudo loginctl enable-linger %q\n' "$(id -un)"
fi
