//! CronJobs page: CRUD on the recurring orders (D3, D16, D23). An order
//! runs an assignment's hosts (Assess or Reach) on a fixed interval or a
//! cron expression. Writes are `order.upsert` / `order.delete` commands
//! (D10): backend checks the order (schedule, assignment), dbwriter applies
//! it, so the list is reloaded a moment after a change.

use std::collections::BTreeMap;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::Session;
use crate::client::{api, authed, client, command_id, format_ms};
use crate::config::{Entry, confirm, load_catalog_with, message_view, report, str_of};
use crate::expected::job_label;
use crate::feeds::Feeds;

/// How long dbwriter usually needs to apply an order command.
const RELOAD_AFTER: Duration = Duration::from_millis(1200);

type Message = RwSignal<Option<(bool, String)>>;

/// The form of a new or edited order.
#[derive(Clone, Debug, PartialEq)]
struct OrderForm {
    order_id: String,
    /// Editing an existing order: its id is fixed.
    existing: bool,
    cron: bool,
    every: String,
    expression: String,
    timezone: String,
    assignment: String,
    job: i32,
    pool: String,
    enabled: bool,
}

impl OrderForm {
    fn new(assignment: String) -> Self {
        Self {
            order_id: String::new(),
            existing: false,
            cron: false,
            every: "1h".into(),
            expression: "0 3 * * *".into(),
            timezone: "UTC".into(),
            assignment,
            job: api::JobKind::Assess as i32,
            pool: String::new(),
            enabled: true,
        }
    }

    fn from_order(order: &api::Order) -> Self {
        let mut form = Self::new(order.assignment.clone());
        form.order_id = order.order_id.clone();
        form.existing = true;
        match &order.schedule {
            Some(api::order::Schedule::EverySeconds(seconds)) => {
                form.every = format_duration(*seconds)
            }
            Some(api::order::Schedule::Cron(cron)) => {
                form.cron = true;
                form.expression = cron.expression.clone();
                form.timezone = cron.timezone.clone();
            }
            None => {}
        }
        form.job = order.job;
        form.pool = order.pool.clone();
        form.enabled = order.enabled;
        form
    }

    /// The order to send; backend checks the rest (cron syntax, timezone,
    /// assignment).
    fn to_order(&self) -> Result<api::Order, String> {
        let order_id = self.order_id.trim();
        if order_id.is_empty() || order_id.contains(['/', ':']) {
            return Err("name must be non-empty, without '/' or ':'".into());
        }
        if self.assignment.is_empty() {
            return Err("pick an assignment".into());
        }
        let schedule = if self.cron {
            if self.expression.trim().is_empty() {
                return Err("cron expression is required".into());
            }
            api::order::Schedule::Cron(api::CronSchedule {
                expression: self.expression.trim().to_string(),
                timezone: self.timezone.trim().to_string(),
            })
        } else {
            api::order::Schedule::EverySeconds(parse_duration(&self.every)?)
        };
        Ok(api::Order {
            order_id: order_id.to_string(),
            schedule: Some(schedule),
            assignment: self.assignment.clone(),
            job: self.job,
            pool: self.pool.trim().to_string(),
            enabled: self.enabled,
            ..Default::default()
        })
    }
}

/// `90`, `90s`, `15m`, `1h30m`, `1d` → seconds (> 0).
fn parse_duration(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let invalid = || format!("interval `{text}`: e.g. 300, 15m, 1h30m, 1d");
    if let Ok(seconds) = text.parse::<u64>() {
        return (seconds > 0).then_some(seconds).ok_or_else(invalid);
    }
    let mut total: u64 = 0;
    let mut number = String::new();
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            _ => return Err(invalid()),
        };
        let value: u64 = number.parse().map_err(|_| invalid())?;
        total = value
            .checked_mul(unit)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(invalid)?;
        number.clear();
    }
    if !number.is_empty() || total == 0 {
        return Err(invalid());
    }
    Ok(total)
}

