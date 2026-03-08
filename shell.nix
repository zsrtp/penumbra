{ pkgs ? import <nixpkgs> {} }:
let
  libPath = with pkgs; lib.makeLibraryPath [
    wayland
    libxkbcommon
    libGL
  ];
in {
  devShell = with pkgs; mkShell {
    buildInputs = [
      gcc
      cargo
      rustc
      rust-analyzer
    ];
    RUST_LOG = "debug";
    RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
    LD_LIBRARY_PATH = libPath;
  };
}