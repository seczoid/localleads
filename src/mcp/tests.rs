use super::LocalLeadsMcp;
use super::leads::{LeadSelectionParams, enrichment_json};
use super::searches::{LocationSelectionInput, SearchInput, build_config};
use crate::models::{LocationMode, LocationSelection, SearchConfig, SearchMode};
use serde_json::{Value, json};

fn input(value: Value) -> SearchInput {
    serde_json::from_value(value).expect("valid search input")
}

#[test]
fn search_input_maps_onto_config() {
    let config = build_config(
        input(json!({
            "name": "Austin plumbers",
            "queries": ["plumber", " ", "roofer"],
            "mode": "text",
            "location_selection": { "country": "United States", "mode": "states", "states": ["Texas", "Ohio"], "depth": "region" },
            "collect_all": true,
            "concurrency": 8,
            "proxy_group_id": "",
            "filters": ["rating >= 4"],
            "tags": ["hvac"],
            "region": "US",
        })),
        None,
    )
    .expect("valid config");
    assert_eq!(config.job_name, "Austin plumbers");
    assert_eq!(config.queries, vec!["plumber", "roofer"]);
    assert_eq!(config.searches().len(), 4);
    // Like the dashboard, places are worked out from the selection at run time.
    assert!(config.locations.is_empty());
    let selection = config.location_selection.expect("selection");
    assert_eq!(selection.mode, LocationMode::States);
    assert_eq!(selection.locations(), vec!["Texas, United States", "Ohio, United States"]);
    assert!(config.collect_all);
    assert_eq!(config.concurrency, 8);
    assert_eq!(config.proxy_group_id, None);
    assert_eq!(config.region, "us");
    assert_eq!(config.output_fields, SearchConfig::default().output_fields);
}

#[test]
fn custom_locations_and_modes() {
    let config = build_config(
        input(json!({
            "job_name": "urls",
            "queries": ["https://www.google.com/maps/search/cafe"],
            "mode": "maps_url",
            "locations": ["Austin, TX", "Dallas, TX"],
        })),
        None,
    )
    .expect("valid config");
    assert_eq!(config.mode, SearchMode::MapsUrl);
    assert!(config.location_selection.is_none());
    assert_eq!(config.locations, vec!["Austin, TX", "Dallas, TX"]);
}

#[test]
fn search_input_needs_a_location() {
    let error = build_config(input(json!({ "job_name": "x", "queries": ["cafe"] })), None)
        .expect_err("locations are required");
    assert!(error.contains("location_selection"), "{error}");
    let error = build_config(
        input(json!({
            "job_name": "x", "queries": ["cafe"], "locations": ["A"],
            "location_selection": { "mode": "all" },
        })),
        None,
    )
    .expect_err("not both");
    assert!(error.contains("not both"), "{error}");
}

#[test]
fn search_input_reports_validation_errors() {
    let error = build_config(
        input(json!({ "job_name": "x", "queries": ["cafe"], "locations": ["A"], "concurrency": 0 })),
        None,
    )
    .expect_err("concurrency zero");
    assert!(error.contains("parallel") || error.contains("concurrency"), "{error}");
    let error = build_config(
        input(json!({ "job_name": "x", "queries": ["cafe"], "locations": ["A"], "filters": ["secret = 1"] })),
        None,
    )
    .expect_err("bad filter");
    assert!(error.contains("unsupported filter field"), "{error}");
}

#[test]
fn unknown_settings_are_rejected_and_known_ones_pass_through() {
    let error = build_config(
        input(json!({ "job_name": "x", "queries": ["cafe"], "locations": ["A"], "concurrancy": 3 })),
        None,
    )
    .expect_err("typo");
    assert!(error.contains("unknown search setting 'concurrancy'"), "{error}");
    // Any SearchConfig field is accepted by its serde name, even one the
    // typed input doesn't list.
    let config = build_config(
        input(json!({ "job_name": "x", "queries": ["cafe"], "locations": ["A"] })),
        None,
    )
    .expect("valid");
    let Value::Object(fields) = serde_json::to_value(&config).unwrap() else {
        panic!("object");
    };
    assert!(fields.contains_key("job_name"));
}

