#!/bin/bash
# fly .deb post-install (Electron-shell migration U7).
set -e

# Replaces electron-builder's default afterInstall, so it must keep the
# default's one load-bearing step: the Chromium SUID sandbox helper. Without
# the 4755 chrome-sandbox the packaged app refuses to start (dev checkouts
# use --no-sandbox; the product must not). Deliberately NOT `|| true`: a
# failed chmod here means a package that installs cleanly and then refuses
# to launch with no explanation — fail the install instead.
chmod 4755 '/opt/fly/chrome-sandbox'

# The same /usr/bin/fly story as the Tauri package: the bundled Rust binary
# serves the CLI (`fly notify|hooks|automation|agents|send`) and the headless
# core (`fly core`). Hooks installed by `fly hooks setup` embed this absolute
# path, so it must stay valid across the Tauri→Electron cutover.
ln -sf '/opt/fly/resources/fly' /usr/bin/fly

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database /usr/share/applications || true
fi
