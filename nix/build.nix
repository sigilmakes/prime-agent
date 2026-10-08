{ lib
, stdenv
, buildNpmPackage
, fetchurl
, autoPatchelfHook
}:

let
    version = (builtins.fromJSON (builtins.readFile ../package.json)).version;
    catalogRelease = fetchurl {
        url = "https://pub-728493de92a943e2a9b2d17b4719f318.r2.dev/releases/v0.9.8/prime-agent-0.9.8.tgz";
        hash = "sha256-17cnhRGe/Ci/vKjsSn9Hofzc9H/NjOvbYL/6p54+EnQ=";
    };
in
buildNpmPackage {
    pname = "prime-agent-compiled";
    inherit version;
    src = lib.cleanSourceWith {
        src = ../.;
        filter = path: type:
            let name = baseNameOf path;
            in lib.cleanSourceFilter path type
                && !(builtins.elem name [ "node_modules" "dist" "result" ".prime" ".env" ])
                && !(lib.hasSuffix ".bundled.json" name);
    };

    npmDepsHash = "sha256-7Oh9oLB/c/gwxJbD14rbU12O+s05AIsgGjTlA9/4Y7E=";
    npmDepsFetcherVersion = 2;
    # Platform binaries come from the lockfile. Never bootstrap user state or
    # invoke the development-only canvas downloader during dependency setup.
    npmFlags = [ "--ignore-scripts" ];
    HUSKY = "0";
    nativeBuildInputs = [ autoPatchelfHook ];
    buildInputs = [ stdenv.cc.cc.lib ];
    dontAutoPatchelf = true;
    dontPatchELF = true;

    preBuild = ''
        export GOMAXPROCS="$NIX_BUILD_CORES"
        mkdir -p packages/coding-agent/catalog
        tar -xOf ${catalogRelease} package/dist/models.bundled.json \
            > packages/coding-agent/catalog/models.bundled.json
        tar -xOf ${catalogRelease} package/dist/mcp-services.bundled.json \
            > packages/coding-agent/catalog/mcp-services.bundled.json
        node packages/coding-agent/scripts/catalog-assets.mjs verify \
            --out packages/coding-agent/catalog
        autoPatchelf \
            node_modules/@esbuild/linux-x64 \
            node_modules/@typescript/native-preview-linux-x64 \
            node_modules/@biomejs/cli-linux-x64 \
            node_modules/@rolldown/binding-linux-x64-gnu \
            node_modules/koffi/build/koffi/linux_x64 \
            node_modules/@mariozechner/clipboard-linux-x64-gnu
    '';

    # Consumers copy this one compiled workspace, keeping relative npm workspace
    # links valid. The development tree is not the installed application.
    installPhase = ''
        runHook preInstall
        mkdir -p "$out"
        cp -r ./. "$out/"
        runHook postInstall
    '';

    meta = {
        description = "Shared compiled Prime Agent workspace for packaging and checks";
        license = lib.licenses.mit;
        platforms = [ "x86_64-linux" ];
    };
}
