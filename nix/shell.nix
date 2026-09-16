{
  mkShell,
  rust-bin,
  cargo-make,
  cargo-release,
  nodejs,
  python3,
}:

let
  rust-toolchain = rust-bin.stable.latest.default.override {
    extensions = [
      "rust-src"
      "rustfmt"
      "clippy"
      "rust-analyzer"
    ];
    # Extra std libs needed to cross-build release binaries for macOS
    # targets from either macOS host arch, and for the musl release
    # target, while still linking via the runner's own system toolchain.
    targets = [
      "x86_64-unknown-linux-musl"
      "x86_64-apple-darwin"
      "aarch64-apple-darwin"
    ];
  };
in

mkShell {
  nativeBuildInputs = [
    rust-toolchain
    cargo-make
    cargo-release
    nodejs
    python3
  ];
}
