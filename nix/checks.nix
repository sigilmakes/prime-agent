{ pkgs, compiled, prime-agent }:

{
    inherit prime-agent;
    static = pkgs.runCommand "prime-agent-downstream-static" {
        nativeBuildInputs = [ pkgs.python3 pkgs.git pkgs.actionlint pkgs.shellcheck ];
        src = pkgs.lib.cleanSource ../.;
    } ''
        cp -r "$src" source
        chmod -R u+w source
        cd source
        patchShebangs .husky
        export HOME="$TMPDIR/home"
        export TMPDIR="$TMPDIR/private"
        mkdir -m 700 -p "$HOME" "$TMPDIR"
        python3 nix/tests/test_packaging.py
        python3 -B -m unittest discover -s nix/tests -p test_privacy_lifecycle.py
        python3 scripts/test_downstream_hooks.py
        python3 nix/check_privacy.py --source .
        python3 nix/check_workflows.py .
        actionlint -shellcheck="" \
            -ignore '^label "gaia-runner" is unknown' \
            -ignore '^specifying action "https://data.forgejo.org/actions/checkout@11d5960a326750d5838078e36cf38b85af677262" in invalid format' \
            .forgejo/workflows/ci.yml
        touch "$out"
    '';
    audited = compiled;
}
