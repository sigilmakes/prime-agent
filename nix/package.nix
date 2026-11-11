{ lib, stdenvNoCC, stdenv, compiled, makeWrapper, python3, fd, ripgrep, uv, zlib }:

stdenvNoCC.mkDerivation {
    pname = "weave";
    inherit (compiled) version;
    dontUnpack = true;
    dontConfigure = true;
    dontBuild = true;
    nativeBuildInputs = [ makeWrapper python3 ];
    installPhase = ''
        runHook preInstall
        mkdir -p "$out/bin" "$out/lib"
        ln -s ${compiled}/lib/weave "$out/lib/weave"
        makeWrapper ${compiled}/bin/wv "$out/bin/wv" \
            --set PI_PACKAGE_DIR "$out/lib/weave" \
            --set PI_SKIP_VERSION_CHECK 1 \
            --set PRIME_AGENT_INSTALL_MANAGED_BY nix \
            --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ stdenv.cc.cc.lib zlib ]} \
            --prefix PATH : ${lib.makeBinPath [ fd ripgrep uv ]}
        runHook postInstall
    '';
    doInstallCheck = true;
    installCheckPhase = ''
        runHook preInstallCheck
        python3 ${./check_privacy.py} --artifact "$out/lib/weave"
        python3 ${./runtime_smoke.py} "$out/bin/wv" '${compiled.version}'
        python3 ${./privacy_lifecycle.py} "$out/bin/wv" --timeout 90
        runHook postInstallCheck
    '';
    passthru = { inherit compiled; };
    meta = {
        description = "Weave downstream Rust coding agent";
        homepage = "https://mnemosyne.sigilzero.dev/sigilzero/weave";
        license = with lib.licenses; [ mit cc-by-40 ];
        platforms = [ "x86_64-linux" ];
        mainProgram = "wv";
    };
}