/// Seconds → `1h30m`, `15m`, `45s`, `1d`.
fn format_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "0s".into();
    }
    let mut rest = seconds;
    let mut text = String::new();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)] {
        if rest >= size {
            text.push_str(&format!("{}{unit}", rest / size));
            rest %= size;
        }
    }
    text
}

fn schedule_text(order: &api::Order) -> String {
    match &order.schedule {
        Some(api::order::Schedule::EverySeconds(seconds)) => {
            format!("every {}", format_duration(*seconds))
        }
        Some(api::order::Schedule::Cron(cron)) => {
            let zone = if cron.timezone.is_empty() {
                "UTC"
            } else {
                &cron.timezone
            };
            format!("{} ({zone})", cron.expression)
        }
        None => "?".into(),
    }
}

/// Next fire time of an interval order (D3 slots aligned on the epoch), in
/// ms; cron orders are computed by the scheduler only.
fn next_fire_ms(order: &api::Order, now_ms: f64) -> Option<i64> {
    match order.schedule {
        Some(api::order::Schedule::EverySeconds(seconds)) if seconds > 0 => {
            let now = (now_ms / 1000.0) as u64;
            Some(((now / seconds + 1) * seconds * 1000) as i64)
        }
        _ => None,
    }
}

/// An order's jobs of the last hour (the app-wide feed): count, how many
/// ended well / badly, latest creation time.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Recent {
    jobs: usize,
    ok: usize,
    bad: usize,
    last: i64,
}

fn recent_of(jobs: &BTreeMap<String, api::JobSummary>) -> BTreeMap<String, Recent> {
    let mut recent: BTreeMap<String, Recent> = BTreeMap::new();
    for job in jobs.values() {
        let entry = recent.entry(job.origin.clone()).or_default();
        entry.jobs += 1;
        match job.compliance.as_str() {
            "compliant" | "reach_success" => entry.ok += 1,
            "not_compliant" | "reach_failed" | "error" => entry.bad += 1,
            _ => {}
        }
        entry.last = entry.last.max(job.created_at);
    }
    recent
}

fn load_orders(session: Session, into: RwSignal<Vec<api::Order>>, error: RwSignal<Option<String>>) {
    spawn_local(async move {
        match client()
            .list_orders(authed(api::ListOrdersRequest {}, &session.token()))
            .await
        {
            Ok(response) => {
                let mut orders = response.into_inner().orders;
                orders.sort_by(|a, b| a.order_id.cmp(&b.order_id));
                let _ = into.try_set(orders);
                let _ = error.try_set(None);
            }
            Err(status) => {
                session.check(&status);
                let _ = error.try_set(Some(status.message().to_string()));
            }
        }
    });
}

fn upsert(
    session: Session,
    message: Message,
    order: api::Order,
    on_saved: impl Fn() + Send + Sync + 'static,
    reload: impl Fn() + Copy + Send + Sync + 'static,
) {
    spawn_local(async move {
        let request = api::UpsertOrderRequest {
            command_id: command_id("order"),
            order: Some(order),
        };
        let result = client()
            .upsert_order(authed(request, &session.token()))
            .await
            .map(|_| "accepted: the list updates in a moment".to_string());
        let ok = result.is_ok();
        report(message, session, result);
        if ok {
            on_saved();
            set_timeout(reload, RELOAD_AFTER);
        }
    });
}

fn remove(
    session: Session,
    message: Message,
    order_id: String,
    reload: impl Fn() + Copy + Send + Sync + 'static,
) {
    spawn_local(async move {
        let request = api::DeleteOrderRequest {
            command_id: command_id("order-delete"),
            order_id,
        };
        let result = client()
            .delete_order(authed(request, &session.token()))
            .await
            .map(|_| "deletion accepted".to_string());
        report(message, session, result);
        set_timeout(reload, RELOAD_AFTER);
    });
}

