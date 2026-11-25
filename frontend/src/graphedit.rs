//! D32: editing assignments and host labels from the graph ("React Flow"
//! like). A selector is an AND of label matchers, so making an assignment
//! select more hosts (an arrow drawn to a cluster) or fewer (an arrow
//! removed) can be done two ways, offered each time: relabel the hosts
//! (their host definitions) or change the assignment's selector. Every
//! option shows its preview (hosts joining and leaving each assignment,
//! computed here from the labels the map knows) and is saved at once: one
//! `catalog.upsert` / `catalog.delete` per item (D10, D11).

use std::collections::{BTreeMap, BTreeSet};

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::Session;
use crate::catalog::{CatalogData, reference_of};
use crate::client::api;
use crate::config::{Entry, delete_item, str_of, upsert_item};
use crate::labels::{HostLabels, LabelSelector, Matcher, Op, RESERVED};

/// Above this many hosts, a selector suggestion never lists host names.
const HOST_LIST_MAX: usize = 50;

/// Label changes: `Some(value)` sets the label, `None` removes it.
pub type LabelChange = BTreeMap<String, Option<String>>;

/// A host of the map, as the graph edits it.
#[derive(Clone, Debug, PartialEq)]
pub struct EditHost {
    pub id: String,
    /// The sources defining it (and its agent's, D28): built-in `source`.
    pub sources: Vec<String>,
    /// Its labels, as matched (definitions and agent, D28).
    pub own: BTreeMap<String, String>,
    /// The host definition relabelling writes, else why it cannot.
    pub definition: Result<Entry, String>,
}

impl EditHost {
    pub fn labels(&self) -> HostLabels {
        HostLabels::new(&self.id, self.sources.iter().map(String::as_str), &self.own)
    }

    /// Its labels once `change` is applied.
    fn own_after(&self, change: &LabelChange) -> BTreeMap<String, String> {
        let mut own = self.own.clone();
        apply(&mut own, change);
        own
    }

    fn labels_after(&self, change: &LabelChange) -> HostLabels {
        HostLabels::new(
            &self.id,
            self.sources.iter().map(String::as_str),
            &self.own_after(change),
        )
    }
}

fn apply(labels: &mut BTreeMap<String, String>, change: &LabelChange) {
    for (key, value) in change {
        match value {
            Some(v) => {
                labels.insert(key.clone(), v.clone());
            }
            None => {
                labels.remove(key);
            }
        }
    }
}

/// The labels of a definition (`labels` object of a `hostdef` item).
fn definition_labels(entry: &Entry) -> BTreeMap<String, String> {
    entry.item["labels"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// The map's hosts with what relabelling them needs: their only
/// definition, when it comes from a source regency may edit
/// (`editable`: not Git, D11).
pub fn edit_hosts<'a>(
    hosts: impl IntoIterator<Item = &'a api::HostSummary>,
    hostdefs: &[Entry],
    editable: impl Fn(&str) -> bool,
) -> Vec<EditHost> {
    let mut defs: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    for entry in hostdefs {
        defs.entry(str_of(&entry.item, "host_id"))
            .or_default()
            .push(entry);
    }
    hosts
        .into_iter()
        .map(|host| {
            let mine = defs.get(&host.host_id).map(Vec::as_slice).unwrap_or(&[]);
            let mut sources: Vec<String> =
                mine.iter().map(|e| str_of(&e.item, "source_id")).collect();
            if !host.agent.is_empty() {
                sources.push(host.agent.clone());
            }
            let definition = match mine {
                [] => Err("no host definition (only its agent reports it)".to_string()),
                [one] => {
                    let source = str_of(&one.item, "source_id");
                    if editable(&source) {
                        Ok((*one).clone())
                    } else {
                        Err(format!("defined in source {source}: edit it there"))
                    }
                }
                several => Err(format!(
                    "defined by {} sources: changing one would make them conflict",
                    several.len()
                )),
            };
            EditHost {
                id: host.host_id.clone(),
                sources,
                own: host.labels.clone().into_iter().collect(),
                definition,
            }
        })
        .collect()
}

/// What relabelling some hosts writes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Relabel {
    /// Each definition to write, with its new labels.
    pub writes: Vec<(Entry, BTreeMap<String, String>)>,
    /// Hosts left unchanged, and why.
    pub refused: Vec<(String, String)>,
    /// The written hosts' labels afterwards.
    pub after: BTreeMap<String, HostLabels>,
}

