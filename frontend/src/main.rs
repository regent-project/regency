//! regency production frontend (D14): Leptos, client-side rendering, talking
//! gRPC-Web to backend.

mod builder;
mod canvas;
mod client;
mod config;
mod expected;
mod jobchart;
mod map;
mod oneshot;
mod pages;

use leptos::prelude::*;
use leptos_router::components::{A, Route, Router, Routes};
use leptos_router::path;

use crate::client::{save_token, saved_token};

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

#[component]
fn App() -> impl IntoView {
    let session = Session {
        token: RwSignal::new(saved_token()),
    };
    provide_context(session);
    view! {
        <Show when=move || session.token.get().is_some() fallback=pages::Login>
            <Router>
                <header>
                    <span class="brand"><img src="/regent-logo.png" alt="" />"regency"</span>
                    <A href="/">"Map"</A>
                    <A href="/hosts">"Hosts"</A>
                    <A href="/jobs">"Jobs"</A>
                    <A href="/config/hosts">"Configuration"</A>
                    <span class="spacer"></span>
                    <pages::WhoAmI />
                    <button on:click=move |_| {
                        save_token(None);
                        session.token.set(None);
                    }>"Log out"</button>
                </header>
                <main>
                    <Routes fallback=|| view! { <p>"Not found."</p> }>
                        <Route path=path!("/") view=map::Map />
                        <Route path=path!("/hosts") view=pages::Hosts />
                        <Route path=path!("/hosts/:id") view=pages::HostPage />
                        <Route path=path!("/jobs") view=pages::Jobs />
                        <Route path=path!("/config") view=config::HostsConfig />
                        <Route path=path!("/config/hosts") view=config::HostsConfig />
                        <Route path=path!("/config/sources") view=config::SourcesConfig />
                        <Route path=path!("/config/services") view=config::ServicesConfig />
                        <Route path=path!("/config/security") view=config::SecurityConfig />
                        <Route path=path!("/config/settings") view=config::SettingsConfig />
                    </Routes>
                </main>
            </Router>
        </Show>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
