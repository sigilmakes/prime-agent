{
    description = "Prime Agent built from source";

    inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    outputs = { nixpkgs, ... }:
        let
            system = "x86_64-linux";
            pkgs = import nixpkgs { inherit system; };
            prime-agent = pkgs.callPackage ./nix/package.nix { };
            app = {
                type = "app";
                program = pkgs.lib.getExe prime-agent;
            };
        in {
            packages.${system} = {
                inherit prime-agent;
                default = prime-agent;
            };
            apps.${system} = {
                prime-agent = app;
                default = app;
            };
            checks.${system} = {
                inherit prime-agent;
                source = pkgs.callPackage ./nix/checks.nix { inherit prime-agent; };
            };
            devShells.${system}.default = pkgs.mkShell {
                packages = with pkgs; [ nodejs git uv fd ripgrep ];
            };
        };
}
