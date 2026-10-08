{ prime-agent, git }:

prime-agent.overrideAttrs (old: {
    pname = "prime-agent-checks";
    nativeBuildInputs = old.nativeBuildInputs ++ [ git ];
    preBuild = old.preBuild + ''
        autoPatchelf node_modules/@biomejs/cli-linux-x64 node_modules/@rolldown/binding-linux-x64-gnu
    '';
    postBuild = ''
        export HOME="$TMPDIR/check-home"
        export XDG_CONFIG_HOME="$HOME/.config"
        export XDG_CACHE_HOME="$HOME/.cache"
        export XDG_DATA_HOME="$HOME/.local/share"
        export XDG_STATE_HOME="$HOME/.local/state"
        export XDG_RUNTIME_DIR="$HOME/runtime"
        export PRIME_AGENT_CODING_AGENT_DIR="$HOME/agent"
        export PRIME_AGENT_SESSION_DIR="$HOME/sessions"
        export PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 DO_NOT_TRACK=1
        export NODE_DISABLE_COMPILE_CACHE=1 PRIME_AGENT_INSTALL_METHOD=node
        mkdir -p "$HOME" "$XDG_RUNTIME_DIR"
        chmod 700 "$XDG_RUNTIME_DIR"
        npm run check:static
        (cd packages/coding-agent && ../../node_modules/.bin/vitest --run test/args.test.ts test/catalog-assets.test.ts)
    '';
    installPhase = ''
        mkdir -p "$out"
        printf 'Static checks and focused tests passed.\n' > "$out/result"
    '';
    doInstallCheck = false;
})
