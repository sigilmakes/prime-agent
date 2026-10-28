mod geometry_metrics;
mod mmds_metrics;
pub(super) mod mutations;
mod phase_attribution;
mod proportionality;
mod render_metrics;
mod render_surfaces;
mod report;
mod route_diagnostics;

use mmds_metrics::Endpoint;
use phase_attribution::PhaseName;
use route_diagnostics::RouteDecisionClass;

use crate::engines::graph::algorithms::layered::kernel::trace::{
    DummyTraceKey, DummyTraceRole, LayeredPhaseTrace, TraceDummySnapshot, TraceNodeSnapshot,
    TraceStage, TraceStageSnapshot,
};
use crate::graph::GeometryLevel;

#[test]
fn input_magnitude_keeps_incompatible_axes_separate() {
    let magnitude = proportionality::InputMagnitude {
        label_dimension: Some(proportionality::LabelDimensionMagnitude {
            changed_labels: 1,
            max_axis_delta: 24.0,
            total_axis_delta: 24.0,
        }),
        edge_insertion: Some(proportionality::EdgeInsertionMagnitude {
            semantic_added_edge_ids: vec!["e7".to_string()],
            route_input_added_edges: 3,
        }),
        node_insertion: None,
        subgraph_change: None,
    };

    assert_eq!(
        magnitude.non_zero_axes(),
        vec![
            proportionality::InputMagnitudeAxis::LabelDimensionUnits,
            proportionality::InputMagnitudeAxis::EdgeInsertion,
        ]
    );
    assert_eq!(magnitude.scalar_denominator(), None);
}

#[test]
fn input_magnitude_basis_names_label_and_edge_cases_without_mixing_units() {
    assert_eq!(
        proportionality::InputMagnitudeBasis::for_pair_id("M14"),
        proportionality::InputMagnitudeBasis::LabelDimensionUnits
    );
    assert_eq!(
        proportionality::InputMagnitudeBasis::for_pair_id("M11"),
        proportionality::InputMagnitudeBasis::EdgeInsertionContext
    );
    assert_eq!(
        proportionality::InputMagnitudeBasis::for_pair_id("M15"),
        proportionality::InputMagnitudeBasis::MultiAxis
    );
}

#[test]
fn input_magnitude_collects_m14_label_dimension_without_edge_context() {
    let pair = mutations::pair_by_id("M14").expect("M14 pair");
    let magnitude = proportionality::input_magnitude_for_pair(pair).expect("M14 input magnitude");
    let label = magnitude
        .label_dimension
        .expect("label dimension magnitude");

    assert_eq!(label.changed_labels, 1);
    assert!(label.max_axis_delta > 0.0);
    assert!(label.total_axis_delta >= label.max_axis_delta);
    assert!(magnitude.edge_insertion.is_none());
    assert_eq!(
        proportionality::InputMagnitudeBasis::for_pair_id("M14"),
        proportionality::InputMagnitudeBasis::LabelDimensionUnits
    );
}

#[test]
fn input_magnitude_collects_m11_edge_insertion_context_without_label_units() {
    let pair = mutations::pair_by_id("M11").expect("M11 pair");
    let magnitude = proportionality::input_magnitude_for_pair(pair).expect("M11 input magnitude");
    let edge = magnitude.edge_insertion.expect("edge insertion magnitude");

    assert_eq!(edge.semantic_added_edge_ids, vec!["e7".to_string()]);
    assert!(edge.route_input_added_edges > 0);
    assert!(magnitude.label_dimension.is_none());
    assert_eq!(
        proportionality::InputMagnitudeBasis::for_pair_id("M11"),
        proportionality::InputMagnitudeBasis::EdgeInsertionContext
    );
}

#[test]
fn input_magnitude_collects_control_axes_without_classifying_them() {
    for pair_id in ["M02", "M03", "M04"] {
        let pair = mutations::pair_by_id(pair_id).expect("control pair");
        let magnitude =
            proportionality::input_magnitude_for_pair(pair).expect("control input magnitude");
        assert!(
            magnitude.edge_insertion.is_some() || magnitude.node_insertion.is_some(),
            "{pair_id} should expose edge or node insertion context"
        );
    }

    for pair_id in ["M05", "M07", "S02", "S03"] {
        let pair = mutations::pair_by_id(pair_id).expect("guardrail pair");
        let magnitude =
            proportionality::input_magnitude_for_pair(pair).expect("guardrail input magnitude");
        assert!(
            magnitude.non_zero_axes().len() <= 1,
            "{pair_id} should stay simple enough to be a guardrail"
        );
    }
}

#[test]
fn route_output_magnitude_uses_absolute_length_to_avoid_cancellation() {
    let paths = vec![
        proportionality::PathOutputMagnitude {
            edge_id: "e1".to_string(),
            signed_route_len_delta: 12.0,
            abs_route_len_delta: 12.0,
            bend_count_delta: 1,
            abs_bend_count_delta: 1,
            port_face_changed: false,
            endpoint_face_changed: false,
            endpoint_moved: false,
            decision_classes: vec![route_diagnostics::RouteDecisionClass::PathShape],
        },
        proportionality::PathOutputMagnitude {
            edge_id: "e2".to_string(),
            signed_route_len_delta: -10.0,
            abs_route_len_delta: 10.0,
            bend_count_delta: -1,
            abs_bend_count_delta: 1,
            port_face_changed: false,
            endpoint_face_changed: false,
            endpoint_moved: true,
            decision_classes: vec![route_diagnostics::RouteDecisionClass::BendTopology],
        },
    ];

    let summary = proportionality::RouteOutputMagnitudeSummary::from_paths(paths);

    assert_eq!(summary.changed_path_count, 2);
    assert_eq!(summary.signed_route_len_delta_sum, 2.0);
    assert_eq!(summary.sum_abs_route_len_delta, 22.0);
    assert_eq!(summary.max_abs_route_len_delta, 12.0);
    assert_eq!(summary.abs_bend_count_delta_sum, 2);
    assert_eq!(summary.layout_amplified_path_count, 1);
}

#[test]
fn route_output_magnitude_empty_summary_is_zeroed() {
    let summary = proportionality::RouteOutputMagnitudeSummary::from_paths(Vec::new());

    assert_eq!(summary.changed_path_count, 0);
    assert_eq!(summary.signed_route_len_delta_sum, 0.0);
    assert_eq!(summary.sum_abs_route_len_delta, 0.0);
    assert_eq!(summary.median_abs_route_len_delta, 0.0);
    assert_eq!(summary.p75_abs_route_len_delta, 0.0);
    assert_eq!(summary.max_abs_route_len_delta, 0.0);
    assert_eq!(summary.abs_bend_count_delta_sum, 0);
    assert_eq!(summary.port_face_change_count, 0);
    assert_eq!(summary.endpoint_face_change_count, 0);
    assert_eq!(summary.layout_amplified_path_count, 0);
}

#[test]
fn route_output_magnitude_collects_m14_changed_paths_and_additive_classes() {
    let pair = mutations::pair_by_id("M14").expect("M14 pair");
    let output =
        proportionality::route_output_magnitude_for_pair(pair).expect("M14 output magnitude");

    assert_eq!(output.summary.changed_path_count, 15);
    assert_eq!(output.paths.len(), 15);
    assert!(output.summary.sum_abs_route_len_delta > 0.0);
    assert!(
        output.summary.sum_abs_route_len_delta >= output.summary.signed_route_len_delta_sum.abs()
    );
    assert_eq!(
        output.decision_edge_count(route_diagnostics::RouteDecisionClass::PathShape),
        15
    );
}

#[test]
fn route_output_magnitude_collects_m11_without_label_lane_class() {
    let pair = mutations::pair_by_id("M11").expect("M11 pair");
    let output =
        proportionality::route_output_magnitude_for_pair(pair).expect("M11 output magnitude");

    assert_eq!(output.summary.changed_path_count, 7);
    assert!(output.summary.port_face_change_count > 0);
    assert_eq!(
        output.decision_edge_count(route_diagnostics::RouteDecisionClass::LabelLane),
        0
    );
}

