#!/bin/bash
# fly .deb post-remove (Electron-shell migration U7): drop the /usr/bin/fly
# symlink only if it still points into this package's install root.
if [ -L /usr/bin/fly ] && [ "$(readlink /usr/bin/fly)" = "/opt/fly/resources/fly" ]; then
  rm -f /usr/bin/fly
fi

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database /usr/share/applications || true
fi
