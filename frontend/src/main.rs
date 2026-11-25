//! regency production frontend (D14): Leptos, client-side rendering, talking
//! gRPC-Web to backend.

mod admin;
mod builder;
mod canvas;
mod catalog;
mod client;
mod config;
mod cronjobs;
mod expected;
mod feeds;
mod graphedit;
mod hostdefs;
mod icons;
mod jobchart;
mod labelpicker;
// D26: the same label selectors as the services (regencylib), for previews
#[path = "../../regencylib/src/labels.rs"]
#[allow(dead_code)] // parts only the services use
mod labels;
mod live;
mod map;
mod onboarding;
mod oneshot;
mod pages;
mod runs;

use leptos::prelude::*;
use leptos_router::components::{A, Redirect, Route, Router, Routes};
use leptos_router::hooks::{use_location, use_query_map};
use leptos_router::path;

use crate::client::{save_token, saved_token, storage};

/// Session shared by every page.
#[derive(Clone, Copy)]
pub struct Session {
    pub token: RwSignal<Option<String>>,
}

impl Session {
    pub fn token(&self) -> String {
        self.token.get_untracked().unwrap_or_default()
    }

    /// Unauthenticated answers drop the session (back to the login form).
    pub fn check(&self, status: &tonic::Status) {
        if status.code() == tonic::Code::Unauthenticated {
            save_token(None);
            self.token.set(None);
        }
    }
}

/// Colour theme: `light` or `dark`, the `data-theme` attribute of `<html>`
/// (first set by `index.html` from the saved choice or the system's).
/// Canvas drawings track it to re-read their colours.
#[derive(Clone, Copy)]
pub struct Theme(pub RwSignal<&'static str>);

impl Theme {
    const KEY: &'static str = "regency.theme";

    fn load() -> Self {
        let current = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.document_element())
            .and_then(|e| e.get_attribute("data-theme"));
        Self(RwSignal::new(match current.as_deref() {
            Some("dark") => "dark",
            _ => "light",
        }))
    }

    fn toggle(self) {
        let next = if self.0.get_untracked() == "dark" {
            "light"
        } else {
            "dark"
        };
        if let Some(root) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.document_element())
        {
            let _ = root.set_attribute("data-theme", next);
        }
        if let Some(storage) = storage() {
            let _ = storage.set_item(Self::KEY, next);
        }
        self.0.set(next);
    }
}

/// Sidebar folded to icons, remembered by the browser.
const NAV_KEY: &str = "regency.nav";

#[component]
fn NavItem(href: &'static str, icon: &'static str, label: &'static str) -> impl IntoView {
    view! {
        <A href=href attr:title=label>
            <span class="nav-icon"><icons::Icon name=icon /></span>
            <span class="nav-label">{label}</span>
        </A>
    }
}

/// The masthead's fleet counts: hosts per state, each a link to the Hosts
/// page filtered on that state (`/hosts?state=…`; the one shown is marked).
#[component]
fn FleetStats() -> impl IntoView {
    let feeds = expect_context::<feeds::Feeds>();
    // in legend order, coloured like the map's dots
    let fleet = Memo::new(move |_| {
        let mut counts = feeds.fleet();
        counts.sort_by_key(|(state, _)| (map::tone(state).rank(), state.clone()));
        counts
    });
    let location = use_location();
    let query = use_query_map();
    let shown_state = Memo::new(move |_| {
        (location.pathname.get() == "/hosts")
            .then(|| query.with(|q| q.get("state")))
            .flatten()
    });
    view! {
    <div class="stats" title="Hosts by state">
        <A href="/hosts" attr:class="stats-label" attr:title="All hosts">"Fleet"</A>
        <span class="stats-total">
            {move || if feeds.ready.get() {
                format!("{} hosts", fleet.with(|c| c.iter().map(|(_, n)| n).sum::<usize>()))
            } else {
                "loading…".to_string()
            }}
        </span>
        // each state: a link to the Hosts page filtered on it
        {move || fleet.get().into_iter().map(|(state, n)| {
            let href = format!("/hosts?state={}", js_sys::encode_uri_component(&state));
            let title = format!("Show the {} hosts", state.replace('_', " "));
            let current = {
                let state = state.clone();
                move || shown_state.get().as_deref() == Some(state.as_str())
            };
            let style = format!("--tone: {}", map::tone(&state).css());
            view! {
                <A href=href attr:class=move || if current() { "stats-count current" } else { "stats-count" }
                    attr:title=title attr:style=style>
                    <i class="dot" aria-hidden="true"></i>
                    <span class="state">{state.replace('_', " ")}</span>
                    <b class="n">{n}</b>
                </A>
            }
        }).collect::<Vec<_>>()}
        <Show when=move || feeds.error.with(Option::is_some)>
            <span class="stats-warning" title=move || feeds.error.get().unwrap_or_default()><icons::Icon name="warning" />"Reconnecting…"</span>
        </Show>
    </div>    }
}

/// The signed-in user (avatar + name) and a dropdown with "Log out".
#[component]
fn UserMenu() -> impl IntoView {
    let session = expect_context::<Session>();
    let open = RwSignal::new(false);
    view! {
        <div class="user-menu">
            <button class="user" aria-haspopup="menu" aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)>
                <pages::WhoAmI />
                <icons::Icon name="chevron-down" />
            </button>
            <Show when=move || open.get()>
                // a click anywhere else closes the menu
                <div class="menu-backdrop" on:click=move |_| open.set(false)></div>
                <div class="dropdown" role="menu">
                    <button role="menuitem" on:click=move |_| {
                        save_token(None);
                        session.token.set(None);
                    }><icons::Icon name="logout" />"Log out"</button>
                </div>
            </Show>
        </div>
    }
}

