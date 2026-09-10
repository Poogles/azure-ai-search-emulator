{ pkgs ? import <nixpkgs> {}, system ? builtins.currentSystem, }:

# Set static dependency sources.
let
  # Standard source that is static.
  pinned = import (fetchTarball {
      name = "nixos-26.05";
      url = https://github.com/NixOS/nixpkgs/tarball/nixos-26.05;
  }) {};

  systemBuildExports = (
    with pinned;
    {
      x86_64-linux = ''
        export LD_LIBRARY_PATH=${pinned.stdenv.cc.cc.lib}/lib/:$LD_LIBRARY_PATH
      '';
      x86_64-darwin = ''
        export DYLD_LIBRARY_PATH=${pinned.stdenv.cc.cc.lib}/lib/:$DYLD_LIBRARY_PATH
      '';
      aarch64-darwin = ''
        export DYLD_LIBRARY_PATH=${pinned.stdenv.cc.cc.lib}/lib/:$DYLD_LIBRARY_PATH
      '';
    }
  );

# Install our dependencies with soruces defined above.
in
  pkgs.mkShell {
    name = "projects.aisearch-emulator";

    buildInputs = [
      pinned.autoPatchelfHook
      pinned.cargo
      pinned.clippy
      pinned.cmake
      pinned.coreutils
      pinned.direnv
      pinned.docker
      pinned.dotnet-sdk_10
      pinned.git
      pinned.pgcli
      pinned.poetry
      pinned.pre-commit
      pinned.python314
      pinned.rustc
      pinned.rustfmt
      pinned.stdenv.cc.cc.lib # required for numpy
    ];

    nativeBuildInputs = [ pinned.autoPatchelfHook ];

    # Set the required env vars to run the app.
    NIX_LDFLAGS = if system ? "x86_64-linux" then [ "-lstdc++"] else [];
    LANG="en_UK.UTF-8";

    shellHook = ''
      PATH="${pinned.poetry}/bin:${pinned.python314}/bin:${pinned.cargo}/bin:${pinned.rustc}/bin:$PATH";
    '' + systemBuildExports.${system} + pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
      # The nix darwin sdkroot does not ship usr/lib/libiconv.tbd, so the
      # linker cannot resolve -liconv when building Rust binaries/tests.
      # Add the system SDK lib directory to the linker search path.
      export RUSTFLAGS="-L $(xcrun --show-sdk-path)/usr/lib";
    '';
}
