//! One-shot raw attributes (D23): run once with Assess or Reach on the hosts
//! of a map group. Their outcomes are kept in the hosts' history and never
//! change a host's colour. Written with a form (**Builder**, generated from
//! regent-sdk's attribute schema, `crate::builder`) or as YAML (**Code**),
//! like Grafana's query editor; switching converts through backend
//! (`ConvertAttributes`, the SDK's own serde), which also checks the
//! attributes as they are built. The dialog follows the run's jobs live; the
//! browser remembers the last runs to show their results again.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Session;
use crate::builder::{AttributeBuilder, Schema, strip_nulls};
use crate::client::{api, authed, client, command_id, format_ms, storage};
use crate::expected::{GroupTarget, job_label};
use crate::pages::{badge, watch_jobs_of};

const RUNS_KEY: &str = "regency.oneshot.runs";
const KEPT_RUNS: usize = 20;

/// The Code view's placeholder (a syntax hint, not a value).
const PLACEHOLDER: &str = r#"Name: motd present
Privilege: !None
Detail: !LineInFile
  FilePath: /etc/motd
  State: !Present
    Create: true
  Line: !Raw "Managed by regency"
"#;

/// A past run, remembered by the browser.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PastRun {
    pub command_id: String,
    pub target: String,
    pub job: i32,
    pub at: i64,
    pub yaml: String,
}

