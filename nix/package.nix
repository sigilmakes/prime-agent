{ lib
, stdenv
, buildNpmPackage
, fetchurl
, nodejs
, makeWrapper
, autoPatchelfHook
, fd
, ripgrep
, uv
}:

let
    version = (builtins.fromJSON (builtins.readFile ../package.json)).version;
    catalogRelease = fetchurl {
        url = "https://pub-728493de92a943e2a9b2d17b4719f318.r2.dev/releases/v0.9.8/prime-agent-0.9.8.tgz";
        hash = "sha256-17cnhRGe/Ci/vKjsSn9Hofzc9H/NjOvbYL/6p54+EnQ=";
    };
in
buildNpmPackage {
    pname = "prime-agent";
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
    # Native runtime modules ship platform binaries. Do not run canvas's
    # development-only downloader or the workspace's bootstrap lifecycle.
    npmFlags = [ "--ignore-scripts" ];
    HUSKY = "0";
    nativeBuildInputs = [ makeWrapper autoPatchelfHook ];
    buildInputs = [ stdenv.cc.cc.lib ];
    dontAutoPatchelf = true;

    preBuild = ''
        mkdir -p packages/coding-agent/catalog
        tar -xOf ${catalogRelease} package/dist/models.bundled.json \
            > packages/coding-agent/catalog/models.bundled.json
        tar -xOf ${catalogRelease} package/dist/mcp-services.bundled.json \
            > packages/coding-agent/catalog/mcp-services.bundled.json
        node packages/coding-agent/scripts/catalog-assets.mjs verify \
            --out packages/coding-agent/catalog
        autoPatchelf node_modules/@esbuild/linux-x64 node_modules/@typescript/native-preview-linux-x64
    '';

    installPhase = ''
        runHook preInstall
        npm prune --offline --omit=dev --ignore-scripts
        target="$out/lib/prime-agent"
        mkdir -p "$target/packages" "$out/bin"
        cp package.json "$target/"
        cp -r node_modules "$target/"
        for workspace in tui ai agent coding-agent; do
            mkdir -p "$target/packages/$workspace"
            for entry in package.json dist node_modules docs examples skills README.md CHANGELOG.md; do
                if [ -e "packages/$workspace/$entry" ]; then
                    cp -r "packages/$workspace/$entry" "$target/packages/$workspace/"
                fi
            done
        done
        autoPatchelf \
            "$target/node_modules/koffi/build/koffi/linux_x64" \
            "$target/node_modules/@mariozechner/clipboard-linux-x64-gnu"
        makeWrapper ${lib.getExe nodejs} "$out/bin/prime-agent" \
            --set PI_SKIP_VERSION_CHECK 1 \
            --prefix PATH : ${lib.makeBinPath [ fd ripgrep uv ]} \
            --add-flags "$target/packages/coding-agent/dist/bundle/cli.js"
        runHook postInstall
    '';

    doInstallCheck = true;
    installCheckPhase = ''
        runHook preInstallCheck
        env -i PATH="$PATH" ${stdenv.shell} ${./check.sh} \
            "$out" '${version}' \
            "$PWD/packages/coding-agent/scripts/catalog-assets.mjs" \
            "$TMPDIR/install-check"
        runHook postInstallCheck
    '';

    meta = {
        description = "Terminal coding agent with a persistent Python REPL kernel";
        homepage = "https://github.com/PrimeIntellect-ai/prime-agent";
        license = lib.licenses.mit;
        platforms = [ "x86_64-linux" ];
        mainProgram = "prime-agent";
    };
}
