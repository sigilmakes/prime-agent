{ fetchurl, runCommand, python3 }:

let
    # Real catalog snapshot. Update the commit and both hashes together.
    revision = "b90e08807c37ccdfab04eb31b23de6a86dd6e7fe";
    base = "https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/${revision}";
    models = fetchurl {
        url = "${base}/models/catalog.v1.json";
        hash = "sha256-uVS5tK7qJGalx4plf8VqBbLW9K6IPywSt8LkKvjRb9I=";
    };
    services = fetchurl {
        url = "${base}/plugins/catalog.v2.json";
        hash = "sha256-Uir8FHGDQgg4dFF+AEDDj5t6Ex6/zYTDrZ54h5YFc3w=";
    };
in
runCommand "prime-agent-catalog-${builtins.substring 0 12 revision}" {
    nativeBuildInputs = [ python3 ];
} ''
    mkdir -p "$out"
    cp ${models} "$out/models.bundled.json"
    cp ${services} "$out/mcp-services.bundled.json"
    python3 ${../scripts/release/bundle_catalog.py} verify --out "$out"
''