#[test]
fn proportionality_summary_reports_ratio_without_classification() {
    let input = proportionality::InputMagnitude {
        label_dimension: Some(proportionality::LabelDimensionMagnitude {
            changed_labels: 1,
            max_axis_delta: 20.0,
            total_axis_delta: 20.0,
        }),
        edge_insertion: None,
        node_insertion: None,
        subgraph_change: None,
    };
    let output =
        proportionality::RouteOutputMagnitudeSummary::from_abs_lengths(vec![10.0, 20.0, 30.0]);

    let summary = proportionality::ProportionalitySummary::from_parts(
        "M14",
        proportionality::InputMagnitudeBasis::LabelDimensionUnits,
        input,
        output,
    )
    .expect("summary");

    assert_eq!(summary.pair_id, "M14");
    assert_eq!(
        summary.basis,
        proportionality::InputMagnitudeBasis::LabelDimensionUnits
    );
    assert_eq!(
        summary
            .input
            .label_dimension
            .as_ref()
            .expect("label magnitude")
            .total_axis_delta,
        20.0
    );
    assert_eq!(summary.output.changed_path_count, 3);
    assert_eq!(summary.aggregate_ratio, Some(3.0));
    assert_eq!(summary.max_path_ratio, Some(1.5));
    assert_eq!(summary.classification, None);
}

#[test]
fn calibration_profile_is_explicit_before_classification() {
    let calibration = proportionality::CalibrationProfile::seeded();

    assert_eq!(calibration.label_ratio_proportionate_max, 2.0);
    assert_eq!(
        calibration.source,
        proportionality::CalibrationSource::Seeded
    );
    assert!(
        calibration.control_pair_ids.is_empty(),
        "control data is added by a later task"
    );
    assert_eq!(calibration.label_ratio_disproportionate_min, 3.0);
    assert_eq!(calibration.event_control_p75_abs_route_len_delta, None);
    let control_derived = proportionality::CalibrationSource::ControlDerived;
    assert_ne!(control_derived, calibration.source);
}

#[test]
fn proportionality_class_vocabulary_is_not_applied_without_classification() {
    let classes = [
        proportionality::ProportionalityClass::Proportionate,
        proportionality::ProportionalityClass::Disproportionate,
        proportionality::ProportionalityClass::Mixed,
    ];

    assert_eq!(classes.len(), 3);
}

#[test]
fn control_calibration_status_reports_inconsistent_controls() {
    let consistent = proportionality::control_calibration_status(&[
        proportionality::ControlCalibrationPoint {
            pair_id: "M02",
            p75_abs_route_len_delta: 20.0,
        },
        proportionality::ControlCalibrationPoint {
            pair_id: "M04",
            p75_abs_route_len_delta: 40.0,
        },
    ]);
    assert_eq!(
        consistent,
        proportionality::ControlCalibrationStatus::Consistent
    );

    let inconsistent = proportionality::control_calibration_status(&[
        proportionality::ControlCalibrationPoint {
            pair_id: "M02",
            p75_abs_route_len_delta: 10.0,
        },
        proportionality::ControlCalibrationPoint {
            pair_id: "M04",
            p75_abs_route_len_delta: 100.0,
        },
    ]);

    match inconsistent {
        proportionality::ControlCalibrationStatus::Inconsistent { reason } => {
            assert!(reason.contains("M02"));
            assert!(reason.contains("M04"));
        }
        proportionality::ControlCalibrationStatus::Consistent => {
            panic!("spread controls should not calibrate cleanly")
        }
    }
}

#[test]
fn proportionality_classification_uses_calibration_profile() {
    let calibration = proportionality::CalibrationProfile::seeded();

    assert_eq!(
        proportionality::classify_label_ratio(1.5, 1.8, &calibration),
        proportionality::ProportionalityClass::Proportionate
    );
    assert_eq!(
        proportionality::classify_label_ratio(2.5, 1.4, &calibration),
        proportionality::ProportionalityClass::Mixed
    );
    assert_eq!(
        proportionality::classify_label_ratio(3.5, 3.0, &calibration),
        proportionality::ProportionalityClass::Disproportionate
    );
}

#[test]
fn route_proportionality_report_classifies_or_data_defers_m11() {
    let report =
        proportionality::run_route_proportionality_report().expect("route proportionality report");

    let m14 = report.summary_for("M14").expect("M14 summary");
    assert_eq!(
        m14.classification,
        Some(proportionality::ProportionalityClass::Disproportionate)
    );
    assert_eq!(m14.classification_reason, None);

    let m11 = report.summary_for("M11").expect("M11 summary");
    assert_eq!(
        m11.basis,
        proportionality::InputMagnitudeBasis::EdgeInsertionContext
    );

    match m11.classification {
        Some(classification) => {
            assert!(matches!(
                classification,
                proportionality::ProportionalityClass::Proportionate
                    | proportionality::ProportionalityClass::Mixed
                    | proportionality::ProportionalityClass::Disproportionate
            ));
            assert_eq!(m11.classification_reason, None);
        }
        None => {
            let reason = m11
                .classification_reason
                .as_deref()
                .expect("data-driven deferral reason");
            assert_ne!(
                reason,
                "pure-edge-insertion controls are insufficient for honest M11 classification"
            );
            assert!(
                reason.contains("pure-edge")
                    || reason.contains("cluster")
                    || reason.contains("sub-band"),
                "{reason}"
            );
        }
    }
}

#[test]
fn route_proportionality_report_uses_explicit_event_control_families() {
    assert_eq!(
        proportionality::node_plus_edge_control_pair_ids(),
        ["M02", "M03"]
    );
    assert_eq!(
        proportionality::pure_edge_control_pair_ids(),
        ["M04", "M19", "M20", "M21"]
    );

    let report =
        proportionality::run_route_proportionality_report().expect("route proportionality report");

    for id in ["M02", "M03", "M04", "M19", "M20", "M21", "M14", "M11"] {
        assert!(report.summary_for(id).is_some(), "{id} missing from report");
    }

    assert!(!proportionality::pure_edge_control_pair_ids().contains(&"M11"));
    assert!(!proportionality::node_plus_edge_control_pair_ids().contains(&"M04"));
}

#[test]
fn pure_edge_cluster_summary_reports_consistent_and_split_spreads() {
    let consistent = proportionality::pure_edge_cluster_status(&[
        proportionality::ControlCalibrationPoint {
            pair_id: "M04",
            p75_abs_route_len_delta: 10.0,
        },
        proportionality::ControlCalibrationPoint {
            pair_id: "M19",
            p75_abs_route_len_delta: 20.0,
        },
        proportionality::ControlCalibrationPoint {
            pair_id: "M20",
            p75_abs_route_len_delta: 30.0,
        },
    ]);
    assert_eq!(
        consistent,
        proportionality::PureEdgeClusterStatus::OneCluster
    );

    let split = proportionality::pure_edge_cluster_status(&[
        proportionality::ControlCalibrationPoint {
            pair_id: "M04",
            p75_abs_route_len_delta: 10.0,
        },
        proportionality::ControlCalibrationPoint {
            pair_id: "M21",
            p75_abs_route_len_delta: 80.0,
        },
    ]);
    assert!(matches!(
        split,
        proportionality::PureEdgeClusterStatus::Split { .. }
    ));
}

#[test]
fn route_proportionality_report_exposes_pure_edge_cluster_summary() {
    let report =
        proportionality::run_route_proportionality_report().expect("route proportionality report");
    let cluster = report
        .pure_edge_cluster
        .as_ref()
        .expect("pure-edge cluster");

    assert_eq!(cluster.control_pair_ids, vec!["M04", "M19", "M20", "M21"]);
    assert!(cluster.p75_min > 0.0);
    assert!(cluster.p75_max >= cluster.p75_min);
    assert!(cluster.spread_ratio.unwrap_or_default() >= 1.0);
}

#[test]
fn plan_0166_guardrails_preserve_prior_calibration_outputs() {
    let report =
        proportionality::run_route_proportionality_report().expect("route proportionality report");

    let m14 = report.summary_for("M14").expect("M14 summary");
    assert_eq!(
        m14.classification,
        Some(proportionality::ProportionalityClass::Disproportionate)
    );
    assert!((m14.aggregate_ratio.expect("M14 aggregate ratio") - 4.5881).abs() < 0.25);
    assert_eq!(m14.additive_path_shape_count, Some(15));
    assert_eq!(m14.label_geometry_overlap_count, Some(6));

    for (id, expected_p75) in [("M02", 78.13), ("M03", 78.81), ("M04", 13.37)] {
        let summary = report.summary_for(id).expect("control summary");
        assert!(
            (summary.output.p75_abs_route_len_delta - expected_p75).abs() < 1.0,
            "{id} p75 changed: {}",
            summary.output.p75_abs_route_len_delta
        );
    }

    for id in ["M05", "M07", "S02", "S03"] {
        let pair = mutations::pair_by_id(id).expect("guardrail pair");
        let pair_report = report::run_pair(pair).expect("guardrail report");
        assert_eq!(pair_report.routed.changed_path_geometry_count(), 0, "{id}");
    }
}

