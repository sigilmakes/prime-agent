{ lib
, stdenv
, compiled
, nodejs
, makeWrapper
, fd
, ripgrep
, uv
}:

stdenv.mkDerivation {
    pname = "prime-agent";
    inherit (compiled) version;
    src = compiled;
    nativeBuildInputs = [ nodejs makeWrapper ];
    dontConfigure = true;
    dontBuild = true;
    dontStrip = true;
    dontPatchELF = true;
    strictDeps = true;

    installPhase = ''
        runHook preInstall
        export HOME="$TMPDIR/npm-home"
        export npm_config_cache="$TMPDIR/npm-cache"
        mkdir -p "$HOME"
        cp -r ${compiled.npmDeps}/. "$npm_config_cache"
        chmod -R u+w "$npm_config_cache"
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
            "$out" '${compiled.version}' \
            "$PWD/packages/coding-agent/scripts/catalog-assets.mjs" \
            "$TMPDIR/install-check" ${./runtime-smoke.mjs}
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
