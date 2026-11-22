//! Label selector builder (D26), like Grafana's label filters on a Loki
//! data source: one row per matcher, each made of three comboboxes, the
//! label (keys known from the catalog's hosts), the operator, then the value
//! (values known for that label, or any typed one). Lists only open on a
//! click and filter as you type, so they stay short however many labels
//! the fleet has. The selector's text stays the single source of truth: the
//! builder writes it on every change; a Code mode edits it directly.

use leptos::prelude::*;

use crate::labels::{HOST, HostLabels, LabelSelector, Op};

/// At most this many entries in an open list (type to narrow it).
const SHOWN: usize = 100;
const OPS: [&str; 4] = ["=", "!=", "=~", "!~"];

/// One matcher being edited (possibly incomplete).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub key: String,
    pub op: String,
    pub value: String,
}

impl Draft {
    fn complete(&self) -> bool {
        !self.key.is_empty() && !self.value.is_empty()
    }
}

fn op_text(op: Op) -> &'static str {
    match op {
        Op::Eq => "=",
        Op::Ne => "!=",
        Op::Re => "=~",
        Op::Nre => "!~",
    }
}

/// The rows of a selector.
pub fn drafts_of(selector: &LabelSelector) -> Vec<Draft> {
    selector
        .matchers
        .iter()
        .map(|m| Draft {
            key: m.key.clone(),
            op: op_text(m.op).to_string(),
            value: m.value.clone(),
        })
        .collect()
}