#[test]
fn tier_a_surface_includes_plan_0166_pure_edge_controls() {
    let expected_ids = [
        "M01", "M02", "M03", "M04", "M05", "M06", "M07", "M08", "M09", "M10", "M11", "M12", "M14",
        "M15", "M05-ID", "M19", "M20", "M21", "S01", "S02", "S03", "S04",
    ];

    let corpus = mutations::tier_a_pairs();
    assert_eq!(corpus.len(), expected_ids.len());
    for id in expected_ids {
        assert!(corpus.iter().any(|pair| pair.id == id), "missing {id}");
    }
}

#[test]
fn route_proportionality_guardrails_preserve_prior_diagnostics() {
    let reports = report::run_corpus(mutations::tier_a_pairs()).expect("tier a reports");

    for pair_id in ["M05", "M07", "S02", "S03"] {
        let report = reports
            .iter()
            .find(|report| report.pair_id == pair_id)
            .expect("guardrail report");
        assert_eq!(
            report.routed.changed_path_geometry_count(),
            0,
            "{pair_id} should remain a zero-path-churn guardrail"
        );
    }

    let m14 = route_diagnostics::run_pair_route_diagnostics(
        mutations::pair_by_id("M14").expect("M14 pair"),
    )
    .expect("M14 route diagnostics");
    assert_eq!(
        m14.route_decisions.primary_histogram()[&route_diagnostics::RouteDecisionClass::LabelLane],
        6
    );
    assert_eq!(
        m14.route_decisions
            .edge_ids_for_class(route_diagnostics::RouteDecisionClass::PathShape)
            .len(),
        15
    );
    assert_eq!(
        m14.path_shape_correlation
            .as_ref()
            .expect("M14 path-shape correlation")
            .overlap_count,
        6
    );

    let m11 = route_diagnostics::run_pair_route_diagnostics(
        mutations::pair_by_id("M11").expect("M11 pair"),
    )
    .expect("M11 route diagnostics");
    assert_eq!(
        m11.route_decisions
            .edge_ids_for_class(route_diagnostics::RouteDecisionClass::LabelLane)
            .len(),
        0
    );

    let proportionality =
        proportionality::run_route_proportionality_report().expect("route proportionality report");
    assert!(proportionality.summary_for("M14").is_some());
    assert!(proportionality.summary_for("M11").is_some());
}

#[test]
#[ignore]
fn print_route_proportionality_diagnostics() {
    let report =
        proportionality::run_route_proportionality_report().expect("route proportionality report");
    println!("{}", report.summary());
}

#[test]
fn user_node_order_change_sets_first_divergence_to_order() {
    let before = synthetic_trace_with_user_node("A", 1, 0, 10.0, 20.0);
    let after = synthetic_trace_with_user_node("A", 1, 2, 10.0, 20.0);

    let attribution = phase_attribution::compare_phase_traces(&before, &after);

    assert_eq!(attribution.first_divergence(), Some(PhaseName::Order));
    assert_eq!(attribution.order.changed_nodes, vec!["A"]);
    assert!(attribution.rank.changed_nodes.is_empty());
}

#[test]
fn user_node_rank_change_precedes_order_and_position() {
    let before = synthetic_trace_with_user_node("A", 1, 0, 10.0, 20.0);
    let after = synthetic_trace_with_user_node("A", 2, 3, 40.0, 80.0);

    let attribution = phase_attribution::compare_phase_traces(&before, &after);

    assert_eq!(attribution.first_divergence(), Some(PhaseName::Rank));
    assert_eq!(attribution.rank.changed_nodes, vec!["A"]);
    assert_eq!(attribution.order.changed_nodes, vec!["A"]);
    assert_eq!(attribution.position.changed_nodes, vec!["A"]);
}

#[test]
fn generated_dummy_id_change_does_not_count_as_normalize_churn() {
    let before = synthetic_trace_with_dummy("_d1", edge_dummy_key(3, 0), 1, 0);
    let after = synthetic_trace_with_dummy("_d8", edge_dummy_key(3, 0), 1, 0);

    let attribution = phase_attribution::compare_phase_traces(&before, &after);

    assert!(attribution.normalize.changed_dummy_chains.is_empty());
    assert_ne!(before.generated_dummy_ids(), after.generated_dummy_ids());
}

#[test]
fn label_dummy_rank_change_counts_as_normalize_churn() {
    let key = label_dummy_key(14, 1);
    let before = synthetic_trace_with_dummy("_ld1", key.clone(), 2, 0);
    let after = synthetic_trace_with_dummy("_ld9", key, 3, 0);

    let attribution = phase_attribution::compare_phase_traces(&before, &after);

    assert_eq!(attribution.first_divergence(), Some(PhaseName::Normalize));
    assert_eq!(attribution.normalize.changed_dummy_chains.len(), 1);
}

#[test]
fn changed_path_with_stable_endpoints_is_route_phase_churn() {
    let path_metric = changed_path_metric("e1", "A", "B");
    let attribution = attribution_with_no_user_node_movement();

    let route_delta = phase_attribution::attribute_route_metric(&path_metric, &attribution);

    assert_eq!(route_delta.phase, PhaseName::Route);
    assert!(!route_delta.endpoint_layout_changed);
}

#[test]
fn changed_path_with_moved_endpoint_is_layout_amplified_route_churn() {
    let path_metric = changed_path_metric("e1", "A", "B");
    let attribution = attribution_with_position_change("A");

    let route_delta = phase_attribution::attribute_route_metric(&path_metric, &attribution);

    assert_eq!(route_delta.phase, PhaseName::Route);
    assert!(route_delta.endpoint_layout_changed);
}

#[test]
fn m06_report_includes_phase_attribution() {
    let report = report::run_pair(mutations::pair_by_id("M06").unwrap()).unwrap();
    let summary = report.summary();

    assert!(summary.contains("first_divergence="));
    assert!(summary.contains("phase_components="));
    assert!(summary.contains("paths_changed=1"));
}

#[test]
fn m14_and_m11_reports_include_route_attribution() {
    for id in ["M14", "M11"] {
        let report = report::run_pair(mutations::pair_by_id(id).unwrap()).unwrap();
        let summary = report.summary();

        assert!(summary.contains("first_divergence="), "{id}: {summary}");
        assert!(summary.contains("Route"), "{id}: {summary}");
    }
}

#[test]
fn primary_phase_attribution_canaries_are_decomposed() {
    for id in ["M14", "M11", "M02", "M03", "M04", "M06"] {
        let report = report::run_pair(mutations::pair_by_id(id).unwrap()).unwrap();
        let attribution = report.phase_attribution();

        assert!(
            attribution.first_divergence().is_some(),
            "{id} should have a first divergence: {}",
            report.summary()
        );
        assert!(
            !attribution.phase_components_for_test().is_empty(),
            "{id} should report phase components: {}",
            report.summary()
        );
    }
}

#[test]
fn calibration_guardrails_stay_calibrated_with_phase_attribution() {
    let m05 = report::run_pair(mutations::pair_by_id("M05").unwrap()).unwrap();
    let m07 = report::run_pair(mutations::pair_by_id("M07").unwrap()).unwrap();
    let s02 = report::run_pair(mutations::pair_by_id("S02").unwrap()).unwrap();
    let s03 = report::run_pair(mutations::pair_by_id("S03").unwrap()).unwrap();

    assert_eq!(m05.routed.changed_path_geometry_count(), 0);
    assert!(
        m05.churn_explanation()
            .is_label_only_without_route_geometry()
    );
    assert!(
        m07.churn_explanation()
            .is_style_only_without_route_geometry()
    );
    assert_eq!(s02.routed.changed_path_geometry_count(), 0);
    assert!(
        s03.churn_explanation()
            .is_style_only_without_route_geometry()
    );
}

