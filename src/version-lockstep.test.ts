// R6 of docs/plans/2026-08-27-001: the version lives in three files — the
// root package (the frontend), the Electron shell package (what dpkg
// reports), and the Rust crate (what `fly --version` and `core/ping`
// report). They must agree, and prose was the only thing enforcing it.
import { describe, it, expect } from "vitest";
import rootPkg from "../package.json";
import shellPkg from "../electron/package.json";
import cargoToml from "../core/Cargo.toml?raw";

describe("version lockstep", () => {
  it("package.json, electron/package.json, and Cargo.toml agree", () => {
    const cargo = /^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m.exec(cargoToml)?.[1];
    expect(cargo, "Cargo.toml [package] version").toBeTruthy();
    expect(shellPkg.version).toBe(rootPkg.version);
    expect(cargo).toBe(rootPkg.version);
  });
});