#[test]
fn copy_from_keeps_base_settings() {
    let base = SearchConfig {
        job_name: "base".to_owned(),
        queries: vec!["dentist".to_owned()],
        locations: vec!["Boise, ID".to_owned()],
        concurrency: 5,
        tags: vec!["t".to_owned()],
        ..SearchConfig::default()
    };
    let config = build_config(input(json!({ "job_name": "copy", "concurrency": 3 })), Some(base))
        .expect("valid");
    assert_eq!(config.job_name, "copy");
    assert_eq!(config.queries, vec!["dentist"]);
    assert_eq!(config.locations, vec!["Boise, ID"]);
    assert_eq!(config.concurrency, 3);
    assert_eq!(config.tags, vec!["t"]);
}

/// Fails when `SearchConfig` gains a field the MCP input doesn't document.
/// The field already works as a pass-through key; add it to `SearchInput`
/// (with a doc comment) so agents can discover it.
#[test]
fn search_input_documents_every_config_field() {
    let schema = serde_json::to_value(schemars::schema_for!(SearchInput)).unwrap();
    let properties = schema["properties"].as_object().expect("properties");
    let Value::Object(config) = serde_json::to_value(SearchConfig::default()).unwrap() else {
        panic!("object");
    };
    let missing: Vec<&String> = config
        .keys()
        .filter(|key| !properties.contains_key(*key))
        .collect();
    assert!(
        missing.is_empty(),
        "SearchConfig fields missing from mcp::searches::SearchInput: {missing:?}"
    );
}

/// Same guard for the location picker's fields.
#[test]
fn location_input_documents_every_selection_field() {
    let schema = serde_json::to_value(schemars::schema_for!(LocationSelectionInput)).unwrap();
    let properties = schema["properties"].as_object().expect("properties");
    let Value::Object(selection) = serde_json::to_value(LocationSelection::default()).unwrap() else {
        panic!("object");
    };
    let missing: Vec<&String> = selection
        .keys()
        .filter(|key| !properties.contains_key(*key))
        .collect();
    assert!(
        missing.is_empty(),
        "LocationSelection fields missing from mcp::searches::LocationSelectionInput: {missing:?}"
    );
}

#[test]
fn depth_defaults_to_zip_like_the_dashboard() {
    let config = build_config(
        input(json!({
            "job_name": "x", "queries": ["cafe"],
            "location_selection": { "mode": "states", "states": ["Vermont"] },
        })),
        None,
    )
    .expect("valid");
    let selection = config.location_selection.clone().expect("selection");
    assert_eq!(serde_json::to_value(&selection).unwrap()["depth"], "zip");
    assert!(config.lookup_count() > 100, "{}", config.lookup_count());
}

#[test]
fn cities_alone_are_a_valid_selection() {
    let config = build_config(
        input(json!({
            "job_name": "x", "queries": ["cafe"],
            "location_selection": { "mode": "states", "cities": [" Louisville, KY ", ""] },
        })),
        None,
    )
    .expect("valid");
    let selection = config.location_selection.clone().expect("selection");
    assert_eq!(selection.cities, vec!["Louisville, KY"]);
    assert!(selection.place_count() > 1);
}

#[test]
fn region_defaults_from_the_country() {
    let germany = build_config(
        input(json!({
            "job_name": "x", "queries": ["cafe"],
            "location_selection": { "country": "Germany", "mode": "all" },
        })),
        None,
    )
    .expect("valid");
    assert_eq!(germany.region, "de");
    let explicit = build_config(
        input(json!({
            "job_name": "x", "queries": ["cafe"], "region": "FR",
            "location_selection": { "country": "Germany", "mode": "all" },
        })),
        None,
    )
    .expect("valid");
    assert_eq!(explicit.region, "fr");
}

#[test]
fn depth_passes_through_to_the_engine() {
    let config = build_config(
        input(json!({
            "job_name": "x", "queries": ["cafe"],
            "location_selection": { "mode": "states", "states": ["Vermont"], "depth": "city" },
        })),
        None,
    )
    .expect("valid");
    let value = serde_json::to_value(config.location_selection.expect("selection")).unwrap();
    assert_eq!(value["depth"], "city");
    assert_eq!(value["country"], "United States");
}