#[test]
fn route_signature_report_counts_unique_outputs() {
    let report = route_diagnostics::determinism_report_from_signatures(vec![
        "same".to_string(),
        "same".to_string(),
        "different".to_string(),
    ]);

    assert_eq!(report.run_count, 3);
    assert_eq!(report.unique_signature_count, 2);
    assert!(!report.is_deterministic());
}

#[test]
fn primary_canary_determinism_diagnostic_runs_requested_iterations() {
    for id in ["M14", "M11"] {
        let pair = mutations::pair_by_id(id).unwrap();
        let report = route_diagnostics::measure_pair_determinism(pair, 10).unwrap();

        assert_eq!(report.run_count, 10, "{id}: {report:?}");
        assert!(report.unique_signature_count >= 1, "{id}: {report:?}");
    }
}

#[test]
fn routing_determinism_summary_mentions_primary_canaries() {
    let summary = route_diagnostics::determinism_summary_for_pair_ids(&["M14", "M11"], 3).unwrap();

    assert!(summary.contains("M14"), "{summary}");
    assert!(summary.contains("M11"), "{summary}");
    assert!(summary.contains("route_signature_variants="), "{summary}");
    assert!(summary.contains("route_deterministic="), "{summary}");
}

#[test]
#[ignore]
fn print_routing_stability_diagnostics() {
    let summary = route_diagnostics::determinism_summary_for_pair_ids(&["M14", "M11"], 10).unwrap();
    println!("{summary}");
}

#[test]
#[ignore]
fn print_route_input_and_decision_diagnostics() {
    for id in ["M14", "M11"] {
        let pair = mutations::pair_by_id(id).unwrap();
        let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();
        println!("{}", diagnostic.summary());
    }
}

#[test]
#[ignore]
fn print_label_lane_decomposition_diagnostics() {
    let pair = mutations::pair_by_id("M14").unwrap();
    let rendered = render_surfaces::render_pair(pair).unwrap();
    let before = rendered.before.route_trace.label_lanes().unwrap();
    let after = rendered.after.route_trace.label_lanes().unwrap();
    let raw_diff = route_diagnostics::diff_label_lane_snapshots(before, after);
    let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();

    println!("raw_label_lane_diff={raw_diff:?}");
    println!(
        "route_decision_edges={:?}",
        diagnostic.route_decisions.by_edge
    );
    println!("{}", diagnostic.summary());
}

#[test]
fn m14_route_trace_includes_label_lane_snapshot() {
    let pair = mutations::pair_by_id("M14").unwrap();
    let rendered = render_surfaces::render_pair(pair).unwrap();

    let snapshot = rendered
        .after
        .route_trace
        .label_lanes()
        .expect("M14 should capture label-lane stage");

    assert!(!snapshot.edges.is_empty(), "{snapshot:?}");
    assert!(!snapshot.compartments.is_empty(), "{snapshot:?}");
    assert!(!snapshot.subclusters.is_empty(), "{snapshot:?}");
    assert!(
        snapshot
            .edges
            .iter()
            .all(|edge| edge.mmds_edge_id == format!("e{}", edge.edge_index)),
        "{snapshot:?}"
    );
}

#[test]
fn route_input_diff_detects_label_dimension_change() {
    let before = route_diagnostics::synthetic_route_input_with_label(0, 40.0, 20.0);
    let after = route_diagnostics::synthetic_route_input_with_label(0, 80.0, 20.0);

    let diff = route_diagnostics::diff_route_inputs(&before, &after);

    assert_eq!(diff.changed_labels.len(), 1);
    assert_eq!(diff.changed_labels[0].edge_index, 0);
    assert_eq!(diff.changed_labels[0].width_delta, 40.0);
    assert_eq!(diff.changed_labels[0].height_delta, 0.0);
    assert!(diff.changed_labels[0].is_decision_relevant());
}

#[test]
fn route_input_diff_detects_port_intent_change() {
    let before = route_diagnostics::synthetic_route_input_with_source_port(0, "south", 0.25);
    let after = route_diagnostics::synthetic_route_input_with_source_port(0, "south", 0.75);

    let diff = route_diagnostics::diff_route_inputs(&before, &after);

    assert_eq!(diff.changed_ports.len(), 1);
    assert_eq!(diff.changed_ports[0].edge_index, 0);
    assert_eq!(diff.changed_ports[0].before_fraction, Some(0.25));
    assert_eq!(diff.changed_ports[0].after_fraction, Some(0.75));
    assert!(diff.changed_ports[0].is_decision_relevant());
}

#[test]
fn route_input_diff_separates_incidental_label_text_from_decision_inputs() {
    let before =
        route_diagnostics::synthetic_route_input_with_label_text(0, "old label", 80.0, 20.0);
    let after =
        route_diagnostics::synthetic_route_input_with_label_text(0, "new label", 80.0, 20.0);

    let diff = route_diagnostics::diff_route_inputs(&before, &after);

    assert_eq!(diff.incidental_label_changes.len(), 1);
    assert_eq!(diff.decision_input_change_count(), 0);
}

#[test]
fn route_input_diff_ignores_identical_inputs() {
    let before = route_diagnostics::synthetic_route_input_with_label(0, 40.0, 20.0);
    let after = before.clone();

    let diff = route_diagnostics::diff_route_inputs(&before, &after);

    assert!(!diff.has_changes(), "{diff:?}");
}

#[test]
fn label_lane_diff_classifies_compartment_before_track() {
    let before = route_diagnostics::synthetic_label_lane_snapshot("e1")
        .with_compartment("c-old")
        .with_track(0);
    let after = route_diagnostics::synthetic_label_lane_snapshot("e1")
        .with_compartment("c-new")
        .with_track(1);

    let diff = route_diagnostics::diff_label_lane_snapshots(&before, &after);

    assert_eq!(
        diff.edge_deltas["e1"].class,
        route_diagnostics::LabelLaneDeltaClass::CompartmentMembership
    );
}

#[test]
fn label_lane_diff_classifies_label_step_only_after_stable_track() {
    let before = route_diagnostics::synthetic_label_lane_snapshot("e1").with_label_step(32.0);
    let after = route_diagnostics::synthetic_label_lane_snapshot("e1").with_label_step(40.0);

    let diff = route_diagnostics::diff_label_lane_snapshots(&before, &after);

    assert_eq!(
        diff.edge_deltas["e1"].class,
        route_diagnostics::LabelLaneDeltaClass::LabelStepOnly
    );
}

#[test]
fn m14_label_lane_decomposition_reports_geometry_only_changes() {
    let pair = mutations::pair_by_id("M14").unwrap();
    let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();
    let decomposition = diagnostic
        .label_lane_decomposition
        .as_ref()
        .expect("M14 label-lane decomposition");

    assert_eq!(decomposition.total_edges(), 6, "{decomposition:?}");
    assert!(
        decomposition
            .edge_deltas
            .keys()
            .all(|edge_id| edge_id.starts_with('e')),
        "{decomposition:?}"
    );
    assert_eq!(decomposition.histogram_total(), 6, "{decomposition:?}");
    assert_eq!(
        decomposition.histogram()[&route_diagnostics::LabelLaneDeltaClass::LabelGeometryOnly],
        6,
        "{decomposition:?}"
    );
    assert_eq!(decomposition.structural_total(), 0, "{decomposition:?}");
}

#[test]
fn m14_route_input_diff_reports_label_input_change() {
    let pair = mutations::pair_by_id("M14").unwrap();
    let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();

    assert!(
        !diagnostic.route_input_diff.changed_labels.is_empty(),
        "{diagnostic:?}"
    );
    assert_eq!(
        diagnostic.phase_attribution.first_divergence(),
        Some(PhaseName::Route)
    );
}

#[test]
fn m11_route_input_diff_reports_added_route_edge_context() {
    let pair = mutations::pair_by_id("M11").unwrap();
    let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();

    assert!(
        !diagnostic.route_input_diff.added_edges.is_empty(),
        "{diagnostic:?}"
    );
    assert_eq!(
        diagnostic.phase_attribution.first_divergence(),
        Some(PhaseName::Route)
    );
}

#[test]
fn m11_label_lane_decomposition_is_not_applicable() {
    let pair = mutations::pair_by_id("M11").unwrap();
    let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();

    assert_eq!(
        diagnostic.label_lane_non_applicability_reason.as_deref(),
        Some("M11 has no LabelLane route decision edges")
    );
}