/// The selector text of the complete rows (written as is: a bad key or
/// regex shows up in the preview, which parses it).
pub fn text_of(drafts: &[Draft]) -> String {
    let parts: Vec<String> = drafts
        .iter()
        .filter(|d| d.complete())
        .map(|d| {
            let value = d.value.replace('\\', "\\\\").replace('"', "\\\"");
            let op = if OPS.contains(&d.op.as_str()) {
                d.op.as_str()
            } else {
                "="
            };
            format!("{}{op}\"{value}\"", d.key)
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

/// Label keys of the hosts (built-in ones included), sorted.
pub fn known_keys(hosts: &[(String, HostLabels)]) -> Vec<String> {
    let mut keys: Vec<String> = hosts
        .iter()
        .flat_map(|(_, labels)| labels.iter().map(|(k, _)| k.clone()))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Values of `key` on the hosts, sorted.
pub fn known_values(hosts: &[(String, HostLabels)], key: &str) -> Vec<String> {
    let mut values: Vec<String> = hosts
        .iter()
        .flat_map(|(_, labels)| {
            labels
                .iter()
                .filter(|(k, _)| k.as_str() == key)
                .flat_map(|(_, v)| v.iter().cloned())
                .collect::<Vec<_>>()
        })
        .collect();
    values.sort();
    values.dedup();
    values
}

/// What an open list shows for `typed`: matching options (at most
/// [`SHOWN`]) and whether more were left out.
pub fn shown_options(options: &[String], typed: &str) -> (Vec<String>, bool) {
    let needle = typed.to_lowercase();
    let mut matching = options
        .iter()
        .filter(|o| needle.is_empty() || o.to_lowercase().contains(&needle));
    let shown: Vec<String> = matching.by_ref().take(SHOWN).cloned().collect();
    (shown, matching.next().is_some())
}

/// A combobox: a click opens the list of `options`, typing filters it,
/// a click or Enter picks; with `free`, a typed text not in the list can be
/// used as is.
#[component]
fn Combo(
    #[prop(into)] value: Signal<String>,
    #[prop(into)] options: Signal<Vec<String>>,
    on_change: Callback<String>,
    placeholder: &'static str,
    #[prop(optional)] free: bool,
    #[prop(optional)] narrow: bool,
) -> impl IntoView {
    let open = RwSignal::new(false);
    // the text typed while the list is open
    let typed = RwSignal::new(String::new());
    let shown = Memo::new(move |_| options.with(|o| shown_options(o, &typed.get())));
    let pick = move |choice: String| {
        on_change.run(choice);
        typed.set(String::new());
        open.set(false);
    };
    // Enter / leaving the field: the typed text, if it may be used
    let commit = move || {
        let text = typed.get_untracked().trim().to_string();
        if text.is_empty() {
            open.set(false);
            return;
        }
        let first = shown.with_untracked(|(list, _)| list.first().cloned());
        let exact = options.with_untracked(|o| o.contains(&text));
        if free || exact {
            pick(text);
        } else if let Some(first) = first {
            pick(first);
        } else {
            typed.set(String::new());
            open.set(false);
        }
    };
    view! {
        <div class="combo" class:narrow=narrow>
            <input spellcheck="false"
                prop:value=move || if open.get() { typed.get() } else { value.get() }
                placeholder=move || {
                    let current = value.get();
                    if open.get() && !current.is_empty() { current } else { placeholder.to_string() }
                }
                on:focus=move |_| { typed.set(String::new()); open.set(true); }
                on:click=move |_| open.set(true)
                on:input=move |ev| { typed.set(event_target_value(&ev)); open.set(true); }
                on:keydown=move |ev| match ev.key().as_str() {
                    "Enter" => { ev.prevent_default(); commit(); }
                    "Escape" => { typed.set(String::new()); open.set(false); }
                    _ => {}
                }
                on:blur=move |_| commit() />
            <Show when=move || open.get()>
                <ul class="combo-list" role="listbox">
                    <For each=move || shown.get().0 key=|o| o.clone() let:option>
                        {
                            let chosen = option.clone();
                            let selected = option == value.get_untracked();
                            view! {
                                <li class:selected=selected
                                    on:mousedown=move |ev| { ev.prevent_default(); pick(chosen.clone()); }>{option}</li>
                            }
                        }
                    </For>
                    {move || shown.with(|(_, more)| *more).then(|| view! { <li class="hint">"… type to narrow the list"</li> })}
                    {move || {
                        let text = typed.get().trim().to_string();
                        let known = options.with(|o| o.contains(&text));
                        (free && !text.is_empty() && !known).then(|| {
                            let label = format!("Use “{text}”");
                            view! { <li class="new" on:mousedown=move |ev| { ev.prevent_default(); pick(text.clone()); }>{label}</li> }
                        })
                    }}
                    {move || (shown.with(|(list, _)| list.is_empty()) && !free).then(|| view! { <li class="hint">"no match"</li> })}
                </ul>
            </Show>
        </div>
    }
}

/// How a matcher reads on its chip: `kind=~"web|api"`.
fn chip_text(draft: &Draft) -> String {
    let key = if draft.key.is_empty() {
        "?"
    } else {
        &draft.key
    };
    let value = if draft.value.is_empty() {
        "?"
    } else {
        &draft.value
    };
    format!("{key}{}\"{value}\"", draft.op)
}

/// The builder of a label selector stored as text in `selector`, with the
/// labels of `hosts` as suggestions. The filters read as chips on one
/// wrapping line; a click on a chip (or "+ Label filter") edits one
/// filter at a time, with label / operator / value comboboxes.
#[component]
pub fn SelectorBuilder(
    /// The selector's text (read once at creation, then written).
    selector: Signal<String>,
    set_selector: Callback<String>,
    #[prop(into)] hosts: Signal<Vec<(String, HostLabels)>>,
) -> impl IntoView {
    let drafts = RwSignal::new(
        LabelSelector::parse(&selector.get_untracked())
            .map(|s| drafts_of(&s))
            .unwrap_or_default(),
    );
    // the filter being edited
    let editing = RwSignal::new(None::<usize>);
    let keys = Signal::derive(move || hosts.with(|h| known_keys(h)));
    let write = move || set_selector.run(drafts.with_untracked(|d| text_of(d)));
    let update = move |index: usize, change: Box<dyn Fn(&mut Draft)>| {
        drafts.update(|d| {
            if let Some(draft) = d.get_mut(index) {
                change(draft);
            }
        });
        write();
    };
    let remove = move |index: usize| {
        drafts.update(|d| {
            if index < d.len() {
                d.remove(index);
            }
        });
        editing.set(None);
        write();
    };
    // leave the editor; an unfinished filter is dropped
    let done = move || {
        if let Some(index) = editing.get_untracked()
            && drafts.with_untracked(|d| {
                d.get(index)
                    .is_some_and(|x| x.key.is_empty() || x.value.is_empty())
            })
        {
            remove(index);
        }
        editing.set(None);
    };
    let add = move |_| {
        done();
        drafts.update(|d| {
            d.push(Draft {
                op: "=".into(),
                ..Default::default()
            })
        });
        editing.set(Some(drafts.with_untracked(Vec::len) - 1));
    };
    let chips = move || {
        let current = editing.get();
        drafts.with(|d| {
            d.iter()
                .enumerate()
                .map(|(index, draft)| {
                    let text = chip_text(draft);
                    view! {
                        <span class="chip" class:active=current == Some(index)>
                            <button type="button" class="chip-text" title="Edit this filter"
                                on:click=move |_| { done(); editing.set(Some(index.min(drafts.with_untracked(Vec::len).saturating_sub(1)))); }>{text}</button>
                            <button type="button" class="chip-x" title="Remove this filter" aria-label="Remove this filter"
                                on:click=move |_| remove(index)>"×"</button>
                        </span>
                    }
                })
                .collect::<Vec<_>>()
        })
    };
    let editor = move || {
        let index = editing.get()?;
        let key = Signal::derive(move || {
            drafts.with(|d| d.get(index).map(|x| x.key.clone()).unwrap_or_default())
        });
        let op = Signal::derive(move || {
            drafts.with(|d| d.get(index).map(|x| x.op.clone()).unwrap_or_default())
        });
        let value = Signal::derive(move || {
            drafts.with(|d| d.get(index).map(|x| x.value.clone()).unwrap_or_default())
        });
        let values = Signal::derive(move || {
            let key = key.get();
            if key.is_empty() {
                Vec::new()
            } else {
                hosts.with(|h| known_values(h, &key))
            }
        });
        let ops = Signal::derive(|| OPS.map(String::from).to_vec());
        let value_hint = move || {
            if op.get().contains('~') {
                "regex"
            } else {
                "value"
            }
        };
        Some(view! {
            <div class="matcher">
                <Combo value=key options=keys free=true placeholder="label"
                    on_change=Callback::new(move |k: String| update(index, Box::new(move |d| {
                        if d.key != k { d.value.clear(); }
                        d.key = k.clone();
                    }))) />
                <Combo value=op options=ops narrow=true placeholder="="
                    on_change=Callback::new(move |o: String| update(index, Box::new(move |d| d.op = o.clone()))) />
                {move || {
                    let hint = value_hint();
                    view! {
                        <Combo value=value options=values free=true placeholder=hint
                            on_change=Callback::new(move |v: String| update(index, Box::new(move |d| d.value = v.clone()))) />
                    }
                }}
                <button type="button" title="Done with this filter" on:click=move |_| done()>"✓"</button>
            </div>
        })
    };
    view! {
        <div class="selector-builder">
            <div class="chip-line">
                {chips}
                <Show when=move || drafts.with(Vec::is_empty)>
                    <span class="muted small">"every host"</span>
                </Show>
                <button type="button" class="chip-add"
                    title=format!("Add a filter on a label; built in: {HOST} (the host id) and source")
                    on:click=add>"+ Label filter"</button>
            </div>
            {editor}
        </div>
    }
}

/// What a selector selects, for the "Matching hosts" panel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchReport {
    /// Hosts of the catalog.
    pub total: usize,
    /// Hosts the selector selects.
    pub matching: usize,
    /// Hosts to list.
    pub rows: Vec<HostRow>,
    /// Rows left out beyond the limit.
    pub more: usize,
}

/// A host of the "Matching hosts" list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRow {
    pub id: String,
    /// Its labels `(key, value)`, the built-in `host` left out (it is the
    /// id); a label filtered on by the selector is flagged.
    pub labels: Vec<(String, String, bool)>,
    /// For a host that does not match (listed only when searched): the
    /// filter excluding it.
    pub excluded: Option<String>,
}

fn host_row(
    id: &str,
    labels: &HostLabels,
    selector: &LabelSelector,
    excluded: Option<String>,
) -> HostRow {
    let used = |key: &str| selector.matchers.iter().any(|m| m.key == key);
    let labels = labels
        .iter()
        .filter(|(key, _)| key.as_str() != HOST)
        .flat_map(|(key, values)| {
            values
                .iter()
                .map(move |value| (key.clone(), value.clone(), used(key)))
        })
        .collect();
    HostRow {
        id: id.to_string(),
        labels,
        excluded,
    }
}

/// Why `labels` fails `matcher`: the matcher and the host's values.
fn excluded_by(matcher: &crate::labels::Matcher, labels: &HostLabels) -> String {
    let has = match labels.values_of(&matcher.key) {
        Some(values) if !values.is_empty() => format!(
            "has {}",
            values
                .iter()
                .map(|v| format!("{}=\"{v}\"", matcher.key))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => format!("no {} label", matcher.key),
    };
    format!("{matcher} ({has})")
}

/// The matching hosts whose id contains `search` (case-insensitive), then,
/// when searching, the non-matching ones with the filter excluding them; at
/// most `limit` rows.
pub fn match_report(
    selector: &LabelSelector,
    hosts: &[(String, HostLabels)],
    search: &str,
    limit: usize,
) -> MatchReport {
    let needle = search.trim().to_lowercase();
    let found = |id: &str| needle.is_empty() || id.to_lowercase().contains(&needle);
    let mut matching_rows = Vec::new();
    let mut excluded_rows = Vec::new();
    let mut matching = 0;
    for (id, labels) in hosts {
        let failed = selector.matchers.iter().find(|m| !m.matches(labels));
        match failed {
            None => {
                matching += 1;
                if found(id) {
                    matching_rows.push(host_row(id, labels, selector, None));
                }
            }
            Some(matcher) if !needle.is_empty() && found(id) => {
                let reason = excluded_by(matcher, labels);
                excluded_rows.push(host_row(id, labels, selector, Some(reason)));
            }
            Some(_) => {}
        }
    }
    let mut rows = matching_rows;
    rows.extend(excluded_rows);
    let more = rows.len().saturating_sub(limit);
    rows.truncate(limit);
    MatchReport {
        total: hosts.len(),
        matching,
        rows,
        more,
    }
}

/// Rows shown at once in the panel (search to find the others).
const LISTED: usize = 300;

/// "Matching hosts": how many hosts the selector (`selector`, as text)
/// selects, and a searchable list of them, so particular hosts can be
/// checked; a searched host that does not match shows the filter excluding
/// it.
#[component]
pub fn MatchingHosts(
    #[prop(into)] selector: Signal<String>,
    #[prop(into)] hosts: Signal<Vec<(String, HostLabels)>>,
) -> impl IntoView {
    let search = RwSignal::new(String::new());
    let report = Memo::new(move |_| {
        LabelSelector::parse(&selector.get())
            .map(|parsed| hosts.with(|h| match_report(&parsed, h, &search.get(), LISTED)))
    });
    view! {
        <aside class="matching-hosts">
            <h3>"Matching hosts"</h3>
            {move || match report.get() {
                Err(e) => view! { <p class="error small">{format!("Not a valid selector: {e}")}</p> }.into_any(),
                Ok(r) => view! {
                    <p class="matching-count"><strong>{r.matching}</strong>
                        <span class="muted">{format!(" of {} host(s)", r.total)}</span></p>
                }.into_any(),
            }}
            <input type="search" placeholder="Search a host…" spellcheck="false"
                prop:value=move || search.get() on:input=move |ev| search.set(event_target_value(&ev)) />
            {move || report.get().ok().map(|r| {
                let searching = !search.with(|s| s.trim().is_empty());
                let empty = r.rows.is_empty();
                view! {
                    <ul class="matching-list">
                        {r.rows.into_iter().map(|row| {
                            let matches = row.excluded.is_none();
                            let labels = row.labels.into_iter().map(|(key, value, used)| view! {
                                <span class="label-chip" class:used=used
                                    title=if used { "filtered on by the selector" } else { "" }>
                                    <span class="k">{key}</span>"="<span class="v">{value}</span></span>
                            }).collect_view();
                            view! {
                                <li class:match=matches class:no-match=!matches
                                    title=if matches { "matches the selector" } else { "does not match the selector" }>
                                    <span class="mark" aria-label=if matches { "matches" } else { "does not match" }>
                                        {if matches { "✓" } else { "✗" }}</span>
                                    <a class="host" href=format!("/hosts/{}", row.id) target="_blank">{row.id.clone()}</a>
                                    <span class="labels">{labels}</span>
                                    {row.excluded.map(|reason| view! { <span class="reason">{format!("excluded by {reason}")}</span> })}
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                    {(empty && searching).then(|| view! { <p class="muted small">"No host of the catalog has this name."</p> })}
                    {(r.more > 0).then(|| view! { <p class="muted small">{format!("+{} more: search to narrow", r.more)}</p> })}
                }
            })}
        </aside>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn host(id: &str, labels: &[(&str, &str)]) -> (String, HostLabels) {
        let labels: BTreeMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (id.to_string(), HostLabels::new(id, ["ui"], &labels))
    }

    #[test]
    fn rows_and_text_round_trip() {
        let selector =
            LabelSelector::parse(r#"{env="prod", role=~"web|api", tier!~"d\"b"}"#).unwrap();
        let drafts = drafts_of(&selector);
        assert_eq!(drafts.len(), 3);
        assert_eq!(drafts[1].op, "=~");
        assert_eq!(LabelSelector::parse(&text_of(&drafts)).unwrap(), selector);
        // incomplete rows are left out
        let mut more = drafts.clone();
        more.push(Draft {
            key: "team".into(),
            op: "=".into(),
            value: String::new(),
        });
        more.push(Draft {
            key: String::new(),
            op: "=".into(),
            value: "x".into(),
        });
        assert_eq!(text_of(&more), text_of(&drafts));
        assert_eq!(text_of(&[]), "{}");
    }

    #[test]
    fn suggestions_come_from_the_hosts() {
        let hosts = [
            host("web-01", &[("env", "prod"), ("role", "web")]),
            host("db-01", &[("env", "staging")]),
        ];
        assert_eq!(known_keys(&hosts), ["env", "host", "role", "source"]);
        assert_eq!(known_values(&hosts, "env"), ["prod", "staging"]);
        assert_eq!(known_values(&hosts, "host"), ["db-01", "web-01"]);
        assert!(known_values(&hosts, "nope").is_empty());
    }

    #[test]
    fn matching_report_lists_and_explains() {
        let hosts = [
            host("web-01", &[("env", "prod"), ("role", "web")]),
            host("web-02", &[("env", "staging"), ("role", "web")]),
            host("db-01", &[("env", "prod"), ("role", "db")]),
            host("lab", &[]),
        ];
        let selector = LabelSelector::parse(r#"{env="prod", role=~"web|db"}"#).unwrap();
        let all = match_report(&selector, &hosts, "", 10);
        assert_eq!((all.total, all.matching), (4, 2));
        let ids: Vec<&str> = all.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["web-01", "db-01"]);
        assert!(all.rows.iter().all(|r| r.excluded.is_none()));
        // labels shown with each host, built-in host left out, filtered ones flagged
        assert_eq!(
            all.rows[0].labels,
            [
                ("env".to_string(), "prod".to_string(), true),
                ("role".to_string(), "web".to_string(), true),
                ("source".to_string(), "ui".to_string(), false),
            ]
        );
        // searching shows the non-matching ones too, with the reason
        let web = match_report(&selector, &hosts, "WEB", 10);
        assert_eq!(web.matching, 2, "the count ignores the search");
        assert_eq!(
            (web.rows[0].id.as_str(), web.rows[0].excluded.as_deref()),
            ("web-01", None)
        );
        assert_eq!(web.rows[1].id, "web-02");
        assert_eq!(
            web.rows[1].excluded.as_deref(),
            Some(r#"env="prod" (has env="staging")"#)
        );
        let lab = match_report(&selector, &hosts, "lab", 10);
        assert_eq!(
            lab.rows[0].excluded.as_deref(),
            Some(r#"env="prod" (no env label)"#)
        );
        let limited = match_report(&LabelSelector::all(), &hosts, "", 3);
        assert_eq!((limited.rows.len(), limited.more), (3, 1));
    }

    #[test]
    fn open_lists_stay_short() {
        let many: Vec<String> = (0..250).map(|i| format!("host-{i:03}")).collect();
        let (shown, more) = shown_options(&many, "");
        assert_eq!((shown.len(), more), (SHOWN, true));
        let (shown, more) = shown_options(&many, "HOST-24");
        assert_eq!(
            shown.len(),
            10,
            "host-240 … host-249, case-insensitive: {shown:?}"
        );
        assert!(shown.iter().all(|h| h.starts_with("host-24")));
        assert!(!more);
    }
}