/// Dashboard capability → MCP tool. Keep in step with docs/MCP.md.
#[test]
fn every_dashboard_capability_has_a_tool() {
    let expected = [
        // searches
        "search_options",
        "find_cities",
        "preview_search",
        "create_search",
        "rerun_search",
        "list_searches",
        "get_search",
        "pause_search",
        "resume_search",
        "stop_search",
        "delete_search",
        // leads
        "browse_leads",
        "lead_facets",
        "lead_lists",
        "get_lead",
        "delete_leads",
        "enrich_leads",
        "export_leads",
        // proxies
        "list_proxies",
        "proxy_status",
        "add_proxies",
        "import_proxy_file",
        "check_proxies",
        "cancel_proxy_check",
        "set_proxies_enabled",
        "delete_proxies",
        "prune_proxies",
        "proxy_history",
        "list_proxy_groups",
        "create_proxy_group",
        "rename_proxy_group",
        "delete_proxy_group",
        "assign_proxies_to_group",
        "remove_proxies_from_group",
        "get_checker_settings",
        "update_checker_settings",
        // dashboard and settings
        "overview",
        "app_info",
        "list_notices",
        "backup_database",
    ];
    let mut names = LocalLeadsMcp::tool_names();
    names.sort();
    let mut wanted: Vec<String> = expected.iter().map(|name| (*name).to_owned()).collect();
    wanted.sort();
    assert_eq!(names, wanted);
}

#[test]
fn every_tool_has_a_description_and_object_schema() {
    for tool in LocalLeadsMcp::router().list_all() {
        assert!(
            tool.description.as_ref().is_some_and(|text| text.len() > 20),
            "{} needs a description",
            tool.name
        );
        assert_eq!(
            tool.input_schema.get("type").and_then(Value::as_str),
            Some("object"),
            "{} input schema",
            tool.name
        );
    }
}

#[test]
fn tool_schemas_have_no_nullable_unions() {
    for tool in LocalLeadsMcp::router().list_all() {
        let text = serde_json::to_string(&*tool.input_schema).unwrap();
        assert!(!text.contains("\"null\""), "{} schema still has null: {text}", tool.name);
    }
}

#[test]
fn bulk_lead_actions_need_ids_or_all_matching() {
    let params = |value: Value| -> LeadSelectionParams { serde_json::from_value(value).expect("params") };
    let error = params(json!({ "categories": ["Bakery"] })).query().expect_err("no selection");
    assert!(error.contains("all_matching"), "{error}");
    let query = params(json!({ "ids": [" a ", "", "b"] })).query().expect("ids");
    assert_eq!(query.ids, vec!["a", "b"]);
    let query = params(json!({ "all_matching": true, "has_phone": true, "email": "safe" }))
        .query()
        .expect("all matching");
    assert!(query.ids.is_empty() && query.has_phone);
    assert_eq!(query.email, "safe");
    let many: Vec<String> = (0..1001).map(|index| index.to_string()).collect();
    let error = params(json!({ "ids": many })).query().expect_err("too many ids");
    assert!(error.contains("at most 1000"), "{error}");
}

#[test]
fn enrichment_reads_like_the_lead_popup() {
    use crate::web::views::{EmailView, EnrichView};
    let view = EnrichView {
        status: "done".to_owned(),
        emails: vec![EmailView {
            email: "hi@example.com".to_owned(),
            verdict: "Verified",
            found: "mailto link".to_owned(),
            ..EmailView::default()
        }],
        socials: vec![("Facebook", "https://facebook.com/x".to_owned())],
        per_score: vec![(5, 40, 100), (4, 3, 7)],
        verified: Some(true),
        ..EnrichView::default()
    };
    let value = enrichment_json(&view, Some("2026-11-17T10:00:00Z".to_owned()));
    assert_eq!(value["status"], "done");
    assert_eq!(value["emails"][0]["verdict"], "Verified");
    assert_eq!(value["socials"]["Facebook"], "https://facebook.com/x");
    assert_eq!(value["google_place"]["reviews_per_star"]["5"], 40);
    assert_eq!(value["google_place"]["claimed_by_owner"], true);
    assert!(value.get("people").is_none(), "empty parts are left out: {value}");
    let empty = enrichment_json(&EnrichView::default(), None);
    assert_eq!(empty, json!({ "status": "not enriched" }));
}
