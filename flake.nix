{
    description = "Weave downstream Rust source build";

    inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    outputs = { nixpkgs, ... }:
        let
            system = "x86_64-linux";
            pkgs = import nixpkgs { inherit system; };
            compiled = pkgs.callPackage ./nix/build.nix { };
            # CI has already run the full workspace gate; rebuilding the audited
            # test profile here used to duplicate the package checkPhase.
            compiled-ci = compiled.overrideAttrs (_: { doCheck = false; });
            weave = pkgs.callPackage ./nix/package.nix { inherit compiled; };
            weave-ci = pkgs.callPackage ./nix/package.nix { compiled = compiled-ci; };
        in {
            packages.${system} = {
                inherit weave compiled weave-ci;
                default = weave;
            };
            apps.${system}.default = {
                type = "app";
                program = pkgs.lib.getExe weave;
            };
            checks.${system} = import ./nix/checks.nix {
                inherit pkgs compiled weave;
            };
            devShells.${system}.default = pkgs.mkShell {
                packages = with pkgs; [
                    cargo rustc rustfmt clippy pkg-config openssl.dev
                    git uv fd ripgrep python3 python311 actionlint shellcheck gnumake
                    util-linux iproute2 procps bash
                ] ++ lib.optionals stdenv.hostPlatform.isLinux [ tini ];
                # Cold CI bootstrap must use a Nix-native interpreter, not a uv download.
                PA_WORKSPACE_BOOTSTRAP_PYTHON = "${pkgs.python311}/bin/python3.11";
                CARGO_BUILD_JOBS = "8";
                CARGO_PROFILE_DEV_DEBUG = "0";
                CARGO_PROFILE_TEST_DEBUG = "0";
                # Python kernel wheels need the same native libraries as the
                # installed wrapper, including in CI containers.
                LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.stdenv.cc.cc.lib pkgs.zlib ];
            };
        };
}