/// Relabelling `hosts` with `change` (hosts it does not change are left
/// out).
pub fn plan_relabel(hosts: &[&EditHost], change: &LabelChange) -> Relabel {
    let mut plan = Relabel::default();
    for host in hosts {
        if host.own_after(change) == host.own {
            continue;
        }
        let entry = match &host.definition {
            Ok(entry) => entry,
            Err(why) => {
                plan.refused.push((host.id.clone(), why.clone()));
                continue;
            }
        };
        let mut labels = definition_labels(entry);
        // a label its agent reports (D28) would come back
        let from_agent = change
            .iter()
            .find(|(k, v)| v.is_none() && host.own.contains_key(*k) && !labels.contains_key(*k));
        if let Some((key, _)) = from_agent {
            plan.refused.push((
                host.id.clone(),
                format!("its agent reports label {key}: change it in the agent's configuration"),
            ));
            continue;
        }
        apply(&mut labels, change);
        plan.after
            .insert(host.id.clone(), host.labels_after(change));
        plan.writes.push((entry.clone(), labels));
    }
    plan
}

/// The labels hosts need for `selector` to select them: its `key="value"`
/// matchers. Refused when a host would still not match (other matchers,
/// built-in labels).
pub fn join_change(selector: &LabelSelector, hosts: &[&EditHost]) -> Result<LabelChange, String> {
    let mut change = LabelChange::new();
    for m in &selector.matchers {
        if m.op == Op::Eq && !RESERVED.contains(&m.key.as_str()) {
            change.insert(m.key.clone(), Some(m.value.clone()));
        }
    }
    for host in hosts {
        let after = host.labels_after(&change);
        if let Some(m) = selector.matchers.iter().find(|m| !m.matches(&after)) {
            return Err(format!(
                "{}: {m} would still not hold (labels only meet key=\"value\" filters on labels of your own)",
                host.id
            ));
        }
    }
    Ok(change)
}

/// The labels whose removal makes `selector` stop selecting a host: keys
/// of its `=` / `=~` matchers that an absent label (`""`) fails.
pub fn leave_keys(selector: &LabelSelector) -> Vec<String> {
    let none = HostLabels::default();
    let mut keys = Vec::new();
    for m in &selector.matchers {
        if matches!(m.op, Op::Eq | Op::Re)
            && !RESERVED.contains(&m.key.as_str())
            && !m.matches(&none)
            && !keys.contains(&m.key)
        {
            keys.push(m.key.clone());
        }
    }
    keys
}

