use mmdflux::commands::{Command, apply};
use mmdflux::graph::{Arrow, Shape, Stroke};
use mmdflux::mmds::events::ModelEvent;
use mmdflux::mmds::{Document, Subject};
use mmdflux::views::{
    AnchorRef, Selector, TraversalDirection, ViewEvent, ViewSpec, ViewStatement, project,
};
use mmdflux::{OutputFormat, RenderConfig, materialize_diagram, render_document};

const SOURCE: &str = r#"graph TD
api[API] --> auth[Auth]
api --> billing[Billing]
auth --> users[(Users)]
billing --> ledger[(Ledger)]
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut document: Document = materialize_diagram(SOURCE, &RenderConfig::default())?;
    let mut model_events = Vec::new();

    for command in [
        Command::AddNode {
            id: "cache".to_string(),
            label: "Cache".to_string(),
            shape: Shape::Cylinder,
            parent: None,
        },
        Command::AddEdge {
            id: None,
            source: "api".to_string(),
            target: "cache".to_string(),
            from_subgraph: None,
            to_subgraph: None,
            label: Some("warms".to_string()),
            stroke: Stroke::Dashed,
            arrow_start: Arrow::None,
            arrow_end: Arrow::Normal,
            minlen: 1,
        },
        Command::ChangeNodeLabel {
            node: "billing".to_string(),
            label: "Billing API".to_string(),
        },
    ] {
        model_events.extend(apply(&command, &mut document)?);
    }

    println!("model events:");
    for event in &model_events {
        println!("- {}", describe_model_event(event));
    }

    let spec = ViewSpec::new(vec![ViewStatement::Include(Selector::Traversal {
        anchor: AnchorRef::Node("api".to_string()),
        direction: TraversalDirection::Downstream,
        hops: 1,
    })]);
    let (view, view_events) = project(&document, &spec)?;

    println!("\nview nodes:");
    for node in &view.nodes {
        println!("- {}", node.id);
    }

    println!("\nview events:");
    for event in &view_events {
        if let ViewEvent::NodeLeftView { id, .. } = event {
            println!("- node left view: {id}");
        }
    }

    let text = render_document(&view, OutputFormat::Text, &RenderConfig::default())?;
    println!("\n{text}");

    Ok(())
}

fn describe_model_event(event: &ModelEvent) -> String {
    let subject = match &event.subject {
        Subject::Document => "document".to_string(),
        Subject::Node(id) => format!("node {id}"),
        Subject::Edge(id) => format!("edge {id}"),
        Subject::Subgraph(id) => format!("subgraph {id}"),
        _ => "<unknown>".to_string(),
    };

    format!("{:?} on {subject}", event.kind)
}
