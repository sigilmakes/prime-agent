pub fn script(source: &str, interpreter: &str) -> String {
    use std::os::unix::fs::PermissionsExt;

    let path = std::env::var_os("PATH").expect("fixture interpreter requires tool PATH");
    let executable = std::env::split_paths(&path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(interpreter))
        .find(|candidate| {
            candidate.is_file()
                && candidate
                    .metadata()
                    .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
        })
        .and_then(|candidate| candidate.canonicalize().ok())
        .expect("fixture interpreter must be an absolute executable from tool PATH");
    let executable = executable
        .to_str()
        .expect("fixture interpreter path is UTF-8");
    assert!(
        !executable.chars().any(char::is_whitespace),
        "fixture interpreter path cannot contain whitespace"
    );
    let (_, body) = source.split_once('\n').expect("fixture has a shebang line");
    format!("#!{executable}\n{body}")
}
