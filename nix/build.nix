{ lib, rustPlatform, callPackage, python3, pkg-config }:

let
    manifest = builtins.fromTOML (builtins.readFile ../Cargo.toml);
    catalog = callPackage ./catalog.nix { };
in
rustPlatform.buildRustPackage {
    pname = "weave-compiled";
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
    cargoBuildFlags = [ "-p" "pa-cli" "--bin" "wv" ];
    # Build once for the package and audited check. Never inherit the full
    # upstream test suite: some batteries require live credentials/toolchains.
    doCheck = true;
    CARGO_PROFILE_TEST_DEBUG = "0";
    checkPhase = ''
        runHook preCheck
        bash ${./check_audited.sh}
        runHook postCheck
    '';
    # The CI host has 32 cores; two jobs left the release build as the
    # slowest job on every push. Eight still leaves the host responsive.
    CARGO_BUILD_JOBS = "8";
    CARGO_PROFILE_RELEASE_DEBUG = "0";
    postInstall = ''
        mkdir -p "$out/lib/weave"
        python3 ${./stage_payload.py} "$PWD" "$out/lib/weave"
        cp ${catalog}/*.bundled.json "$out/lib/weave/"
        ln -s "$out/bin/wv" "$out/lib/weave/wv"
    '';
    passthru = { inherit catalog; };
    meta = {
        description = "Shared Weave Rust source build";
        license = with lib.licenses; [ mit cc-by-40 ];
        platforms = [ "x86_64-linux" ];
    };
}
