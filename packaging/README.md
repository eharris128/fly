# packaging/

Despite the name, this directory holds **no packaging configuration** — only
the one-shot icon toolchain: `gen-icon.mjs` (a dependency-free generator that
produced `icon-source.png`, fed once to `tauri icon` to populate
`src-tauri/icons/`). It is not wired into any build script; re-run it only to
regenerate the icon set.

The real packaging config lives elsewhere:

- **Electron .deb (the shipped product)** — `electron/package.json` (`build`
  section) + `electron/deb/postinst.sh` / `postrm.sh`.
- **Tauri .deb (the rollback)** — `src-tauri/tauri.conf.json` (`bundle`) +
  `src-tauri/deb/postinst`.
- **Icons** — `src-tauri/icons/` (shared by both shells).
