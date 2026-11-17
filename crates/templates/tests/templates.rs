//! The template format, and the templates that come with Display Hub.

use hub_core::api::NewPage;
use hub_core::{Settings, Source};
use hub_templates::{built_in, built_in_template, Template};
use serde_json::{json, Value};
use usage::{NewSubscription, Service};

fn answers(v: Value) -> Settings {
    v.as_object().unwrap().clone()
}

/// The sources the manager offers a usage page today (`apps/hub-manager/src/model.rs`,
/// `sources_for`): the helper reports Claude Max and ChatGPT Plus only.
fn usage_sources(service: Service) -> Vec<Source> {
    [Source::Manual, Source::Helper, Source::Demo, Source::None]
        .into_iter()
        .filter(|s| *s != Source::Helper || matches!(service, Service::ClaudeMax | Service::ChatgptPlus))
        .collect()
}

#[test]
fn the_built_in_templates_are_valid_and_in_order() {
    let ids: Vec<String> = built_in().into_iter().map(|t| t.id).collect();
    assert_eq!(
        ids,
        [
            "usage-limits/1",
            "pi-status/1",
            "big-number/1",
            "short-text/1",
            "list/1",
            "meter/1"
        ]
    );
    assert!(built_in_template("nope/1").is_none());
}

#[test]
fn usage_limits_makes_what_the_manager_makes_today() {
    let t = built_in_template("usage-limits/1").unwrap();
    for service in Service::ALL {
        for source in usage_sources(service) {
            for title in [None, Some("Work Claude")] {
                let mut form = answers(json!({"service": service.slug(), "source": source.slug()}));
                if let Some(title) = title {
                    form.insert("title".into(), json!(title));
                }
                let pages = t.make_pages(&form).unwrap();
                let today = NewSubscription {
                    service,
                    source,
                    title: title.map(String::from),
                    id: None,
                }
                .to_new_page();
                let mut settings = answers(json!({"service": service.slug(), "source": source.slug()}));
                if let Some(title) = title {
                    settings.insert("title".into(), json!(title));
                }
                assert_eq!(
                    pages,
                    [NewPage {
                        template: Some("usage-limits/1".into()),
                        settings: Some(settings),
                        ..today
                    }],
                    "{service:?} {source:?} {title:?}"
                );
            }
        }
    }
}

#[test]
fn usage_limits_offers_the_pc_helper_only_where_something_reports() {
    let t = built_in_template("usage-limits/1").unwrap();
    let source = t.field("source").unwrap();
    for service in Service::ALL {
        let form = answers(json!({"service": service.slug()}));
        let offered: Vec<Source> = source
            .available_options(&form)
            .map(|c| Source::from_slug(&c.value).unwrap())
            .collect();
        assert_eq!(offered, usage_sources(service), "{service:?}");
    }
    let e = t
        .make_pages(&answers(
            json!({"service": "opencode-go-plus", "source": "helper"}),
        ))
        .unwrap_err();
    assert_eq!(e, "Helper isn't available when Service is OpenCode Go Plus.");
    // Not answering takes the defaults, as the manager's form starts.
    let pages = t.make_pages(&Settings::new()).unwrap();
    assert_eq!(
        (pages[0].title.as_str(), pages[0].source, pages[0].feed.as_deref()),
        ("Claude Max", Source::Manual, Some("claude-max"))
    );
    let e = t
        .make_pages(&answers(json!({"title": "x".repeat(49)})))
        .unwrap_err();
    assert_eq!(e, "Title can be at most 48 characters.");
}

#[test]
fn pi_status_is_the_example_helpers_page_and_says_how_to_start_it() {
    let t = built_in_template("pi-status/1").unwrap();
    let pages = t.make_pages(&Settings::new()).unwrap();
    assert_eq!(pages.len(), 1);
    let p = &pages[0];
    assert_eq!(
        (p.title.as_str(), p.kind.as_str(), p.source, p.feed.as_deref()),
        ("Pi status", "big_number", Source::Helper, Some("pi-status"))
    );
    assert_eq!(p.settings, Some(answers(json!({"title": "Pi status"}))));
    // The command is the one the helper and the Pi guide give, and the helper sends this kind.
    let setup = t.setup.clone().unwrap();
    let helper = include_str!("../../../deploy/raspberry-pi/helpers/pi-status");
    assert!(
        helper.contains(&format!("#   {}\n", setup.command)),
        "{}",
        setup.command
    );
    assert!(include_str!("../../../docs/deploy-raspberry-pi.md").contains(&setup.command));
    assert!(helper.contains("--kind big_number"));
    let named = t.make_pages(&answers(json!({"title": " Shed  Pi "}))).unwrap();
    assert_eq!(named[0].title, "Shed Pi");
}

