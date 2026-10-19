{
    description = "Prime Agent downstream Rust source build";

    inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    outputs = { nixpkgs, ... }:
        let
            system = "x86_64-linux";
            pkgs = import nixpkgs { inherit system; };
            compiled = pkgs.callPackage ./nix/build.nix { };
            prime-agent = pkgs.callPackage ./nix/package.nix { inherit compiled; };
        in {
            packages.${system} = {
                inherit prime-agent compiled;
                default = prime-agent;
            };
            apps.${system}.default = {
                type = "app";
                program = pkgs.lib.getExe prime-agent;
            };
            checks.${system} = import ./nix/checks.nix {
                inherit pkgs compiled prime-agent;
            };
            devShells.${system}.default = pkgs.mkShell {
                packages = with pkgs; [
                    cargo rustc rustfmt clippy pkg-config
                    git uv fd ripgrep python3 actionlint shellcheck gnumake
                ];
                CARGO_BUILD_JOBS = "2";
                CARGO_PROFILE_DEV_DEBUG = "0";
                CARGO_PROFILE_TEST_DEBUG = "0";
            };
        };
}