#[test]
fn plan_0164_guardrails_preserve_0162_and_0163_calibration() {
    for id in ["M14", "M11"] {
        let pair = mutations::pair_by_id(id).unwrap();
        let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();
        assert_eq!(
            diagnostic.phase_attribution.first_divergence(),
            Some(PhaseName::Route)
        );
    }

    for id in ["M02", "M03", "M04"] {
        let report = report::run_pair(mutations::pair_by_id(id).unwrap()).unwrap();
        assert!(
            !report
                .phase_attribution()
                .route
                .layout_amplified_paths
                .is_empty()
        );
    }

    for id in ["M05", "M07", "S02", "S03"] {
        let report = report::run_pair(mutations::pair_by_id(id).unwrap()).unwrap();
        assert_eq!(report.routed.changed_path_geometry_count(), 0);
    }
}

#[test]
fn route_input_summary_does_not_claim_all_inputs_are_identical() {
    let pair = mutations::pair_by_id("M14").unwrap();
    let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();
    let summary = diagnostic.summary();

    assert!(summary.contains("route_input_diffs="), "{summary}");
    assert!(summary.contains("decision_input_changes="), "{summary}");
    assert!(
        !summary.contains("upstream_layout_is_identical"),
        "{summary}"
    );
}

#[test]
fn port_change_classifies_before_bend_or_length() {
    let input = route_diagnostics::synthetic_route_decision_input()
        .with_changed_port("e0", Endpoint::Source, "south", "east")
        .with_bend_delta("e0", 2)
        .with_length_delta("e0", 25.0);

    let decisions = route_diagnostics::classify_route_decisions(input);

    assert_eq!(
        decisions.by_edge["e0"].primary_class,
        RouteDecisionClass::Port
    );
    assert!(
        decisions.by_edge["e0"]
            .classes
            .contains(&RouteDecisionClass::BendTopology)
    );
}

#[test]
fn label_track_change_classifies_as_label_lane() {
    let input =
        route_diagnostics::synthetic_route_decision_input().with_label_track_delta("e0", 0, 1);

    let decisions = route_diagnostics::classify_route_decisions(input);

    assert_eq!(
        decisions.by_edge["e0"].primary_class,
        RouteDecisionClass::LabelLane
    );
}

#[test]
fn length_only_change_classifies_as_length_only() {
    let input = route_diagnostics::synthetic_route_decision_input().with_length_delta("e0", 12.5);

    let decisions = route_diagnostics::classify_route_decisions(input);

    assert_eq!(
        decisions.by_edge["e0"].primary_class,
        RouteDecisionClass::LengthOnly
    );
}

#[test]
fn route_decision_edge_sets_use_additive_classes() {
    let input = route_diagnostics::synthetic_route_decision_input()
        .with_label_track_delta("e0", 0, 1)
        .with_path_shape_changed("e0");

    let decisions = route_diagnostics::classify_route_decisions(input);

    assert!(
        decisions
            .edge_ids_for_class(RouteDecisionClass::LabelLane)
            .contains("e0")
    );
    assert!(
        decisions
            .edge_ids_for_class(RouteDecisionClass::PathShape)
            .contains("e0")
    );
}

#[test]
fn path_shape_correlation_thresholds_are_count_based() {
    assert_eq!(
        route_diagnostics::classify_path_shape_correlation(5),
        route_diagnostics::PathShapeCorrelationOutcome::MostlyOverlapsLabelGeometry
    );
    assert_eq!(
        route_diagnostics::classify_path_shape_correlation(1),
        route_diagnostics::PathShapeCorrelationOutcome::MostlyDisjointFromLabelGeometry
    );
    assert_eq!(
        route_diagnostics::classify_path_shape_correlation(3),
        route_diagnostics::PathShapeCorrelationOutcome::Mixed
    );
}

#[test]
fn m14_path_shape_correlation_reports_counts_and_outcome() {
    let pair = mutations::pair_by_id("M14").unwrap();
    let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();
    let correlation = diagnostic
        .path_shape_correlation
        .as_ref()
        .expect("M14 path-shape correlation");

    assert_eq!(correlation.label_geometry_count, 6, "{correlation:?}");
    assert_eq!(correlation.path_shape_count, 15, "{correlation:?}");
    assert!(correlation.overlap_count <= 6, "{correlation:?}");
    assert_eq!(
        correlation.outcome,
        route_diagnostics::PathShapeCorrelationOutcome::MostlyOverlapsLabelGeometry,
        "{correlation:?}"
    );
}

#[test]
fn m14_and_m11_route_decision_histograms_cover_changed_paths() {
    for id in ["M14", "M11"] {
        let pair = mutations::pair_by_id(id).unwrap();
        let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();

        let changed = diagnostic.phase_attribution.route.changed_paths.len();
        let histogram_total = diagnostic.route_decisions.primary_histogram_total();

        assert_eq!(histogram_total, changed, "{id}: {diagnostic:?}");
        assert!(changed > 0, "{id}: {diagnostic:?}");
    }
}

#[test]
fn route_decision_summary_mentions_histogram_classes() {
    let pair = mutations::pair_by_id("M14").unwrap();
    let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();
    let summary = diagnostic.summary();

    assert!(summary.contains("route_decisions="), "{summary}");
    assert!(summary.contains("decision_input_changes="), "{summary}");
    assert!(
        summary.contains("Port")
            || summary.contains("LabelLane")
            || summary.contains("BendTopology")
            || summary.contains("PathShape")
            || summary.contains("LengthOnly"),
        "{summary}"
    );
}

#[test]
fn primary_route_canaries_keep_route_first_attribution_with_route_diagnostics() {
    for id in ["M14", "M11"] {
        let pair = mutations::pair_by_id(id).unwrap();
        let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair).unwrap();

        assert_eq!(
            diagnostic.phase_attribution.first_divergence(),
            Some(PhaseName::Route),
            "{id}: {}",
            diagnostic.summary()
        );
        assert!(
            diagnostic
                .phase_attribution
                .route
                .layout_amplified_paths
                .is_empty(),
            "{id}: {}",
            diagnostic.summary()
        );
    }
}

#[test]
fn secondary_localized_canaries_keep_layout_amplified_classification() {
    for id in ["M02", "M03", "M04"] {
        let report = report::run_pair(mutations::pair_by_id(id).unwrap()).unwrap();

        assert!(
            !report
                .phase_attribution()
                .route
                .layout_amplified_paths
                .is_empty(),
            "{id}: {}",
            report.summary()
        );
    }
}

#[test]
fn calibration_guardrails_keep_zero_path_churn_where_expected() {
    for id in ["M05", "M07", "S02", "S03"] {
        let report = report::run_pair(mutations::pair_by_id(id).unwrap()).unwrap();

        assert_eq!(
            report.routed.changed_path_geometry_count(),
            0,
            "{id}: {}",
            report.summary()
        );
    }
}

fn synthetic_trace_with_user_node(
    id: &str,
    rank: i32,
    order: usize,
    x: f64,
    y: f64,
) -> LayeredPhaseTrace {
    let node = TraceNodeSnapshot {
        id: id.to_string(),
        rank,
        order,
        x: Some(x),
        y: Some(y),
    };
    let mut trace = LayeredPhaseTrace::default();
    trace.push_stage(TraceStageSnapshot {
        stage: TraceStage::Rank,
        nodes: vec![node.clone()],
        dummies: Vec::new(),
        reversed_edges: Vec::new(),
    });
    trace.push_stage(TraceStageSnapshot {
        stage: TraceStage::Order,
        nodes: vec![node.clone()],
        dummies: Vec::new(),
        reversed_edges: Vec::new(),
    });
    trace.push_stage(TraceStageSnapshot {
        stage: TraceStage::Position,
        nodes: vec![node],
        dummies: Vec::new(),
        reversed_edges: Vec::new(),
    });
    trace
}

fn synthetic_trace_with_dummy(
    generated_id: &str,
    key: DummyTraceKey,
    rank: i32,
    order: usize,
) -> LayeredPhaseTrace {
    let mut trace = LayeredPhaseTrace::default();
    trace.push_stage(TraceStageSnapshot {
        stage: TraceStage::Normalize,
        nodes: Vec::new(),
        dummies: vec![TraceDummySnapshot {
            generated_id: generated_id.to_string(),
            key,
            rank,
            order,
        }],
        reversed_edges: Vec::new(),
    });
    trace
}

