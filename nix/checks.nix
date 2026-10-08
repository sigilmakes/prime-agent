{ lib, stdenvNoCC, compiled, nodejs, git }:

let
    groups = builtins.fromJSON (builtins.readFile ./test-groups.json);
    testCommand = _: group:
        let
            runner = if group.runner == "vitest"
                then "../../node_modules/.bin/vitest --run --maxWorkers=2"
                else if group.runner == "node"
                then "node --test --test-concurrency=2 --import tsx"
                else throw "Unknown test runner: ${group.runner}";
        in "cd ${lib.escapeShellArg "packages/${group.package}"} && ${runner} ${lib.escapeShellArgs group.files}";
    commands = lib.mapAttrs testCommand groups // {
        static = "npm run check:static";
    };
    check = group: command: stdenvNoCC.mkDerivation {
        pname = "prime-agent-check-${group}";
        inherit (compiled) version;
        src = compiled;
        nativeBuildInputs = [ nodejs ] ++ lib.optional (group == "static") git;
        dontConfigure = true;
        strictDeps = true;
        buildPhase = ''
            runHook preBuild
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
            export GOMAXPROCS="$NIX_BUILD_CORES"
            mkdir -p "$HOME" "$XDG_RUNTIME_DIR"
            chmod 700 "$XDG_RUNTIME_DIR"
            ${command}
            runHook postBuild
        '';
        installPhase = ''
            mkdir -p "$out"
            printf '%s\n' '${group} checks passed.' > "$out/result"
        '';
    };
in
lib.mapAttrs check commands
