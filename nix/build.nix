{ lib, rustPlatform, callPackage, python3, pkg-config }:

let
    manifest = builtins.fromTOML (builtins.readFile ../Cargo.toml);
    catalog = callPackage ./catalog.nix { };
in
rustPlatform.buildRustPackage {
    pname = "prime-agent-compiled";
    version = manifest.workspace.package.version;
    src = lib.cleanSourceWith {
        src = ../.;
        filter = path: type:
            let name = baseNameOf path;
            in lib.cleanSourceFilter path type
                && !(builtins.elem name [ "target" "result" ".prime" ".env" ".venv" "__pycache__" ])
                && !(lib.hasSuffix ".bundled.json" name);
    };
    cargoLock.lockFile = ../Cargo.lock;
    nativeBuildInputs = [ pkg-config python3 ];
    strictDeps = true;
    cargoBuildFlags = [ "-p" "pa-cli" "--bin" "prime-agent" ];
    # Build once for the package and audited check. Never inherit the full
    # upstream test suite: some batteries require live credentials/toolchains.
    doCheck = true;
    CARGO_PROFILE_TEST_DEBUG = "0";
    checkPhase = ''
        runHook preCheck
        bash ${./check_audited.sh}
        runHook postCheck
    '';
    CARGO_BUILD_JOBS = "2";
    CARGO_PROFILE_RELEASE_DEBUG = "0";
    postInstall = ''
        mkdir -p "$out/lib/prime-agent"
        python3 ${./stage_payload.py} "$PWD" "$out/lib/prime-agent"
        cp ${catalog}/*.bundled.json "$out/lib/prime-agent/"
        ln -s "$out/bin/prime-agent" "$out/lib/prime-agent/prime-agent"
    '';
    passthru = { inherit catalog; };
    meta = {
        description = "Shared downstream Prime Agent Rust source build";
        license = with lib.licenses; [ mit cc-by-40 ];
        platforms = [ "x86_64-linux" ];
    };
}