#[test]
fn each_shape_has_a_hand_filled_template() {
    for (id, kind, title) in [
        ("big-number/1", "big_number", "Big number"),
        ("short-text/1", "text", "Note"),
        ("list/1", "list", "List"),
        ("meter/1", "meter", "Meter"),
    ] {
        let t = built_in_template(id).unwrap();
        let pages = t.make_pages(&Settings::new()).unwrap();
        assert_eq!(pages.len(), 1, "{id}");
        assert_eq!(
            (
                pages[0].title.as_str(),
                pages[0].kind.as_str(),
                pages[0].source,
                pages[0].feed.as_deref()
            ),
            (title, kind, Source::Manual, None),
            "{id}"
        );
        // A blank title takes the default; a typed one is kept.
        let blank = t.make_pages(&answers(json!({"title": "   "}))).unwrap();
        assert_eq!(blank[0].title, title, "{id}");
        let typed = t.make_pages(&answers(json!({"title": "Tanks"}))).unwrap();
        assert_eq!(typed[0].title, "Tanks", "{id}");
    }
}

/// A maker's template using every kind of setting, conditions and placeholders.
const GARDEN: &str = r#"{
  "id": "garden/2",
  "name": "Garden",
  "description": "Soil and weather for one bed.",
  "settings": [
    { "key": "bed", "label": "Bed", "type": "choice", "required": true,
      "options": [ { "value": "north", "label": "North bed" }, { "value": "south", "label": "South bed" } ] },
    { "key": "name", "label": "Name", "type": "text", "max_length": 20 },
    { "key": "depth", "label": "Sensor depth", "type": "number", "min": 1, "max": 60, "default": 10 },
    { "key": "shows", "label": "Shows", "type": "multi_choice", "min": 1,
      "default": ["moisture"],
      "options": [
        { "value": "moisture", "label": "Moisture" },
        { "value": "rain", "label": "Rain", "when": { "bed": "south" } }
      ] },
    { "key": "weather", "label": "Show the forecast", "type": "yes_no" }
  ],
  "pages": [
    { "title": "{name|bed.label}", "kind": "meter", "source": "helper", "feed": "garden-{bed}",
      "when": { "shows": "moisture" } },
    { "title": "Rain, {bed.label}", "kind": "big_number", "source": "helper", "feed": "garden-{bed}",
      "when": { "shows": "rain" } },
    { "title": "Forecast", "kind": "text", "source": "manual", "when": { "weather": true } }
  ]
}"#;

#[test]
fn a_makers_template_fills_in_every_kind_of_setting() {
    let t = Template::parse(GARDEN).unwrap();
    let pages = t.make_pages(&answers(json!({"bed": "north"}))).unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(
        (pages[0].title.as_str(), pages[0].feed.as_deref()),
        ("North bed", Some("garden-north"))
    );
    assert_eq!(
        pages[0].settings,
        Some(answers(
            json!({"bed": "north", "depth": 10, "shows": ["moisture"], "weather": false})
        ))
    );

    let pages = t
        .make_pages(&answers(json!({
            "bed": "south", "name": "Veges", "depth": "12.5", "shows": ["rain", "moisture"], "weather": true
        })))
        .unwrap();
    let titles: Vec<&str> = pages.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, ["Veges", "Rain, South bed", "Forecast"]);
    assert_eq!(pages[2].source, Source::Manual);
    assert_eq!(pages[0].settings.as_ref().unwrap()["depth"], json!(12.5));

    for (form, said) in [
        (json!({}), "Choose an option for Bed."),
        (
            json!({"bed": "east"}),
            "\"east\" isn't one of the options for Bed.",
        ),
        (
            json!({"bed": "north", "depth": 0}),
            "Sensor depth needs to be at least 1.",
        ),
        (
            json!({"bed": "north", "depth": 61}),
            "Sensor depth can be at most 60.",
        ),
        (
            json!({"bed": "north", "depth": "deep"}),
            "Sensor depth needs to be a number.",
        ),
        (
            json!({"bed": "north", "name": "x".repeat(21)}),
            "Name can be at most 20 characters.",
        ),
        // Too long by a little, with a space where a cut would land: still refused, not shortened.
        (
            json!({"bed": "north", "name": format!("{} yy", "x".repeat(19))}),
            "Name can be at most 20 characters.",
        ),
        (
            json!({"bed": "north", "shows": []}),
            "Choose at least 1 option for Shows.",
        ),
        (
            json!({"bed": "north", "shows": ["moisture", "moisture"]}),
            "Moisture is chosen twice for Shows.",
        ),
        (
            json!({"bed": "north", "shows": ["rain"]}),
            "Rain isn't available when Bed is North bed.",
        ),
        (
            json!({"bed": "north", "weather": "yes"}),
            "Show the forecast needs to be yes (true) or no (false).",
        ),
        (
            json!({"bed": "north", "colour": "red"}),
            "This template has no setting called \"colour\".",
        ),
    ] {
        assert_eq!(t.make_pages(&answers(form.clone())).unwrap_err(), said, "{form}");
    }
}

