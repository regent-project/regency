//! gRPC-Web client of backend (D14, D17) and the session token.

use tonic::Request;
use tonic_web_wasm_client::Client;

pub mod api {
    tonic::include_proto!("regency.v1");
}

pub type Regency = api::regency_client::RegencyClient<Client>;

const TOKEN_KEY: &str = "regency.token";

pub fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

pub fn saved_token() -> Option<String> {
    storage()?.get_item(TOKEN_KEY).ok().flatten()
}

pub fn save_token(token: Option<&str>) {
    if let Some(storage) = storage() {
        let _ = match token {
            Some(token) => storage.set_item(TOKEN_KEY, token),
            None => storage.remove_item(TOKEN_KEY),
        };
    }
}

/// Backend URL from `<meta name="regency-backend">`, else this origin.
pub fn backend_url() -> String {
    let document = web_sys::window().and_then(|w| w.document());
    let configured = document
        .as_ref()
        .and_then(|d| {
            d.query_selector("meta[name=regency-backend]")
                .ok()
                .flatten()
        })
        .and_then(|e| e.get_attribute("content"))
        .filter(|c| !c.trim().is_empty());
    configured.unwrap_or_else(|| {
        web_sys::window()
            .and_then(|w| w.location().origin().ok())
            .unwrap_or_default()
    })
}

pub fn client() -> Regency {
    api::regency_client::RegencyClient::new(Client::new(backend_url()))
}

/// A request carrying the session token.
pub fn authed<T>(message: T, token: &str) -> Request<T> {
    let mut request = Request::new(message);
    if let Ok(value) = format!("Bearer {token}").parse() {
        request.metadata_mut().insert("authorization", value);
    }
    request
}

/// A client-chosen command id (D10), unique enough for retries.
pub fn command_id(kind: &str) -> String {
    format!(
        "ui-{kind}-{}-{:08x}",
        js_sys::Date::now() as u64,
        (js_sys::Math::random() * u32::MAX as f64) as u32
    )
}

/// `"2026-11-04 10:05:00"` (local time) from Unix milliseconds.
pub fn format_ms(ms: i64) -> String {
    if ms <= 0 {
        return "—".to_string();
    }
    let date = js_sys::Date::new(&wasm_bindgen_value(ms as f64));
    let pad = |v: u32| format!("{v:02}");
    format!(
        "{}-{}-{} {}:{}:{}",
        date.get_full_year(),
        pad(date.get_month() + 1),
        pad(date.get_date()),
        pad(date.get_hours()),
        pad(date.get_minutes()),
        pad(date.get_seconds())
    )
}

fn wasm_bindgen_value(v: f64) -> js_sys::wasm_bindgen::JsValue {
    js_sys::wasm_bindgen::JsValue::from_f64(v)
}