fn past_runs() -> Vec<PastRun> {
    storage()
        .and_then(|s| s.get_item(RUNS_KEY).ok().flatten())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn remember(run: PastRun) {
    let mut runs = past_runs();
    runs.insert(0, run);
    runs.truncate(KEPT_RUNS);
    if let (Some(storage), Ok(text)) = (storage(), serde_json::to_string(&runs)) {
        let _ = storage.set_item(RUNS_KEY, &text);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Builder,
    Code,
}

enum Input {
    Yaml(String),
    Json(String),
}

/// Both forms of the attributes, by backend (the SDK's serde): `(yaml,
/// json, problem)`; `Err` when they are not attributes at all.
async fn convert(
    session: Session,
    input: Input,
) -> Result<(String, String, Option<String>), String> {
    use api::convert_attributes_request::Input as Field;
    let request = api::ConvertAttributesRequest {
        input: Some(match input {
            Input::Yaml(yaml) => Field::Yaml(yaml),
            Input::Json(json) => Field::Json(json),
        }),
    };
    match client()
        .convert_attributes(authed(request, &session.token()))
        .await
    {
        Ok(response) => {
            let response = response.into_inner();
            let problem = (!response.error.is_empty()).then_some(response.error);
            if response.json.is_empty() {
                Err(problem.unwrap_or_else(|| "not an attribute".into()))
            } else {
                Ok((response.yaml, response.json, problem))
            }
        }
        Err(status) => {
            session.check(&status);
            Err(status.message().to_string())
        }
    }
}

/// The dialog, for `target` (its host count on the map: `hosts`).
#[component]
pub fn OneShotDialog(target: GroupTarget, hosts: usize, on_close: Callback<()>) -> impl IntoView {
    let session = expect_context::<Session>();
    let mode = RwSignal::new(Mode::Code);
    // the user starts from nothing: no example, nothing of the assignment
    let yaml = RwSignal::new(String::new());
    let data = RwSignal::new(Value::Array(Vec::new()));
    let schema = RwSignal::new(None::<Arc<Schema>>);
    // why the builder is not available, if so
    let no_builder = RwSignal::new(None::<String>);
    // the builder's attributes as checked by backend: Ok(yaml) or the problem
    let check = RwSignal::new(None::<Result<String, String>>);
    let job = RwSignal::new(api::JobKind::Assess);
    let error = RwSignal::new(None::<String>);
    // information (not an error), e.g. switching with incomplete attributes
    let notice = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let runs = RwSignal::new(past_runs());
    // the run whose results are shown
    let shown = RwSignal::new(None::<PastRun>);
    let description = target.describe();
    let selector = target.selector();

    // the schema, then one blank attribute in the builder (Grafana starts
    // there too)
    spawn_local(async move {
        let request = api::GetAttributeSchemaRequest {};
        match client()
            .get_attribute_schema(authed(request, &session.token()))
            .await
        {
            Ok(response) => {
                match serde_json::from_str::<Value>(&response.into_inner().schema_json) {
                    Ok(root) => {
                        let loaded = Schema::new(root);
                        let fresh = loaded.blank_for(loaded.root());
                        let _ = data.try_set(Value::Array(vec![fresh]));
                        let _ = schema.try_set(Some(Arc::new(loaded)));
                        let _ = mode.try_set(Mode::Builder);
                    }
                    Err(e) => {
                        let _ = no_builder.try_set(Some(format!("unreadable schema: {e}")));
                    }
                }
            }
            Err(status) => {
                session.check(&status);
                let _ = no_builder.try_set(Some(status.message().to_string()));
            }
        }
    });

    // builder: check the attributes as they change (debounced)
    let generation = RwSignal::new(0u64);
    Effect::new(move |_| {
        let json = data.with(|d| d.to_string());
        if mode.get() != Mode::Builder {
            return;
        }
        let this = generation.get_untracked() + 1;
        generation.set(this);
        set_timeout(
            move || {
                if generation.get_untracked() != this {
                    return;
                }
                spawn_local(async move {
                    let result = convert(session, Input::Json(json)).await;
                    if generation.get_untracked() == this {
                        let _ = check.try_set(Some(match result {
                            Ok((yaml, _, None)) => Ok(yaml),
                            Ok((_, _, Some(problem))) | Err(problem) => Err(problem),
                        }));
                    }
                });
            },
            Duration::from_millis(400),
        );
    });

    let to_code = move || {
        error.set(None);
        notice.set(None);
        let json = data.with(|d| d.to_string());
        spawn_local(async move {
            match convert(session, Input::Json(json)).await {
                Ok((text, _, _)) => {
                    let _ = yaml.try_set(text);
                }
                Err(e) => {
                    // not attributes yet: Code keeps its own text
                    let _ = notice.try_set(Some(format!(
                        "The builder's attributes are not complete ({e}): the code view keeps its own text."
                    )));
                }
            }
            let _ = mode.try_set(Mode::Code);
        });
    };
    let to_builder = move || {
        error.set(None);
        notice.set(None);
        let text = yaml.get_untracked();
        if text.trim().is_empty() {
            // nothing written: one blank attribute
            if let Some(loaded) = schema.get_untracked() {
                data.set(Value::Array(vec![loaded.blank_for(loaded.root())]));
                check.set(None);
                mode.set(Mode::Builder);
            }
            return;
        }
        spawn_local(async move {
            match convert(session, Input::Yaml(text)).await {
                Ok((_, json, _)) => match serde_json::from_str(&json) {
                    Ok(value) => {
                        let _ = data.try_set(strip_nulls(value));
                        let _ = check.try_set(None);
                        let _ = mode.try_set(Mode::Builder);
                    }
                    Err(e) => {
                        let _ = error.try_set(Some(e.to_string()));
                    }
                },
                Err(e) => {
                    let _ = error.try_set(Some(format!(
                        "this YAML cannot be shown in the builder: {e}"
                    )));
                }
            }
        });
    };

    let run = {
        let description = description.clone();
        move || {
            let (description, selector) = (description.clone(), selector.clone());
            busy.set(true);
            error.set(None);
            spawn_local(async move {
                // the builder's attributes go through backend's YAML first
                let text = if mode.get_untracked() == Mode::Builder {
                    match convert(session, Input::Json(data.with_untracked(|d| d.to_string())))
                        .await
                    {
                        Ok((text, _, None)) => text,
                        Ok((_, _, Some(problem))) | Err(problem) => {
                            let _ = error.try_set(Some(problem));
                            let _ = busy.try_set(false);
                            return;
                        }
                    }
                } else {
                    yaml.get_untracked()
                };
                let command = command_id("oneshot");
                let request = api::RunNowRequest {
                    command_id: command.clone(),
                    selector: Some(selector),
                    job: job.get_untracked() as i32,
                    pool: String::new(),
                    one_shot_yaml: text.clone(),
                };
                match client().run_now(authed(request, &session.token())).await {
                    Ok(_) => {
                        let past = PastRun {
                            command_id: command,
                            target: description,
                            job: job.get_untracked() as i32,
                            at: js_sys::Date::now() as i64,
                            yaml: text,
                        };
                        remember(past.clone());
                        let _ = runs.try_set(past_runs());
                        let _ = shown.try_set(Some(past));
                    }
                    Err(status) => {
                        session.check(&status);
                        let _ = error.try_set(Some(status.message().to_string()));
                    }
                }
                let _ = busy.try_set(false);
            });
        }
    };
    let reach = move || job.get() == api::JobKind::Reach;
    // nothing to run yet: no text, or builder attributes not valid
    let empty = move || match mode.get() {
        Mode::Code => yaml.with(|y| y.trim().is_empty()),
        Mode::Builder => !matches!(check.get(), Some(Ok(_))),
    };
    // what is still blank, from the schema (clearer than serde's complaint)
    let to_fill = Memo::new(move |_| {
        schema.with(|loaded| {
            let Some(loaded) = loaded else {
                return Vec::new();
            };
            data.with(|d| {
                let attributes = d.as_array().cloned().unwrap_or_default();
                let several = attributes.len() > 1;
                attributes
                    .iter()
                    .enumerate()
                    .flat_map(|(i, attribute)| {
                        loaded
                            .missing(loaded.root(), attribute)
                            .into_iter()
                            .map(move |path| {
                                if several {
                                    format!("#{} {path}", i + 1)
                                } else {
                                    path
                                }
                            })
                    })
                    .collect::<Vec<_>>()
            })
        })
    });
    // untouched blank attributes: a hint rather than serde's complaint
    let untouched = move || {
        schema.with(|loaded| {
            loaded.as_ref().is_some_and(|loaded| {
                let fresh = loaded.blank_for(loaded.root());
                data.with(|d| d.as_array().is_some_and(|a| a.iter().all(|x| *x == fresh)))
            })
        })
    };
    let builder_title = move || {
        no_builder
            .get()
            .map(|why| format!("Builder unavailable: {why}"))
            .unwrap_or_else(|| "Build the attribute with a form".into())
    };

    view! {
        <div class="overlay" on:click=move |_| on_close.run(())>
            <form class="dialog wide" on:click=|ev| { ev.stop_propagation(); }
                on:submit=move |ev| { ev.prevent_default(); run(); }>
                <div class="dialog-head">
                    <h2>{format!("Run an attribute on {description}")}</h2>
                    <div class="modes" role="tablist">
                        <button type="button" role="tab" title=builder_title
                            class:active=move || mode.get() == Mode::Builder
                            disabled=move || schema.with(Option::is_none)
                            on:click=move |_| if mode.get_untracked() == Mode::Code { to_builder() }>"Builder"</button>
                        <button type="button" role="tab" title="Write the attribute as YAML"
                            class:active=move || mode.get() == Mode::Code
                            on:click=move |_| if mode.get_untracked() == Mode::Builder { to_code() }>"Code"</button>
                    </div>
                </div>
                <p class="muted">{format!("{hosts} hosts on the map. One-shot: the results are kept in each host's history but do not change its state on the map.")}</p>
                {move || no_builder.get().map(|why| view! {
                    <p class="muted small">{format!("Builder unavailable ({why}): write the attribute as YAML.")}</p>
                })}
                {move || match (mode.get(), schema.get()) {
                    (Mode::Builder, Some(schema)) => view! {
                        <AttributeBuilder schema data />
                        <p class="b-check">{move || match check.get() {
                            _ if untouched() => view! { <span class="muted">"Choose the attribute's nature to start."</span> }.into_any(),
                            _ if !to_fill.with(Vec::is_empty) => view! { <span class="muted">{format!("To fill in: {}", to_fill.get().join(", "))}</span> }.into_any(),
                            None => view! { <span class="muted">"checking…"</span> }.into_any(),
                            Some(Ok(_)) => view! { <span class="notice">"✓ valid attributes"</span> }.into_any(),
                            Some(Err(problem)) => view! { <span class="error">{problem}</span> }.into_any(),
                        }}</p>
                    }.into_any(),
                    _ => view! {
                        <label>"Attribute (YAML: one attribute, a list, or an expected state)"
                            <textarea class="yaml" rows="12" spellcheck="false" placeholder=PLACEHOLDER
                                prop:value=move || yaml.get()
                                on:input=move |ev| yaml.set(event_target_value(&ev))></textarea>
                        </label>
                    }.into_any(),
                }}
                <div class="toolbar">
                    <label class="inline"><input type="radio" name="job" checked=move || !reach()
                        on:change=move |_| job.set(api::JobKind::Assess) />"Assess"</label>
                    <label class="inline"><input type="radio" name="job" checked=reach
                        on:change=move |_| job.set(api::JobKind::Reach) />"Reach"</label>
                </div>
                <Show when=reach>
                    <p class="reach-warning">"Reach changes the hosts to match this attribute."</p>
                </Show>
                {move || notice.get().map(|n| view! { <p class="muted small">{n}</p> })}
                {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                <div class="actions">
                    <button type="button" on:click=move |_| on_close.run(())>"Close"</button>
                    <button class="primary" type="submit" disabled=move || busy.get() || empty()>
                        {move || job_label(job.get() as i32)}
                    </button>
                </div>
                {move || shown.get().map(|run| view! { <RunResults run /> })}
                <Show when=move || runs.with(|r| !r.is_empty())>
                    <details class="past-runs">
                        <summary>"Previous one-shot runs (this browser)"</summary>
                        <ul>
                            <For each=move || runs.get() key=|r| r.command_id.clone() let:past>
                                {
                                    let label = format!("{} — {} on {}", format_ms(past.at), job_label(past.job), past.target);
                                    let chosen = past.clone();
                                    view! {
                                        <li>
                                            <a href="#" on:click=move |ev| {
                                                ev.prevent_default();
                                                yaml.set(chosen.yaml.clone());
                                                shown.set(Some(chosen.clone()));
                                                if mode.get_untracked() == Mode::Builder {
                                                    to_builder();
                                                }
                                            }>{label}</a>
                                        </li>
                                    }
                                }
                            </For>
                        </ul>
                    </details>
                </Show>
            </form>
        </div>
    }
}

/// Live results of one run: one row per host.
#[component]
fn RunResults(run: PastRun) -> impl IntoView {
    let session = expect_context::<Session>();
    let jobs = RwSignal::new(BTreeMap::<String, api::JobSummary>::new());
    let error = RwSignal::new(None::<String>);
    // runs are recent: a month of look-back is plenty
    watch_jobs_of(
        Some(api::watch_jobs_request::Filter::Origin(
            run.command_id.clone(),
        )),
        24 * 31,
        jobs,
        error,
    );
    let detail = RwSignal::new(None::<(String, String)>);
    let show = move |job: api::JobSummary| {
        spawn_local(async move {
            let request = api::GetOutcomeRequest {
                job_id: job.job_id.clone(),
                month: job.outcome_month.clone(),
            };
            let text = match client()
                .get_outcome(authed(request, &session.token()))
                .await
            {
                Ok(response) => {
                    let json = response.into_inner().result_json;
                    serde_json::from_str::<serde_json::Value>(&json)
                        .and_then(|v| serde_json::to_string_pretty(&v))
                        .unwrap_or(json)
                }
                Err(status) => format!("cannot load the outcome: {}", status.message()),
            };
            let _ = detail.try_set(Some((job.host_id, text)));
        });
    };
    let summary = move || {
        jobs.with(|jobs| {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for job in jobs.values() {
                let key = if job.compliance.is_empty() {
                    job.state.clone()
                } else {
                    job.compliance.clone()
                };
                *counts.entry(key).or_default() += 1;
            }
            counts.into_iter().collect::<Vec<_>>()
        })
    };
    view! {
        <h3>{format!("Results — {} on {} ({})", job_label(run.job), run.target, format_ms(run.at))}</h3>
        {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
        <div class="counts">
            {move || summary().into_iter().map(|(state, n)| view! { <span>{badge(state)} " " {n}</span> }).collect::<Vec<_>>()}
        </div>
        <div class="results">
            <table>
                <thead><tr><th>"Host"</th><th>"Job"</th><th>"Result"</th><th></th></tr></thead>
                <tbody>
                    <For each=move || jobs.with(|j| j.values().cloned().collect::<Vec<_>>())
                        key=|j| (j.job_id.clone(), j.state.clone(), j.compliance.clone())
                        let:job>
                        {
                            let finished = !job.outcome_month.is_empty();
                            let chosen = job.clone();
                            view! {
                                <tr>
                                    <td>{job.host_id.clone()}</td>
                                    <td>{badge(job.state.clone())}</td>
                                    <td>{(!job.compliance.is_empty()).then(|| badge(job.compliance.clone()))} " " <span class="muted">{job.error.clone()}</span></td>
                                    <td>{finished.then(|| view! { <button type="button" class="small" on:click=move |_| show(chosen.clone())>"Details"</button> })}</td>
                                </tr>
                            }
                        }
                    </For>
                </tbody>
            </table>
            <Show when=move || jobs.with(BTreeMap::is_empty)>
                <p class="muted">"Waiting for the jobs…"</p>
            </Show>
        </div>
        {move || detail.get().map(|(host, text)| view! {
            <h3>{format!("Outcome on {host}")}</h3>
            <pre class="outcome">{text}</pre>
        })}
    }
}