#[test]
fn settings_that_wont_fit_on_the_hub_are_refused() {
    let t = Template::parse(
        r#"{"id": "notes/1", "name": "Notes", "description": "Notes.",
            "settings": [
              {"key": "a", "label": "A", "type": "text", "max_length": 500},
              {"key": "b", "label": "B", "type": "text", "max_length": 500},
              {"key": "c", "label": "C", "type": "text", "max_length": 500},
              {"key": "d", "label": "D", "type": "text", "max_length": 500},
              {"key": "e", "label": "E", "type": "text", "max_length": 500}
            ],
            "pages": [{"title": "Notes", "kind": "text", "source": "manual"}]}"#,
    )
    .unwrap();
    let long = "x".repeat(500);
    let form = answers(json!({"a": long, "b": long, "c": long, "d": long, "e": long}));
    assert!(t.make_pages(&form).unwrap_err().contains("at most 2048"));
    let form = answers(json!({"a": long, "b": long, "c": long, "d": long}));
    assert_eq!(t.make_pages(&form).unwrap().len(), 1);
}

#[test]
fn a_broken_template_is_refused_with_where_and_why() {
    let base: Value = serde_json::from_str(GARDEN).unwrap();
    let with = |path: &[&str], v: Value| {
        let mut t = base.clone();
        let mut at = &mut t;
        for (i, p) in path.iter().enumerate() {
            if i == path.len() - 1 {
                match p.parse::<usize>() {
                    Ok(n) => at[n] = v.clone(),
                    Err(_) => at[*p] = v.clone(),
                }
                break;
            }
            at = match p.parse::<usize>() {
                Ok(n) => &mut at[n],
                Err(_) => &mut at[*p],
            };
        }
        serde_json::to_string(&t).unwrap()
    };
    for (json, said) in [
        (with(&["id"], json!("garden")), "needs a name and a version"),
        (with(&["name"], json!(" Garden")), "The template's name needs"),
        (with(&["colour"], json!("red")), "unknown field `colour`"),
        (
            with(&["settings", "1", "key"], json!("bed")),
            "Two settings are called \"bed\".",
        ),
        (
            with(&["settings", "1", "key"], json!("Name")),
            "Setting \"Name\": a key is",
        ),
        (
            with(&["settings", "1", "type"], json!("colour")),
            "unknown variant `colour`",
        ),
        (
            with(&["settings", "1", "min"], json!(1)),
            "Setting \"name\": min and max isn't used by a text setting.",
        ),
        (
            with(&["settings", "0", "max_length"], json!(5)),
            "max_length isn't used by a choice setting.",
        ),
        (
            with(
                &["settings", "1", "options"],
                json!([{"value": "a", "label": "A"}]),
            ),
            "options isn't used by a text setting.",
        ),
        (
            with(&["settings", "0", "options"], json!([])),
            "it needs 1 to 20 options.",
        ),
        (
            with(&["settings", "0", "options", "0", "value"], json!("North")),
            "the option value \"North\"",
        ),
        (
            with(&["settings", "0", "options", "1", "value"], json!("north")),
            "two options are \"north\".",
        ),
        (
            with(&["settings", "2", "default"], json!(100)),
            "its default doesn't fit: Sensor depth can be at most 60.",
        ),
        (
            with(&["settings", "2", "min"], json!(70)),
            "min is more than max.",
        ),
        (
            with(&["settings", "3", "max"], json!(3)),
            "whole numbers from 0 to 2",
        ),
        (
            with(
                &["settings", "3", "options", "1", "when"],
                json!({"shows": "rain"}),
            ),
            "depends on its own setting",
        ),
        (
            with(&["settings", "3", "options", "1", "when"], json!({"bed": "east"})),
            "wants \"east\" for \"bed\"",
        ),
        (
            with(&["settings", "3", "options", "1", "when"], json!({"name": "x"})),
            "only choice, multi-choice and yes/no settings can be tested",
        ),
        (with(&["pages"], json!([])), "A template makes 1 to 10 pages."),
        (
            with(&["pages", "0", "title"], json!("{nope}")),
            "Page 1: its title names \"nope\", which isn't a setting.",
        ),
        (
            with(&["pages", "0", "title"], json!("{name.label}")),
            "only choices have labels",
        ),
        (
            with(&["pages", "0", "title"], json!("Bed {")),
            "has a { with no } after it",
        ),
        (
            with(&["pages", "0", "kind"], json!("Meter")),
            "its kind \"Meter\" isn't",
        ),
        (
            with(&["pages", "0", "source"], json!("pc")),
            "its source could be \"pc\"",
        ),
        (
            with(&["pages", "0", "source"], json!("{name}")),
            "needs to name a choice setting",
        ),
        (
            with(&["pages", "0", "source"], json!("{bed}")),
            "its source could be \"north\"",
        ),
        (
            with(&["pages", "0", "feed"], json!("Garden")),
            "its feed \"Garden\" needs",
        ),
        (with(&["pages", "0", "feed"], Value::Null), "so it needs a feed"),
        (
            with(&["pages", "2", "when"], json!({"weather": "yes"})),
            "Page 3: its condition wants \"yes\"",
        ),
        ("{".to_string(), "This template can't be read"),
    ] {
        let e = Template::parse(&json).unwrap_err();
        assert!(e.contains(said), "{said:?} not in {e:?}");
    }
    let huge = format!("{{\"id\": \"x/1\", \"name\": \"{}\"}}", "x".repeat(17_000));
    assert!(Template::parse(&huge).unwrap_err().contains("at most 16384"));
}