/// The labels every host of `hosts` has with the same value (D32: a
/// cluster's "selector").
pub fn shared(hosts: &[&EditHost]) -> BTreeMap<String, String> {
    let Some((first, rest)) = hosts.split_first() else {
        return BTreeMap::new();
    };
    first
        .own
        .iter()
        .filter(|(k, v)| rest.iter().all(|h| h.own.get(*k) == Some(*v)))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn host_list(hosts: &[&EditHost], op: Op) -> Option<Matcher> {
    let names: Vec<String> = hosts.iter().map(|h| regex::escape(&h.id)).collect();
    Matcher::new(crate::labels::HOST, op, &names.join("|")).ok()
}

/// The selector of the labels `hosts` share, as text; `{}` (every host)
/// when they share none.
pub fn shared_selector(hosts: &[&EditHost]) -> String {
    LabelSelector::equal(&shared(hosts))
        .unwrap_or_default()
        .to_string()
}

/// A selector selecting `current` and `added`: the labels they all share,
/// else (a few hosts) their names.
pub fn widened(current: &[&EditHost], added: &[&EditHost]) -> String {
    let all: Vec<&EditHost> = current.iter().chain(added).copied().collect();
    let common = shared(&all);
    if common.is_empty()
        && all.len() <= HOST_LIST_MAX
        && let Some(list) = host_list(&all, Op::Re)
    {
        return LabelSelector {
            matchers: vec![list],
        }
        .to_string();
    }
    LabelSelector::equal(&common)
        .unwrap_or_default()
        .to_string()
}

/// `selector` plus an exclusion of `removed`: a label value they all share
/// and none of `keep` has, else their names.
pub fn narrowed(selector: &LabelSelector, keep: &[&EditHost], removed: &[&EditHost]) -> String {
    let mut out = selector.clone();
    let exclusion = shared(removed)
        .into_iter()
        .find(|(k, v)| keep.iter().all(|h| h.own.get(k) != Some(v)))
        .and_then(|(k, v)| Matcher::new(&k, Op::Ne, &v).ok())
        .or_else(|| host_list(removed, Op::Nre));
    out.matchers.extend(exclusion);
    out.to_string()
}

/// Hosts joining and leaving one assignment.
#[derive(Clone, Debug, PartialEq)]
pub struct Moves {
    pub assignment: String,
    pub joined: Vec<String>,
    pub left: Vec<String>,
}

/// What changes for each assignment (only those with changes) when hosts
/// get the labels of `after` and assignment `replaced.0` the selector
/// `replaced.1`.
pub fn preview(
    assignments: &[(String, LabelSelector)],
    hosts: &[EditHost],
    after: &BTreeMap<String, HostLabels>,
    replaced: Option<(&str, &LabelSelector)>,
) -> Vec<Moves> {
    let labels: Vec<(String, HostLabels)> =
        hosts.iter().map(|h| (h.id.clone(), h.labels())).collect();
    assignments
        .iter()
        .filter_map(|(reference, selector)| {
            let new = match replaced {
                Some((r, s)) if r == reference => s,
                _ => selector,
            };
            let (mut joined, mut left) = (Vec::new(), Vec::new());
            for (id, before) in &labels {
                let now = after.get(id).unwrap_or(before);
                match (selector.matches(before), new.matches(now)) {
                    (false, true) => joined.push(id.clone()),
                    (true, false) => left.push(id.clone()),
                    _ => {}
                }
            }
            (!joined.is_empty() || !left.is_empty()).then(|| Moves {
                assignment: reference.clone(),
                joined,
                left,
            })
        })
        .collect()
}

/// The catalog's assignments with their selectors (unreadable ones left
/// out).
pub fn selectors(assignments: &[Entry]) -> Vec<(String, LabelSelector)> {
    assignments
        .iter()
        .filter_map(|e| {
            let s = LabelSelector::parse(&str_of(&e.item, "selector")).ok()?;
            Some((reference_of(e), s))
        })
        .collect()
}

/// "a, b, c … (N)": at most 6 names.
fn names(list: &[String]) -> String {
    let mut text = list.iter().take(6).cloned().collect::<Vec<_>>().join(", ");
    if list.len() > 6 {
        text.push_str(&format!(" … ({} hosts)", list.len()));
    }
    text
}

// ---------------------------------------------------------------- writing

/// Write each definition with its new labels, one after the other;
/// `progress`: written so far.
async fn write_labels(
    session: Session,
    writes: Vec<(Entry, BTreeMap<String, String>)>,
    progress: RwSignal<usize>,
) -> Result<usize, String> {
    let total = writes.len();
    for (i, (entry, labels)) in writes.into_iter().enumerate() {
        let mut item = entry.item;
        item["labels"] = json!(labels);
        if let Err(status) = upsert_item(session, item).await {
            session.check(&status);
            return Err(format!(
                "{} of {total} hosts relabelled, then: {}",
                i,
                status.message()
            ));
        }
        let _ = progress.try_set(i + 1);
    }
    Ok(total)
}

/// Save assignment `entry` with `selector`.
async fn write_selector(session: Session, entry: Entry, selector: String) -> Result<(), String> {
    let mut item = entry.item;
    item["selector"] = Value::String(selector);
    upsert_item(session, item)
        .await
        .map(|_| ())
        .map_err(|status| {
            session.check(&status);
            status.message().to_string()
        })
}

/// What the graph edits with: the catalog, the map's hosts, and where to
/// tell what happened.
#[derive(Clone, Copy)]
pub struct GraphEdit {
    pub catalog: CatalogData,
    pub hosts: RwSignal<BTreeMap<String, api::HostSummary>>,
    pub message: RwSignal<Option<String>>,
}

impl GraphEdit {
    /// The map's hosts as they are now (computed when a dialog opens, not
    /// on every live update).
    pub fn snapshot(&self) -> Vec<EditHost> {
        let sources = self.catalog.sources;
        self.catalog.hostdefs.with_untracked(|defs| {
            self.hosts.with_untracked(|hosts| {
                edit_hosts(hosts.values(), defs, |s| sources.is_editable(s))
            })
        })
    }

    fn done(&self, session: Session, text: String) {
        let _ = self.message.try_set(Some(text));
        self.catalog.reload_soon(session);
    }
}

fn preview_view(moves: Vec<Moves>) -> impl IntoView {
    if moves.is_empty() {
        return view! { <p class="muted">"No assignment gains or loses a host."</p> }.into_any();
    }
    view! {
        <ul class="edit-preview">
            {moves.into_iter().map(|m| view! {
                <li>
                    <b>{m.assignment}</b>
                    {(!m.joined.is_empty()).then(|| view! {
                        <span class="joined" title=m.joined.join(", ")>{format!(" +{} ", m.joined.len())}<span class="muted">{names(&m.joined)}</span></span>
                    })}
                    {(!m.left.is_empty()).then(|| view! {
                        <span class="left" title=m.left.join(", ")>{format!(" −{} ", m.left.len())}<span class="muted">{names(&m.left)}</span></span>
                    })}
                </li>
            }).collect_view()}
        </ul>
    }
    .into_any()
}

fn refused_view(refused: &[(String, String)]) -> impl IntoView + use<> {
    (!refused.is_empty()).then(|| {
        let shown: Vec<_> = refused.iter().take(8).cloned().collect();
        let more = refused.len().saturating_sub(shown.len());
        view! {
            <div class="edit-refused">
                <p class="warn-text">{format!("{} host(s) left unchanged:", refused.len())}</p>
                <ul>
                    {shown.into_iter().map(|(id, why)| view! { <li><b>{id}</b> " — " {why}</li> }).collect_view()}
                    {(more > 0).then(|| view! { <li class="muted">{format!("… and {more} more")}</li> })}
                </ul>
            </div>
        }
    })
}

/// How the dialog changes the selection.
#[derive(Clone, Copy, PartialEq)]
enum Way {
    Labels,
    Selector,
}

/// D32: an arrow drawn (`adding`) or removed between assignment
/// `assignment` and hosts `targets`: relabel the hosts, or change the
/// assignment's selector, with the preview of each.
#[component]
pub fn ArrowDialog(
    edit: GraphEdit,
    assignment: String,
    targets: Vec<String>,
    adding: bool,
    #[prop(into)] on_close: Callback<()>,
) -> impl IntoView {
    let session = expect_context::<Session>();
    let all = StoredValue::new(edit.snapshot());
    let entry = edit.catalog.assignment(&assignment);
    let current = entry
        .as_ref()
        .and_then(|e| LabelSelector::parse(&str_of(&e.item, "selector")).ok())
        .unwrap_or_default();
    let assignments = edit.catalog.assignments.with_untracked(|a| selectors(a));
    let wanted: BTreeSet<String> = targets.iter().cloned().collect();
    // the targets (those the map knows), and the assignment's other hosts
    let (target_hosts, others): (Vec<EditHost>, Vec<EditHost>) = all.with_value(|hosts| {
        let t = hosts
            .iter()
            .filter(|h| wanted.contains(&h.id))
            .cloned()
            .collect();
        let o = hosts
            .iter()
            .filter(|h| !wanted.contains(&h.id) && current.matches(&h.labels()))
            .cloned()
            .collect();
        (t, o)
    });
    let t_refs: Vec<&EditHost> = target_hosts.iter().collect();
    let o_refs: Vec<&EditHost> = others.iter().collect();
    let suggested = if adding {
        widened(&o_refs, &t_refs)
    } else {
        narrowed(&current, &o_refs, &t_refs)
    };
    let keys = leave_keys(&current);
    let join = adding.then(|| join_change(&current, &t_refs));
    let target_hosts = StoredValue::new(target_hosts);
    let assignments = StoredValue::new(assignments);

    let way = RwSignal::new(Way::Labels);
    let text = RwSignal::new(suggested);
    let key = RwSignal::new(keys.first().cloned().unwrap_or_default());
    let progress = RwSignal::new(0usize);
    let busy = RwSignal::new(false);
    let failed = RwSignal::new(None::<String>);

    // the label change of the Labels way, or why there is none
    let change = {
        let join = join.clone();
        Memo::new(move |_| -> Result<LabelChange, String> {
            match &join {
                Some(result) => result.clone(),
                None => {
                    let k = key.get();
                    if k.is_empty() {
                        Err("its selector has no key=\"value\" (or =~) filter a host can lose: change the selector instead".into())
                    } else {
                        Ok([(k, None)].into())
                    }
                }
            }
        })
    };
    let plan = Memo::new(move |_| {
        change
            .get()
            .map(|c| target_hosts.with_value(|t| plan_relabel(&t.iter().collect::<Vec<_>>(), &c)))
    });
    let parsed = Memo::new(move |_| LabelSelector::parse(&text.get()));
    let reference = StoredValue::new(assignment.clone());
    let moves = Memo::new(move |_| match way.get() {
        Way::Labels => plan.with(|p| {
            p.as_ref().ok().map(|p| {
                all.with_value(|hosts| {
                    assignments.with_value(|a| preview(a, hosts, &p.after, None))
                })
            })
        }),
        Way::Selector => parsed.with(|s| {
            s.as_ref().ok().map(|s| {
                all.with_value(|hosts| {
                    assignments.with_value(|a| {
                        preview(
                            a,
                            hosts,
                            &BTreeMap::new(),
                            Some((&reference.get_value(), s)),
                        )
                    })
                })
            })
        }),
    });
    let has_entry = entry.is_some();
    let can_apply = move || {
        !busy.get()
            && match way.get() {
                Way::Labels => plan.with(|p| p.as_ref().is_ok_and(|p| !p.writes.is_empty())),
                Way::Selector => parsed.with(Result::is_ok) && has_entry,
            }
    };
    let entry = StoredValue::new(entry);
    let apply = move || {
        busy.set(true);
        failed.set(None);
        let name = reference.get_value();
        match way.get_untracked() {
            Way::Labels => {
                let Some(Ok(plan)) = plan.try_get_untracked() else {
                    return;
                };
                spawn_local(async move {
                    match write_labels(session, plan.writes, progress).await {
                        Ok(n) => {
                            edit.done(session, format!("{n} host(s) relabelled for {name}"));
                            on_close.run(());
                        }
                        Err(e) => {
                            let _ = failed.try_set(Some(e));
                            let _ = busy.try_set(false);
                        }
                    }
                });
            }
            Way::Selector => {
                let (Some(entry), Ok(selector)) = (entry.get_value(), parsed.get_untracked())
                else {
                    return;
                };
                spawn_local(async move {
                    match write_selector(session, entry, selector.to_string()).await {
                        Ok(()) => {
                            edit.done(session, format!("selector of {name} saved"));
                            on_close.run(());
                        }
                        Err(e) => {
                            let _ = failed.try_set(Some(e));
                            let _ = busy.try_set(false);
                        }
                    }
                });
            }
        }
    };
    let count = targets.len();
    let title = if adding {
        format!("Add {count} host(s) to {assignment}")
    } else {
        format!("Remove {count} host(s) from {assignment}")
    };
    let keys_view = (!adding && keys.len() > 1).then(|| {
        view! {
            <label>"Label to remove"
                <select on:change=move |ev| key.set(event_target_value(&ev))>
                    {keys.iter().map(|k| view! { <option value=k.clone() selected={let k = k.clone(); move || key.get() == k}>{k.clone()}</option> }).collect_view()}
                </select>
            </label>
        }
    });
    let current_text = current.to_string();
    view! {
        <div class="overlay" on:click=move |_| on_close.run(())>
            <form class="dialog graph-edit" on:click=|ev| { ev.stop_propagation(); }
                on:submit=move |ev| { ev.prevent_default(); if can_apply() { apply(); } }>
                <h2>{title}</h2>
                <p class="muted">{format!("It selects {current_text}: every filter must hold, so choose what changes.")}</p>
                <div class="modes" role="tablist">
                    <button type="button" role="tab" class:active=move || way.get() == Way::Labels
                        on:click=move |_| way.set(Way::Labels)>{if adding { "Label the hosts" } else { "Relabel the hosts" }}</button>
                    <button type="button" role="tab" class:active=move || way.get() == Way::Selector
                        on:click=move |_| way.set(Way::Selector)>"Change its selector"</button>
                </div>
                {move || match way.get() {
                    Way::Labels => match plan.get() {
                        Err(e) => view! { <p class="error">{e}</p> }.into_any(),
                        Ok(p) => {
                            let what = change.get().unwrap_or_default().into_iter().map(|(k, v)| match v {
                                Some(v) => format!("{k}=\"{v}\""),
                                None => format!("remove {k}"),
                            }).collect::<Vec<_>>().join(", ");
                            view! {
                                <p>"On each host's definition: " <code>{what}</code>
                                    {format!(" — {} host(s) to write.", p.writes.len())}</p>
                                {refused_view(&p.refused)}
                            }.into_any()
                        }
                    },
                    Way::Selector => view! {
                        <label>"Selector"
                            <input class="mono" spellcheck="false" prop:value=move || text.get()
                                on:input=move |ev| text.set(event_target_value(&ev)) />
                        </label>
                        {move || parsed.get().err().map(|e| view! { <p class="error">{e}</p> })}
                        {(!adding).then(|| view! { <p class="muted small">"Hosts matching the exclusion later are left out too."</p> })}
                    }.into_any(),
                }}
                {keys_view.map(|v| view! { <Show when=move || way.get() == Way::Labels>{v.clone()}</Show> })}
                <h3>"Preview"</h3>
                {move || moves.get().map(preview_view)}
                {move || failed.get().map(|e| view! { <p class="error">{e}</p> })}
                <div class="actions">
                    <button type="button" on:click=move |_| on_close.run(())>"Cancel"</button>
                    <button class="primary" type="submit" disabled=move || !can_apply()>
                        {move || if busy.get() { format!("Saving… {}", progress.get()) } else { "Apply".to_string() }}
                    </button>
                </div>
            </form>
        </div>
    }
}

/// D32: delete an assignment, confirmed with its host count.
#[component]
pub fn DeleteAssignmentDialog(
    edit: GraphEdit,
    assignment: String,
    hosts: usize,
    #[prop(into)] on_close: Callback<()>,
) -> impl IntoView {
    let session = expect_context::<Session>();
    let entry = StoredValue::new(edit.catalog.assignment(&assignment));
    let failed = RwSignal::new(None::<String>);
    let name = assignment.clone();
    let remove = move || {
        let Some(entry) = entry.get_value() else {
            return;
        };
        let name = name.clone();
        spawn_local(async move {
            let source = str_of(&entry.item, "source_id");
            match delete_item(session, entry.doc_id, source).await {
                Ok(_) => {
                    edit.done(session, format!("assignment {name} deleted"));
                    on_close.run(());
                }
                Err(status) => {
                    session.check(&status);
                    let _ = failed.try_set(Some(status.message().to_string()));
                }
            }
        });
    };
    view! {
        <div class="overlay" on:click=move |_| on_close.run(())>
            <form class="dialog" on:click=|ev| { ev.stop_propagation(); }
                on:submit=move |ev| { ev.prevent_default(); remove(); }>
                <h2>{format!("Delete assignment {assignment}?")}</h2>
                <p>{format!("Its {hosts} host(s) no longer get its expected states (nothing is changed on them).")}</p>
                {move || failed.get().map(|e| view! { <p class="error">{e}</p> })}
                <div class="actions">
                    <button type="button" on:click=move |_| on_close.run(())>"Cancel"</button>
                    <button class="primary danger" type="submit">"Delete"</button>
                </div>
            </form>
        </div>
    }
}

/// D32: a cluster in the right panel: the labels its hosts share (its
/// "selector"), editable; applying relabels them all, after the preview.
#[component]
pub fn ClusterPanel(
    edit: GraphEdit,
    /// Its assignments (empty: the unassigned hosts).
    signature: Vec<String>,
    /// A new assignment from these hosts: its selector.
    #[prop(into)]
    on_new: Callback<String>,
    #[prop(into)] on_close: Callback<()>,
) -> impl IntoView {
    let session = expect_context::<Session>();
    let all = StoredValue::new(edit.snapshot());
    let members: BTreeSet<String> = edit.hosts.with_untracked(|hosts| {
        hosts
            .values()
            .filter(|h| {
                let mut s = h.assignments.clone();
                s.sort();
                s.dedup();
                s == signature
            })
            .map(|h| h.host_id.clone())
            .collect()
    });
    let mine = StoredValue::new(all.with_value(|hosts| {
        hosts
            .iter()
            .filter(|h| members.contains(&h.id))
            .cloned()
            .collect::<Vec<_>>()
    }));
    let original = mine.with_value(|m| shared(&m.iter().collect::<Vec<_>>()));
    let selector = mine.with_value(|m| shared_selector(&m.iter().collect::<Vec<_>>()));
    let rows = RwSignal::new(
        original
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>(),
    );
    let assignments = StoredValue::new(edit.catalog.assignments.with_untracked(|a| selectors(a)));
    let original = StoredValue::new(original);
    let progress = RwSignal::new(0usize);
    let busy = RwSignal::new(false);
    let failed = RwSignal::new(None::<String>);
    let change = Memo::new(move |_| -> Result<LabelChange, String> {
        let mut change = LabelChange::new();
        let wanted = rows.get();
        for (k, v) in &wanted {
            let k = k.trim();
            if k.is_empty() {
                continue;
            }
            if !crate::labels::valid_key(k) || RESERVED.contains(&k) {
                return Err(format!(
                    "label key {k:?}: letters, digits and '_' (not host or source)"
                ));
            }
            if original.with_value(|o| o.get(k) != Some(v)) {
                change.insert(k.to_string(), Some(v.clone()));
            }
        }
        original.with_value(|o| {
            for k in o.keys() {
                if !wanted.iter().any(|(w, _)| w.trim() == k) {
                    change.insert(k.clone(), None);
                }
            }
        });
        Ok(change)
    });
    let plan = Memo::new(move |_| {
        change
            .get()
            .map(|c| mine.with_value(|m| plan_relabel(&m.iter().collect::<Vec<_>>(), &c)))
    });
    let moves = Memo::new(move |_| {
        plan.with(|p| {
            p.as_ref().ok().map(|p| {
                all.with_value(|hosts| {
                    assignments.with_value(|a| preview(a, hosts, &p.after, None))
                })
            })
        })
    });
    let apply = move || {
        let Some(Ok(plan)) = plan.get_untracked().into() else {
            return;
        };
        busy.set(true);
        failed.set(None);
        spawn_local(async move {
            match write_labels(session, plan.writes, progress).await {
                Ok(n) => {
                    edit.done(session, format!("{n} host(s) relabelled"));
                    on_close.run(());
                }
                Err(e) => {
                    let _ = failed.try_set(Some(e));
                    let _ = busy.try_set(false);
                }
            }
        });
    };
    let can_apply =
        move || !busy.get() && plan.with(|p| p.as_ref().is_ok_and(|p| !p.writes.is_empty()));
    let count = members.len();
    let heading = if signature.is_empty() {
        "Unassigned hosts".to_string()
    } else {
        signature.join(" + ")
    };
    let can_create = move || !edit.catalog.sources.editable().is_empty();
    let indices = move || rows.with(|r| (0..r.len()).collect::<Vec<usize>>());
    let selector_text = selector.clone();
    view! {
        <div class="cluster-panel">
            <div class="host-head">
                <h2>{heading}</h2>
                <button title="Close" on:click=move |_| on_close.run(())>"×"</button>
            </div>
            <p class="muted">{format!("{count} host(s) selected by exactly these assignments. They share {selector}.")}</p>
            <h3>"Shared labels"</h3>
            <p class="muted small">"Change, add or remove a label: every host of the cluster gets the change."</p>
            <div class="label-rows">
                <For each=indices key=|i| *i let:i>
                    <div class="label-row">
                        <input class="mono" placeholder="key" prop:value=move || rows.with(|r| r.get(i).map(|r| r.0.clone()).unwrap_or_default())
                            on:input=move |ev| { let v = event_target_value(&ev); rows.update(|r| if let Some(r) = r.get_mut(i) { r.0 = v }); } />
                        <span>"="</span>
                        <input class="mono" placeholder="value" prop:value=move || rows.with(|r| r.get(i).map(|r| r.1.clone()).unwrap_or_default())
                            on:input=move |ev| { let v = event_target_value(&ev); rows.update(|r| if let Some(r) = r.get_mut(i) { r.1 = v }); } />
                        <button type="button" class="chip-x" title="Remove this label from every host"
                            on:click=move |_| rows.update(|r| { if i < r.len() { r.remove(i); } })>"×"</button>
                    </div>
                </For>
                <button type="button" class="small" on:click=move |_| rows.update(|r| r.push((String::new(), String::new())))>"+ Label"</button>
            </div>
            {move || match plan.get() {
                Err(e) => view! { <p class="error">{e}</p> }.into_any(),
                Ok(p) => view! {
                    {refused_view(&p.refused)}
                    <h3>"Preview"</h3>
                    {moves.get().map(preview_view)}
                }.into_any(),
            }}
            {move || failed.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="toolbar">
                <button class="primary" disabled=move || !can_apply() on:click=move |_| apply()>
                    {move || if busy.get() { format!("Saving… {}", progress.get()) } else { "Relabel the hosts".to_string() }}
                </button>
                <button disabled=move || !can_create() title="A new assignment selecting the labels these hosts share"
                    on:click=move |_| on_new.run(selector_text.clone())>"New assignment from these hosts"</button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, labels: &[(&str, &str)], def: Result<&str, &str>) -> EditHost {
        let own: BTreeMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        EditHost {
            id: id.into(),
            sources: vec!["ui".into()],
            definition: def
                .map(|source| Entry {
                    doc_id: format!("hostdef:{source}:{id}"),
                    item: json!({ "type": "hostdef", "source_id": source, "host_id": id, "labels": own }),
                })
                .map_err(str::to_string),
            own,
        }
    }

    fn sel(text: &str) -> LabelSelector {
        LabelSelector::parse(text).unwrap()
    }

    #[test]
    fn joining_sets_the_equality_labels() {
        let a = host("a", &[("env", "dev")], Ok("ui"));
        let change = join_change(&sel(r#"{role="web", env="prod"}"#), &[&a]).unwrap();
        assert_eq!(change.len(), 2);
        assert_eq!(change["env"], Some("prod".into()));
        let plan = plan_relabel(&[&a], &change);
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(plan.writes[0].1["role"], "web");
        assert!(sel(r#"{role="web", env="prod"}"#).matches(&plan.after["a"]));
        // regex, negation and built-in labels cannot be met by labels
        assert!(join_change(&sel(r#"{role=~"web|api"}"#), &[&a]).is_err());
        assert!(join_change(&sel(r#"{host="b"}"#), &[&a]).is_err());
        assert!(join_change(&sel(r#"{env!="dev"}"#), &[&a]).is_err());
    }

    #[test]
    fn relabelling_writes_only_editable_definitions() {
        let git = host("g", &[], Err("defined in source git: edit it there"));
        let ok = host("o", &[], Ok("ui"));
        let same = host("s", &[("role", "web")], Ok("ui"));
        let change: LabelChange = [("role".to_string(), Some("web".to_string()))].into();
        let plan = plan_relabel(&[&git, &ok, &same], &change);
        assert_eq!(plan.writes.len(), 1, "{plan:?}");
        assert_eq!(plan.refused.len(), 1);
        // a label reported by the agent cannot be removed from the definition
        let mut agent = host("r", &[], Ok("ui"));
        agent.own.insert("role".into(), "web".into());
        let plan = plan_relabel(&[&agent], &[("role".to_string(), None)].into());
        assert!(plan.writes.is_empty() && plan.refused.len() == 1);
    }

    #[test]
    fn leaving_removes_a_needed_label() {
        assert_eq!(
            leave_keys(&sel(r#"{role="web", env!="dev", tier=~"a|"}"#)),
            vec!["role".to_string()]
        );
    }

    #[test]
    fn suggestions_use_shared_labels_else_names() {
        let a = host("a", &[("role", "web"), ("env", "prod")], Ok("ui"));
        let b = host("b", &[("role", "web"), ("env", "dev")], Ok("ui"));
        let c = host("c.1", &[("role", "db"), ("env", "dev")], Ok("ui"));
        assert_eq!(widened(&[&a], &[&b]), r#"{role="web"}"#);
        assert_eq!(widened(&[&a], &[&c]), r#"{host=~"a|c\\.1"}"#);
        // exclusion: a value the removed share and the kept lack
        assert_eq!(
            narrowed(&sel(r#"{role="web"}"#), &[&a], &[&b]),
            r#"{role="web", env!="dev"}"#
        );
        assert_eq!(narrowed(&sel("{}"), &[&b], &[&c]), r#"{role!="db"}"#);
        let d = host("d", &[("role", "web"), ("env", "dev")], Ok("ui"));
        assert_eq!(narrowed(&sel("{}"), &[&b], &[&d]), r#"{host!~"d"}"#);
        assert_eq!(shared_selector(&[&b, &c]), r#"{env="dev"}"#);
    }

    #[test]
    fn preview_lists_hosts_joining_and_leaving() {
        let a = host("a", &[("role", "web")], Ok("ui"));
        let b = host("b", &[("role", "db")], Ok("ui"));
        let hosts = vec![a.clone(), b.clone()];
        let assignments = vec![
            ("ui/web".to_string(), sel(r#"{role="web"}"#)),
            ("ui/db".to_string(), sel(r#"{role="db"}"#)),
        ];
        // b relabelled web: joins web, leaves db
        let change: LabelChange = [("role".to_string(), Some("web".to_string()))].into();
        let plan = plan_relabel(&[&b], &change);
        let moves = preview(&assignments, &hosts, &plan.after, None);
        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0].joined, vec!["b".to_string()]);
        assert_eq!(moves[1].left, vec!["b".to_string()]);
        // web's selector widened to every host
        let all = sel("{}");
        let moves = preview(
            &assignments,
            &hosts,
            &BTreeMap::new(),
            Some(("ui/web", &all)),
        );
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].joined, vec!["b".to_string()]);
    }

    #[test]
    fn hosts_get_their_only_editable_definition() {
        let def = |source: &str, id: &str| Entry {
            doc_id: format!("hostdef:{source}:{id}"),
            item: json!({ "type": "hostdef", "source_id": source, "host_id": id }),
        };
        let summary = |id: &str, agent: &str| api::HostSummary {
            host_id: id.into(),
            agent: agent.into(),
            ..Default::default()
        };
        let defs = vec![
            def("ui", "a"),
            def("git", "b"),
            def("ui", "c"),
            def("git", "c"),
        ];
        let hosts = [
            summary("a", ""),
            summary("b", ""),
            summary("c", ""),
            summary("r", "regops"),
        ];
        let edit = edit_hosts(hosts.iter(), &defs, |s| s != "git");
        assert!(edit[0].definition.is_ok());
        assert!(edit[1].definition.is_err());
        assert!(edit[2].definition.is_err());
        assert!(edit[3].definition.is_err());
        assert_eq!(edit[3].sources, vec!["regops".to_string()]);
    }
}
