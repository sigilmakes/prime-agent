//! The python-skill manifest concern: the pyproject parsing, and the normalization that
//! deduplicates and resolves sibling-local dependencies.

use super::{Digest, KernelPythonSkill, Path};

/// One normalized skill as recorded in the bootstrap version file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BootstrapPythonSkill {
    #[serde(alias = "import_name")]
    pub(super) import_name: String,
    #[serde(alias = "package_path")]
    pub(super) package_path: String,
    #[serde(alias = "pyproject_path")]
    pub(super) pyproject_path: String,
    #[serde(alias = "pyproject_hash")]
    pub(super) pyproject_hash: String,
}

pub(super) fn file_content_hash(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => format!("sha256:{:x}", sha2::Sha256::digest(&bytes)),
        Err(_) => "unreadable".to_string(),
    }
}

fn read_project(pyproject: &Path) -> Option<toml::Value> {
    let text = std::fs::read_to_string(pyproject).ok()?;
    toml::from_str::<toml::Value>(&text)
        .ok()?
        .get("project")
        .cloned()
}

pub(super) fn read_python_skill_project_name(skill: &BootstrapPythonSkill) -> String {
    read_project(Path::new(&skill.pyproject_path))
        .and_then(|project| project.get("name")?.as_str().map(str::to_string))
        .unwrap_or_else(|| skill.import_name.replace('_', "-"))
}

fn parse_dependency_package_name(dependency: &str) -> Option<String> {
    let without_marker = dependency.split(';').next()?.trim();
    if without_marker.is_empty() {
        return None;
    }
    let name: String = without_marker
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .collect();
    if name.is_empty() {
        return None;
    }
    Some(name.replace('_', "-").to_lowercase())
}

/// Names in the `[project] dependencies` array, tolerating quotes/escapes.
pub(super) fn read_python_skill_dependency_names(skill: &BootstrapPythonSkill) -> Vec<String> {
    read_project(Path::new(&skill.pyproject_path))
        .and_then(|project| project.get("dependencies")?.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(toml::Value::as_str)
        .filter_map(parse_dependency_package_name)
        .collect()
}

pub(crate) fn to_bootstrap_skill(skill: &KernelPythonSkill) -> BootstrapPythonSkill {
    let package_path =
        std::path::absolute(&skill.package_path).unwrap_or_else(|_| skill.package_path.clone());
    let pyproject_path =
        std::path::absolute(&skill.pyproject_path).unwrap_or_else(|_| skill.pyproject_path.clone());
    BootstrapPythonSkill {
        import_name: skill.import_name.clone(),
        package_path: package_path.to_string_lossy().to_string(),
        pyproject_path: pyproject_path.to_string_lossy().to_string(),
        pyproject_hash: file_content_hash(&skill.pyproject_path),
    }
}

/// Deduplicate skills (by importName + packagePath), resolve sibling-local
/// dependencies, and sort deterministically — matching `normalizePythonSkills`.
pub(crate) fn normalize_python_skills(
    python_skills: &[KernelPythonSkill],
) -> Vec<BootstrapPythonSkill> {
    fn add_skill(by_key: &mut Vec<(String, BootstrapPythonSkill)>, skill: &BootstrapPythonSkill) {
        let key = format!("{}\u{0}{}", skill.import_name, skill.package_path);
        if by_key.iter().any(|(existing, _)| *existing == key) {
            return;
        }
        by_key.push((key, skill.clone()));
        for dependency_name in read_python_skill_dependency_names(skill) {
            if let Some(sibling) = resolve_sibling_python_skill_dependency(skill, &dependency_name)
            {
                add_skill(by_key, &sibling);
            }
        }
    }
    let mut by_key: Vec<(String, BootstrapPythonSkill)> = Vec::new();
    for skill in python_skills {
        add_skill(&mut by_key, &to_bootstrap_skill(skill));
    }
    let mut skills: Vec<BootstrapPythonSkill> = by_key.into_iter().map(|(_, s)| s).collect();
    skills.sort_by(|a, b| {
        a.package_path
            .cmp(&b.package_path)
            .then(a.import_name.cmp(&b.import_name))
    });
    skills
}

fn resolve_sibling_python_skill_dependency(
    skill: &BootstrapPythonSkill,
    dependency_name: &str,
) -> Option<BootstrapPythonSkill> {
    let siblings_dir = Path::new(&skill.package_path).parent()?;
    for entry in std::fs::read_dir(siblings_dir).ok()? {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        let package_path = entry.path();
        let pyproject_path = package_path.join("pyproject.toml");
        if !pyproject_path.exists() {
            continue;
        }
        let candidate = BootstrapPythonSkill {
            import_name: entry.file_name().to_string_lossy().replace('-', "_"),
            package_path: package_path.to_string_lossy().to_string(),
            pyproject_path: pyproject_path.to_string_lossy().to_string(),
            pyproject_hash: file_content_hash(&pyproject_path),
        };
        if read_python_skill_project_name(&candidate)
            .replace('_', "-")
            .to_lowercase()
            == dependency_name
        {
            return Some(candidate);
        }
    }
    None
}