#[component]
pub fn CronJobs() -> impl IntoView {
    let session = expect_context::<Session>();
    let feeds = expect_context::<Feeds>();
    let orders = RwSignal::new(Vec::<api::Order>::new());
    let assignments = RwSignal::new(Vec::<Entry>::new());
    let error = RwSignal::new(None::<String>);
    let message: Message = RwSignal::new(None);
    let reload = move || load_orders(session, orders, error);
    reload();
    load_catalog_with(session, "assignment", assignments, error);
    let assignment_refs = move || {
        assignments.with(|list| {
            list.iter()
                .map(|e| {
                    format!(
                        "{}/{}",
                        str_of(&e.item, "source_id"),
                        str_of(&e.item, "name")
                    )
                })
                .collect::<Vec<_>>()
        })
    };
    let recent = Memo::new(move |_| feeds.jobs.with(recent_of));
    // "now" for the next fire times, refreshed every 10 s
    let now = RwSignal::new(js_sys::Date::now());
    if let Ok(handle) = set_interval_with_handle(
        move || {
            let _ = now.try_set(js_sys::Date::now());
        },
        Duration::from_secs(10),
    ) {
        on_cleanup(move || handle.clear());
    }

    let form = RwSignal::new(None::<OrderForm>);
    let save = move || {
        let Some(current) = form.get_untracked() else {
            return;
        };
        match current.to_order() {
            Err(e) => message.set(Some((false, e))),
            Ok(order) => upsert(
                session,
                message,
                order,
                move || {
                    let _ = form.try_set(None);
                },
                reload,
            ),
        }
    };
    let set_enabled = move |mut order: api::Order, enabled: bool| {
        order.enabled = enabled;
        order.author.clear();
        order.rev.clear();
        upsert(session, message, order, || {}, reload);
    };
    let edit = move |f: fn(&mut OrderForm, String)| {
        move |ev: leptos::ev::Event| {
            let value = event_target_value(&ev);
            form.update(|form| {
                if let Some(form) = form {
                    f(form, value)
                }
            });
        }
    };
    let read = move |f: fn(&OrderForm) -> String| {
        move || form.with(|form| form.as_ref().map(f).unwrap_or_default())
    };

    view! {
        <h1>"CronJob " <span class="muted">"(recurring Assess / Reach of an assignment's hosts)"</span></h1>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}
        <div class="toolbar">
            <button class="primary"
                on:click=move |_| form.set(Some(OrderForm::new(assignment_refs().first().cloned().unwrap_or_default())))>
                "New CronJob"
            </button>
            <button on:click=move |_| reload()>"Refresh list"</button>
        </div>
        <Show when=move || form.get().is_some()>
            <div class="card form">
                <h2>{move || if form.with(|f| f.as_ref().is_some_and(|f| f.existing)) { "Edit CronJob" } else { "New CronJob" }}</h2>
                <label>"Name"
                    <input prop:value=read(|f| f.order_id.clone()) placeholder="web-assess"
                        prop:disabled=move || form.with(|f| f.as_ref().is_some_and(|f| f.existing))
                        on:input=edit(|f, v| f.order_id = v) />
                </label>
                <label>"Assignment"
                    <select on:change=edit(|f, v| f.assignment = v)>
                        <For each=assignment_refs key=|r| r.clone() let:reference>
                            {
                                let (value, text) = (reference.clone(), reference.clone());
                                view! { <option value=value selected=move || form.with(|f| f.as_ref().is_some_and(|f| f.assignment == reference))>{text}</option> }
                            }
                        </For>
                    </select>
                </label>
                <label>"Job"
                    <select on:change=edit(|f, v| f.job = if v == "reach" { api::JobKind::Reach as i32 } else { api::JobKind::Assess as i32 })>
                        <option value="assess" selected=move || form.with(|f| f.as_ref().is_some_and(|f| f.job != api::JobKind::Reach as i32))>"Assess"</option>
                        <option value="reach" selected=move || form.with(|f| f.as_ref().is_some_and(|f| f.job == api::JobKind::Reach as i32))>"Reach"</option>
                    </select>
                </label>
                <label>"Schedule"
                    <select on:change=edit(|f, v| f.cron = v == "cron")>
                        <option value="every" selected=move || form.with(|f| f.as_ref().is_some_and(|f| !f.cron))>"Fixed interval"</option>
                        <option value="cron" selected=move || form.with(|f| f.as_ref().is_some_and(|f| f.cron))>"Cron expression"</option>
                    </select>
                </label>
                <Show when=move || form.with(|f| f.as_ref().is_some_and(|f| f.cron))
                    fallback=move || view! {
                        <label>"Every"
                            <input prop:value=read(|f| f.every.clone()) placeholder="15m, 1h30m, 1d or seconds"
                                on:input=edit(|f, v| f.every = v) />
                        </label>
                    }>
                    <label>"Cron expression"
                        <input prop:value=read(|f| f.expression.clone()) placeholder="minute hour day month weekday, e.g. */15 * * * *"
                            on:input=edit(|f, v| f.expression = v) />
                    </label>
                    <label>"Timezone"
                        <input prop:value=read(|f| f.timezone.clone()) placeholder="UTC, Europe/Paris…"
                            on:input=edit(|f, v| f.timezone = v) />
                    </label>
                </Show>
                <label>"Runner pool"
                    <input prop:value=read(|f| f.pool.clone()) placeholder="empty: each host's own pool"
                        on:input=edit(|f, v| f.pool = v) />
                </label>
                <label>"Enabled "
                    <input type="checkbox" prop:checked=move || form.with(|f| f.as_ref().is_some_and(|f| f.enabled))
                        on:change=move |ev| { let on = event_target_checked(&ev); form.update(|f| if let Some(f) = f { f.enabled = on }); } />
                </label>
                <p class="muted">"Missed runs are never replayed: after downtime, only the current slot runs."</p>
                <div class="toolbar">
                    <button class="primary" on:click=move |_| save()>"Save"</button>
                    <button on:click=move |_| form.set(None)>"Cancel"</button>
                </div>
            </div>
        </Show>
        <table>
            <thead><tr>
                <th>"Name"</th><th>"Schedule"</th><th>"Assignment"</th><th>"Job"</th><th>"Pool"</th>
                <th>"State"</th><th>"Next run"</th><th title="Jobs of the last hour">"Last hour"</th><th>"Author"</th><th></th>
            </tr></thead>
            <tbody>
                <For each=move || orders.get() key=|o| (o.order_id.clone(), o.rev.clone()) let:order>
                    {
                        let order_id = order.order_id.clone();
                        let (toggle, to_edit, to_delete) = (order.clone(), order.clone(), order_id.clone());
                        let enabled = order.enabled;
                        let next = {
                            let order = order.clone();
                            move || match (enabled, next_fire_ms(&order, now.get())) {
                                (false, _) => "—".to_string(),
                                (true, Some(ms)) => format_ms(ms),
                                (true, None) => "per cron".to_string(),
                            }
                        };
                        let activity = {
                            let order_id = order_id.clone();
                            move || match recent.with(|r| r.get(&order_id).copied()) {
                                None => view! { <span class="muted">"no job"</span> }.into_any(),
                                Some(r) => view! {
                                    <span title=format!("last created {}", format_ms(r.last))>
                                        {format!("{} job(s)", r.jobs)}
                                        {(r.ok > 0).then(|| view! { " " <span class="badge compliant">{r.ok}</span> })}
                                        {(r.bad > 0).then(|| view! { " " <span class="badge not_compliant">{r.bad}</span> })}
                                    </span>
                                }.into_any(),
                            }
                        };
                        view! {
                            <tr>
                                <td><b>{order_id.clone()}</b></td>
                                <td><code>{schedule_text(&order)}</code></td>
                                <td>{order.assignment.clone()}</td>
                                <td>{job_label(order.job)}</td>
                                <td class="muted">{if order.pool.is_empty() { "host's".to_string() } else { order.pool.clone() }}</td>
                                <td>{if enabled {
                                    view! { <span class="badge compliant">"Enabled"</span> }.into_any()
                                } else {
                                    view! { <span class="badge unknown">"Disabled"</span> }.into_any()
                                }}</td>
                                <td><span class="ts">{next}</span></td>
                                <td>{activity}</td>
                                <td class="muted">{order.author.clone()}</td>
                                <td>
                                    <button on:click=move |_| set_enabled(toggle.clone(), !enabled)>
                                        {if enabled { "Disable" } else { "Enable" }}
                                    </button>
                                    <button on:click=move |_| { message.set(None); form.set(Some(OrderForm::from_order(&to_edit))) }>"Edit"</button>
                                    <button on:click=move |_| {
                                        let order_id = to_delete.clone();
                                        if confirm(&format!("Delete the CronJob {order_id}? Jobs already queued still run.")) {
                                            remove(session, message, order_id, reload);
                                        }
                                    }>"Delete"</button>
                                </td>
                            </tr>
                        }
                    }
                </For>
            </tbody>
        </table>
        <Show when=move || orders.with(Vec::is_empty) && error.with(Option::is_none)>
            <p class="muted">"No CronJob yet."</p>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_both_ways() {
        assert_eq!(parse_duration("300"), Ok(300));
        assert_eq!(parse_duration("15m"), Ok(900));
        assert_eq!(parse_duration("1h30m"), Ok(5_400));
        assert_eq!(parse_duration(" 1d "), Ok(86_400));
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("1h30").is_err());
        assert!(parse_duration("").is_err());
        assert_eq!(format_duration(5_400), "1h30m");
        assert_eq!(format_duration(600), "10m");
        assert_eq!(format_duration(90_061), "1d1h1m1s");
    }

    #[test]
    fn form_round_trips_an_order() {
        let order = api::Order {
            order_id: "web-assess".into(),
            schedule: Some(api::order::Schedule::Cron(api::CronSchedule {
                expression: "*/15 * * * *".into(),
                timezone: "Europe/Paris".into(),
            })),
            assignment: "ui/web".into(),
            job: api::JobKind::Reach as i32,
            pool: "dmz".into(),
            enabled: false,
            author: "admin".into(),
            rev: "3-abc".into(),
        };
        let form = OrderForm::from_order(&order);
        assert!(form.existing && form.cron);
        let back = form.to_order().unwrap();
        assert_eq!(back.schedule, order.schedule);
        assert_eq!(
            (back.job, back.enabled, back.pool.as_str()),
            (order.job, false, "dmz")
        );
        // author and revision are the backend's
        assert!(back.author.is_empty() && back.rev.is_empty());

        let mut form = OrderForm::new("ui/web".into());
        form.order_id = "a:b".into();
        assert!(form.to_order().is_err());
        form.order_id = "nightly".into();
        form.every = "1h".into();
        assert_eq!(
            form.to_order().unwrap().schedule,
            Some(api::order::Schedule::EverySeconds(3_600))
        );
    }

    #[test]
    fn interval_orders_fire_on_slots() {
        let order = api::Order {
            schedule: Some(api::order::Schedule::EverySeconds(600)),
            ..Default::default()
        };
        assert_eq!(next_fire_ms(&order, 1_000_000.0), Some(1_200_000));
        let cron = api::Order {
            schedule: Some(api::order::Schedule::Cron(Default::default())),
            ..Default::default()
        };
        assert_eq!(next_fire_ms(&cron, 0.0), None);
    }
}