#[test]
fn the_example_in_the_docs_works_as_it_says() {
    let doc = include_str!("../../../docs/templates.md");
    let start = doc.find("```json\n{").unwrap() + "```json\n".len();
    let end = start + doc[start..].find("```").unwrap();
    let t = Template::parse(&doc[start..end]).unwrap();
    let pages = t.make_pages(&answers(json!({"bed": "north"}))).unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(
        (
            pages[0].title.as_str(),
            pages[0].kind.as_str(),
            pages[0].feed.as_deref()
        ),
        ("North bed", "meter", Some("garden-north"))
    );
    assert_eq!(
        pages[0].settings,
        Some(answers(
            json!({"bed": "north", "depth": 10, "shows": ["moisture"], "forecast": false})
        ))
    );
}

#[test]
fn a_required_multi_choice_refuses_an_empty_list() {
    let t = Template::parse(
        r#"{
  "id": "picks/1", "name": "Picks", "description": "Some picks.",
  "settings": [
    { "key": "shows", "label": "Shows", "type": "multi_choice", "required": true,
      "options": [ { "value": "a", "label": "A" }, { "value": "b", "label": "B" } ] }
  ],
  "pages": [ { "title": "Picks", "kind": "text", "source": "manual", "when": { "shows": "a" } } ]
}"#,
    )
    .unwrap();
    assert_eq!(
        t.make_pages(&answers(json!({"shows": []}))).unwrap_err(),
        "Choose at least 1 option for Shows."
    );
    assert!(t.make_pages(&answers(json!({"shows": ["a"]}))).is_ok());

    let two = Template::parse(
        r#"{
  "id": "picks/2", "name": "Picks", "description": "Some picks.",
  "settings": [
    { "key": "shows", "label": "Shows", "type": "multi_choice", "required": true, "min": 2,
      "options": [ { "value": "a", "label": "A" }, { "value": "b", "label": "B" } ] }
  ],
  "pages": [ { "title": "Picks", "kind": "text", "source": "manual", "when": { "shows": "a" } } ]
}"#,
    )
    .unwrap();
    assert_eq!(
        two.make_pages(&answers(json!({"shows": []}))).unwrap_err(),
        "Choose at least 2 options for Shows."
    );
}
