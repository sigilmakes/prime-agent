use mmdflux::mmds::Document;
use mmdflux::views::{
    AnchorRef, Selector, TraversalDirection, ViewEvent, ViewSpec, ViewStatement, project,
};
use mmdflux::{OutputFormat, RenderConfig, materialize_diagram, render_document};

const SOURCE: &str = r#"graph TD
service_a[Service A] --> service_b[Service B]
external[External] --> service_a
service_b --> service_c[Service C]
service_c --> database[Database]
service_a --> audit[Audit]
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let canonical: Document = materialize_diagram(SOURCE, &RenderConfig::default())?;
    let spec = ViewSpec::new(vec![ViewStatement::Include(Selector::Traversal {
        anchor: AnchorRef::Node("service_a".to_string()),
        direction: TraversalDirection::Downstream,
        hops: 2,
    })]);

    let (view, events) = project(&canonical, &spec)?;
    let text = render_document(&view, OutputFormat::Text, &RenderConfig::default())?;

    println!("retained nodes:");
    for node in &view.nodes {
        println!("- {}", node.id);
    }

    println!("\nelided edges:");
    for event in &events {
        if let ViewEvent::EdgeElided {
            source,
            target,
            ordinal,
            ..
        } = event
        {
            println!("- {source} -> {target} (ordinal {ordinal})");
        }
    }

    println!("\n{text}");
    Ok(())
}