fn edge_dummy_key(edge_index: usize, chain_position: usize) -> DummyTraceKey {
    DummyTraceKey {
        edge_index,
        role: DummyTraceRole::Edge,
        chain_position,
        is_label_dummy: false,
    }
}

fn label_dummy_key(edge_index: usize, chain_position: usize) -> DummyTraceKey {
    DummyTraceKey {
        edge_index,
        role: DummyTraceRole::EdgeLabel,
        chain_position,
        is_label_dummy: true,
    }
}

fn changed_path_metric(edge_id: &str, source: &str, target: &str) -> mmds_metrics::EdgePathMetric {
    mmds_metrics::EdgePathMetric {
        edge_id: edge_id.to_string(),
        source: source.to_string(),
        target: target.to_string(),
        point_count_delta: 0,
        bend_count_delta: 0,
        length_delta: mmds_metrics::DISPLAY_EPS + 1.0,
        envelope_delta: mmds_metrics::BoundsDelta {
            width_delta: 0.0,
            height_delta: 0.0,
            area_delta: 0.0,
            changed: false,
        },
    }
}

fn attribution_with_no_user_node_movement() -> phase_attribution::PhaseAttribution {
    phase_attribution::PhaseAttribution::default()
}

fn attribution_with_position_change(node_id: &str) -> phase_attribution::PhaseAttribution {
    phase_attribution::PhaseAttribution {
        position: phase_attribution::NodePhaseDelta {
            changed_nodes: vec![node_id.to_string()],
        },
        ..phase_attribution::PhaseAttribution::default()
    }
}

#[test]
fn mutation_corpus_exposes_seed_pairs_and_identity_policy() {
    let corpus = mutations::tier_a_pairs();

    assert!(corpus.iter().any(|pair| pair.id == "M01"));
    assert!(corpus.iter().any(|pair| pair.id == "M06"));
    assert!(corpus.iter().any(|pair| pair.id == "M10"));
    assert!(corpus.iter().any(|pair| pair.id == "M14"));
    assert!(corpus.iter().any(|pair| pair.id == "M15"));
    assert!(corpus.iter().any(|pair| pair.id == "M05-ID"));
    assert!(corpus.iter().any(|pair| pair.id == "S01"));

    let m01 = corpus.iter().find(|pair| pair.id == "M01").unwrap();
    assert!(
        m01.expected_changes
            .contains(&mutations::SemanticChangeKind::EdgeSplit)
    );

    let m05 = corpus.iter().find(|pair| pair.id == "M05").unwrap();
    assert!(
        m05.expected_changes
            .contains(&mutations::SemanticChangeKind::NodeLabelChanged)
    );
    assert_eq!(
        m05.identity_policy,
        mutations::IdentityPolicy::IdsAreCanonical
    );

    let m05_id = corpus.iter().find(|pair| pair.id == "M05-ID").unwrap();
    assert!(
        m05_id
            .expected_changes
            .contains(&mutations::SemanticChangeKind::NodeRemoved)
    );
    assert!(
        m05_id
            .expected_changes
            .contains(&mutations::SemanticChangeKind::NodeAdded)
    );
}

#[test]
fn class_canaries_are_defined_with_the_corpus() {
    let canaries = mutations::class_canaries();

    assert!(canaries.iter().any(|pair| pair.id == "M16"));
    assert!(canaries.iter().any(|pair| pair.id == "M17"));
    assert!(
        canaries
            .iter()
            .all(|pair| pair.family == mutations::DiagramFamily::Class)
    );
}

#[test]
fn sequence_canary_is_excluded_from_graph_family_metrics() {
    let canary = mutations::sequence_exclusion_canary();

    assert_eq!(canary.id, "S-SEQ");
    assert_eq!(canary.family, mutations::DiagramFamily::TimelineExcluded);
    assert!(!canary.include_in_graph_stability_metrics);
}

#[test]
fn mutation_corpus_tier_a_pairs_cover_task_1_1_expected_surface() {
    let corpus = mutations::tier_a_pairs();
    let expected_ids = [
        "M01", "M02", "M03", "M04", "M05", "M06", "M07", "M08", "M09", "M10", "M11", "M12", "M14",
        "M15", "M05-ID", "M19", "M20", "M21", "S01", "S02", "S03", "S04",
    ];

    assert_eq!(corpus.len(), expected_ids.len());
    for id in expected_ids {
        assert!(corpus.iter().any(|pair| pair.id == id), "missing {id}");
        assert_eq!(mutations::pair_by_id(id).map(|pair| pair.id), Some(id));
    }
}

#[test]
fn pure_edge_control_ids_reserve_prior_tier_b_slots_and_are_explicit() {
    let tier_a = mutations::tier_a_pairs();
    let tier_a_ids = tier_a.iter().map(|pair| pair.id).collect::<Vec<_>>();

    assert!(!tier_a_ids.contains(&"M13"));
    assert!(mutations::pair_by_id("M13").is_none());
    assert!(!tier_a_ids.contains(&"M18"));
    assert!(mutations::pair_by_id("M18").is_none());

    assert_eq!(
        proportionality::pure_edge_control_pair_ids(),
        ["M04", "M19", "M20", "M21"]
    );

    for id in ["M19", "M20", "M21"] {
        let pair = mutations::pair_by_id(id).expect("pure-edge control pair");
        assert_eq!(pair.tier, mutations::CorpusTier::TierA);
        assert_eq!(pair.family, mutations::DiagramFamily::Flowchart);
        assert_eq!(
            pair.expected_changes,
            &[mutations::SemanticChangeKind::EdgeAdded]
        );
        assert!(pair.direct_nodes.is_empty());
        assert_eq!(pair.direct_edges.len(), 1);
        assert!(pair.include_in_graph_stability_metrics);
        assert_eq!(
            proportionality::InputMagnitudeBasis::for_pair_id(id),
            proportionality::InputMagnitudeBasis::EdgeInsertionContext
        );
    }

    let edge_added_tier_a_ids = tier_a
        .iter()
        .filter(|pair| {
            pair.expected_changes
                .contains(&mutations::SemanticChangeKind::EdgeAdded)
        })
        .map(|pair| pair.id)
        .collect::<Vec<_>>();

    assert!(edge_added_tier_a_ids.contains(&"M11"));
    assert!(!proportionality::pure_edge_control_pair_ids().contains(&"M11"));
    assert_ne!(
        proportionality::pure_edge_control_pair_ids().to_vec(),
        edge_added_tier_a_ids
    );
}

#[test]
fn pure_edge_input_magnitude_controls_collect_edge_context_without_other_axes() {
    for id in proportionality::pure_edge_control_pair_ids() {
        let pair = mutations::pair_by_id(id).expect("pure-edge control pair");
        let input =
            proportionality::input_magnitude_for_pair(pair).expect("pure-edge input magnitude");
        let edge = input.edge_insertion.expect("edge insertion magnitude");

        assert_eq!(
            edge.semantic_added_edge_ids,
            pair.direct_edges
                .iter()
                .map(|edge| edge.to_string())
                .collect::<Vec<_>>()
        );
        assert!(
            edge.route_input_added_edges > 0,
            "{id} should add route-input edge context"
        );
        assert!(
            input.label_dimension.is_none(),
            "{id} should not be a label mutation"
        );
        assert!(input.node_insertion.is_none(), "{id} should not add nodes");
        assert!(
            input.subgraph_change.is_none(),
            "{id} should not change subgraph semantics"
        );

        let output = proportionality::route_output_magnitude_for_pair(pair)
            .expect("pure-edge output magnitude");
        assert!(
            output.summary.changed_path_count > 0,
            "{id} should produce observable routed output magnitude"
        );
    }
}

#[test]
fn mutation_corpus_class_canaries_are_disjoint_from_tier_a_pairs() {
    let corpus = mutations::tier_a_pairs();
    let canaries = mutations::class_canaries();

    assert!(
        canaries
            .iter()
            .all(|canary| !corpus.iter().any(|pair| pair.id == canary.id))
    );
}

#[test]
fn mutation_corpus_m05_id_direct_nodes_include_removed_and_added_ids() {
    let m05_id = mutations::pair_by_id("M05-ID").unwrap();

    assert_eq!(m05_id.direct_nodes, &["Lint", "Audit"]);
}

