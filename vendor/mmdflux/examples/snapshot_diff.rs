use mmdflux::mmds::diff::{Change, ChangeKind, diff_documents};
use mmdflux::mmds::{Document, Subject};
use mmdflux::{RenderConfig, materialize_diagram};

const BEFORE: &str = r#"graph TD
api[API] --> auth[Auth]
api --> billing[Billing]
"#;

const AFTER: &str = r#"graph TD
api[Public API] --> auth[Auth]
api --> billing[Billing API]
billing --> ledger[(Ledger)]
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let before: Document = materialize_diagram(BEFORE, &RenderConfig::default())?;
    let after: Document = materialize_diagram(AFTER, &RenderConfig::default())?;
    let diff = diff_documents(&before, &after);

    println!(
        "geometry level: {} -> {}",
        diff.before_geometry_level, diff.after_geometry_level
    );

    println!("\nmodel changes:");
    for change in diff.changes.iter().filter(|change| change.kind.is_model()) {
        println!("- {}", describe_change(change));
    }

    println!("\ngeometry changes:");
    for change in diff
        .changes
        .iter()
        .filter(|change| change.kind.is_geometry())
    {
        println!("- {}", describe_change(change));
    }

    if diff
        .changes
        .iter()
        .any(|change| change.kind == ChangeKind::NodeLabelChanged)
    {
        println!("\nThe snapshot diff reports what differs, not which command produced it.");
    }

    Ok(())
}

fn describe_change(change: &Change) -> String {
    let subject = match &change.subject {
        Subject::Document => "document".to_string(),
        Subject::Node(id) => format!("node {id}"),
        Subject::Edge(id) => format!("edge {id}"),
        Subject::Subgraph(id) => format!("subgraph {id}"),
        _ => "<unknown>".to_string(),
    };

    if change.evidence.is_empty() {
        format!("{:?} on {subject}", change.kind)
    } else {
        format!(
            "{:?} on {subject} ({})",
            change.kind,
            change.evidence.join("; ")
        )
    }
}
