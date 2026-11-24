//! Line icons of the console (sidebar, header): 24×24, drawn with
//! `currentColor` strokes, in the plain outline style of admin dashboards.

use leptos::prelude::*;

/// The SVG body of an icon.
fn paths(name: &str) -> &'static str {
    match name {
        // four tiles: the main (dashboard) page
        "dashboard" => {
            r#"<rect x="3.5" y="3.5" width="7" height="7" rx="1.5"/><rect x="13.5" y="3.5" width="7" height="7" rx="1.5"/><rect x="3.5" y="13.5" width="7" height="7" rx="1.5"/><rect x="13.5" y="13.5" width="7" height="7" rx="1.5"/>"#
        }
        "server" => {
            r#"<rect x="3.5" y="4" width="17" height="7" rx="1.5"/><rect x="3.5" y="13" width="17" height="7" rx="1.5"/><path d="M7.5 7.5h.01M7.5 16.5h.01M11 7.5h2M11 16.5h2"/>"#
        }
        "clipboard" => {
            r#"<path d="M9 4.5H7a2 2 0 0 0-2 2V19a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V6.5a2 2 0 0 0-2-2h-2"/><rect x="9" y="3" width="6" height="3" rx="1"/><path d="m9 13.5 2 2 4-4"/>"#
        }
        "layers" => {
            r#"<path d="M12 3.5 20.5 8 12 12.5 3.5 8z"/><path d="m3.5 12 8.5 4.5 8.5-4.5"/><path d="m3.5 16 8.5 4.5 8.5-4.5"/>"#
        }
        // a calendar with a repeat arrow: recurring jobs (CronJob)
        "calendar" => {
            r#"<rect x="3.5" y="5" width="17" height="15" rx="2"/><path d="M3.5 9.5h17M8 3v4M16 3v4"/><path d="M15 14.5a3 3 0 1 1-1-2.2M15 11.5v1.8h-1.8"/>"#
        }
        // two people: accounts (Users)
        "users" => {
            r#"<circle cx="9" cy="8.5" r="3.5"/><path d="M3 20c0-3.3 2.7-6 6-6s6 2.7 6 6"/><path d="M15.5 5.2a3.5 3.5 0 0 1 0 6.6M17.5 14.4c2 .8 3.5 2.9 3.5 5.6"/>"#
        }
        // a key: authorization policies (Policies)
        "key" => {
            r#"<circle cx="8" cy="15" r="4.5"/><path d="m11.2 11.8 8.8-8.8M16.5 6.5l2.5 2.5M14 9l2 2"/>"#
        }
        "clock" => r#"<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/>"#,
        "edit" => {
            r#"<path d="M4 20h4L18.5 9.5a2.12 2.12 0 0 0-3-3L5 17z"/><path d="m13.5 8.5 3 3"/>"#
        }
        "branch" => {
            r#"<circle cx="7" cy="5.5" r="2"/><circle cx="7" cy="18.5" r="2"/><circle cx="17" cy="7.5" r="2"/><path d="M7 7.5v9M17 9.5v.5a4 4 0 0 1-4 4h-2a4 4 0 0 0-4 4"/>"#
        }
        "box" => {
            r#"<rect x="4" y="4" width="16" height="16" rx="2"/><rect x="9" y="9" width="6" height="6" rx="1"/>"#
        }
        "shield" => {
            r#"<path d="M12 3.5 19 6v5c0 4.5-3 8-7 9.5-4-1.5-7-5-7-9.5V6z"/><path d="m9 12 2 2 4-4"/>"#
        }
        "sliders" => {
            r#"<path d="M4 7h3M11 7h9M4 17h9M17 17h3"/><circle cx="9" cy="7" r="2"/><circle cx="15" cy="17" r="2"/>"#
        }
        "menu" => r#"<path d="M4 7h16M4 12h16M4 17h16"/>"#,
        "sun" => {
            r#"<circle cx="12" cy="12" r="4"/><path d="M12 2.5v2M12 19.5v2M4.6 4.6 6 6M18 18l1.4 1.4M2.5 12h2M19.5 12h2M4.6 19.4 6 18M18 6l1.4-1.4"/>"#
        }
        "moon" => r#"<path d="M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z"/>"#,
        "chevron-down" => r#"<path d="m6 9 6 6 6-6"/>"#,
        "logout" => {
            r#"<path d="M15 4h3a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-3"/><path d="m10 8-4 4 4 4M6 12h10"/>"#
        }
        "warning" => r#"<path d="M12 4 21 19.5H3z"/><path d="M12 10v4M12 17h.01"/>"#,
        _ => "",
    }
}

/// An icon (`name` as in [`paths`]), sized by CSS (`.icon-svg`, 20 px).
#[component]
pub fn Icon(name: &'static str) -> impl IntoView {
    view! {
        <svg class="icon-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"
            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" inner_html=paths(name)></svg>
    }
}