#[component]
fn App() -> impl IntoView {
    let session = Session {
        token: RwSignal::new(saved_token()),
    };
    provide_context(session);
    let theme = Theme::load();
    provide_context(theme);
    // live feeds of every host and of the last hour's jobs, for the
    // masthead and the pages (run by feeds::FeedsRunner once logged in)
    let feeds = feeds::Feeds::new();
    provide_context(feeds);
    let nav_folded = RwSignal::new(
        storage()
            .and_then(|s| s.get_item(NAV_KEY).ok().flatten())
            .is_some_and(|v| v == "folded"),
    );
    let fold_nav = move |_| {
        let folded = !nav_folded.get_untracked();
        if let Some(storage) = storage() {
            let _ = storage.set_item(NAV_KEY, if folded { "folded" } else { "open" });
        }
        nav_folded.set(folded);
    };
    view! {
        <Show when=move || session.token.get().is_some() fallback=pages::Login>
            <Router>
                <div class="app" class:nav-folded=move || nav_folded.get()>
                <aside class="sidebar">
                    <A href="/" attr:class="sidebar-brand" attr:title="regency">
                        <img src="/regent-logo.png" alt="" /><span class="nav-label">"regency"</span>
                    </A>
                    <nav>
                        <div class="nav-section">"Menu"</div>
                        <NavItem href="/" icon="dashboard" label="Live view" />
                        <NavItem href="/hosts" icon="server" label="Hosts" />
                        <NavItem href="/states" icon="clipboard" label="States" />
                        <NavItem href="/assignments" icon="layers" label="Assignments" />
                        <div class="nav-section">"Activity"</div>
                        <NavItem href="/jobs" icon="clock" label="Jobs" />
                        <NavItem href="/cronjobs" icon="calendar" label="CronJob" />
                        <div class="nav-section">"Administration"</div>
                        <NavItem href="/config/sources" icon="branch" label="Sources" />
                        <NavItem href="/config/services" icon="box" label="Services" />
                        <NavItem href="/config/users" icon="users" label="Users" />
                        <NavItem href="/config/policies" icon="key" label="Policies" />
                        <NavItem href="/config/security" icon="shield" label="Security" />
                        <NavItem href="/config/settings" icon="sliders" label="Settings" />
                    </nav>
                </aside>
                <div class="content">
                <header class="topbar">
                    <button class="icon-button" title="Fold the menu" aria-label="Fold the menu" on:click=fold_nav>
                        <icons::Icon name="menu" />
                    </button>
                    <feeds::FeedsRunner />
                    <FleetStats />
                    <div class="stats jobs-stats">
                        <A href="/jobs" attr:class="stats-label" attr:title="All jobs">"Jobs in progress"</A>
                        <jobchart::JobChart jobs=feeds.jobs compact=true />
                    </div>
                    <span class="spacer"></span>
                    <button class="icon-button round" title="Light / dark theme" aria-label="Light / dark theme"
                        on:click=move |_| theme.toggle()>
                        {move || if theme.0.get() == "dark" {
                            view! { <icons::Icon name="sun" /> }.into_any()
                        } else {
                            view! { <icons::Icon name="moon" /> }.into_any()
                        }}
                    </button>
                    <UserMenu />
                </header>
                <main>
                    <Routes fallback=|| view! { <p>"Not found."</p> }>
                        <Route path=path!("/") view=map::Map />
                        <Route path=path!("/hosts") view=pages::Hosts />
                        <Route path=path!("/hosts/:id") view=pages::HostPage />
                        // D29: inventories are gone, assignments select their hosts
                        <Route path=path!("/inventories") view=catalog::AssignmentsConfig />
                        <Route path=path!("/states") view=catalog::ExpectedStatesConfig />
                        <Route path=path!("/assignments") view=catalog::AssignmentsConfig />
                        <Route path=path!("/jobs") view=pages::Jobs />
                        <Route path=path!("/cronjobs") view=cronjobs::CronJobs />
                        // host definitions are on the Hosts pages now
                        <Route path=path!("/config") view=|| view! { <Redirect path="/hosts" /> } />
                        <Route path=path!("/config/hosts") view=|| view! { <Redirect path="/hosts" /> } />
                        // former addresses (bookmarks)
                        <Route path=path!("/config/inventories") view=catalog::AssignmentsConfig />
                        <Route path=path!("/config/expected-states") view=catalog::ExpectedStatesConfig />
                        <Route path=path!("/config/assignments") view=catalog::AssignmentsConfig />
                        <Route path=path!("/config/sources") view=config::SourcesConfig />
                        <Route path=path!("/config/services") view=config::ServicesConfig />
                        <Route path=path!("/config/users") view=admin::UsersConfig />
                        <Route path=path!("/config/policies") view=admin::PoliciesConfig />
                        <Route path=path!("/config/security") view=config::SecurityConfig />
                        <Route path=path!("/config/settings") view=config::SettingsConfig />
                    </Routes>
                </main>
                </div>
                </div>
            </Router>
        </Show>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