#[test]
fn render_surfaces_include_text_svg_layout_and_routed_mmds() {
    let pair = mutations::pair_by_id("M06").expect("M06 pair should exist");
    let rendered = render_surfaces::render_pair(pair).expect("pair should render");

    assert!(!rendered.before.text.is_empty());
    assert!(!rendered.after.text.is_empty());
    assert!(rendered.before.svg.contains("<svg"));
    assert!(rendered.after.svg.contains("<svg"));
    assert_eq!(
        rendered.before.layout_mmds.geometry_level,
        GeometryLevel::Layout
    );
    assert_eq!(
        rendered.before.routed_mmds.geometry_level,
        GeometryLevel::Routed
    );
    assert_eq!(
        rendered.after.layout_mmds.geometry_level,
        GeometryLevel::Layout
    );
    assert_eq!(
        rendered.after.routed_mmds.geometry_level,
        GeometryLevel::Routed
    );
}

#[test]
fn render_surfaces_allow_lossless_routed_mmds_for_style_canaries() {
    let pair = mutations::pair_by_id("S03").expect("S03 pair should exist");
    let rendered = render_surfaces::render_pair(pair).expect("pair should render");
    let (_, lossless) =
        render_surfaces::render_lossless_routed_mmds(&rendered.after.source).unwrap();

    assert_eq!(
        rendered.after.routed_mmds.geometry_level,
        GeometryLevel::Routed
    );
    assert_eq!(lossless.geometry_level, GeometryLevel::Routed);
}

#[test]
fn render_surfaces_error_reports_pair_and_source_context() {
    let pair = mutations::MutationPair {
        id: "MISSING",
        family: mutations::DiagramFamily::Flowchart,
        tier: mutations::CorpusTier::TierA,
        base: mutations::MutationInput::Fixture {
            family: "flowchart",
            name: "missing-fixture.mmd",
        },
        mutated: mutations::MutationInput::Inline("graph TD\n    A --> B\n"),
        expected_changes: &[],
        direct_nodes: &[],
        direct_edges: &[],
        include_in_graph_stability_metrics: true,
        identity_policy: mutations::IdentityPolicy::IdsAreCanonical,
    };
    let message = render_surfaces::render_pair(&pair).unwrap_err().to_string();

    assert!(message.contains("MISSING"));
    assert!(message.contains("before"));
    assert!(message.contains("missing-fixture.mmd"));
}

#[test]
fn layout_metrics_ignore_direct_impact_nodes_and_use_coord_tolerance() {
    let pair = mutations::pair_by_id("S01").expect("S01 pair should exist");
    let rendered = render_surfaces::render_pair(pair).unwrap();
    let report = mmds_metrics::compare_layout_mmds(
        pair,
        &rendered.before.layout_mmds,
        &rendered.after.layout_mmds,
    );

    assert_eq!(report.coord_eps, 0.01);
    assert!(report.direct_impact_nodes.iter().any(|id| *id == "X"));
    assert!(!report.unchanged_node_displacements.contains_key("X"));
    assert!(report.unchanged_node_count > 0);
}

#[test]
fn layout_metrics_treat_id_edits_as_remove_add_not_rename() {
    let pair = mutations::pair_by_id("M05-ID").expect("M05 ID canary should exist");
    let rendered = render_surfaces::render_pair(pair).unwrap();
    let report = mmds_metrics::compare_layout_mmds(
        pair,
        &rendered.before.layout_mmds,
        &rendered.after.layout_mmds,
    );

    assert!(report.removed_nodes.contains("Lint"));
    assert!(report.added_nodes.contains("Audit"));
    assert!(report.inferred_renames.is_empty());
}

#[test]
fn layout_metrics_do_not_collapse_global_shift_when_anchor_set_is_too_small() {
    let pair = mutations::pair_by_id("S01").expect("S01 pair should exist");
    let rendered = render_surfaces::render_pair(pair).unwrap();
    let report = mmds_metrics::compare_layout_mmds(
        pair,
        &rendered.before.layout_mmds,
        &rendered.after.layout_mmds,
    );

    if report.global_shift.anchor_count < 2 {
        assert_eq!(
            report.global_shift.confidence,
            mmds_metrics::ShiftConfidence::Low
        );
        assert!(!report.global_shift.collapsed_movement);
    }
}

#[test]
fn routed_metrics_report_label_rect_for_label_mutation() {
    let pair = mutations::pair_by_id("M06").expect("M06 pair should exist");
    let rendered = render_surfaces::render_pair(pair).unwrap();
    let report = mmds_metrics::compare_routed_mmds(
        pair,
        &rendered.before.routed_mmds,
        &rendered.after.routed_mmds,
    );

    assert!(
        report
            .label_rect_changes
            .iter()
            .any(|change| change.edge_id == "e0")
    );
    assert!(
        report
            .path_metrics
            .iter()
            .any(|metric| metric.edge_id == "e0")
    );
}

#[test]
fn routed_metrics_keep_visible_endpoint_and_port_intent_separate() {
    let pair = mutations::pair_by_id("M10").expect("M10 pair should exist");
    let rendered = render_surfaces::render_pair(pair).unwrap();
    let report = mmds_metrics::compare_routed_mmds(
        pair,
        &rendered.before.routed_mmds,
        &rendered.after.routed_mmds,
    );

    assert!(
        report
            .endpoint_face_changes
            .iter()
            .all(|change| change.source == "path")
    );
    assert!(
        report
            .port_intent_changes
            .iter()
            .all(|change| change.source == "port")
    );
}

#[test]
fn routed_metrics_treat_id_rename_incident_edges_as_removed_added() {
    let pair = mutations::pair_by_id("M05-ID").expect("M05 ID canary should exist");
    let rendered = render_surfaces::render_pair(pair).unwrap();
    let report = mmds_metrics::compare_routed_mmds(
        pair,
        &rendered.before.routed_mmds,
        &rendered.after.routed_mmds,
    );

    assert!(!report.removed_edges.is_empty());
    assert!(!report.added_edges.is_empty());
    assert!(
        report
            .path_metrics
            .iter()
            .all(|metric| !metric.subject_mentions_node("Lint"))
    );
}

#[test]
fn style_only_canaries_compare_paths_without_geometry_changes() {
    let m07 = report::run_pair(mutations::pair_by_id("M07").unwrap()).unwrap();
    let s03 = report::run_pair(mutations::pair_by_id("S03").unwrap()).unwrap();

    assert_eq!(m07.routed.path_metrics.len(), 4);
    assert_eq!(s03.routed.path_metrics.len(), 2);
    assert_eq!(m07.routed.changed_path_geometry_count(), 0);
    assert_eq!(s03.routed.changed_path_geometry_count(), 0);
}

#[test]
fn style_only_canaries_explain_style_without_route_geometry_churn() {
    for id in ["M07", "S03"] {
        let report = report::run_pair(mutations::pair_by_id(id).unwrap()).unwrap();
        let explanation = report.churn_explanation();

        assert!(
            explanation
                .components
                .contains(&report::ChurnComponent::Style)
        );
        assert!(
            !explanation
                .components
                .contains(&report::ChurnComponent::RouteGeometry)
        );
        assert_eq!(report.routed.changed_path_geometry_count(), 0);
    }
}

#[test]
fn m05_label_only_churn_is_attributed_to_label_and_bounds_not_route_geometry() {
    let report = report::run_pair(mutations::pair_by_id("M05").unwrap()).unwrap();
    let explanation = report.churn_explanation();

    assert_eq!(report.routed.path_metrics.len(), 6);
    assert_eq!(report.routed.changed_path_geometry_count(), 0);
    assert!(
        explanation
            .components
            .contains(&report::ChurnComponent::LabelRect)
    );
    assert!(
        explanation
            .components
            .contains(&report::ChurnComponent::Bounds)
    );
    assert!(
        !explanation
            .components
            .contains(&report::ChurnComponent::RouteGeometry)
    );
}

#[test]
fn m14_label_cascade_reports_route_and_port_contributors() {
    let report = report::run_pair(mutations::pair_by_id("M14").unwrap()).unwrap();
    let explanation = report.churn_explanation();

    assert!(report.routed.changed_path_geometry_count() > 0);
    assert!(report.quality.route_length_total_delta.abs() > 1.0);
    assert_ne!(report.quality.bend_count_total_delta, 0);
    assert!(!report.routed.path_port_divergence.is_empty());
    assert!(
        explanation
            .components
            .contains(&report::ChurnComponent::RouteGeometry)
    );
    assert!(
        explanation
            .components
            .contains(&report::ChurnComponent::PortDivergence)
    );
    assert!(
        explanation
            .components
            .contains(&report::ChurnComponent::LabelRect)
    );
}

#[test]
fn mixed_churn_exposes_all_detected_components() {
    let report = report::run_pair(mutations::pair_by_id("M14").unwrap()).unwrap();
    let explanation = report.churn_explanation();

    assert_eq!(report.render_churn, render_metrics::RenderChurnClass::Mixed);
    assert!(explanation.components.len() > 1);
    assert_eq!(
        explanation.components,
        explanation.components_sorted_for_test()
    );
}

#[test]
fn report_summary_distinguishes_compared_paths_from_changed_paths() {
    let report = report::run_pair(mutations::pair_by_id("M07").unwrap()).unwrap();
    let summary = report.summary();

    assert!(summary.contains("paths_compared=4"));
    assert!(summary.contains("paths_changed=0"));
    assert!(summary.contains("components=[Style]"));
}

#[test]
fn h1_h2_canaries_have_calibrated_explanations() {
    let m07 = report::run_pair(mutations::pair_by_id("M07").unwrap()).unwrap();
    let s03 = report::run_pair(mutations::pair_by_id("S03").unwrap()).unwrap();
    let m05 = report::run_pair(mutations::pair_by_id("M05").unwrap()).unwrap();
    let m14 = report::run_pair(mutations::pair_by_id("M14").unwrap()).unwrap();

    assert_eq!(m07.routed.changed_path_geometry_count(), 0);
    assert_eq!(s03.routed.changed_path_geometry_count(), 0);
    assert_eq!(m05.routed.changed_path_geometry_count(), 0);
    assert!(m14.routed.changed_path_geometry_count() > 0);

    assert!(
        m07.churn_explanation()
            .is_style_only_without_route_geometry()
    );
    assert!(
        s03.churn_explanation()
            .is_style_only_without_route_geometry()
    );
    assert!(
        m05.churn_explanation()
            .is_label_only_without_route_geometry()
    );
    assert!(m14.churn_explanation().has_route_geometry_cascade());
}

#[test]
fn geometry_metrics_count_bends_and_route_length() {
    let path = vec![
        point(0.0, 0.0),
        point(10.0, 0.0),
        point(10.0, 5.0),
        point(20.0, 5.0),
    ];

    assert_eq!(geometry_metrics::bend_count(&path), 2);
    assert_eq!(geometry_metrics::polyline_length(&path), 25.0);
}

#[test]
fn geometry_metrics_detect_label_overlap_and_drift() {
    let a = rect(0.0, 0.0, 10.0, 10.0);
    let b = rect(5.0, 5.0, 10.0, 10.0);
    let path = vec![point(0.0, 0.0), point(20.0, 0.0)];

    assert!(geometry_metrics::rects_overlap(a, b));
    assert_eq!(
        geometry_metrics::distance_point_to_path(point(10.0, 5.0), &path),
        5.0
    );
}

fn point(x: f64, y: f64) -> crate::graph::geometry::FPoint {
    crate::graph::geometry::FPoint::new(x, y)
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> crate::graph::geometry::FRect {
    crate::graph::geometry::FRect::new(x, y, width, height)
}

#[test]
fn render_churn_classifier_prioritizes_route_topology_over_bounds_only() {
    let summary = render_metrics::RenderMetricDelta {
        svg_viewbox_changed: true,
        path_topology_changed: true,
        ..Default::default()
    };

    assert_eq!(
        render_metrics::classify_render_churn(&summary),
        render_metrics::RenderChurnClass::RouteTopologyChanged,
    );
}

#[test]
fn render_churn_classifier_marks_text_quantization_when_only_text_size_changes() {
    let summary = render_metrics::RenderMetricDelta {
        text_dimensions_changed: true,
        ..Default::default()
    };

    assert_eq!(
        render_metrics::classify_render_churn(&summary),
        render_metrics::RenderChurnClass::TextGridQuantizationOrGlyphChurn,
    );
}

#[test]
fn render_churn_classifier_marks_mixed_for_multiple_real_changes() {
    let summary = render_metrics::RenderMetricDelta {
        path_topology_changed: true,
        label_rect_changed: true,
        ..Default::default()
    };

    assert_eq!(
        render_metrics::classify_render_churn(&summary),
        render_metrics::RenderChurnClass::Mixed,
    );
}

#[test]
fn render_churn_collectors_extract_text_and_svg_surface_metrics() {
    let text = "A\nwide line\n";
    let svg = r#"<svg viewBox="0 0 120 40"><path d="M0 0L1 1"/><path d="M1 1L2 2"/></svg>"#;

    let text_metrics = render_metrics::collect_text_metrics(text);
    let svg_metrics = render_metrics::collect_svg_metrics(svg);

    assert_eq!(text_metrics.line_count, 2);
    assert_eq!(text_metrics.max_line_width, 9);
    assert_eq!(svg_metrics.viewbox_width, Some(120.0));
    assert_eq!(svg_metrics.viewbox_height, Some(40.0));
    assert_eq!(svg_metrics.path_count, 2);
}

#[test]
fn tier_a_corpus_produces_deterministic_reports() {
    let reports = report::run_corpus(mutations::tier_a_pairs()).expect("corpus should run");
    let class_reports =
        report::run_corpus(mutations::class_canaries()).expect("class canaries should run");

    assert!(
        reports.len() >= 19,
        "Tier A should include M01-M12, M14-M15, M05-ID, and S01-S04"
    );
    assert_eq!(
        class_reports.len(),
        2,
        "M16/M17 class canaries are disjoint from Tier A"
    );
    assert!(reports.iter().any(|report| report.pair_id == "M10"));
    assert!(class_reports.iter().any(|report| report.pair_id == "M16"));
    assert!(class_reports.iter().any(|report| report.pair_id == "M17"));
    assert!(reports.iter().all(|report| !report.summary().is_empty()));
    assert!(
        class_reports
            .iter()
            .all(|report| !report.summary().is_empty())
    );

    let first = report::format_reports(&reports);
    let second = report::format_reports(&reports);
    assert_eq!(first, second);
}

#[test]
fn corpus_reports_have_positive_signal_for_known_mutations() {
    let reports = report::run_corpus(mutations::tier_a_pairs()).expect("corpus should run");

    let m06 = reports
        .iter()
        .find(|report| report.pair_id == "M06")
        .unwrap();
    assert!(
        !m06.routed.label_rect_changes.is_empty(),
        "M06 should exercise label geometry"
    );

    let m10 = reports
        .iter()
        .find(|report| report.pair_id == "M10")
        .unwrap();
    assert!(
        !m10.routed.endpoint_face_changes.is_empty() || m10.routed.routed_bounds_delta.changed,
        "M10 should produce routed endpoint or bounds signal"
    );

    let m15 = reports
        .iter()
        .find(|report| report.pair_id == "M15")
        .unwrap();
    assert!(
        !m15.quality.label_path_drift.is_empty() || !m15.quality.label_rect_overlaps.is_empty(),
        "M15 should exercise label drift or overlap oracle signal"
    );
}

#[test]
fn class_canaries_are_supported_or_explicitly_tier_b() {
    let canaries = mutations::class_canaries();

    assert!(canaries.iter().any(|pair| pair.id == "M16"));
    assert!(canaries.iter().any(|pair| pair.id == "M17"));
    assert!(
        canaries
            .iter()
            .all(|pair| pair.family == mutations::DiagramFamily::Class)
    );
}

#[test]
fn sequence_canary_never_enters_graph_stability_reports() {
    let reports = report::run_corpus(mutations::tier_a_pairs()).expect("corpus should run");

    assert!(reports.iter().all(|report| report.pair_id != "S-SEQ"));
}

#[test]
fn sequence_canary_is_rejected_by_report_guardrails() {
    let result = report::run_pair(mutations::sequence_exclusion_canary());

    assert!(matches!(
        result,
        Err(report::ReportError::ExcludedFamily { .. })
    ));
}
